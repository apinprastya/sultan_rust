//! Request extractors: who is calling, from which machine, and whether this node may write
//! master data (plan Phase 4; spec 2.1 §1, §2, §4).
//!
//! - [`RegisterCtx`]: the register and branch behind `X-Register-Token`. Register and branch are
//!   taken **from the token only**; a `branch_id`/`register_id` in a request body is ignored
//!   (spec 2.1 §2).
//! - [`UserSession`]: the bearer session. A register-bound session (PIN login) also needs the
//!   device token of *its* register, so a revoked device is `401` even with a valid session.
//! - [`RegisterSession`]: a PIN session at the register that sends the request (cashier sessions,
//!   overrides; spec 2.1 §5, §6). A back-office session is `403 register_session_required`.
//! - [`CatalogReader`]: catalog reads (spec 2.2 §11): a user session **or** a device token alone.
//! - [`RequireHq`]: refuses master-data writes on a branch node (`403 read_only_replica`).
//! - [`ApiJson`] / [`ApiQuery`]: `axum::Json` / `axum::extract::Query` whose rejections are
//!   problem+json.

use axum::extract::{FromRequest, FromRequestParts};
use axum::http::header::AUTHORIZATION;
use axum::http::request::Parts;
use sultan_core::access::{PermissionSet, effective_permissions};
use sultan_core::ids::{BranchId, RegisterId};
use sultan_db::auth::{self, AuthSession};
use sultan_db::devices::{self, DeviceIdentity};
use sultan_db::users::{self, User};
use sultan_db::{Db, TokenHash, roles};

use crate::error::ApiError;
use crate::state::AppState;
use crate::token;

/// The header carrying the device token (spec 2.1 §2).
pub const REGISTER_TOKEN_HEADER: &str = "x-register-token";

/// A JSON request body; malformed bodies become `invalid_body` problems.
#[derive(Debug, Clone, Copy, Default, FromRequest)]
#[from_request(via(axum::Json), rejection(ApiError))]
pub struct ApiJson<T>(pub T);

/// Query parameters; a parameter of the wrong type becomes an `invalid_query` problem.
#[derive(Debug, Clone, Copy, Default, FromRequestParts)]
#[from_request(via(axum::extract::Query), rejection(ApiError))]
pub struct ApiQuery<T>(pub T);

/// The paired register making the request (spec 2.1 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RegisterCtx {
    /// The register, from the token.
    pub register: RegisterId,
    /// The register's branch, from the token.
    pub branch: BranchId,
}

/// Resolves `X-Register-Token`: `Ok(None)` when the header is absent, `401` when it is present
/// but unknown or revoked (or the register is archived).
async fn device_from_parts(parts: &Parts, db: &Db) -> Result<Option<DeviceIdentity>, ApiError> {
    let Some(value) = parts.headers.get(REGISTER_TOKEN_HEADER) else {
        return Ok(None);
    };
    let token = value
        .to_str()
        .map_err(|_| ApiError::invalid_device_token())?;
    devices::find_device(db, &token::hash(token))
        .await?
        .map(Some)
        .ok_or_else(ApiError::invalid_device_token)
}

impl FromRequestParts<AppState> for RegisterCtx {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let device = device_from_parts(parts, &state.db)
            .await?
            .ok_or_else(ApiError::invalid_device_token)?;
        Ok(Self {
            register: device.register,
            branch: device.branch,
        })
    }
}

/// An authenticated user session (spec 2.1 §4).
///
/// Extraction checks, in order: the bearer token names an active session (not revoked, not
/// idle-expired); a register-bound session comes with its own register's valid device token; the
/// user is still active (a deactivation synced from HQ applies on the next request). It then
/// moves the idle expiry forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserSession {
    /// Hash of the bearer token (the session's key).
    pub token_hash: TokenHash,
    /// The stored session, as it was before this request touched it.
    pub session: AuthSession,
    /// The logged-in user.
    pub user: User,
}

impl UserSession {
    /// The user's effective permissions at the session's branch, loaded fresh so role changes
    /// apply on the next request (spec 2.1 §3–4).
    ///
    /// # Errors
    /// The database error.
    pub async fn permissions(&self, db: &Db) -> Result<PermissionSet, ApiError> {
        let access = roles::load_user_access(db, self.user.id).await?;
        Ok(effective_permissions(
            &access.assignments,
            &access.roles,
            self.session.branch,
        ))
    }
}

fn bearer(parts: &Parts) -> Option<&str> {
    let value = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    let token = token.trim();
    (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty()).then_some(token)
}

impl FromRequestParts<AppState> for UserSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let token_hash = token::hash(bearer(parts).ok_or_else(ApiError::invalid_session)?);
        let now = state.now();
        let session = auth::find_session(&state.db, &token_hash)
            .await?
            .filter(|s| s.is_active(now))
            .ok_or_else(ApiError::invalid_session)?;
        if let Some(register) = session.register {
            let device = device_from_parts(parts, &state.db).await?;
            if device.map(|d| d.register) != Some(register) {
                return Err(ApiError::invalid_device_token());
            }
        }
        let user = users::get_user(&state.db, session.user)
            .await?
            .filter(|u| u.active)
            .ok_or_else(ApiError::invalid_session)?;
        let expires_at = now.saturating_add_millis(state.session_idle_ms);
        if !auth::touch_session(&state.db, &token_hash, now, expires_at).await? {
            return Err(ApiError::invalid_session());
        }
        Ok(Self {
            token_hash,
            session,
            user,
        })
    }
}

/// A PIN session bound to the register that sends the request (spec 2.1 §4–6): the user at the
/// till, and the register and branch from the device token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSession {
    /// The register and its branch (from the device token).
    pub register: RegisterCtx,
    /// The logged-in user.
    pub user: UserSession,
}

impl FromRequestParts<AppState> for RegisterSession {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let user = UserSession::from_request_parts(parts, state).await?;
        let Some(register) = user.session.register else {
            return Err(ApiError::register_session_required());
        };
        // `UserSession` has already checked that the device token belongs to this register.
        Ok(Self {
            register: RegisterCtx {
                register,
                branch: user.session.branch,
            },
            user,
        })
    }
}

/// Who reads the catalog (spec 2.2 §11, §12): any user session, or a paired register's device
/// token without a session. A bearer token, when sent, must be valid (`401 invalid_session`);
/// without one, `X-Register-Token` must be (`401 invalid_device_token`). Neither is
/// `401 invalid_session`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogReader {
    /// A register with its device token only.
    Device(RegisterCtx),
    /// A user session (at a register or in back office); boxed, it is much larger.
    User(Box<UserSession>),
}

impl CatalogReader {
    /// The branch of the register behind the request (from the device token), if any: the
    /// branch whose assortment applies (spec 2.2 §10).
    #[must_use]
    pub fn register_branch(&self) -> Option<BranchId> {
        match self {
            Self::Device(ctx) => Some(ctx.branch),
            Self::User(user) => user.session.register.map(|_| user.session.branch),
        }
    }

    /// The user session, if any.
    #[must_use]
    pub fn user(&self) -> Option<&UserSession> {
        match self {
            Self::Device(_) => None,
            Self::User(user) => Some(user.as_ref()),
        }
    }
}

impl FromRequestParts<AppState> for CatalogReader {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if parts.headers.contains_key(AUTHORIZATION) {
            return Ok(Self::User(Box::new(
                UserSession::from_request_parts(parts, state).await?,
            )));
        }
        if parts.headers.contains_key(REGISTER_TOKEN_HEADER) {
            return Ok(Self::Device(
                RegisterCtx::from_request_parts(parts, state).await?,
            ));
        }
        Err(ApiError::invalid_session())
    }
}

/// Guard for writes to HQ-owned master data (spec 2.1 §1). Put it in the handler's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RequireHq;

impl FromRequestParts<AppState> for RequireHq {
    type Rejection = ApiError;

    async fn from_request_parts(_parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        if state.node_role.owns_master_data() {
            Ok(Self)
        } else {
            Err(ApiError::read_only_replica())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn bearer_parsing() {
        let cases = [
            (Some("Bearer abc"), Some("abc")),
            (Some("bearer  abc "), Some("abc")),
            (Some("Basic abc"), None),
            (Some("Bearer "), None),
            (Some("Bearer"), None),
            (None, None),
        ];
        for (header, expected) in cases {
            let mut request = Request::builder();
            if let Some(h) = header {
                request = request.header(AUTHORIZATION, h);
            }
            let (parts, ()) = request.body(()).unwrap().into_parts();
            assert_eq!(bearer(&parts), expected, "{header:?}");
        }
    }
}
