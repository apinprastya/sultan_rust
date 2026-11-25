//! Nota photos of a cash movement (spec 2.7 §2a, §11; phase K6).
//!
//! - `PUT /cash-movements/{id}/photos/{photo_id}` takes the **raw image bytes** (any
//!   `Content-Type`; the format is sniffed): PNG, JPEG or WebP, at most 10 MB
//!   (`413 payload_too_large`), anything else `422 invalid_image`. The image is scaled to fit
//!   1600×1600 and stored as a JPEG of at most 400 KB ([`crate::product_image::NOTA`]) in the
//!   node's file store ([`crate::attachments`]), **before** the database row; the row, its audit
//!   entry `cash_movement.photo` and its outbox event `cash_movement.photo_added`
//!   `{photo_id, movement_id, sha256, size}` are one transaction. `photo_id` is a client
//!   `UUIDv7`. Answers `201` with the photo; the same `photo_id` with the same stored hash again
//!   is `200` and changes nothing; the same `photo_id` with another image or movement is
//!   `409 conflict`; a fourth photo is `409 photo_limit` (checked before the image is processed,
//!   and again in the transaction). A photo is never deleted or replaced.
//! - **Who:** at the till (a PIN session), the movement's own cashier at its register while its
//!   cashier session is open; in the back office (a password session), `cash.out` (any limit) at
//!   the movement's branch. Anyone else: `403 permission_denied`. An unknown movement is `404`.
//! - `GET /cash-movements/{id}/photos/{photo_id}`: the JPEG with an `ETag` (`304` on a matching
//!   `If-None-Match`), for anyone who may read the movement (its poster, or `report.sales` at its
//!   branch, as `GET /cash-movements/{id}`). A photo of another movement, or whose file is not on
//!   this node (an HQ copy whose bytes have not synced yet), is `404 not_found`.
//!
//! A new photo publishes `cash.moved {session_id, movement_id, number, kind, photo_id}` to the
//! branch, so an open back-office list refreshes its photo count and missing-nota flag.

use axum::Json;
use axum::body::{Body, to_bytes};
use axum::extract::{Path, State};
use axum::http::header::{CACHE_CONTROL, CONTENT_TYPE, ETAG, IF_NONE_MATCH};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use serde_json::json;
use sultan_core::access::{AccessError, BranchScope, Permission};
use sultan_core::cash::{CashError, MAX_PHOTOS};
use sultan_core::ids::{CashMovementId, NotaPhotoId};
use sultan_db::cash::{self, CashMovementRecord, NotaPhoto};
use sultan_db::cashier_sessions;
use utoipa::ToSchema;
use uuid::Uuid;

use super::cash::{Fail, run};
use super::cash_movements::readable;
use super::catalog::actor;
use super::parse_id;
use crate::access::Caller;
use crate::attachments::{self, Attachments};
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::UserSession;
use crate::product_image::{self, MAX_UPLOAD_BYTES, NOTA, STORED_MIME};
use crate::state::AppState;

/// A stored nota photo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PhotoDto {
    /// The photo ID.
    pub id: String,
    /// The cash movement.
    pub cash_movement_id: String,
    /// SHA-256 of the stored JPEG, lower-case hex (its file name and sync key).
    pub sha256: String,
    /// Size of the stored JPEG in bytes (at most 400 KB).
    pub size: u64,
    /// Who added it.
    pub added_by: String,
    /// When (unix ms).
    pub added_at: i64,
    /// The `ETag` `GET` answers with.
    pub etag: String,
}

impl PhotoDto {
    fn new(photo: &NotaPhoto) -> Self {
        Self {
            id: photo.id.to_string(),
            cash_movement_id: photo.movement.to_string(),
            sha256: photo.sha256_hex(),
            size: photo.size,
            added_by: photo.added_by.to_string(),
            added_at: photo.added_at.unix_millis(),
            etag: etag(&photo.sha256),
        }
    }
}

/// The `ETag` of a stored file: the same as [`product_image::etag`] of its bytes (a quoted
/// SHA-256 prefix), from the stored hash without reading the file.
fn etag(hash: &[u8; 32]) -> String {
    format!("\"{}\"", &attachments::hex(hash)[..32])
}

fn store(state: &AppState) -> Result<Attachments, ApiError> {
    state.attachments.clone().ok_or_else(|| {
        tracing::error!("nota photo: this node has no attachment directory");
        ApiError::internal()
    })
}

fn photo_id(value: &str) -> Result<NotaPhotoId, ApiError> {
    let uuid: Uuid = parse_id(value, "photo_id")?;
    if uuid.get_version_num() == 7 {
        Ok(NotaPhotoId::from(uuid))
    } else {
        Err(ApiError::invalid_body("`photo_id` must be a UUIDv7"))
    }
}

fn denied() -> ApiError {
    AccessError::PermissionDenied {
        permission: Permission::CashOut,
    }
    .into()
}

/// Whether `caller` may add a photo to `record` (see the module docs).
async fn may_add(
    state: &AppState,
    caller: &UserSession,
    record: &CashMovementRecord,
) -> Result<(), ApiError> {
    if let Some(register) = caller.session.register {
        let open = cashier_sessions::get(&state.db, record.movement.session)
            .await?
            .is_some_and(|s| s.session.is_open());
        return if register == record.register && record.user == caller.user.id && open {
            Ok(())
        } else {
            Err(denied())
        };
    }
    let access = Caller::load(&state.db, caller).await?;
    if access
        .at_scope(BranchScope::Branch(record.branch))
        .contains(Permission::CashOut)
    {
        Ok(())
    } else {
        Err(denied())
    }
}

/// `PUT /api/v1/cash-movements/{id}/photos/{photo_id}`: adds a nota photo (see the module docs).
///
/// # Errors
/// `400 invalid_body`, `403 permission_denied`, `404 not_found`, `409 photo_limit` /
/// `conflict`, `413 payload_too_large`, `422 invalid_image`.
#[utoipa::path(put, path = "/api/v1/cash-movements/{id}/photos/{photo_id}", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cash movement ID"),
        ("photo_id" = String, Path, description = "Client-generated UUIDv7")),
    request_body(content = Vec<u8>, description = "PNG, JPEG or WebP bytes, at most 10 MB",
        content_type = "application/octet-stream"),
    responses((status = 201, body = PhotoDto), (status = 200, body = PhotoDto),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 413, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    caller: UserSession,
    Path((id, photo)): Path<(String, String)>,
    body: Body,
) -> Result<(StatusCode, Json<PhotoDto>), ApiError> {
    let id: CashMovementId = parse_id(&id, "id")?;
    let photo = photo_id(&photo)?;
    let record = cash::get_cash_movement(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cash movement"))?;
    may_add(&state, &caller, &record).await?;
    let known = cash::get_photo(&state.db, photo).await?;
    if known.is_none() && cash::count_photos(&state.db, id).await? >= MAX_PHOTOS {
        return Err(CashError::PhotoLimit { max: MAX_PHOTOS }.into());
    }
    let files = store(&state)?;
    let bytes = to_bytes(body, MAX_UPLOAD_BYTES)
        .await
        .map_err(|_| product_image::too_large())?;
    let processed = product_image::process_blocking_with(bytes, NOTA).await?;
    let data = processed.data;
    let size = data.len() as u64;
    let sha256 = tokio::task::spawn_blocking(move || files.store(&data))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|e| {
            tracing::error!(error = %e, "nota photo: the file cannot be written");
            ApiError::internal()
        })?;
    let now = state.now();
    let new = NotaPhoto {
        id: photo,
        movement: id,
        sha256,
        size,
        added_by: caller.user.id,
        added_at: now,
    };
    let (state_ref, actor, new_ref) = (&state, &actor(&caller), &new);
    let written = run("nota photo add", || async move {
        let mut tx = state_ref.db.begin().await?;
        let written = cash::add_photo(&mut tx, new_ref, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(written)
    })
    .await?;
    if written == cash::Written::Unchanged {
        let stored = cash::get_photo(&state.db, photo)
            .await?
            .ok_or_else(ApiError::internal)?;
        return Ok((StatusCode::OK, Json(PhotoDto::new(&stored))));
    }
    state.events.publish(Event::for_branch(
        "cash.moved",
        record.branch,
        json!({
            "session_id": record.movement.session.to_string(),
            "movement_id": id.to_string(),
            "number": record.number,
            "kind": record.movement.kind.as_str(),
            "photo_id": photo.to_string(),
        }),
    ));
    Ok((StatusCode::CREATED, Json(PhotoDto::new(&new))))
}

/// `GET /api/v1/cash-movements/{id}/photos/{photo_id}`: the JPEG (see the module docs).
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/cash-movements/{id}/photos/{photo_id}", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cash movement ID"),
        ("photo_id" = String, Path, description = "Photo ID"),
        ("If-None-Match" = Option<String>, Header, description = "A previous ETag")),
    responses((status = 200, description = "The photo (`image/jpeg`), with an `ETag`",
            content_type = "image/jpeg"),
        (status = 304, description = "Not modified"),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    caller: UserSession,
    Path((id, photo)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let record = readable(&state, &caller, &id).await?;
    let photo: NotaPhotoId = parse_id(&photo, "photo_id")?;
    let photo = cash::get_photo(&state.db, photo)
        .await?
        .filter(|p| p.movement == record.movement.id)
        .ok_or_else(|| ApiError::not_found("nota photo"))?;
    let tag = etag(&photo.sha256);
    let etag_value = HeaderValue::from_str(&tag).map_err(|_| ApiError::internal())?;
    let cache = HeaderValue::from_static("private, no-cache");
    let matches = headers
        .get(IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == tag || t.trim() == "*"));
    if matches {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [(ETAG, etag_value), (CACHE_CONTROL, cache)],
        )
            .into_response());
    }
    let files = store(&state)?;
    let hash = photo.sha256;
    let data = tokio::task::spawn_blocking(move || files.read(&hash))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ApiError::not_found("nota photo file")
            } else {
                tracing::error!(error = %e, "nota photo: the file cannot be read");
                ApiError::internal()
            }
        })?;
    Ok((
        [
            (CONTENT_TYPE, HeaderValue::from_static(STORED_MIME)),
            (ETAG, etag_value),
            (CACHE_CONTROL, cache),
        ],
        data,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_etag_matches_the_product_image_etag() {
        let data = b"jpeg bytes";
        assert_eq!(etag(&attachments::sha256(data)), product_image::etag(data));
    }

    #[test]
    fn photo_ids_are_uuidv7() {
        assert!(photo_id(&Uuid::now_v7().to_string()).is_ok());
        let err = photo_id(&Uuid::nil().to_string()).unwrap_err();
        assert_eq!(err.code(), "invalid_body");
        assert_eq!(photo_id("x").unwrap_err().code(), "invalid_body");
    }
}
