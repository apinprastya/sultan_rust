//! Stock locations (spec 2.1 §1, §8): `GET /locations[?branch_id=]`, `GET|PUT|DELETE
//! /locations/{id}`.
//!
//! Read: `org.manage`, or a location of a branch the caller is assigned to. Write: HQ only,
//! `org.manage`. A location never moves to another branch.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::Permission;
use sultan_core::ids::{BranchId, LocationId};
use sultan_db::org::{self, StockLocation};
use sultan_db::{Tx, audit};
use utoipa::{IntoParams, ToSchema};

use super::{audit_entry, parse_id, parse_opt_id, parse_put_id, saved_status, text, to_json};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, RequireHq, UserSession};
use crate::state::AppState;

/// Body of `PUT /locations/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LocationBody {
    /// The branch. Cannot change once created.
    pub branch_id: String,
    /// Display name, unique per branch, e.g. "Gudang".
    pub name: String,
    /// The branch's default receiving location.
    #[serde(default)]
    pub receiving_default: bool,
    /// `true` archives; `false` (default) restores.
    #[serde(default)]
    pub archived: bool,
}

/// A stock location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LocationDto {
    /// ID (`UUIDv7`).
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// Display name.
    pub name: String,
    /// Default receiving location.
    pub receiving_default: bool,
    /// Archived (soft-deleted).
    pub archived: bool,
}

impl From<&StockLocation> for LocationDto {
    fn from(l: &StockLocation) -> Self {
        Self {
            id: l.id.to_string(),
            branch_id: l.branch.to_string(),
            name: l.name.clone(),
            receiving_default: l.receiving_default,
            archived: l.archived,
        }
    }
}

/// Filter of the list endpoints of registers and locations.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct BranchFilter {
    /// Only this branch.
    pub branch_id: Option<String>,
}

/// `GET /api/v1/locations`: locations of visible branches, including archived ones.
///
/// # Errors
/// `401 invalid_session`, `400 invalid_body` (bad `branch_id`).
#[utoipa::path(get, path = "/api/v1/locations", tag = "org", security(("bearer" = [])),
    params(BranchFilter),
    responses((status = 200, body = [LocationDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(filter): ApiQuery<BranchFilter>,
) -> Result<Json<Vec<LocationDto>>, ApiError> {
    let branch: Option<BranchId> = parse_opt_id(filter.branch_id.as_deref(), "branch_id")?;
    let view = Caller::load(&state.db, &session).await?.branch_view();
    let locations = org::list_locations(&state.db, branch).await?;
    Ok(Json(
        locations
            .iter()
            .filter(|l| view.allows(l.branch))
            .map(LocationDto::from)
            .collect(),
    ))
}

/// `GET /api/v1/locations/{id}`. A location of an invisible branch is `404`.
///
/// # Errors
/// `401 invalid_session`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/locations/{id}", tag = "org", security(("bearer" = [])),
    params(("id" = String, Path, description = "Location ID")),
    responses((status = 200, body = LocationDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<LocationDto>, ApiError> {
    let id: LocationId = parse_id(&id, "id")?;
    let view = Caller::load(&state.db, &session).await?.branch_view();
    org::get_location(&state.db, id)
        .await?
        .filter(|l| view.allows(l.branch))
        .map(|l| Json(LocationDto::from(&l)))
        .ok_or_else(|| ApiError::not_found("location"))
}

/// `PUT /api/v1/locations/{id}`: create (`201`) or overwrite (`200`); a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422 invalid_field`
/// (unknown branch, branch changed, bad name), `409 conflict` (duplicate name in the branch).
#[utoipa::path(put, path = "/api/v1/locations/{id}", tag = "org", security(("bearer" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = LocationBody,
    responses((status = 201, body = LocationDto), (status = 200, body = LocationDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<LocationBody>,
) -> Result<(StatusCode, Json<LocationDto>), ApiError> {
    let id: LocationId = parse_put_id(&id)?;
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let branch: BranchId = parse_id(&body.branch_id, "branch_id")?;
    let name = text("name", &body.name, 100)?;

    let mut tx = state.db.begin().await?;
    let before = org::get_location(&mut tx, id).await?;
    super::registers::ensure_branch(&mut tx, branch, before.as_ref().map(|l| l.branch)).await?;
    let after = StockLocation {
        id,
        branch,
        name,
        receiving_default: body.receiving_default,
        archived: body.archived,
    };
    let status = save(tx, &state, &session, before, &after, "location.save").await?;
    Ok((status, Json(LocationDto::from(&after))))
}

/// `DELETE /api/v1/locations/{id}`: archive (soft delete).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/locations/{id}", tag = "org", security(("bearer" = [])),
    params(("id" = String, Path, description = "Location ID")),
    responses((status = 200, body = LocationDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<LocationDto>, ApiError> {
    let id: LocationId = parse_id(&id, "id")?;
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let mut tx = state.db.begin().await?;
    let before = org::get_location(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("location"))?;
    let after = StockLocation {
        archived: true,
        ..before.clone()
    };
    save(
        tx,
        &state,
        &session,
        Some(before),
        &after,
        "location.archive",
    )
    .await?;
    Ok(Json(LocationDto::from(&after)))
}

/// Saves `after` with its audit entry and outbox event, unless nothing changed.
async fn save(
    mut tx: Tx,
    state: &AppState,
    session: &UserSession,
    before: Option<StockLocation>,
    after: &StockLocation,
    action: &str,
) -> Result<StatusCode, ApiError> {
    if before.as_ref() == Some(after) {
        return Ok(StatusCode::OK);
    }
    let now = state.now();
    let saved = org::save_location(&mut tx, after, now).await?;
    let entry = audit_entry(
        session,
        now,
        action,
        format!("location:{}", after.id),
        before.as_ref().map(|l| to_json(&LocationDto::from(l))),
        Some(to_json(&LocationDto::from(after))),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(saved_status(saved))
}
