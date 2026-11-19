//! Nota photos of a cash movement (spec 2.7 §2a, K8): the database record of a file the server
//! stores on the node's disk. At most [`sultan_core::cash::MAX_PHOTOS`] per movement, never
//! deleted or replaced. The file itself is the server's job (K6): it writes the file first, then
//! calls [`add_photo`] in the transaction that also writes the audit and outbox rows.

use std::fmt::Write as _;

use serde_json::json;
use sultan_core::cash::ensure_photo_slot;
use sultan_core::ids::{CashMovementId, NotaPhotoId, UserId};
use sultan_core::time::Timestamp;

use super::{Actor, CashDbError, CashResult, Change, Written, record};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// A nota photo's record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NotaPhoto {
    /// The ID (client `UUIDv7`).
    pub id: NotaPhotoId,
    /// The cash movement.
    pub movement: CashMovementId,
    /// The SHA-256 of the stored JPEG (its file name).
    pub sha256: [u8; 32],
    /// The stored file's size in bytes, above 0.
    pub size: u64,
    /// Who added it.
    pub added_by: UserId,
    /// When.
    pub added_at: Timestamp,
}

impl NotaPhoto {
    /// The hash in lower-case hex (the file name and the sync key).
    #[must_use]
    pub fn sha256_hex(&self) -> String {
        self.sha256
            .iter()
            .fold(String::with_capacity(64), |mut s, b| {
                let _ = write!(s, "{b:02x}");
                s
            })
    }
}

const SELECT: &str =
    "SELECT id, movement_id, sha256, size, added_by, added_at FROM cash_movement_photo";

fn map(row: &dyn Row) -> Result<NotaPhoto> {
    let size = row.i64("size")?;
    Ok(NotaPhoto {
        id: sql::id(row, "id")?,
        movement: sql::id(row, "movement_id")?,
        sha256: sql::hash(row, "sha256")?,
        size: u64::try_from(size).map_err(|_| DbError::Corrupt(format!("photo size {size}")))?,
        added_by: sql::id(row, "added_by")?,
        added_at: sql::ts(row, "added_at")?,
    })
}

/// One photo.
///
/// # Errors
/// The database error.
pub async fn get_photo<'c>(
    conn: impl Into<Conn<'c>>,
    id: NotaPhotoId,
) -> Result<Option<NotaPhoto>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// The photos of a movement, in the order they were added.
///
/// # Errors
/// The database error.
pub async fn list_photos<'c>(
    conn: impl Into<Conn<'c>>,
    movement: CashMovementId,
) -> Result<Vec<NotaPhoto>> {
    let sql = format!("{SELECT} WHERE movement_id = ? ORDER BY photo_no");
    sql::fetch_all(conn.into(), &sql, args![movement], map).await
}

/// The number of photos of a movement.
///
/// # Errors
/// The database error.
pub async fn count_photos<'c>(
    conn: impl Into<Conn<'c>>,
    movement: CashMovementId,
) -> Result<usize> {
    let n = sql::fetch_optional(
        conn.into(),
        "SELECT COUNT(*) AS n FROM cash_movement_photo WHERE movement_id = ?",
        args![movement],
        |row| row.i64("n"),
    )
    .await?
    .unwrap_or(0);
    usize::try_from(n).map_err(|_| DbError::Corrupt(format!("photo count {n}")))
}

/// Adds a photo record in `tx` (`PUT /cash-movements/{id}/photos/{photo_id}`, §2a), with the
/// audit row `cash_movement.photo` and the outbox event `cash_movement.photo_added`
/// `{photo_id, movement_id, sha256, size}`. The same photo ID with the same movement and hash
/// again writes nothing ([`Written::Unchanged`]). Who may add it is the caller's job.
///
/// # Errors
/// [`CashDbError::Conflict`] when the photo ID exists with another movement or hash;
/// [`CashDbError::NotFound`] (`cash movement`); [`CashDbError::Rule`] `photo_limit` at three
/// photos; [`CashDbError::Invalid`] (`size`) for an empty file; a retryable [`DbError::Conflict`]
/// when another upload took the same slot.
pub async fn add_photo(
    tx: &mut Tx,
    photo: &NotaPhoto,
    actor: &Actor,
    now: Timestamp,
) -> CashResult<Written> {
    if let Some(stored) = get_photo(Conn::Tx(&mut *tx), photo.id).await? {
        return if (stored.movement, stored.sha256) == (photo.movement, photo.sha256) {
            Ok(Written::Unchanged)
        } else {
            Err(CashDbError::Conflict("nota photo", *photo.id.as_uuid()))
        };
    }
    let size = i64::try_from(photo.size)
        .ok()
        .filter(|s| *s > 0)
        .ok_or_else(|| CashDbError::invalid("size", "must be above 0"))?;
    if super::get_cash_movement(Conn::Tx(&mut *tx), photo.movement)
        .await?
        .is_none()
    {
        return Err(CashDbError::NotFound("cash movement"));
    }
    let photos = count_photos(Conn::Tx(&mut *tx), photo.movement).await?;
    ensure_photo_slot(photos)?;
    let last = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT MAX(photo_no) AS n FROM cash_movement_photo WHERE movement_id = ?",
        args![photo.movement],
        |row| row.opt_i64("n"),
    )
    .await?
    .flatten()
    .unwrap_or(0);
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO cash_movement_photo (id, movement_id, photo_no, sha256, size, added_by, \
         added_at) VALUES (?, ?, ?, ?, ?, ?, ?)",
        args![
            photo.id,
            photo.movement,
            last + 1,
            &photo.sha256,
            size,
            photo.added_by,
            photo.added_at,
        ],
    )
    .await?;
    let payload = json!({
        "photo_id": photo.id.to_string(),
        "movement_id": photo.movement.to_string(),
        "sha256": photo.sha256_hex(),
        "size": photo.size,
    });
    record(
        tx,
        actor,
        now,
        Change {
            action: "cash_movement.photo",
            target: format!("cash_movement:{}", photo.movement),
            before: None,
            after: payload.clone(),
            outbox: ("cash_movement.photo_added", &payload),
        },
    )
    .await?;
    Ok(Written::Created)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex() {
        let mut sha256 = [0u8; 32];
        sha256[0] = 0xab;
        sha256[31] = 0x01;
        let p = NotaPhoto {
            id: NotaPhotoId::generate(),
            movement: CashMovementId::generate(),
            sha256,
            size: 1,
            added_by: UserId::generate(),
            added_at: Timestamp::from_unix_millis(0),
        };
        let hex = p.sha256_hex();
        assert_eq!(hex.len(), 64);
        assert!(hex.starts_with("ab00"));
        assert!(hex.ends_with("0001"));
    }
}
