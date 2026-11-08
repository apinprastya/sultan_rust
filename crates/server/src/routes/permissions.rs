//! The permission catalog (spec 2.1 §3, §8): `GET /permissions`, read-only, for any logged-in
//! user. The catalog is fixed in code (`core::access::Permission`).

use axum::Json;
use serde::Serialize;
use sultan_core::access::{LimitKind, Permission};
use utoipa::ToSchema;

use crate::error::ApiError;
use crate::extract::UserSession;

/// One catalog entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PermissionDto {
    /// Stable key, e.g. `sale.discount`.
    pub key: &'static str,
    /// `pct` (limit in basis points), `amount` (limit in minor units) or `null` (no limit).
    pub limit_kind: Option<&'static str>,
    /// Whether a manager override can approve it for a user who lacks it (spec 2.1 §6).
    pub overridable: bool,
}

impl From<Permission> for PermissionDto {
    fn from(p: Permission) -> Self {
        Self {
            key: p.as_str(),
            limit_kind: p.limit_kind().map(|k| match k {
                LimitKind::Pct => "pct",
                LimitKind::Amount => "amount",
            }),
            overridable: p.is_overridable(),
        }
    }
}

/// `GET /api/v1/permissions`: the whole catalog in catalog order.
///
/// # Errors
/// `401 invalid_session`.
#[utoipa::path(get, path = "/api/v1/permissions", tag = "roles", security(("bearer" = [])),
    responses((status = 200, body = [PermissionDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list(_session: UserSession) -> Result<Json<Vec<PermissionDto>>, ApiError> {
    Ok(Json(
        Permission::ALL
            .into_iter()
            .map(PermissionDto::from)
            .collect(),
    ))
}
