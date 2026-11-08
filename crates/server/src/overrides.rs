//! The manager-override guard for endpoints protected by limits (spec 2.1 §6, plan Phase 6).
//!
//! An endpoint whose action is limited (a discount, a refund amount, a void...) calls
//! [`authorize`] with the caller's permissions, the action, its value and the document or cart it
//! applies to (`context_ref`):
//!
//! 1. Allowed by the caller's own permissions → no override needed; an `X-Override` header is
//!    ignored and **not** used up.
//! 2. Not overridable (back-office permissions) → `403 permission_denied`.
//! 3. Needs an override and no `X-Override` → `403 needs_override` with `permission` and the
//!    caller's `limit`, so the register opens the manager PIN dialog (`POST /overrides`).
//! 4. Needs an override and `X-Override` is present → the token is used up atomically if it was
//!    approved for exactly this (action, value, register, `context_ref`) and this requester, is
//!    unused and unexpired; otherwise `403 invalid_override`. The use is audited as
//!    `override.use` in the caller's transaction, and the approver is returned for the
//!    document's `approved_by` (spec 2.1 §2).
//!
//! A token presented by another user than the one it was approved for is refused. Every refusal
//! returns an error, so the caller's transaction rolls back and the token is **not** used up;
//! only a successful action (whose transaction commits) consumes it.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use serde_json::json;
use sultan_core::access::{AccessError, Decision, Permission, PermissionSet, Requested};
use sultan_core::ids::UserId;
use sultan_db::Tx;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::overrides::{self, OverrideUse};

use crate::error::ApiError;
use crate::extract::RegisterSession;
use crate::state::AppState;
use crate::token;

/// The header carrying an override token (spec 2.1 §6).
pub const OVERRIDE_HEADER: &str = "x-override";

/// Lifetime of an override token: 2 minutes (spec 2.1 §6).
pub const OVERRIDE_TTL_MS: i64 = 2 * 60 * 1_000;

/// The `X-Override` header, if sent. Never rejects.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverrideHeader(pub Option<String>);

impl<S: Send + Sync> FromRequestParts<S> for OverrideHeader {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .headers
                .get(OVERRIDE_HEADER)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.trim().to_owned())
                .filter(|v| !v.is_empty()),
        ))
    }
}

/// The value as stored and returned: basis points or minor units.
#[must_use]
pub const fn requested_value(value: Requested) -> i64 {
    match value {
        Requested::Pct(bp) => bp as i64,
        Requested::Amount(amount) => amount.minor(),
    }
}

/// One limited action to authorize.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Action<'a> {
    /// The permission.
    pub permission: Permission,
    /// The requested value, for limited permissions.
    pub value: Option<Requested>,
    /// The document or cart the action applies to.
    pub context_ref: &'a str,
}

/// Authorizes `action` for the caller at the register (see the module docs). Returns the
/// approving manager when an override was used, `None` when the caller's own permissions suffice.
///
/// # Errors
/// `403 permission_denied` / `needs_override` / `invalid_override`, or the database error.
pub async fn authorize(
    state: &AppState,
    tx: &mut Tx,
    caller: &RegisterSession,
    permissions: &PermissionSet,
    action: Action<'_>,
    header: &OverrideHeader,
) -> Result<Option<UserId>, ApiError> {
    let Action {
        permission,
        value,
        context_ref,
    } = action;
    let limit = match permissions.check(permission, value) {
        Decision::Allowed => return Ok(None),
        Decision::Denied => return Err(AccessError::PermissionDenied { permission }.into()),
        Decision::NeedsOverride { limit } => limit,
    };
    let Some(presented) = &header.0 else {
        return Err(AccessError::NeedsOverride { permission, limit }.into());
    };
    let now = state.now();
    let hash = token::hash(presented);
    let use_ = OverrideUse {
        token_hash: &hash,
        permission,
        value,
        register: caller.register.register,
        context_ref,
    };
    let requester = caller.user.user.id;
    let approval = overrides::consume(&mut *tx, &use_, now)
        .await?
        .filter(|a| a.requester == requester)
        .ok_or_else(ApiError::invalid_override)?;

    let mut entry = AuditEntry::new(now, "override.use");
    entry.branch = Some(caller.register.branch);
    entry.register = Some(caller.register.register);
    entry.user = Some(requester);
    entry.target = Some(format!("context:{context_ref}"));
    entry.after = Some(json!({
        "requester_id": requester.to_string(),
        "approver_id": approval.approver.to_string(),
        "action": permission.as_str(),
        "value": value.map(requested_value),
        "register_id": caller.register.register.to_string(),
        "context_ref": context_ref,
    }));
    audit::append(&mut *tx, &entry).await?;
    Ok(Some(approval.approver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use sultan_core::money::Money;

    #[tokio::test]
    async fn header_is_optional_and_trimmed() {
        let cases = [(Some(" tok "), Some("tok")), (Some(""), None), (None, None)];
        for (header, expected) in cases {
            let mut request = Request::builder();
            if let Some(h) = header {
                request = request.header(OVERRIDE_HEADER, h);
            }
            let (mut parts, ()) = request.body(()).unwrap().into_parts();
            let got = OverrideHeader::from_request_parts(&mut parts, &())
                .await
                .unwrap();
            assert_eq!(got.0.as_deref(), expected, "{header:?}");
        }
    }

    #[test]
    fn values_are_basis_points_or_minor_units() {
        assert_eq!(requested_value(Requested::Pct(1_500)), 1_500);
        assert_eq!(
            requested_value(Requested::Amount(Money::from_minor(25_000))),
            25_000
        );
    }
}
