//! Cash in / out reasons (spec 2.7 §1, §2, K1, K25; phase K3): HQ master data with a fixed
//! direction.
//!
//! - `GET /cash-reasons[?archived=true]`: the active reasons in the register's order (in before
//!   out, then sort); a user session or the device token. `archived=true` adds inactive and
//!   archived ones and needs `tender.manage` (from any role assignment).
//! - `PUT /cash-reasons/{id} {name, direction, note_required, active, sort}`: create (`201`) or
//!   overwrite (`200`; an unchanged replay too). HQ only (`403 read_only_replica` on a branch
//!   node) and `tender.manage` (K25). The direction never changes (`422 invalid_field`,
//!   `direction`); an archived reason cannot be edited (`409 archived`).
//! - `DELETE /cash-reasons/{id}`: archive (idempotent); answers the archived reason. Movements
//!   keep it; new movements are refused it (`invalid_field`, `reason`).
//!
//! Audit and outbox rows (`cash_reason.put` / `.archive`) come from `sultan_db::cash` in the same
//! transaction. Publishes `catalog.changed {scope: "cash_reasons"}` so open registers reload.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::Permission;
use sultan_core::cash::{CashReason, Direction};
use sultan_core::ids::CashReasonId;
use sultan_db::cash::{self, CashDbError, CashReasonRecord};
use utoipa::{IntoParams, ToSchema};

use super::cash::{Fail, run};
use super::catalog::{actor, publish_changed, written_status};
use super::{parse_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, CatalogReader, RequireHq, UserSession};
use crate::state::AppState;

/// Query of `GET /cash-reasons`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ReasonQuery {
    /// Also inactive and archived reasons (`tender.manage`).
    #[serde(default)]
    pub archived: bool,
}

/// Body of `PUT /cash-reasons/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CashReasonBody {
    /// Name shown and printed, 1–60 characters.
    pub name: String,
    /// `in` or `out`; fixed once created.
    pub direction: String,
    /// A note is required with it (the seeded "Other").
    #[serde(default)]
    pub note_required: bool,
    /// Offered at the register.
    #[serde(default = "yes")]
    pub active: bool,
    /// Position in the register's list, ascending.
    #[serde(default)]
    pub sort: i32,
}

const fn yes() -> bool {
    true
}

/// A cash reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CashReasonDto {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// `in` or `out`.
    pub direction: String,
    /// A note is required.
    pub note_required: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position.
    pub sort: i32,
    /// Archived (deleted).
    pub archived: bool,
}

impl From<&CashReasonRecord> for CashReasonDto {
    fn from(r: &CashReasonRecord) -> Self {
        Self {
            id: r.reason.id.to_string(),
            name: r.reason.name.clone(),
            direction: r.reason.direction.as_str().to_owned(),
            note_required: r.reason.note_required,
            active: r.reason.active,
            sort: r.reason.sort,
            archived: r.archived_at.is_some(),
        }
    }
}

/// `GET /api/v1/cash-reasons[?archived=true]`.
///
/// # Errors
/// `401`, `403 permission_denied` (`archived=true` without `tender.manage`).
#[utoipa::path(get, path = "/api/v1/cash-reasons", tag = "cash",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(ReasonQuery),
    responses((status = 200, body = [CashReasonDto]), (status = 401, body = crate::openapi::Problem),
        (status = 403, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiQuery(query): ApiQuery<ReasonQuery>,
) -> Result<Json<Vec<CashReasonDto>>, ApiError> {
    if query.archived {
        let Some(user) = reader.user() else {
            return Err(sultan_core::access::AccessError::PermissionDenied {
                permission: Permission::TenderManage,
            }
            .into());
        };
        Caller::load(&state.db, user)
            .await?
            .require_anywhere(Permission::TenderManage)?;
    }
    let list = cash::list_cash_reasons(&state.db, query.archived).await?;
    Ok(Json(list.iter().map(CashReasonDto::from).collect()))
}

/// `PUT /api/v1/cash-reasons/{id}` (see the module docs).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `409 archived`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/cash-reasons/{id}", tag = "cash",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = CashReasonBody,
    responses((status = 201, body = CashReasonDto), (status = 200, body = CashReasonDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CashReasonBody>,
) -> Result<(StatusCode, Json<CashReasonDto>), ApiError> {
    let id: CashReasonId = parse_put_id(&id)?;
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::TenderManage)?;
    let direction = Direction::from_key(&body.direction)
        .ok_or_else(|| ApiError::invalid_field("direction", "`direction` is `in` or `out`"))?;
    let reason = CashReason {
        id,
        name: body.name,
        direction,
        active: body.active,
        sort: body.sort,
        note_required: body.note_required,
    };
    let now = state.now();
    let (state_ref, actor, reason) = (&state, &actor(&session), &reason);
    let (written, stored) = run("cash reason put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let written = cash::put_cash_reason(&mut tx, reason, actor, now).await?;
        let stored = cash::get_cash_reason(&mut tx, id)
            .await?
            .ok_or(CashDbError::NotFound("cash reason"))?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored))
    })
    .await?;
    publish_changed(
        &state,
        written,
        "cash_reasons",
        json!({ "cash_reason_id": id.to_string() }),
    );
    Ok((written_status(written), Json(CashReasonDto::from(&stored))))
}

/// `DELETE /api/v1/cash-reasons/{id}`: archive.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/cash-reasons/{id}", tag = "cash",
    security(("bearer" = [])), params(("id" = String, Path, description = "Cash reason ID")),
    responses((status = 200, body = CashReasonDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn delete(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<CashReasonDto>, ApiError> {
    let id: CashReasonId = parse_id(&id, "id")?;
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::TenderManage)?;
    let now = state.now();
    let (state_ref, actor) = (&state, &actor(&session));
    let (written, stored) = run("cash reason archive", || async move {
        let mut tx = state_ref.db.begin().await?;
        let written = cash::archive_cash_reason(&mut tx, id, actor, now).await?;
        let stored = cash::get_cash_reason(&mut tx, id)
            .await?
            .ok_or(CashDbError::NotFound("cash reason"))?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored))
    })
    .await?;
    publish_changed(
        &state,
        written,
        "cash_reasons",
        json!({ "cash_reason_id": id.to_string() }),
    );
    Ok(Json(CashReasonDto::from(&stored)))
}
