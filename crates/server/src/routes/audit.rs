//! The audit log viewer (spec 2.1 §7, §8): `GET /audit`, read-only, `audit.view`.
//!
//! Filters (all optional, combined with AND): `user_id`, `action`, `branch_id`, `register_id`,
//! `target`, `from` / `until` (unix ms, `from` inclusive, `until` exclusive). Newest first.
//! Pagination: `limit` (default 100, at most 500) and `before_id` = the `next_before_id` of the
//! previous page; `next_before_id` is `null` on the last page.
//!
//! "Lookups only show branches the user is assigned to" (spec 2.1 §3): a caller who holds
//! `audit.view` through an all-branches role sees every entry; a caller who holds it only at
//! their session's branch sees that branch's entries, and asking for another branch is
//! `403 permission_denied`.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sultan_core::access::{AccessError, Permission};
use sultan_core::ids::{BranchId, RegisterId, UserId};
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry, AuditFilter};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::{parse_id, parse_opt_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiQuery, UserSession};
use crate::state::AppState;

/// Default page size.
pub const DEFAULT_LIMIT: u32 = 100;
/// Largest page size.
pub const MAX_LIMIT: u32 = 500;

/// Query parameters of `GET /audit`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct AuditQuery {
    /// Only entries by this acting user.
    pub user_id: Option<String>,
    /// Only this action, e.g. `user.save`.
    pub action: Option<String>,
    /// Only this branch.
    pub branch_id: Option<String>,
    /// Only this register.
    pub register_id: Option<String>,
    /// Only this target, e.g. `user:<uuid>`.
    pub target: Option<String>,
    /// At or after this time (unix ms).
    pub from: Option<i64>,
    /// Before this time (unix ms).
    pub until: Option<i64>,
    /// Page cursor: `next_before_id` of the previous page.
    pub before_id: Option<String>,
    /// Page size, 1–500 (default 100).
    pub limit: Option<u32>,
}

/// One audit entry (spec 2.1 §7).
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct AuditEntryDto {
    /// Entry ID (`UUIDv7`).
    pub id: String,
    /// When (unix ms).
    pub at: i64,
    /// Branch.
    pub branch_id: Option<String>,
    /// Register.
    pub register_id: Option<String>,
    /// Acting user.
    pub user_id: Option<String>,
    /// What happened, e.g. `branch.save`.
    pub action: String,
    /// What it happened to, e.g. `branch:<uuid>`.
    pub target: Option<String>,
    /// State before.
    #[schema(value_type = Option<Object>)]
    pub before: Option<Value>,
    /// State after.
    #[schema(value_type = Option<Object>)]
    pub after: Option<Value>,
}

impl From<AuditEntry> for AuditEntryDto {
    fn from(e: AuditEntry) -> Self {
        Self {
            id: e.id.to_string(),
            at: e.at.unix_millis(),
            branch_id: e.branch.map(|b| b.to_string()),
            register_id: e.register.map(|r| r.to_string()),
            user_id: e.user.map(|u| u.to_string()),
            action: e.action,
            target: e.target,
            before: e.before,
            after: e.after,
        }
    }
}

/// One page of audit entries.
#[derive(Debug, Clone, PartialEq, Serialize, ToSchema)]
pub struct AuditPage {
    /// Entries, newest first.
    pub items: Vec<AuditEntryDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

/// `GET /api/v1/audit`.
///
/// # Errors
/// `401 invalid_session`, `403 permission_denied`, `400 invalid_body` (bad ID) /
/// `invalid_query` (bad number), `422 invalid_field` (`limit` out of range).
#[utoipa::path(get, path = "/api/v1/audit", tag = "audit", security(("bearer" = [])),
    params(AuditQuery),
    responses((status = 200, body = AuditPage), (status = 403, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<AuditQuery>,
) -> Result<Json<AuditPage>, ApiError> {
    let caller = Caller::load(&state.db, &session).await?;
    caller.require(Permission::AuditView)?;
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {MAX_LIMIT}"),
        ));
    }
    let mut branch: Option<BranchId> = parse_opt_id(query.branch_id.as_deref(), "branch_id")?;
    if !caller.holds_everywhere(Permission::AuditView) {
        let own = session.session.branch;
        if branch.is_some_and(|b| b != own) {
            return Err(AccessError::PermissionDenied {
                permission: Permission::AuditView,
            }
            .into());
        }
        branch = Some(own);
    }
    let filter = AuditFilter {
        branch,
        register: parse_opt_id::<RegisterId>(query.register_id.as_deref(), "register_id")?,
        user: parse_opt_id::<UserId>(query.user_id.as_deref(), "user_id")?,
        action: query.action,
        target: query.target,
        from: query.from.map(Timestamp::from_unix_millis),
        until: query.until.map(Timestamp::from_unix_millis),
        before_id: query
            .before_id
            .as_deref()
            .map(|id| parse_id::<Uuid>(id, "before_id"))
            .transpose()?,
        limit,
    };
    let entries = audit::query(&state.db, &filter).await?;
    let full = entries.len() == limit as usize;
    let next_before_id = entries.last().filter(|_| full).map(|e| e.id.to_string());
    Ok(Json(AuditPage {
        items: entries.into_iter().map(AuditEntryDto::from).collect(),
        next_before_id,
    }))
}
