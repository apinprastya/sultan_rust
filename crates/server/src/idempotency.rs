//! `Idempotency-Key` on action `POST`s (`docs/plan.md` §1: "Action POSTs carry an
//! `Idempotency-Key` header. Registers retry after LAN hiccups"; spec 2.1 §9 case 17).
//!
//! A middleware on the action `POST` routes (see [`routes`](crate::routes)):
//!
//! - **No key**: the request runs normally. The spec does not make the key mandatory.
//! - **New key**: the key is reserved, the handler runs, and its response (status, content type,
//!   body) is stored, unless it is a server error (`5xx`), in which case the key is freed so the
//!   client can retry.
//! - **Same key, same request** (method, path and body): the stored response is sent again
//!   **without running the handler**, with `Idempotency-Replayed: true`. A retried
//!   `POST /cashier-sessions` therefore gets the original `201`, not `409`.
//! - **Same key, different request**: `422 idempotency_key_reused`.
//! - **Same key while the first request still runs**: `409 idempotency_in_progress`.
//! - A key must be 1–255 visible ASCII characters, otherwise `400 invalid_idempotency_key`.
//!
//! Keys are scoped to the **caller**: the SHA-256 of the credentials sent with the request
//! (`Authorization` and `X-Register-Token`). A replay therefore needs the same credentials as the
//! original request, and two callers never see each other's responses. A request without any
//! credentials is not deduplicated (every keyed endpoint needs a session anyway).
//!
//! **Retention:** stored responses are kept for [`RETENTION_MS`] (24 hours), far longer than any
//! LAN retry, and purged lazily when new keys arrive. A reservation whose request never finished
//! (the server stopped mid-request) is taken over after [`STALE_MS`].
//!
//! **Not applied** to `POST`s whose response carries a secret (`/auth/pin`, `/auth/password`,
//! `/registers/pair`, `/registers/{id}/pairing-code`, `/overrides`): storing the response would
//! keep a token or pairing code in clear, and the spec stores only hashes of secrets (spec 2.1
//! §2, §4). Retrying those is harmless (another session or token is issued; unused ones expire).

use axum::body::{Body, to_bytes};
use axum::extract::{OriginalUri, Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};
use sultan_db::TokenHash;
use sultan_db::idempotency::{self, Begin, KeyedRequest, StoredResponse};

use crate::error::ApiError;
use crate::extract::REGISTER_TOKEN_HEADER;
use crate::state::AppState;

/// The request header.
pub const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
/// Set on a replayed response.
pub const REPLAYED_HEADER: &str = "idempotency-replayed";
/// How long stored responses are kept: 24 hours.
pub const RETENTION_MS: i64 = 24 * 60 * 60 * 1_000;
/// After this long, an unfinished reservation is considered abandoned: 1 minute.
pub const STALE_MS: i64 = 60 * 1_000;
/// Largest request body that is hashed (larger bodies are refused by axum's own limit first).
const MAX_BODY: usize = 2 * 1024 * 1024;

/// The key, if the header is present: `Ok(None)` without the header.
fn key(headers: &HeaderMap) -> Result<Option<String>, ApiError> {
    let Some(value) = headers.get(IDEMPOTENCY_KEY_HEADER) else {
        return Ok(None);
    };
    let bad = || {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_idempotency_key",
            "`Idempotency-Key` must be 1 to 255 visible ASCII characters",
        )
    };
    let key = value.to_str().map_err(|_| bad())?;
    if key.is_empty() || key.len() > 255 || !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(bad());
    }
    Ok(Some(key.to_owned()))
}

/// Hash of the caller's credentials, or `None` when the request carries none.
fn caller(headers: &HeaderMap) -> Option<TokenHash> {
    let bearer = headers.get(AUTHORIZATION).map(HeaderValue::as_bytes);
    let device = headers
        .get(REGISTER_TOKEN_HEADER)
        .map(HeaderValue::as_bytes);
    if bearer.is_none() && device.is_none() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(b"sultan.idempotency.caller\0");
    hasher.update(bearer.unwrap_or_default());
    hasher.update(b"\0");
    hasher.update(device.unwrap_or_default());
    Some(hasher.finalize().into())
}

fn request_hash(method: &Method, path: &str, body: &[u8]) -> TokenHash {
    let mut hasher = Sha256::new();
    hasher.update(method.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(path.as_bytes());
    hasher.update(b"\0");
    hasher.update(body);
    hasher.finalize().into()
}

fn replay(stored: StoredResponse) -> Response {
    let status = StatusCode::from_u16(stored.status).unwrap_or(StatusCode::OK);
    let mut response = (status, stored.body).into_response();
    let headers = response.headers_mut();
    headers.remove(CONTENT_TYPE);
    if let Some(value) = stored
        .content_type
        .and_then(|c| HeaderValue::from_str(&c).ok())
    {
        headers.insert(CONTENT_TYPE, value);
    }
    headers.insert(REPLAYED_HEADER, HeaderValue::from_static("true"));
    response
}

/// The middleware (see the module docs).
pub async fn middleware(State(state): State<AppState>, request: Request, next: Next) -> Response {
    handle(&state, request, next)
        .await
        .unwrap_or_else(IntoResponse::into_response)
}

async fn handle(state: &AppState, request: Request, next: Next) -> Result<Response, ApiError> {
    if request.method() != Method::POST {
        return Ok(next.run(request).await);
    }
    let Some(key) = key(request.headers())? else {
        return Ok(next.run(request).await);
    };
    let Some(caller) = caller(request.headers()) else {
        return Ok(next.run(request).await);
    };
    let (parts, body) = request.into_parts();
    let body = to_bytes(body, MAX_BODY)
        .await
        .map_err(|_| ApiError::invalid_body("request body too large"))?;
    let path = parts
        .extensions
        .get::<OriginalUri>()
        .map_or(&parts.uri, |o| &o.0)
        .path_and_query()
        .map_or_else(|| parts.uri.path().to_owned(), |p| p.as_str().to_owned());
    let hash = request_hash(&parts.method, &path, &body);
    let now = state.now();
    let keyed = KeyedRequest {
        caller: &caller,
        key: &key,
        method: parts.method.as_str(),
        path: &path,
        request_hash: &hash,
    };
    let begun = idempotency::begin(
        &state.db,
        &keyed,
        now,
        now.saturating_add_millis(-RETENTION_MS),
        now.saturating_add_millis(-STALE_MS),
    )
    .await?;
    match begun {
        Begin::Started => {}
        Begin::Replay(stored) => return Ok(replay(stored)),
        Begin::Mismatch => {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "idempotency_key_reused",
                "this Idempotency-Key was already used for a different request",
            ));
        }
        Begin::InProgress => {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "idempotency_in_progress",
                "a request with this Idempotency-Key is still running; retry later",
            ));
        }
    }

    let response = next.run(Request::from_parts(parts, Body::from(body))).await;
    let (parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX).await.ok();
    let stored = bytes
        .as_ref()
        .filter(|_| !parts.status.is_server_error())
        .and_then(|b| String::from_utf8(b.to_vec()).ok())
        .map(|text| StoredResponse {
            status: parts.status.as_u16(),
            content_type: parts
                .headers
                .get(CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned),
            body: text,
        });
    // The handler has run; a failure to record its outcome must not hide its response.
    let _ = match &stored {
        Some(stored) => idempotency::complete(&state.db, &caller, &key, stored).await,
        None => idempotency::abandon(&state.db, &caller, &key).await,
    };
    Ok(match bytes {
        Some(bytes) => Response::from_parts(parts, Body::from(bytes)),
        None => ApiError::internal().into_response(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_rules() {
        let long = "k".repeat(256);
        let cases = [
            (None, Ok(None)),
            (Some("abc-123"), Ok(Some("abc-123"))),
            (Some(""), Err(())),
            (Some("has space"), Err(())),
            (Some(long.as_str()), Err(())),
        ];
        for (value, expected) in cases {
            let mut headers = HeaderMap::new();
            if let Some(v) = value {
                headers.insert(IDEMPOTENCY_KEY_HEADER, HeaderValue::from_str(v).unwrap());
            }
            let got = key(&headers);
            match expected {
                Ok(k) => assert_eq!(got.unwrap().as_deref(), k, "{value:?}"),
                Err(()) => assert_eq!(got.unwrap_err().code(), "invalid_idempotency_key"),
            }
        }
        let mut headers = HeaderMap::new();
        headers.insert(
            IDEMPOTENCY_KEY_HEADER,
            HeaderValue::from_bytes(b"caf\xe9").unwrap(),
        );
        assert!(key(&headers).is_err());
    }

    #[test]
    fn caller_depends_on_both_credentials() {
        let with = |bearer: Option<&'static str>, device: Option<&'static str>| {
            let mut headers = HeaderMap::new();
            if let Some(b) = bearer {
                headers.insert(AUTHORIZATION, HeaderValue::from_static(b));
            }
            if let Some(d) = device {
                headers.insert(REGISTER_TOKEN_HEADER, HeaderValue::from_static(d));
            }
            caller(&headers)
        };
        assert_eq!(with(None, None), None);
        let a = with(Some("Bearer a"), None).unwrap();
        assert_eq!(with(Some("Bearer a"), None), Some(a));
        assert_ne!(with(Some("Bearer a"), Some("d")), Some(a));
        assert_ne!(with(Some("Bearer b"), None), Some(a));
        assert!(with(None, Some("d")).is_some());
    }

    #[test]
    fn request_hash_covers_method_path_and_body() {
        let base = request_hash(&Method::POST, "/a", b"{}");
        assert_eq!(base, request_hash(&Method::POST, "/a", b"{}"));
        assert_ne!(base, request_hash(&Method::PUT, "/a", b"{}"));
        assert_ne!(base, request_hash(&Method::POST, "/b", b"{}"));
        assert_ne!(base, request_hash(&Method::POST, "/a", b"{ }"));
    }

    #[test]
    fn replay_restores_status_type_and_marker() {
        let r = replay(StoredResponse {
            status: 201,
            content_type: Some("application/json".into()),
            body: "{}".into(),
        });
        assert_eq!(r.status(), StatusCode::CREATED);
        assert_eq!(r.headers()[CONTENT_TYPE], "application/json");
        assert_eq!(r.headers()[REPLAYED_HEADER], "true");
        let r = replay(StoredResponse {
            status: 204,
            content_type: None,
            body: String::new(),
        });
        assert_eq!(r.status(), StatusCode::NO_CONTENT);
        assert!(r.headers().get(CONTENT_TYPE).is_none());
    }
}
