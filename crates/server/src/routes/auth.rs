//! Login and logout (spec 2.1 §4).
//!
//! - Register: `POST /auth/pin {user_id, pin}` with `X-Register-Token` → a session bound to
//!   (user, register). The branch is the register's branch, from the token.
//! - Back office: `POST /auth/password {username, password, branch_id?}` → a session without a
//!   register. `branch_id` picks the branch the session acts at; when omitted, the first branch (by
//!   code) the user is assigned to.
//! - `GET /auth/me`: the session's user, branch and permission keys (at the branch and for all
//!   branches), so the back office can hide what the user may not do.
//! - Lockout: 5 consecutive failures lock the user on that branch for 5 minutes
//!   (`core::access::LoginAttempts`); the 5th failure and every attempt while locked answer
//!   `423 locked_out`. Failures and lockouts are audited (spec 2.1 §7).
//! - Unknown user, inactive user, user without that secret, or user without a role at the branch:
//!   `401 invalid_credentials`, without counting a failure.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::{LoginAttempts, Permission};
use sultan_core::ids::{BranchId, RegisterId, UserId};
use sultan_db::audit::{self, AuditEntry};
use sultan_db::auth::{self, AuthSession};
use sultan_db::users::{self, User};
use sultan_db::{Db, org, roles};
use utoipa::ToSchema;

use super::{attempts_json, parse_id};
use crate::access::Caller;
use crate::credentials;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, RegisterCtx, UserSession};
use crate::state::AppState;
use crate::token;

/// Body of `POST /auth/pin`. Any `branch_id`/`register_id` field is ignored (spec 2.1 §2).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PinLogin {
    /// The user picked on the lock screen.
    pub user_id: String,
    /// Their PIN.
    pub pin: String,
}

/// Body of `POST /auth/password`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PasswordLogin {
    /// Back-office login name.
    pub username: String,
    /// Password.
    pub password: String,
    /// Branch to act at; defaults to the first assigned branch by code.
    #[serde(default)]
    pub branch_id: Option<String>,
}

/// A new session. The token is shown once; the server keeps its hash.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SessionResponse {
    /// Bearer token for `Authorization: Bearer …`.
    pub token: String,
    /// The user.
    pub user_id: String,
    /// The user's display name.
    pub display_name: String,
    /// The branch the session acts at.
    pub branch_id: String,
    /// The register (PIN login) or `null` (back office).
    pub register_id: Option<String>,
    /// Idle expiry (unix ms); every request moves it forward.
    pub expires_at: i64,
}

/// Whether `user` holds any non-archived role at `branch` or at all branches.
pub(crate) async fn is_assigned(db: &Db, user: UserId, branch: BranchId) -> Result<bool, ApiError> {
    let access = roles::load_user_access(db, user).await?;
    Ok(access
        .assignments
        .iter()
        .any(|a| a.scope.applies_to(branch) && access.roles.iter().any(|r| r.id == a.role)))
}

/// Checks the lockout of `user` at `branch`, verifies `secret` against `phc` and records the
/// outcome (spec 2.1 §4): a failure counts towards the lockout and is audited as
/// `{audit_prefix}.failed` / `{audit_prefix}.locked_out`; a success resets the counter. Used by
/// login and by the manager-override PIN (spec 2.1 §6: a wrong approver PIN counts towards the
/// approver's lockout).
///
/// # Errors
/// `423 locked_out`, `401 invalid_credentials`, or the database error.
pub(crate) async fn verify_with_lockout(
    state: &AppState,
    user: &User,
    branch: BranchId,
    register: Option<RegisterId>,
    (phc, secret): (String, String),
    audit_prefix: &str,
) -> Result<(), ApiError> {
    let now = state.now();
    let mut attempts = auth::load_login_attempts(&state.db, user.id, branch).await?;
    attempts.ensure_not_locked(now)?;

    if !credentials::verify_blocking(phc, secret).await? {
        let outcome = attempts.record_failure(now);
        let action = if outcome.is_err() {
            format!("{audit_prefix}.locked_out")
        } else {
            format!("{audit_prefix}.failed")
        };
        let mut entry = AuditEntry::new(now, action);
        entry.branch = Some(branch);
        entry.register = register;
        entry.user = Some(user.id);
        entry.target = Some(format!("user:{}", user.id));
        entry.after = Some(attempts_json(&attempts));
        let mut tx = state.db.begin().await?;
        auth::save_login_attempts(&mut tx, user.id, branch, &attempts).await?;
        audit::append(&mut tx, &entry).await?;
        tx.commit().await?;
        outcome?;
        return Err(ApiError::invalid_credentials());
    }

    if attempts != LoginAttempts::default() {
        attempts.record_success();
        auth::save_login_attempts(&state.db, user.id, branch, &attempts).await?;
    }
    Ok(())
}

/// Verifies the secret (with lockout) and opens a session.
async fn login(
    state: &AppState,
    user: &User,
    branch: BranchId,
    register: Option<RegisterId>,
    phc: String,
    secret: String,
) -> Result<Json<SessionResponse>, ApiError> {
    verify_with_lockout(state, user, branch, register, (phc, secret), "login").await?;
    let now = state.now();
    let token = token::generate();
    let session = AuthSession {
        token_hash: token::hash(&token),
        user: user.id,
        register,
        branch,
        created_at: now,
        last_seen: now,
        expires_at: now.saturating_add_millis(state.session_idle_ms),
        revoked_at: None,
    };
    auth::create_session(&state.db, &session).await?;
    Ok(Json(SessionResponse {
        token,
        user_id: user.id.to_string(),
        display_name: user.display_name.clone(),
        branch_id: branch.to_string(),
        register_id: register.map(|r| r.to_string()),
        expires_at: session.expires_at.unix_millis(),
    }))
}

/// `POST /api/v1/auth/pin` (register login, spec 2.1 §4).
///
/// # Errors
/// `401 invalid_device_token`, `400 invalid_body`, `401 invalid_credentials`, `423 locked_out`.
#[utoipa::path(post, path = "/api/v1/auth/pin", tag = "auth", security(("register_token" = [])),
    request_body = PinLogin,
    responses((status = 200, body = SessionResponse), (status = 401, body = crate::openapi::Problem),
        (status = 423, body = crate::openapi::Problem)))]
pub async fn pin_login(
    State(state): State<AppState>,
    ctx: RegisterCtx,
    ApiJson(request): ApiJson<PinLogin>,
) -> Result<Json<SessionResponse>, ApiError> {
    let user_id: UserId = parse_id(&request.user_id, "user_id")?;
    let user = users::get_user(&state.db, user_id)
        .await?
        .filter(|u| u.active)
        .ok_or_else(ApiError::invalid_credentials)?;
    let phc = user
        .pin_hash
        .clone()
        .ok_or_else(ApiError::invalid_credentials)?;
    if !is_assigned(&state.db, user.id, ctx.branch).await? {
        return Err(ApiError::invalid_credentials());
    }
    login(
        &state,
        &user,
        ctx.branch,
        Some(ctx.register),
        phc,
        request.pin,
    )
    .await
}

/// `POST /api/v1/auth/password` (back-office login, spec 2.1 §4).
///
/// # Errors
/// `400 invalid_body`, `401 invalid_credentials`, `423 locked_out`.
#[utoipa::path(post, path = "/api/v1/auth/password", tag = "auth", request_body = PasswordLogin,
    responses((status = 200, body = SessionResponse), (status = 401, body = crate::openapi::Problem),
        (status = 423, body = crate::openapi::Problem)))]
pub async fn password_login(
    State(state): State<AppState>,
    ApiJson(request): ApiJson<PasswordLogin>,
) -> Result<Json<SessionResponse>, ApiError> {
    let requested: Option<BranchId> = request
        .branch_id
        .as_deref()
        .map(|b| parse_id(b, "branch_id"))
        .transpose()?;
    let user = users::find_by_username(&state.db, &request.username)
        .await?
        .filter(|u| u.active)
        .ok_or_else(ApiError::invalid_credentials)?;
    let phc = user
        .password_hash
        .clone()
        .ok_or_else(ApiError::invalid_credentials)?;
    let mut branch = None;
    for candidate in org::list_branches(&state.db).await? {
        if candidate.archived || requested.is_some_and(|r| r != candidate.id) {
            continue;
        }
        if is_assigned(&state.db, user.id, candidate.id).await? {
            branch = Some(candidate.id);
            break;
        }
    }
    let branch = branch.ok_or_else(ApiError::invalid_credentials)?;
    login(&state, &user, branch, None, phc, request.password).await
}

/// The caller's session and permissions (`GET /auth/me`), so a client can show or hide what
/// the user may do. The server checks every request anyway.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct MeResponse {
    /// The user.
    pub user_id: String,
    /// Login name.
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// The branch the session acts at.
    pub branch_id: String,
    /// The register (PIN session) or `null` (back office).
    pub register_id: Option<String>,
    /// Permission keys held at the session's branch, in catalog order.
    pub permissions: Vec<&'static str>,
    /// Permission keys held through a role assigned for **all** branches (what company-wide
    /// data such as the catalog requires, spec 2.2 §12).
    pub permissions_everywhere: Vec<&'static str>,
}

/// `GET /api/v1/auth/me`: who the session is and what it may do.
///
/// # Errors
/// `401 invalid_session` / `invalid_device_token`.
#[utoipa::path(get, path = "/api/v1/auth/me", tag = "auth",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    responses((status = 200, body = MeResponse), (status = 401, body = crate::openapi::Problem)))]
pub async fn me(
    State(state): State<AppState>,
    session: UserSession,
) -> Result<Json<MeResponse>, ApiError> {
    let caller = Caller::load(&state.db, &session).await?;
    let held = |everywhere: bool| -> Vec<&'static str> {
        Permission::ALL
            .into_iter()
            .filter(|p| {
                if everywhere {
                    caller.holds_everywhere(*p)
                } else {
                    caller.permissions.contains(*p)
                }
            })
            .map(Permission::as_str)
            .collect()
    };
    Ok(Json(MeResponse {
        user_id: session.user.id.to_string(),
        username: session.user.username.clone(),
        display_name: session.user.display_name.clone(),
        branch_id: session.session.branch.to_string(),
        register_id: session.session.register.map(|r| r.to_string()),
        permissions: held(false),
        permissions_everywhere: held(true),
    }))
}

/// `POST /api/v1/auth/logout`: revokes the caller's session → `204` and pushes `session.revoked`
/// to the user's event sockets (spec 2.1 §4). Accepts `Idempotency-Key`: a retried logout gets
/// the same `204` instead of `401`.
///
/// # Errors
/// `401 invalid_session` / `invalid_device_token`, or `500 internal`.
#[utoipa::path(post, path = "/api/v1/auth/logout", tag = "auth", security(("bearer" = [])),
    responses((status = 204), (status = 401, body = crate::openapi::Problem)))]
pub async fn logout(
    State(state): State<AppState>,
    session: UserSession,
) -> Result<StatusCode, ApiError> {
    if auth::revoke_session(&state.db, &session.token_hash, state.now()).await? {
        state.events.publish(Event::session_revoked(
            session.user.id,
            Some(session.token_hash),
            "logout",
        ));
    }
    Ok(StatusCode::NO_CONTENT)
}
