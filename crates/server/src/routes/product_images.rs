//! Product images (spec 2.2 §8): `GET|PUT|DELETE /products/{id}/image`.
//!
//! - `PUT` takes the **raw image bytes** as the body (any `Content-Type`; the format is sniffed):
//!   PNG, JPEG or WebP, at most 10 MB (`413 payload_too_large`); anything else is
//!   `422 invalid_image`. The server scales it to fit within 512×512 (never up) and stores a JPEG
//!   of at most 256 KB ([`crate::product_image`]). Answers the stored image's metadata: `201` for
//!   a product's first image, `200` for a replacement or the same image again.
//! - `GET` returns the stored bytes with their `Content-Type` and an `ETag`; a matching
//!   `If-None-Match` is `304 Not Modified`. Session or device token.
//! - `DELETE` removes the image: `204`, also when there was none (`404` only for an unknown
//!   product).
//!
//! Writes: see [`super::catalog`]; they publish `catalog.changed {scope: "product", product_id}`.

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use sultan_core::ids::ProductId;
use sultan_db::catalog::{self, ProductImage, Written};
use utoipa::ToSchema;

use super::catalog::{actor, publish_changed, require_manage, retrying, written_status};
use super::parse_id;
use crate::error::ApiError;
use crate::extract::{CatalogReader, RequireHq, UserSession};
use crate::product_image::{self, MAX_UPLOAD_BYTES, STORED_MIME};
use crate::state::AppState;

/// The stored image's metadata (answer of `PUT /products/{id}/image`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ImageDto {
    /// The product.
    pub product_id: String,
    /// Media type of the stored image (`image/jpeg`).
    pub mime: String,
    /// Width in pixels (≤ 512).
    pub width: u32,
    /// Height in pixels (≤ 512).
    pub height: u32,
    /// Size in bytes (≤ 256 KB).
    pub bytes: usize,
    /// The `ETag` `GET` answers with.
    pub etag: String,
}

/// `PUT /api/v1/products/{id}/image`: upload (see the module docs).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`, `413 payload_too_large`,
/// `422 invalid_image`.
#[utoipa::path(put, path = "/api/v1/products/{id}/image", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Product ID")),
    request_body(content = Vec<u8>, description = "PNG, JPEG or WebP bytes, at most 10 MB",
        content_type = "application/octet-stream"),
    responses((status = 201, body = ImageDto), (status = 200, body = ImageDto),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 413, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    body: Body,
) -> Result<(StatusCode, Json<ImageDto>), ApiError> {
    let id: ProductId = parse_id(&id, "id")?;
    require_manage(&state, &session).await?;
    let bytes = to_bytes(body, MAX_UPLOAD_BYTES)
        .await
        .map_err(|_| product_image::too_large())?;
    let processed = product_image::process_blocking(bytes).await?;
    let image = ProductImage {
        mime: STORED_MIME.to_owned(),
        width: processed.width,
        height: processed.height,
        data: processed.data,
    };
    let (state, actor, image_ref, now) = (&state, &actor(&session), &image, state.now());
    let written = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = catalog::put_product_image(&mut tx, id, image_ref, actor, now).await?;
        tx.commit().await?;
        Ok(written)
    })
    .await?;
    publish_changed(
        state,
        written,
        "product",
        json!({ "product_id": id.to_string() }),
    );
    Ok((
        written_status(written),
        Json(ImageDto {
            product_id: id.to_string(),
            mime: image.mime,
            width: image.width,
            height: image.height,
            bytes: image.data.len(),
            etag: product_image::etag(&image.data),
        }),
    ))
}

/// `GET /api/v1/products/{id}/image`: the image bytes.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found` (no image).
#[utoipa::path(get, path = "/api/v1/products/{id}/image", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Product ID"),
        ("If-None-Match" = Option<String>, Header, description = "A previous ETag")),
    responses((status = 200, description = "The image (`image/jpeg`), with an `ETag`",
            content_type = "image/jpeg"),
        (status = 304, description = "Not modified"),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    _reader: CatalogReader,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id: ProductId = parse_id(&id, "id")?;
    let image = catalog::get_product_image(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("product image"))?;
    let etag = product_image::etag(&image.data);
    let etag_value = HeaderValue::from_str(&etag).map_err(|_| ApiError::internal())?;
    let cache = HeaderValue::from_static("private, no-cache");
    let matches = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag || t.trim() == "*"));
    if matches {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(ETAG, etag_value), (CACHE_CONTROL, cache)],
        )
            .into_response());
    }
    let mime = HeaderValue::from_str(&image.mime).map_err(|_| ApiError::internal())?;
    Ok((
        [
            (CONTENT_TYPE, mime),
            (ETAG, etag_value),
            (CACHE_CONTROL, cache),
        ],
        image.data,
    )
        .into_response())
}

/// `DELETE /api/v1/products/{id}/image` → `204`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found` (unknown product).
#[utoipa::path(delete, path = "/api/v1/products/{id}/image", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Product ID")),
    responses((status = 204, description = "Removed (or there was none)"),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem)))]
pub async fn delete(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: ProductId = parse_id(&id, "id")?;
    require_manage(&state, &session).await?;
    if catalog::get_product(&state.db, id).await?.is_none() {
        return Err(ApiError::not_found("product"));
    }
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let deleted = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let deleted = catalog::delete_product_image(&mut tx, id, actor, now).await?;
        tx.commit().await?;
        Ok(deleted)
    })
    .await?;
    let written = if deleted {
        Written::Updated
    } else {
        Written::Unchanged
    };
    publish_changed(
        state,
        written,
        "product",
        json!({ "product_id": id.to_string() }),
    );
    Ok(StatusCode::NO_CONTENT)
}
