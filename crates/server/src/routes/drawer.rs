//! Opening the cash drawer without a sale (spec 2.1 §3 `drawer.open`, spec 2.6 §13 F5; phase S5).
//!
//! - `POST /drawer/open {reason?}` with `Idempotency-Key`: a PIN session at the register with an
//!   **open cashier session of the caller on this register** (`409 no_open_session`) and
//!   `drawer.open`, which a manager may approve at the till (`X-Override`, `context_ref` = the
//!   cashier session ID). The reason is an optional note of at most 200 characters.
//! - Writes the audit entry `drawer.open` on the cashier session (with the approver). Spec 2.6
//!   §10 does not list drawer opens in the close report, so they are only audited. The real
//!   drawer kick comes with 2.11 (hardware); until then the register only says "Drawer opened".

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::Permission;
use sultan_db::audit::{self, AuditEntry};
use utoipa::ToSchema;

use super::sale_post;
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, RegisterSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::AppState;

/// The audit action of a drawer open.
pub const DRAWER_OPEN: &str = "drawer.open";

/// Longest reason, in characters.
const MAX_REASON: usize = 200;

/// Body of `POST /drawer/open`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DrawerOpenBody {
    /// Why the drawer is opened (optional, at most 200 characters).
    #[serde(default)]
    pub reason: Option<String>,
}

/// The answer of `POST /drawer/open`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct DrawerOpenDto {
    /// The cashier session the open is recorded on.
    pub cashier_session_id: String,
    /// The reason, trimmed.
    pub reason: Option<String>,
    /// The approving manager.
    pub approved_by: Option<String>,
    /// When (unix ms).
    pub opened_at: i64,
}

/// `POST /api/v1/drawer/open` (see the module docs) → `201`.
///
/// # Errors
/// `403` permission codes, `409 no_open_session`, `422 invalid_field` (`reason`).
#[utoipa::path(post, path = "/api/v1/drawer/open", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)"),
        ("X-Override" = Option<String>, Header, description = "Override token for `drawer.open` (context: the cashier session ID)")),
    request_body = DrawerOpenBody,
    responses((status = 201, body = DrawerOpenDto), (status = 403, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn open(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    ApiJson(body): ApiJson<DrawerOpenBody>,
) -> Result<(StatusCode, Json<DrawerOpenDto>), ApiError> {
    let reason = body
        .reason
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    if reason
        .as_ref()
        .is_some_and(|r| r.chars().count() > MAX_REASON)
    {
        return Err(ApiError::invalid_field(
            "reason",
            "`reason` is at most 200 characters",
        ));
    }
    let access = Caller::load(&state.db, &caller.user).await?;
    let till = sale_post::till(&state, &caller).await?;
    let mut tx = state.db.begin().await?;
    let approver = overrides::authorize(
        &state,
        &mut tx,
        &caller,
        &access.permissions,
        Action {
            permission: Permission::DrawerOpen,
            value: None,
            context_ref: &till.session.to_string(),
        },
        &header,
    )
    .await?;
    let now = state.now();
    let mut entry = AuditEntry::new(now, DRAWER_OPEN);
    entry.branch = Some(till.branch);
    entry.register = Some(till.register);
    entry.user = Some(till.user);
    entry.target = Some(format!("cashier_session:{}", till.session));
    entry.after = Some(json!({
        "reason": reason,
        "approved_by": approver.map(|a| a.to_string()),
    }));
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(DrawerOpenDto {
            cashier_session_id: till.session.to_string(),
            reason,
            approved_by: approver.map(|a| a.to_string()),
            opened_at: now.unix_millis(),
        }),
    ))
}
