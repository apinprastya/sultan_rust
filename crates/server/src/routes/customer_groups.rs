//! Customer groups and membership tiers (spec 2.5 §3, §4, §13; phase M3):
//! `GET /customer-groups`, `GET|PUT /customer-groups/{id}`, `GET /membership-tiers`,
//! `GET|PUT /membership-tiers/{id}`.
//!
//! Reads: any user session; lists leave archived entries out unless `include_archived=true`.
//! Writes: HQ only (`403 read_only_replica`) and `customer.manage` from any role assignment
//! (spec 2.5 §18 item 16).
//! Archiving is `archived: true` in the `PUT` body (customers keep the link; an archived tier
//! stops applying). Names are unique, and so are tier ranks (`409 conflict` with `field`). `201`
//! on create, `200` on update and on a replay. Audit and outbox rows come from
//! `sultan_db::customers` in the same transaction. No event is published: a till reads the names
//! with the customer.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::Permission;
use sultan_db::customers::{self as cust, CustomerDbError, CustomerGroup, MembershipTier};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::catalog::{actor, written_status};
use super::customers::run;
use super::{parse_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, RequireHq, UserSession};
use crate::state::AppState;

/// Query of the group and tier lists.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ListQuery {
    /// Include archived entries.
    #[serde(default)]
    pub include_archived: bool,
}

/// Body of `PUT /customer-groups/{id}`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct GroupBody {
    /// Name, 1–100 characters, unique.
    pub name: String,
    /// `true` archives.
    #[serde(default)]
    pub archived: bool,
}

/// A customer group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct GroupDto {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Archived.
    pub archived: bool,
}

impl From<&CustomerGroup> for GroupDto {
    fn from(g: &CustomerGroup) -> Self {
        Self {
            id: g.id.to_string(),
            name: g.name.clone(),
            archived: g.archived_at.is_some(),
        }
    }
}

/// Body of `PUT /membership-tiers/{id}`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct TierBody {
    /// Name, 1–100 characters, unique.
    pub name: String,
    /// Rank, higher = better, unique.
    pub rank: i64,
    /// `true` archives.
    #[serde(default)]
    pub archived: bool,
}

/// A membership tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TierDto {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Rank, higher = better.
    pub rank: i64,
    /// Archived.
    pub archived: bool,
}

impl From<&MembershipTier> for TierDto {
    fn from(t: &MembershipTier) -> Self {
        Self {
            id: t.id.to_string(),
            name: t.name.clone(),
            rank: t.rank,
            archived: t.archived_at.is_some(),
        }
    }
}

async fn require_manage(state: &AppState, session: &UserSession) -> Result<(), ApiError> {
    Caller::load(&state.db, session)
        .await?
        .require_anywhere(Permission::CustomerManage)
}

/// `GET /api/v1/customer-groups`: groups by name.
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/customer-groups", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(ListQuery),
    responses((status = 200, body = [GroupDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list_groups(
    State(state): State<AppState>,
    _session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<GroupDto>>, ApiError> {
    let groups = cust::list_groups(&state.db, query.include_archived).await?;
    Ok(Json(groups.iter().map(GroupDto::from).collect()))
}

/// `GET /api/v1/customer-groups/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/customer-groups/{id}", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Group ID")),
    responses((status = 200, body = GroupDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_group(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<GroupDto>, ApiError> {
    let id: Uuid = parse_id(&id, "id")?;
    cust::get_group(&state.db, id)
        .await?
        .map(|g| Json(GroupDto::from(&g)))
        .ok_or_else(|| ApiError::not_found("customer group"))
}

/// `PUT /api/v1/customer-groups/{id}`: create (`201`) or change (`200`) a group.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `409 conflict` (name),
/// `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/customer-groups/{id}", tag = "customers",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = GroupBody,
    responses((status = 201, body = GroupDto), (status = 200, body = GroupDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_group(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<GroupBody>,
) -> Result<(StatusCode, Json<GroupDto>), ApiError> {
    let id: Uuid = parse_put_id(&id)?;
    require_manage(&state, &session).await?;
    let now = state.now();
    let group = CustomerGroup {
        id,
        name: body.name,
        archived_at: body.archived.then_some(now),
    };
    let (state, actor, group) = (&state, &actor(&session), &group);
    let (written, stored) = run(|| async move {
        let mut tx = state.db.begin().await?;
        let written = cust::put_group(&mut tx, group, actor, now).await?;
        let stored = cust::get_group(&mut tx, id)
            .await?
            .ok_or(CustomerDbError::NotFound("customer group"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    Ok((written_status(written), Json(GroupDto::from(&stored))))
}

/// `GET /api/v1/membership-tiers`: tiers, best (highest rank) first.
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/membership-tiers", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(ListQuery),
    responses((status = 200, body = [TierDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list_tiers(
    State(state): State<AppState>,
    _session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<TierDto>>, ApiError> {
    let tiers = cust::list_tiers(&state.db, query.include_archived).await?;
    Ok(Json(tiers.iter().map(TierDto::from).collect()))
}

/// `GET /api/v1/membership-tiers/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/membership-tiers/{id}", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Tier ID")),
    responses((status = 200, body = TierDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_tier(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<TierDto>, ApiError> {
    let id: Uuid = parse_id(&id, "id")?;
    cust::get_tier(&state.db, id)
        .await?
        .map(|t| Json(TierDto::from(&t)))
        .ok_or_else(|| ApiError::not_found("membership tier"))
}

/// `PUT /api/v1/membership-tiers/{id}`: create (`201`) or change (`200`) a tier.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `409 conflict` (name or
/// rank, with `field`), `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/membership-tiers/{id}", tag = "customers",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = TierBody,
    responses((status = 201, body = TierDto), (status = 200, body = TierDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_tier(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<TierBody>,
) -> Result<(StatusCode, Json<TierDto>), ApiError> {
    let id: Uuid = parse_put_id(&id)?;
    require_manage(&state, &session).await?;
    let now = state.now();
    let tier = MembershipTier {
        id,
        name: body.name,
        rank: body.rank,
        archived_at: body.archived.then_some(now),
    };
    let (state, actor, tier) = (&state, &actor(&session), &tier);
    let (written, stored) = run(|| async move {
        let mut tx = state.db.begin().await?;
        let written = cust::put_tier(&mut tx, tier, actor, now).await?;
        let stored = cust::get_tier(&mut tx, id)
            .await?
            .ok_or(CustomerDbError::NotFound("membership tier"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    Ok((written_status(written), Json(TierDto::from(&stored))))
}
