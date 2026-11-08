//! Roles (spec 2.1 §3, §8): `GET /roles`, `GET|PUT|DELETE /roles/{id}`.
//!
//! - Read: `role.manage` or `user.manage` (whoever assigns roles needs to list them).
//! - Write: HQ only, `role.manage`. A role is a name plus grants from the fixed catalog, each with
//!   an optional limit (`limit`: basis points for `MaxPct` permissions, minor units for
//!   `MaxAmount` ones; `null` = unlimited). `PUT` replaces all grants.
//! - `DELETE` archives: the role stops granting anything; assignments are kept for history. An
//!   archived role is not listed and reads as `404`; a `PUT` restores it.
//! - **Delegation** (spec 2.1 §3, plan Phase 6 fix): a caller cannot create, edit or archive a role
//!   that grants more than they hold. Because a role applies wherever it is assigned, both the
//!   current and the new grants must be covered by the caller's permissions **for all branches**
//!   (`core::access::ensure_can_grant`); otherwise `403 permission_denied` with
//!   `reason: grant_exceeds_own`.
//! - The seeded Owner role can be renamed but not emptied, limited or archived: every write
//!   checks the Owner invariant in the transaction → `409 last_owner`.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::{BranchScope, Grant, Limit, LimitKind, Permission, Role};
use sultan_core::ids::RoleId;
use sultan_core::money::Money;
use sultan_db::{Tx, audit, roles};
use utoipa::ToSchema;

use super::{audit_entry, parse_id, parse_put_id, saved_status, text, to_json};
use crate::access::{Caller, ensure_owner_remains};
use crate::error::ApiError;
use crate::extract::{ApiJson, RequireHq, UserSession};
use crate::state::AppState;

/// One permission in a role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct GrantDto {
    /// Catalog key, e.g. `sale.discount` (see `GET /permissions`).
    pub permission: String,
    /// Limit for limited permissions: basis points (`1000` = 10%) or minor units of money.
    /// `null` = unlimited, and always `null` for permissions without a limit.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// Body of `PUT /roles/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RoleBody {
    /// Display name, unique.
    pub name: String,
    /// The grants; replaces the current ones. Each permission at most once.
    pub grants: Vec<GrantDto>,
}

/// A role.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RoleDto {
    /// ID (`UUIDv7`; the seeded roles have fixed IDs).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Grants, ordered by permission key.
    pub grants: Vec<GrantDto>,
}

impl From<&Role> for RoleDto {
    fn from(role: &Role) -> Self {
        Self {
            id: role.id.to_string(),
            name: role.name.clone(),
            grants: role
                .grants
                .iter()
                .map(|g| GrantDto {
                    permission: g.permission().as_str().to_owned(),
                    limit: g.limit().map(|l| match l {
                        Limit::MaxPct(bp) => i64::from(bp),
                        Limit::MaxAmount(m) => m.minor(),
                    }),
                })
                .collect(),
        }
    }
}

/// Parses a grant; the limit's kind comes from the permission.
fn parse_grant(dto: &GrantDto) -> Result<Grant, ApiError> {
    let permission: Permission = dto
        .permission
        .parse()
        .map_err(|e| ApiError::invalid_field("grants", format!("{e}")))?;
    let Some(value) = dto.limit else {
        return Ok(Grant::unlimited(permission));
    };
    let limit = match permission.limit_kind() {
        Some(LimitKind::Pct) => Limit::MaxPct(u16::try_from(value).map_err(|_| {
            ApiError::invalid_field("grants", format!("`{permission}` limit must be 0 to 10000"))
        })?),
        // `Grant::limited` rejects a limit on an unlimited permission and negative amounts.
        Some(LimitKind::Amount) | None => Limit::MaxAmount(Money::from_minor(value)),
    };
    Ok(Grant::limited(permission, limit)?)
}

/// Parses all grants, sorted by key as stored; a permission listed twice is `422`.
fn parse_grants(dtos: &[GrantDto]) -> Result<Vec<Grant>, ApiError> {
    let mut seen = BTreeSet::new();
    let mut grants = Vec::with_capacity(dtos.len());
    for dto in dtos {
        let grant = parse_grant(dto)?;
        if !seen.insert(grant.permission()) {
            return Err(ApiError::invalid_field(
                "grants",
                format!("`{}` is listed twice", grant.permission()),
            ));
        }
        grants.push(grant);
    }
    grants.sort_by_key(|g| g.permission().as_str());
    Ok(grants)
}

/// `GET /api/v1/roles`: every non-archived role by name. Needs `role.manage` or `user.manage`.
///
/// # Errors
/// `401 invalid_session`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/roles", tag = "roles", security(("bearer" = [])),
    responses((status = 200, body = [RoleDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
) -> Result<Json<Vec<RoleDto>>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require_any(&[Permission::RoleManage, Permission::UserManage])?;
    let roles = roles::list_roles(&state.db).await?;
    Ok(Json(roles.iter().map(RoleDto::from).collect()))
}

/// `GET /api/v1/roles/{id}`. Needs `role.manage` or `user.manage`.
///
/// # Errors
/// `401 invalid_session`, `403 permission_denied`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/roles/{id}", tag = "roles", security(("bearer" = [])),
    params(("id" = String, Path, description = "Role ID")),
    responses((status = 200, body = RoleDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<RoleDto>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require_any(&[Permission::RoleManage, Permission::UserManage])?;
    let id: RoleId = parse_id(&id, "id")?;
    roles::get_role(&state.db, id)
        .await?
        .map(|r| Json(RoleDto::from(&r)))
        .ok_or_else(|| ApiError::not_found("role"))
}

/// `PUT /api/v1/roles/{id}`: create (`201`) or overwrite (`200`); a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422 invalid_field` /
/// `invalid_grant`, `409 conflict` (duplicate name) / `last_owner`.
#[utoipa::path(put, path = "/api/v1/roles/{id}", tag = "roles", security(("bearer" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = RoleBody,
    responses((status = 201, body = RoleDto), (status = 200, body = RoleDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<RoleBody>,
) -> Result<(StatusCode, Json<RoleDto>), ApiError> {
    let id: RoleId = parse_put_id(&id)?;
    let caller = Caller::load(&state.db, &session).await?;
    caller.require(Permission::RoleManage)?;
    let after = Role {
        id,
        name: text("name", &body.name, 64)?,
        grants: parse_grants(&body.grants)?,
    };
    let mut tx = state.db.begin().await?;
    let before = roles::get_role(&mut tx, id).await?;
    if before.as_ref() == Some(&after) {
        return Ok((StatusCode::OK, Json(RoleDto::from(&after))));
    }
    caller.ensure_can_grant(BranchScope::All, &after.grants)?;
    if let Some(before) = &before {
        caller.ensure_can_grant(BranchScope::All, &before.grants)?;
    }
    let now = state.now();
    let saved = roles::save_role(&mut tx, &after, now).await?;
    finish(
        tx,
        &session,
        now,
        "role.save",
        before.as_ref(),
        Some(&after),
    )
    .await?;
    Ok((saved_status(saved), Json(RoleDto::from(&after))))
}

/// `DELETE /api/v1/roles/{id}` → `204`: archive. Archiving the Owner role is `409 last_owner`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`, `409 last_owner`.
#[utoipa::path(delete, path = "/api/v1/roles/{id}", tag = "roles", security(("bearer" = [])),
    params(("id" = String, Path, description = "Role ID")),
    responses((status = 204), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: RoleId = parse_id(&id, "id")?;
    let caller = Caller::load(&state.db, &session).await?;
    caller.require(Permission::RoleManage)?;
    let mut tx = state.db.begin().await?;
    let before = roles::get_role(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("role"))?;
    caller.ensure_can_grant(BranchScope::All, &before.grants)?;
    let now = state.now();
    roles::archive_role(&mut tx, id, now).await?;
    finish(tx, &session, now, "role.archive", Some(&before), None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Checks the Owner invariant, appends the audit entry and commits.
async fn finish(
    mut tx: Tx,
    session: &UserSession,
    now: sultan_core::time::Timestamp,
    action: &str,
    before: Option<&Role>,
    after: Option<&Role>,
) -> Result<(), ApiError> {
    ensure_owner_remains(&mut tx).await?;
    let id = before
        .or(after)
        .map(|r| r.id.to_string())
        .unwrap_or_default();
    let entry = audit_entry(
        session,
        now,
        action,
        format!("role:{id}"),
        before.map(|r| to_json(&RoleDto::from(r))),
        after.map(|r| to_json(&RoleDto::from(r))),
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grant(permission: &str, limit: Option<i64>) -> GrantDto {
        GrantDto {
            permission: permission.to_owned(),
            limit,
        }
    }

    #[test]
    fn grant_parsing() {
        let cases = [
            (grant("sale.create", None), Ok(None)),
            (grant("sale.discount", Some(1_000)), Ok(Some(1_000))),
            (grant("cash.out", Some(50_000)), Ok(Some(50_000))),
            (grant("sale.nope", None), Err("invalid_field")),
            (grant("sale.discount", Some(-1)), Err("invalid_field")),
            (grant("sale.discount", Some(10_001)), Err("invalid_grant")),
            (grant("sale.create", Some(5)), Err("invalid_grant")),
            (grant("cash.out", Some(-5)), Err("invalid_grant")),
        ];
        for (dto, expected) in cases {
            let got = parse_grant(&dto).map(|g| {
                RoleDto::from(&Role {
                    id: RoleId::generate(),
                    name: String::new(),
                    grants: vec![g],
                })
                .grants[0]
                    .limit
            });
            assert_eq!(got.map_err(|e| e.code()), expected, "{dto:?}");
        }
    }

    #[test]
    fn grants_are_sorted_and_unique() {
        let grants = parse_grants(&[grant("sale.void", None), grant("cash.in", None)]).unwrap();
        let keys: Vec<_> = grants.iter().map(|g| g.permission().as_str()).collect();
        assert_eq!(keys, ["cash.in", "sale.void"]);
        let err = parse_grants(&[grant("cash.in", None), grant("cash.in", None)]).unwrap_err();
        assert_eq!(err.code(), "invalid_field");
    }
}
