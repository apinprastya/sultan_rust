//! Tender types (spec 2.6 D4, §12; phase S3): HQ master data with a fixed kind.
//!
//! - `GET /tender-types[?all=true]`: the active tender types in the register's order (a user
//!   session or the device token). `all=true` adds inactive and archived ones (back office).
//! - `PUT /tender-types/{id} {name, kind, needs_reference, opens_drawer, active, sort}`: create
//!   (`201`) or overwrite (`200`; an unchanged replay too). HQ only (`403 read_only_replica` on a
//!   branch node) and `tender.manage` from any role assignment (back office, not overridable).
//!   The kind never changes (`422 invalid_field`, `kind`); `exchange` is not a tender type kind;
//!   an archived tender type cannot be edited (`409 archived`).
//! - `DELETE /tender-types/{id}`: archive (idempotent); answers the archived record.
//!
//! Audit and outbox rows come from `sultan_db::sales` in the same transaction. Publishes
//! `catalog.changed {scope: "tender_types"}` so open registers reload the list.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::Permission;
use sultan_core::ids::TenderTypeId;
use sultan_core::sale::{TenderKind, TenderType};
use sultan_db::sales::{self, SaleDbError, TenderTypeRecord};
use utoipa::{IntoParams, ToSchema};

use super::catalog::{actor, publish_changed, written_status};
use super::sales::{Fail, run};
use super::{parse_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, CatalogReader, RequireHq, UserSession};
use crate::state::AppState;

/// Query of `GET /tender-types`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct TenderQuery {
    /// Also inactive and archived tender types.
    #[serde(default)]
    pub all: bool,
}

/// Body of `PUT /tender-types/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TenderTypeBody {
    /// Name shown and printed, 1–40 characters.
    pub name: String,
    /// `cash`, `card`, `qris`, `transfer`, `ewallet`, `customer_balance` or `other`; fixed once
    /// created.
    pub kind: String,
    /// A reference must be typed.
    #[serde(default)]
    pub needs_reference: bool,
    /// Paying with it opens the drawer.
    #[serde(default)]
    pub opens_drawer: bool,
    /// Offered at the register.
    #[serde(default = "yes")]
    pub active: bool,
    /// Position in the register's list, ascending.
    #[serde(default)]
    pub sort: u32,
}

const fn yes() -> bool {
    true
}

/// A tender type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[allow(clippy::struct_excessive_bools)] // the stored flags, one field each in the API
pub struct TenderTypeDto {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// The kind.
    pub kind: String,
    /// A reference must be typed.
    pub needs_reference: bool,
    /// Opens the drawer.
    pub opens_drawer: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position.
    pub sort: u32,
    /// Archived (deleted).
    pub archived: bool,
}

impl From<&TenderTypeRecord> for TenderTypeDto {
    fn from(r: &TenderTypeRecord) -> Self {
        Self {
            id: r.tender.id.to_string(),
            name: r.tender.name.clone(),
            kind: r.tender.kind.as_str().to_owned(),
            needs_reference: r.tender.needs_reference,
            opens_drawer: r.tender.opens_drawer,
            active: r.tender.active,
            sort: r.sort,
            archived: r.archived_at.is_some(),
        }
    }
}

/// `GET /api/v1/tender-types[?all=true]`.
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/tender-types", tag = "sales",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(TenderQuery),
    responses((status = 200, body = [TenderTypeDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    _reader: CatalogReader,
    ApiQuery(query): ApiQuery<TenderQuery>,
) -> Result<Json<Vec<TenderTypeDto>>, ApiError> {
    let list = sales::list_tender_types(&state.db, query.all).await?;
    Ok(Json(list.iter().map(TenderTypeDto::from).collect()))
}

/// `PUT /api/v1/tender-types/{id}` (see the module docs).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `409 archived`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/tender-types/{id}", tag = "sales",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = TenderTypeBody,
    responses((status = 201, body = TenderTypeDto), (status = 200, body = TenderTypeDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<TenderTypeBody>,
) -> Result<(StatusCode, Json<TenderTypeDto>), ApiError> {
    let id: TenderTypeId = parse_put_id(&id)?;
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::TenderManage)?;
    let kind: TenderKind = body
        .kind
        .parse()
        .map_err(|_| ApiError::invalid_field("kind", "unknown tender kind"))?;
    let tender = TenderType {
        id,
        name: body.name,
        kind,
        needs_reference: body.needs_reference,
        opens_drawer: body.opens_drawer,
        active: body.active,
    }
    .validate()?;
    let now = state.now();
    let (state_ref, actor, tender_ref, sort) = (&state, &actor(&session), &tender, body.sort);
    let (written, stored) = run("tender type put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let written = sales::put_tender_type(&mut tx, tender_ref, sort, actor, now).await?;
        let stored = sales::get_tender_type(&mut tx, id)
            .await?
            .ok_or(SaleDbError::NotFound("tender type"))?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored))
    })
    .await?;
    publish_changed(
        &state,
        written,
        "tender_types",
        json!({ "tender_type_id": id.to_string() }),
    );
    Ok((written_status(written), Json(TenderTypeDto::from(&stored))))
}

/// `DELETE /api/v1/tender-types/{id}`: archive.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/tender-types/{id}", tag = "sales",
    security(("bearer" = [])), params(("id" = String, Path, description = "Tender type ID")),
    responses((status = 200, body = TenderTypeDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn delete(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<TenderTypeDto>, ApiError> {
    let id: TenderTypeId = parse_id(&id, "id")?;
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::TenderManage)?;
    let now = state.now();
    let (state_ref, actor) = (&state, &actor(&session));
    let (written, stored) = run("tender type archive", || async move {
        let mut tx = state_ref.db.begin().await?;
        let written = sales::archive_tender_type(&mut tx, id, actor, now).await?;
        let stored = sales::get_tender_type(&mut tx, id)
            .await?
            .ok_or(SaleDbError::NotFound("tender type"))?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored))
    })
    .await?;
    publish_changed(
        &state,
        written,
        "tender_types",
        json!({ "tender_type_id": id.to_string() }),
    );
    Ok(Json(TenderTypeDto::from(&stored)))
}
