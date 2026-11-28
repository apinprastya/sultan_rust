//! Stock reasons (spec 2.8 §7, I11): HQ master data with a direction, seeded like 2.7's cash
//! reasons. `PUT /stock-reasons/{id}` creates or overwrites one ([`put_stock_reason`]),
//! `DELETE` archives it ([`archive_stock_reason`]). A reason cannot change direction once an
//! adjustment line used it; it is never hard-deleted.

use sultan_core::ids::StockReasonId;
use sultan_core::stock::{ReasonDirection, SEED_REASONS, StockReason, check_reason_name};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::json::{Change, reason_json, record};
use super::{Actor, StockDbError, StockResult, Written};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

const fn seeded(n: u128) -> StockReasonId {
    StockReasonId::from_uuid(Uuid::from_u128(
        0x0000_0000_0000_7000_8000_0000_0000_0500 | n,
    ))
}

/// The IDs of the seeded reasons (migration `20261126000001_inventory.sql`), in the order of
/// [`SEED_REASONS`]: …0501 damaged, …0502 expired, …0503 lost, …0504 sample, …0505 own use,
/// …0506 correction.
pub const SEED_REASON_IDS: [StockReasonId; SEED_REASONS.len()] = [
    seeded(1),
    seeded(2),
    seeded(3),
    seeded(4),
    seeded(5),
    seeded(6),
];

/// The seeded "Kedaluwarsa" (expired) reason the Expiring list prefills (§9).
pub const EXPIRED_REASON: StockReasonId = seeded(2);

/// A stored stock reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockReasonRecord {
    /// `UUIDv7`.
    pub id: StockReasonId,
    /// 1–60 characters.
    pub name: String,
    /// Which way it moves stock.
    pub direction: ReasonDirection,
    /// A line with it needs a note.
    pub note_required: bool,
    /// Offered for new lines.
    pub active: bool,
    /// Sort position.
    pub sort: i32,
    /// When it was archived; `None` while it exists.
    pub archived_at: Option<Timestamp>,
}

impl StockReasonRecord {
    /// The reason as the rules see it: active only while active **and** not archived.
    #[must_use]
    pub fn for_rules(&self) -> StockReason {
        StockReason {
            direction: self.direction,
            active: self.active && self.archived_at.is_none(),
            note_required: self.note_required,
        }
    }
}

const SELECT: &str = "SELECT id, name, direction, note_required, active, sort_order, archived_at \
                      FROM stock_reason";

fn map(row: &dyn Row) -> Result<StockReasonRecord> {
    let direction = row.text("direction")?;
    let sort = row.i64("sort_order")?;
    Ok(StockReasonRecord {
        id: sql::id(row, "id")?,
        name: row.text("name")?,
        direction: ReasonDirection::from_key(&direction)
            .ok_or_else(|| DbError::Corrupt(format!("unknown direction `{direction}`")))?,
        note_required: row.bool("note_required")?,
        active: row.bool("active")?,
        sort: i32::try_from(sort)
            .map_err(|_| DbError::Corrupt(format!("sort_order {sort} out of range")))?,
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

/// One stock reason, archived or not.
///
/// # Errors
/// The database error.
pub async fn get_stock_reason<'c>(
    conn: impl Into<Conn<'c>>,
    id: StockReasonId,
) -> Result<Option<StockReasonRecord>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// The stock reasons by sort: the active, non-archived ones, or with `all` every one.
///
/// # Errors
/// The database error.
pub async fn list_stock_reasons<'c>(
    conn: impl Into<Conn<'c>>,
    all: bool,
) -> Result<Vec<StockReasonRecord>> {
    let filter = if all {
        ""
    } else {
        " WHERE active = 1 AND archived_at IS NULL"
    };
    let sql = format!("{SELECT}{filter} ORDER BY sort_order, id");
    sql::fetch_all(conn.into(), &sql, args![], map).await
}

async fn used(tx: &mut Tx, id: StockReasonId) -> Result<bool> {
    Ok(sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 AS x FROM stock_adjustment_line WHERE reason_id = ? LIMIT 1",
        args![id],
        |_| Ok(()),
    )
    .await?
    .is_some())
}

/// Creates or overwrites a stock reason (`PUT /stock-reasons/{id}`). The name is trimmed
/// (1–60); the direction cannot change once a line used the reason; an archived reason cannot
/// be edited.
///
/// # Errors
/// [`StockDbError::Rule`] (`name`); [`StockDbError::Invalid`] (`direction`);
/// [`StockDbError::Archived`].
pub async fn put_stock_reason(
    tx: &mut Tx,
    r: &StockReasonRecord,
    actor: &Actor,
    now: Timestamp,
) -> StockResult<(Written, StockReasonRecord)> {
    let name = check_reason_name(&r.name)?;
    let before = get_stock_reason(Conn::Tx(&mut *tx), r.id).await?;
    if let Some(b) = &before {
        if b.archived_at.is_some() {
            return Err(StockDbError::Archived("stock reason"));
        }
        if b.direction != r.direction && used(tx, r.id).await? {
            return Err(StockDbError::invalid(
                "direction",
                "cannot change once the reason was used",
            ));
        }
    }
    let after = StockReasonRecord {
        name,
        archived_at: None,
        ..r.clone()
    };
    if before.as_ref() == Some(&after) {
        return Ok((Written::Unchanged, after));
    }
    let saved = sql::upsert_by_id(
        tx,
        "stock_reason",
        after.id.into(),
        &["name", "direction", "note_required", "active", "sort_order"],
        args![
            after.name.clone(),
            after.direction.as_str(),
            after.note_required,
            after.active,
            i64::from(after.sort),
        ],
        now,
    )
    .await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "stock_reason.put",
            target: format!("stock_reason:{}", after.id),
            before: before.as_ref().map(reason_json),
            after: reason_json(&after),
            event: "stock_reason.changed",
        },
    )
    .await?;
    Ok((
        match saved {
            crate::Saved::Created => Written::Created,
            crate::Saved::Updated => Written::Updated,
        },
        after,
    ))
}

/// Archives a stock reason (`DELETE /stock-reasons/{id}`).
///
/// # Errors
/// [`StockDbError::NotFound`]; the database error.
pub async fn archive_stock_reason(
    tx: &mut Tx,
    id: StockReasonId,
    actor: &Actor,
    now: Timestamp,
) -> StockResult<(Written, StockReasonRecord)> {
    let before = get_stock_reason(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("stock reason"))?;
    if before.archived_at.is_some() {
        return Ok((Written::Unchanged, before));
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE stock_reason SET archived_at = ?, updated_at = ? WHERE id = ?",
        args![now, now, id],
    )
    .await?;
    let after = StockReasonRecord {
        archived_at: Some(now),
        ..before.clone()
    };
    record(
        tx,
        actor,
        now,
        Change {
            action: "stock_reason.archive",
            target: format!("stock_reason:{id}"),
            before: Some(reason_json(&before)),
            after: reason_json(&after),
            event: "stock_reason.changed",
        },
    )
    .await?;
    Ok((Written::Updated, after))
}
