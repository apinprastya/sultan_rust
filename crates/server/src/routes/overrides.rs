//! Manager override (spec 2.1 §6, plan Phase 6).
//!
//! - `POST /overrides {action, value, context_ref, approver_id, approver_pin}`: at the register
//!   (PIN session of the requester), a manager picks their name and enters their PIN. The approver
//!   must be active, have a PIN, hold a role at this branch, hold `action` at this branch with a
//!   limit covering `value`, and must not be the requester (`core::access::validate_override`). A
//!   wrong PIN counts towards the **approver's** lockout (audited `override.failed` /
//!   `override.locked_out`). The answer is a **single-use** token bound to (action, value,
//!   register, `context_ref`) and the requester, valid for 2 minutes; only its hash is stored. The
//!   approval is audited as `override.approve` (requester, approver, action, value, register,
//!   document).
//! - The token is used up through the `X-Override` header by endpoints protected by limits, via
//!   [`crate::overrides::authorize`].
//! - `POST /demo/override-protected {action, value, context_ref}`: a **demo** endpoint that only
//!   runs that guard, for tests and the Postman flow until the sales endpoints (spec 2.6) use it.
//!   It changes nothing except using up the token and auditing `override.use`. Remove it when the
//!   first real protected endpoint exists.
//!
//! `POST /overrides` deliberately ignores `Idempotency-Key`: its response carries a secret, and
//! secrets are never stored in clear (spec 2.1 §2, §4). A retry just issues another token; the
//! unused one expires after 2 minutes.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::{
    AccessError, LimitKind, Permission, Requested, effective_permissions, validate_override,
};
use sultan_core::ids::UserId;
use sultan_core::money::Money;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::overrides::{self, OverrideToken};
use sultan_db::{roles, users};
use utoipa::ToSchema;

use super::auth::{is_assigned, verify_with_lockout};
use super::{parse_id, text};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, RegisterSession};
use crate::overrides::{Action, OVERRIDE_TTL_MS, OverrideHeader, authorize, requested_value};
use crate::state::AppState;
use crate::token;

/// Body of `POST /overrides`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OverrideRequest {
    /// The permission key of the action, e.g. `sale.discount` (see `GET /permissions`).
    pub action: String,
    /// The requested value for limited permissions: basis points (`1500` = 15%) or minor units.
    /// Omit for permissions without a limit.
    #[serde(default)]
    pub value: Option<i64>,
    /// The document or cart the action applies to (1–200 characters).
    pub context_ref: String,
    /// The approving manager.
    pub approver_id: String,
    /// The manager's PIN.
    pub approver_pin: String,
}

/// An issued override token.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct OverrideResponse {
    /// Single-use token for the `X-Override` header. Shown once.
    pub token: String,
    /// The approved action.
    pub action: String,
    /// The approved value, if any.
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
    /// The cashier who asked.
    pub requester_id: String,
    /// The manager who approved.
    pub approver_id: String,
    /// End of validity (unix ms).
    pub expires_at: i64,
}

/// Body of the demo endpoint: the action the guard checks.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ProtectedAction {
    /// Permission key, e.g. `sale.discount`.
    pub action: String,
    /// Requested value (basis points or minor units), for limited permissions.
    #[serde(default)]
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
}

/// Result of the demo endpoint.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProtectedResult {
    /// The authorized action.
    pub action: String,
    /// The authorized value.
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
    /// The manager whose override was used, or `null` when the caller's own permissions sufficed.
    pub approved_by: Option<String>,
}

/// Parses `action` and `value`; the value's kind follows from the permission.
fn parse_action(
    action: &str,
    value: Option<i64>,
) -> Result<(Permission, Option<Requested>), ApiError> {
    let permission: Permission = action
        .parse()
        .map_err(|e| ApiError::invalid_field("action", format!("{e}")))?;
    let Some(value) = value else {
        return Ok((permission, None));
    };
    let requested = match permission.limit_kind() {
        None => {
            return Err(ApiError::invalid_field(
                "value",
                format!("`{permission}` takes no value"),
            ));
        }
        Some(LimitKind::Pct) => u16::try_from(value)
            .ok()
            .filter(|bp| *bp <= sultan_core::access::FULL_PCT_BASIS_POINTS)
            .map(Requested::Pct)
            .ok_or_else(|| ApiError::invalid_field("value", "`value` must be 0 to 10000"))?,
        Some(LimitKind::Amount) if value >= 0 => Requested::Amount(Money::from_minor(value)),
        Some(LimitKind::Amount) => {
            return Err(ApiError::invalid_field(
                "value",
                "`value` must not be negative",
            ));
        }
    };
    Ok((permission, Some(requested)))
}

/// `POST /api/v1/overrides` → `201` with a single-use token (spec 2.1 §6).
///
/// # Errors
/// `401 invalid_session` / `invalid_device_token` / `invalid_credentials` (unknown approver or
/// wrong PIN), `403 register_session_required` / `self_approval` / `override_limit_exceeded` /
/// `permission_denied`, `422 invalid_field`, `423 locked_out`.
#[utoipa::path(post, path = "/api/v1/overrides", tag = "overrides",
    security(("bearer" = [], "register_token" = [])),
    request_body = OverrideRequest,
    responses((status = 201, body = OverrideResponse),
        (status = 401, body = crate::openapi::Problem), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem), (status = 423, body = crate::openapi::Problem)))]
pub async fn approve(
    State(state): State<AppState>,
    caller: RegisterSession,
    ApiJson(body): ApiJson<OverrideRequest>,
) -> Result<(StatusCode, Json<OverrideResponse>), ApiError> {
    let (permission, value) = parse_action(&body.action, body.value)?;
    let context_ref = text("context_ref", &body.context_ref, 200)?;
    let approver_id: UserId = parse_id(&body.approver_id, "approver_id")?;
    let requester = caller.user.user.id;
    let branch = caller.register.branch;
    let register = caller.register.register;
    // Checked before the PIN, so a cashier typing their own PIN does not count towards a lockout.
    if approver_id == requester {
        return Err(AccessError::SelfApproval.into());
    }
    let approver = users::get_user(&state.db, approver_id)
        .await?
        .filter(|u| u.active)
        .ok_or_else(ApiError::invalid_credentials)?;
    let phc = approver
        .pin_hash
        .clone()
        .ok_or_else(ApiError::invalid_credentials)?;
    if !is_assigned(&state.db, approver.id, branch).await? {
        return Err(ApiError::invalid_credentials());
    }
    verify_with_lockout(
        &state,
        &approver,
        branch,
        Some(register),
        (phc, body.approver_pin),
        "override",
    )
    .await?;
    let access = roles::load_user_access(&state.db, approver.id).await?;
    let approver_perms = effective_permissions(&access.assignments, &access.roles, branch);
    validate_override(requester, approver.id, &approver_perms, permission, value)?;

    let now = state.now();
    let secret = token::generate();
    let issued = OverrideToken {
        token_hash: token::hash(&secret),
        permission,
        value,
        register,
        context_ref: context_ref.clone(),
        requester,
        approver: approver.id,
        created_at: now,
        expires_at: now.saturating_add_millis(OVERRIDE_TTL_MS),
    };
    let response = OverrideResponse {
        token: secret,
        action: permission.as_str().to_owned(),
        value: value.map(requested_value),
        context_ref,
        requester_id: requester.to_string(),
        approver_id: approver.id.to_string(),
        expires_at: issued.expires_at.unix_millis(),
    };
    let mut entry = AuditEntry::new(now, "override.approve");
    entry.branch = Some(branch);
    entry.register = Some(register);
    entry.user = Some(approver.id);
    entry.target = Some(format!("context:{}", response.context_ref));
    entry.after = Some(json!({
        "requester_id": response.requester_id,
        "approver_id": response.approver_id,
        "action": response.action,
        "value": response.value,
        "register_id": register.to_string(),
        "context_ref": response.context_ref,
        "expires_at": response.expires_at,
    }));
    let mut tx = state.db.begin().await?;
    overrides::insert(&mut tx, &issued).await?;
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok((StatusCode::CREATED, Json(response)))
}

/// `POST /api/v1/demo/override-protected`: **demo** of the `X-Override` guard (see the module
/// docs). `200` when the caller's own permissions allow the action or a matching override token
/// was used up.
///
/// # Errors
/// `403 needs_override` (with `permission`, `limit`) / `invalid_override` / `permission_denied` /
/// `register_session_required`, `422 invalid_field`.
#[utoipa::path(post, path = "/api/v1/demo/override-protected", tag = "overrides",
    security(("bearer" = [], "register_token" = [])),
    params(("X-Override" = Option<String>, Header, description = "Override token from POST /overrides")),
    request_body = ProtectedAction,
    responses((status = 200, body = ProtectedResult), (status = 403, body = crate::openapi::Problem)))]
pub async fn demo_protected(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    ApiJson(body): ApiJson<ProtectedAction>,
) -> Result<Json<ProtectedResult>, ApiError> {
    let (permission, value) = parse_action(&body.action, body.value)?;
    let context_ref = text("context_ref", &body.context_ref, 200)?;
    let permissions = Caller::load(&state.db, &caller.user).await?.permissions;
    let mut tx = state.db.begin().await?;
    let action = Action {
        permission,
        value,
        context_ref: &context_ref,
    };
    let approved_by = authorize(&state, &mut tx, &caller, &permissions, action, &header).await?;
    tx.commit().await?;
    Ok(Json(ProtectedResult {
        action: permission.as_str().to_owned(),
        value: value.map(requested_value),
        context_ref,
        approved_by: approved_by.map(|u| u.to_string()),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_parsing() {
        let money = |m| Some(Requested::Amount(Money::from_minor(m)));
        #[rustfmt::skip]
        let cases = [
            ("sale.void", None, Ok((Permission::SaleVoid, None))),
            ("sale.discount", Some(1_500), Ok((Permission::SaleDiscount, Some(Requested::Pct(1_500))))),
            ("sale.discount", None, Ok((Permission::SaleDiscount, None))),
            ("cash.out", Some(25_000), Ok((Permission::CashOut, money(25_000)))),
            ("sale.nope", None, Err("action")),
            ("sale.void", Some(1), Err("value")),
            ("sale.discount", Some(10_001), Err("value")),
            ("sale.discount", Some(-1), Err("value")),
            ("cash.out", Some(-1), Err("value")),
        ];
        for (action, value, expected) in cases {
            let got = parse_action(action, value).map_err(|e| {
                assert_eq!(e.code(), "invalid_field");
                e.status().as_u16()
            });
            match expected {
                Ok(ok) => assert_eq!(got, Ok(ok), "{action} {value:?}"),
                Err(_) => assert_eq!(got, Err(422), "{action} {value:?}"),
            }
        }
    }
}
