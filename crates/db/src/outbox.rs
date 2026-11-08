//! The sync outbox (`docs/plan.md` §1): every master-data write appends an event here in the same
//! transaction. The sync worker (a later phase) pushes unsynced events and marks them synced.

use serde_json::Value;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::conn::Conn;
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// One outbox event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEvent {
    /// `UUIDv7`; time-ordered, and the idempotency key of `POST /sync/outbox`.
    pub event_id: Uuid,
    /// Event kind, e.g. `branch.saved`.
    pub kind: String,
    /// The full new state of the changed entity.
    pub payload: Value,
    /// When the event was written.
    pub created_at: Timestamp,
    /// When the sync worker delivered it; `None` = pending.
    pub synced_at: Option<Timestamp>,
}

/// Appends an event and returns its new ID. Master-data repositories call this themselves; call it
/// directly only for other changes that must be synced. Pass the same transaction as the change.
///
/// # Errors
/// The database error.
pub async fn append<'c>(
    conn: impl Into<Conn<'c>>,
    kind: &str,
    payload: &Value,
    now: Timestamp,
) -> Result<Uuid> {
    let event_id = Uuid::now_v7();
    sql::execute(
        conn.into(),
        "INSERT INTO outbox (event_id, kind, payload, created_at) VALUES (?, ?, ?, ?)",
        args![event_id, kind, payload.to_string(), now],
    )
    .await?;
    Ok(event_id)
}

/// The oldest `limit` events not yet synced, oldest first.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for an unreadable payload.
pub async fn list_unsynced<'c>(conn: impl Into<Conn<'c>>, limit: u32) -> Result<Vec<OutboxEvent>> {
    sql::fetch_all(
        conn.into(),
        "SELECT event_id, kind, payload, created_at, synced_at FROM outbox \
         WHERE synced_at IS NULL ORDER BY event_id LIMIT ?",
        args![i64::from(limit)],
        map_event,
    )
    .await
}

/// Marks events as synced at `now`. Already-synced events keep their first sync time. Returns the
/// number of events marked.
///
/// # Errors
/// The database error.
pub async fn mark_synced<'c>(
    conn: impl Into<Conn<'c>>,
    event_ids: &[Uuid],
    now: Timestamp,
) -> Result<u64> {
    if event_ids.is_empty() {
        return Ok(0);
    }
    let placeholders = vec!["?"; event_ids.len()].join(", ");
    let mut values = args![now];
    values.extend(event_ids.iter().map(|id| Arg::from(*id)));
    sql::execute(
        conn.into(),
        &format!(
            "UPDATE outbox SET synced_at = ? WHERE synced_at IS NULL AND event_id IN ({placeholders})"
        ),
        values,
    )
    .await
}

fn map_event(row: &dyn Row) -> Result<OutboxEvent> {
    Ok(OutboxEvent {
        event_id: sql::id(row, "event_id")?,
        kind: row.text("kind")?,
        payload: sql::opt_json(row, "payload")?
            .ok_or_else(|| DbError::Corrupt("outbox payload is NULL".into()))?,
        created_at: sql::ts(row, "created_at")?,
        synced_at: sql::opt_ts(row, "synced_at")?,
    })
}
