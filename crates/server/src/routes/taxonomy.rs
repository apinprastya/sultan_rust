//! Categories and brands (spec 2.2 §8, §9, §12): `GET /categories` (tree),
//! `GET|PUT|DELETE /categories/{id}`, `GET /brands`, `GET|PUT|DELETE /brands/{id}`.
//!
//! Reads: a session or a device token. Lists leave archived entries out; `include_archived=true`
//! adds them and needs `catalog.view_archived` (any role assignment). `GET` of one archived
//! category or brand needs nothing extra: products keep showing their (archived) category and
//! brand. Writes: see [`super::catalog`]; archiving is `archived: true` in the `PUT` body. `201`
//! on create, `200` on update and on a replay. `DELETE` hard-deletes a never-used entry (§9, C3b)
//! → `204`: a category without products (archived ones count) or child categories, a brand
//! without products; otherwise `409 in_use` (`reason`: `has_products` / `has_children`), so
//! archive it instead. Every write that changed something publishes `catalog.changed {scope:
//! "category", category_id}` / `{scope: "brand", brand_id}` (a delete too).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::catalog::{Brand, Category};
use sultan_core::ids::{BrandId, CategoryId};
use sultan_db::catalog::{self, CategoryNode, Written};
use utoipa::{IntoParams, ToSchema};

use super::catalog::{
    actor, not_found, publish_changed, require_manage, require_view_archived, retrying,
    written_status,
};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, CatalogReader, RequireHq, UserSession};
use crate::state::AppState;

/// Query of the category and brand lists.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ListQuery {
    /// Include archived entries (needs `catalog.view_archived`).
    #[serde(default)]
    pub include_archived: bool,
}

/// Body of `PUT /categories/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CategoryBody {
    /// Parent category; `null` = a root. Moving is allowed; cycles and depth > 4 are rejected.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Name, unique among siblings.
    pub name: String,
    /// Optional short code `[A-Z0-9]{0,8}` (stored upper case).
    #[serde(default)]
    pub code: String,
    /// `true` archives (hidden from pickers; products keep it).
    #[serde(default)]
    pub archived: bool,
}

/// A category.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CategoryDto {
    /// ID.
    pub id: String,
    /// Parent.
    pub parent_id: Option<String>,
    /// Name.
    pub name: String,
    /// Short code.
    pub code: String,
    /// Archived.
    pub archived: bool,
}

impl From<&Category> for CategoryDto {
    fn from(c: &Category) -> Self {
        Self {
            id: c.id.to_string(),
            parent_id: c.parent.map(|p| p.to_string()),
            name: c.name.clone(),
            code: c.code.clone(),
            archived: c.archived,
        }
    }
}

/// A category with its children (`GET /categories`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CategoryTreeDto {
    /// ID.
    pub id: String,
    /// Parent.
    pub parent_id: Option<String>,
    /// Name.
    pub name: String,
    /// Short code.
    pub code: String,
    /// Archived.
    pub archived: bool,
    /// Children, by name.
    #[schema(no_recursion)]
    pub children: Vec<CategoryTreeDto>,
}

fn tree_dto(node: &CategoryNode) -> CategoryTreeDto {
    let c = CategoryDto::from(&node.category);
    CategoryTreeDto {
        id: c.id,
        parent_id: c.parent_id,
        name: c.name,
        code: c.code,
        archived: c.archived,
        children: node.children.iter().map(tree_dto).collect(),
    }
}

/// `GET /api/v1/categories`: the category tree, roots and children by name.
///
/// # Errors
/// `401`, `403 permission_denied` (`include_archived` without `catalog.view_archived`).
#[utoipa::path(get, path = "/api/v1/categories", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(ListQuery),
    responses((status = 200, body = [CategoryTreeDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_categories(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<CategoryTreeDto>>, ApiError> {
    if query.include_archived {
        require_view_archived(&state, &reader).await?;
    }
    let tree = catalog::list_categories(&state.db, query.include_archived).await?;
    Ok(Json(tree.iter().map(tree_dto).collect()))
}

/// `GET /api/v1/categories/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/categories/{id}", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Category ID")),
    responses((status = 200, body = CategoryDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_category(
    State(state): State<AppState>,
    _reader: CatalogReader,
    Path(id): Path<String>,
) -> Result<Json<CategoryDto>, ApiError> {
    let id: CategoryId = parse_id(&id, "id")?;
    catalog::get_category(&state.db, id)
        .await?
        .map(|c| Json(CategoryDto::from(&c)))
        .ok_or_else(|| ApiError::not_found("category"))
}

/// `PUT /api/v1/categories/{id}`: create (`201`) or change (`200`) a category.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422` (`category_cycle`,
/// `category_too_deep`, `duplicate_category_name`, `unknown_category`, `invalid_field`, … with
/// `errors`).
#[utoipa::path(put, path = "/api/v1/categories/{id}", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = CategoryBody,
    responses((status = 201, body = CategoryDto), (status = 200, body = CategoryDto),
        (status = 403, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_category(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CategoryBody>,
) -> Result<(StatusCode, Json<CategoryDto>), ApiError> {
    let id: CategoryId = parse_put_id(&id)?;
    require_manage(&state, &session).await?;
    let category = Category {
        id,
        parent: parse_opt_id(body.parent_id.as_deref(), "parent_id")?,
        name: body.name,
        code: body.code,
        archived: body.archived,
    };
    let (state, actor, category, now) = (&state, &actor(&session), &category, state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = catalog::put_category(&mut tx, category, actor, now).await?;
        let stored = catalog::get_category(&mut tx, id)
            .await?
            .ok_or_else(|| not_found("category"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish_changed(
        state,
        written,
        "category",
        json!({ "category_id": id.to_string() }),
    );
    Ok((written_status(written), Json(CategoryDto::from(&stored))))
}

/// `DELETE /api/v1/categories/{id}`: hard-deletes a category without products (archived ones
/// count) or child categories (spec 2.2 §9, C3b) → `204`. Audited (`catalog.category.delete`),
/// with an outbox row, and publishes `catalog.changed {scope: "category", category_id}`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `404 not_found`,
/// `409 in_use` (`reason`: `has_products` / `has_children`).
#[utoipa::path(delete, path = "/api/v1/categories/{id}", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Category ID")),
    responses((status = 204, description = "Deleted"),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem)))]
pub async fn delete_category(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: CategoryId = parse_id(&id, "id")?;
    require_manage(&state, &session).await?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    retrying(|| async move {
        let mut tx = state.db.begin().await?;
        catalog::delete_category(&mut tx, id, actor, now).await?;
        tx.commit().await?;
        Ok(())
    })
    .await?;
    publish_changed(
        state,
        Written::Updated,
        "category",
        json!({ "category_id": id.to_string() }),
    );
    Ok(StatusCode::NO_CONTENT)
}

/// Body of `PUT /brands/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct BrandBody {
    /// Name, unique (ignoring case and accents).
    pub name: String,
    /// `true` archives.
    #[serde(default)]
    pub archived: bool,
}

/// A brand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BrandDto {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Archived.
    pub archived: bool,
}

impl From<&Brand> for BrandDto {
    fn from(b: &Brand) -> Self {
        Self {
            id: b.id.to_string(),
            name: b.name.clone(),
            archived: b.archived,
        }
    }
}

/// `GET /api/v1/brands`: brands by name.
///
/// # Errors
/// `401`, `403 permission_denied` (`include_archived` without `catalog.view_archived`).
#[utoipa::path(get, path = "/api/v1/brands", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(ListQuery),
    responses((status = 200, body = [BrandDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_brands(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<BrandDto>>, ApiError> {
    if query.include_archived {
        require_view_archived(&state, &reader).await?;
    }
    let brands = catalog::list_brands(&state.db, query.include_archived).await?;
    Ok(Json(brands.iter().map(BrandDto::from).collect()))
}

/// `GET /api/v1/brands/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/brands/{id}", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Brand ID")),
    responses((status = 200, body = BrandDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_brand(
    State(state): State<AppState>,
    _reader: CatalogReader,
    Path(id): Path<String>,
) -> Result<Json<BrandDto>, ApiError> {
    let id: BrandId = parse_id(&id, "id")?;
    catalog::get_brand(&state.db, id)
        .await?
        .map(|b| Json(BrandDto::from(&b)))
        .ok_or_else(|| ApiError::not_found("brand"))
}

/// `PUT /api/v1/brands/{id}`: create (`201`) or change (`200`) a brand.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422`
/// (`duplicate_brand_name`, `invalid_field` with `errors`).
#[utoipa::path(put, path = "/api/v1/brands/{id}", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = BrandBody,
    responses((status = 201, body = BrandDto), (status = 200, body = BrandDto),
        (status = 403, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_brand(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<BrandBody>,
) -> Result<(StatusCode, Json<BrandDto>), ApiError> {
    let id: BrandId = parse_put_id(&id)?;
    require_manage(&state, &session).await?;
    let brand = Brand {
        id,
        name: body.name,
        archived: body.archived,
    };
    let (state, actor, brand, now) = (&state, &actor(&session), &brand, state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = catalog::put_brand(&mut tx, brand, actor, now).await?;
        let stored = catalog::get_brand(&mut tx, id)
            .await?
            .ok_or_else(|| not_found("brand"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish_changed(
        state,
        written,
        "brand",
        json!({ "brand_id": id.to_string() }),
    );
    Ok((written_status(written), Json(BrandDto::from(&stored))))
}

/// `DELETE /api/v1/brands/{id}`: hard-deletes a brand without products (archived ones count;
/// spec 2.2 §9, C3b) → `204`. Audited (`catalog.brand.delete`), with an outbox row, and
/// publishes `catalog.changed {scope: "brand", brand_id}`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `404 not_found`,
/// `409 in_use` (`reason`: `has_products`).
#[utoipa::path(delete, path = "/api/v1/brands/{id}", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Brand ID")),
    responses((status = 204, description = "Deleted"),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem)))]
pub async fn delete_brand(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: BrandId = parse_id(&id, "id")?;
    require_manage(&state, &session).await?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    retrying(|| async move {
        let mut tx = state.db.begin().await?;
        catalog::delete_brand(&mut tx, id, actor, now).await?;
        tx.commit().await?;
        Ok(())
    })
    .await?;
    publish_changed(
        state,
        Written::Updated,
        "brand",
        json!({ "brand_id": id.to_string() }),
    );
    Ok(StatusCode::NO_CONTENT)
}
