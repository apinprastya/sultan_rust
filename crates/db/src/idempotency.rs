//! Stored responses for `Idempotency-Key` on action `POST`s (`docs/plan.md` §1, spec 2.1 §9
//! case 17).
//!
//! One row per (caller, key). The server's middleware calls [`begin`] before running the handler:
//! the first request reserves the key (a row without a response), later requests with the same key
//! get the stored response back ([`Begin::Replay`]), a different request under the same key is a
//! [`Begin::Mismatch`], and a request racing the first one sees [`Begin::InProgress`]. After the
//! handler ran, [`complete`] stores the response, or [`abandon`] frees the key (e.g. after a server
//! error, so the client can retry).
//!
//! The caller is identified by a hash of the credentials that sent the request, and requests by a
//! hash of method, path and body; both hashes are computed by the server.

use sultan_core::time::Timestamp;

use crate::TokenHash;
use crate::conn::Conn;
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// The request presented with an `Idempotency-Key`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyedRequest<'a> {
    /// Hash of the caller's credentials.
    pub caller: &'a TokenHash,
    /// The `Idempotency-Key` header value.
    pub key: &'a str,
    /// HTTP method.
    pub method: &'a str,
    /// Request path (with query).
    pub path: &'a str,
    /// Hash of method, path and body.
    pub request_hash: &'a TokenHash,
}

/// A stored response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredResponse {
    /// HTTP status.
    pub status: u16,
    /// `Content-Type`, if the response had one.
    pub content_type: Option<String>,
    /// Response body (UTF-8; empty for `204`).
    pub body: String,
}

/// The outcome of [`begin`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Begin {
    /// The key is new and now reserved for this request: run the handler, then [`complete`] or
    /// [`abandon`].
    Started,
    /// The same request was already answered: send this response again.
    Replay(StoredResponse),
    /// The key was used for a different method, path or body.
    Mismatch,
    /// The first request with this key is still running.
    InProgress,
}

/// Reserves `request`'s key or reports what happened to it before.
///
/// Rows created before `expired_before` are deleted first (the retention window), as are
/// reservations without a response created before `stale_before` (a request that never finished,
/// e.g. the server stopped mid-request), so a retry can take the key over.
///
/// # Errors
/// The database error.
pub async fn begin(
    db: &crate::Db,
    request: &KeyedRequest<'_>,
    now: Timestamp,
    expired_before: Timestamp,
    stale_before: Timestamp,
) -> Result<Begin> {
    purge(db, expired_before).await?;
    sql::execute(
        db.into(),
        "DELETE FROM idempotency_key WHERE caller_hash = ? AND idem_key = ? \
         AND status IS NULL AND created_at < ?",
        args![request.caller, request.key, stale_before],
    )
    .await?;
    let inserted = sql::execute(
        db.into(),
        "INSERT INTO idempotency_key (caller_hash, idem_key, method, path, request_hash, \
         created_at) VALUES (?, ?, ?, ?, ?, ?)",
        args![
            request.caller,
            request.key,
            request.method,
            request.path,
            request.request_hash,
            now,
        ],
    )
    .await;
    match inserted {
        Ok(_) => return Ok(Begin::Started),
        Err(DbError::Conflict(_)) => {}
        Err(other) => return Err(other),
    }
    let stored = sql::fetch_optional(
        db.into(),
        "SELECT method, path, request_hash, status, content_type, body FROM idempotency_key \
         WHERE caller_hash = ? AND idem_key = ?",
        args![request.caller, request.key],
        map_stored,
    )
    .await?;
    // The row can vanish between the insert and the select (abandoned by the first request).
    let Some((method, path, hash, response)) = stored else {
        return Ok(Begin::InProgress);
    };
    if method != request.method || path != request.path || &hash != request.request_hash {
        return Ok(Begin::Mismatch);
    }
    Ok(response.map_or(Begin::InProgress, Begin::Replay))
}

type StoredRow = (String, String, TokenHash, Option<StoredResponse>);

fn map_stored(row: &dyn Row) -> Result<StoredRow> {
    let response = match row.opt_i64("status")? {
        None => None,
        Some(status) => Some(StoredResponse {
            status: u16::try_from(status)
                .map_err(|_| DbError::Corrupt(format!("`status` {status} out of range")))?,
            content_type: row.opt_text("content_type")?,
            body: row.opt_text("body")?.unwrap_or_default(),
        }),
    };
    Ok((
        row.text("method")?,
        row.text("path")?,
        sql::hash(row, "request_hash")?,
        response,
    ))
}

/// Stores the response of a request reserved by [`begin`].
///
/// # Errors
/// [`DbError::NotFound`] if the reservation is gone, or the database error.
pub async fn complete<'c>(
    conn: impl Into<Conn<'c>>,
    caller: &TokenHash,
    key: &str,
    response: &StoredResponse,
) -> Result<()> {
    let changed = sql::execute(
        conn.into(),
        "UPDATE idempotency_key SET status = ?, content_type = ?, body = ? \
         WHERE caller_hash = ? AND idem_key = ? AND status IS NULL",
        args![
            i64::from(response.status),
            response.content_type.as_deref(),
            response.body.as_str(),
            caller,
            key,
        ],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::NotFound("idempotency key reservation"));
    }
    Ok(())
}

/// Frees a reservation made by [`begin`] without storing a response, so the key can be retried.
///
/// # Errors
/// The database error.
pub async fn abandon<'c>(conn: impl Into<Conn<'c>>, caller: &TokenHash, key: &str) -> Result<()> {
    sql::execute(
        conn.into(),
        "DELETE FROM idempotency_key WHERE caller_hash = ? AND idem_key = ? AND status IS NULL",
        args![caller, key],
    )
    .await?;
    Ok(())
}

/// Deletes every row created before `before` (the retention window). Returns how many.
///
/// # Errors
/// The database error.
pub async fn purge<'c>(conn: impl Into<Conn<'c>>, before: Timestamp) -> Result<u64> {
    sql::execute(
        conn.into(),
        "DELETE FROM idempotency_key WHERE created_at < ?",
        args![before],
    )
    .await
}
