//! Branches (spec 2.1 §1, §8): `GET /branches`, `GET|PUT|DELETE /branches/{id}`.
//!
//! Read: `org.manage`, or a branch the caller is assigned to. Write: HQ only, `org.manage`.
//! The company is the install's single company (spec 2.1 §1), so the body has no `company_id`.
//!
//! `timezone` (Phase 9) is the branch's IANA time zone; document numbers use the branch-local
//! date (`core::doc_number`). It is optional in the body and defaults to `Asia/Jakarta`; a name
//! that is not an IANA zone (`WIB`, `+07:00`) is `422 invalid_field`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::Permission;
use sultan_core::doc_number::DEFAULT_TIME_ZONE;
use sultan_core::ids::BranchId;
use sultan_db::org::{self, Branch};
use sultan_db::{Tx, audit};
use utoipa::ToSchema;

use super::{audit_entry, code, parse_id, parse_put_id, saved_status, text, timezone, to_json};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, RequireHq, UserSession};
use crate::state::AppState;

/// Body of `PUT /branches/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BranchBody {
    /// Code used in document numbers, e.g. `JKT01`: 1–10 letters or digits, unique.
    pub code: String,
    /// Display name.
    pub name: String,
    /// Address printed on receipts.
    #[serde(default)]
    pub address: String,
    /// Tax number (NPWP).
    #[serde(default)]
    pub npwp: Option<String>,
    /// Receipt header text.
    #[serde(default)]
    pub receipt_header: String,
    /// Receipt footer text.
    #[serde(default)]
    pub receipt_footer: String,
    /// IANA time zone of document dates, e.g. `Asia/Makassar` (default `Asia/Jakarta`).
    #[serde(default)]
    pub timezone: Option<String>,
    /// `true` archives; `false` (default) restores an archived branch.
    #[serde(default)]
    pub archived: bool,
}

/// A branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BranchDto {
    /// ID (`UUIDv7`).
    pub id: String,
    /// The company.
    pub company_id: String,
    /// Code, e.g. `JKT01`.
    pub code: String,
    /// Display name.
    pub name: String,
    /// Address.
    pub address: String,
    /// Tax number.
    pub npwp: Option<String>,
    /// Receipt header.
    pub receipt_header: String,
    /// Receipt footer.
    pub receipt_footer: String,
    /// IANA time zone of document dates, e.g. `Asia/Jakarta`.
    pub timezone: String,
    /// Archived (soft-deleted).
    pub archived: bool,
}

impl From<&Branch> for BranchDto {
    fn from(b: &Branch) -> Self {
        Self {
            id: b.id.to_string(),
            company_id: b.company.to_string(),
            code: b.code.clone(),
            name: b.name.clone(),
            address: b.address.clone(),
            npwp: b.npwp.clone(),
            receipt_header: b.receipt_header.clone(),
            receipt_footer: b.receipt_footer.clone(),
            timezone: b.timezone.clone(),
            archived: b.archived,
        }
    }
}

/// `GET /api/v1/branches`: the branches the caller may see, including archived ones, by code.
///
/// # Errors
/// `401 invalid_session`.
#[utoipa::path(get, path = "/api/v1/branches", tag = "org",
    security(("bearer" = [])),
    responses((status = 200, body = [BranchDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
) -> Result<Json<Vec<BranchDto>>, ApiError> {
    let view = Caller::load(&state.db, &session).await?.branch_view();
    let branches = org::list_branches(&state.db).await?;
    Ok(Json(
        branches
            .iter()
            .filter(|b| view.allows(b.id))
            .map(BranchDto::from)
            .collect(),
    ))
}

/// `GET /api/v1/branches/{id}`. A branch the caller may not see is `404`.
///
/// # Errors
/// `401 invalid_session`, `400 invalid_body` (bad ID), `404 not_found`.
#[utoipa::path(get, path = "/api/v1/branches/{id}", tag = "org",
    security(("bearer" = [])), params(("id" = String, Path, description = "Branch ID")),
    responses((status = 200, body = BranchDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<BranchDto>, ApiError> {
    let id: BranchId = parse_id(&id, "id")?;
    let view = Caller::load(&state.db, &session).await?.branch_view();
    org::get_branch(&state.db, id)
        .await?
        .filter(|b| view.allows(b.id))
        .map(|b| Json(BranchDto::from(&b)))
        .ok_or_else(|| ApiError::not_found("branch"))
}

/// `PUT /api/v1/branches/{id}`: create (`201`) or overwrite (`200`); a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422 invalid_field`,
/// `409 conflict` (duplicate code), `404 not_found` (no company set up).
#[utoipa::path(put, path = "/api/v1/branches/{id}", tag = "org",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = BranchBody,
    responses((status = 201, body = BranchDto), (status = 200, body = BranchDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<BranchBody>,
) -> Result<(StatusCode, Json<BranchDto>), ApiError> {
    let id: BranchId = parse_put_id(&id)?;
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let code = code("code", &body.code)?;
    let name = text("name", &body.name, 100)?;
    let timezone = timezone(
        "timezone",
        body.timezone.as_deref().unwrap_or(DEFAULT_TIME_ZONE),
    )?;

    let mut tx = state.db.begin().await?;
    let before = org::get_branch(&mut tx, id).await?;
    let company = match &before {
        Some(b) => b.company,
        None => {
            org::get_company(&mut tx)
                .await?
                .ok_or_else(|| ApiError::not_found("company"))?
                .id
        }
    };
    let after = Branch {
        id,
        company,
        code,
        name,
        address: body.address.trim().to_owned(),
        npwp: body
            .npwp
            .map(|n| n.trim().to_owned())
            .filter(|n| !n.is_empty()),
        receipt_header: body.receipt_header,
        receipt_footer: body.receipt_footer,
        timezone,
        archived: body.archived,
    };
    let status = save(tx, &state, &session, before, &after, "branch.save").await?;
    Ok((status, Json(BranchDto::from(&after))))
}

/// `DELETE /api/v1/branches/{id}`: archive (soft delete). Archiving twice is a no-op.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/branches/{id}", tag = "org",
    security(("bearer" = [])), params(("id" = String, Path, description = "Branch ID")),
    responses((status = 200, body = BranchDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<BranchDto>, ApiError> {
    let id: BranchId = parse_id(&id, "id")?;
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::OrgManage)?;
    let mut tx = state.db.begin().await?;
    let before = org::get_branch(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("branch"))?;
    let after = Branch {
        archived: true,
        ..before.clone()
    };
    save(tx, &state, &session, Some(before), &after, "branch.archive").await?;
    Ok(Json(BranchDto::from(&after)))
}

/// Saves `after` with its audit entry and outbox event, unless nothing changed.
async fn save(
    mut tx: Tx,
    state: &AppState,
    session: &UserSession,
    before: Option<Branch>,
    after: &Branch,
    action: &str,
) -> Result<StatusCode, ApiError> {
    if before.as_ref() == Some(after) {
        return Ok(StatusCode::OK);
    }
    let now = state.now();
    let saved = org::save_branch(&mut tx, after, now).await?;
    let entry = audit_entry(
        session,
        now,
        action,
        format!("branch:{}", after.id),
        before.as_ref().map(|b| to_json(&BranchDto::from(b))),
        Some(to_json(&BranchDto::from(after))),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(saved_status(saved))
}
