//! Cash reasons (spec 2.7 §1, §2, K1, K25): HQ master data. `PUT /cash-reasons/{id}` creates or
//! overwrites one ([`put_cash_reason`]), `DELETE` archives it ([`archive_cash_reason`]), and the
//! registers read the active ones ([`list_cash_reasons`]). The direction is fixed once created.
//! A reason is never hard-deleted: movements keep pointing at it.

use serde_json::{Value, json};
use sultan_core::cash::{CashReason, Direction, SEED_REASONS};
use sultan_core::ids::CashReasonId;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::{Actor, CashDbError, CashResult, Change, Written, record};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// The longest reason name, in characters after trimming.
pub const MAX_REASON_NAME: usize = 60;

const fn seeded(n: u128) -> CashReasonId {
    CashReasonId::from_uuid(Uuid::from_u128(
        0x0000_0000_0000_7000_8000_0000_0000_0400 | n,
    ))
}

/// The IDs of the seeded reasons (migration `20261119000001_cash.sql`), in the order of
/// [`SEED_REASONS`]: …0401 float top-up, …0402 other (in), …0403 shop supplies, …0404 delivery /
/// parking, …0405 cleaning / security, …0406 other (out).
pub const SEED_REASON_IDS: [CashReasonId; SEED_REASONS.len()] = [
    seeded(1),
    seeded(2),
    seeded(3),
    seeded(4),
    seeded(5),
    seeded(6),
];

/// A stored cash reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashReasonRecord {
    /// The reason as stored (`active` is the stored flag: offered at the register).
    pub reason: CashReason,
    /// When it was archived (deleted); `None` while it exists.
    pub archived_at: Option<Timestamp>,
}

impl CashReasonRecord {
    /// The reason as the rules of `core::cash` see it: active only while it is active **and**
    /// not archived (an archived reason is refused for new movements, §2).
    #[must_use]
    pub fn for_rules(&self) -> CashReason {
        CashReason {
            active: self.reason.active && self.archived_at.is_none(),
            ..self.reason.clone()
        }
    }
}

const SELECT: &str = "SELECT id, name, direction, note_required, active, sort_order, archived_at \
                      FROM cash_reason";

fn map(row: &dyn Row) -> Result<CashReasonRecord> {
    let direction = row.text("direction")?;
    let sort = row.i64("sort_order")?;
    Ok(CashReasonRecord {
        reason: CashReason {
            id: sql::id(row, "id")?,
            name: row.text("name")?,
            direction: Direction::from_key(&direction)
                .ok_or_else(|| DbError::Corrupt(format!("unknown direction `{direction}`")))?,
            active: row.bool("active")?,
            sort: i32::try_from(sort)
                .map_err(|_| DbError::Corrupt(format!("sort_order {sort} out of range")))?,
            note_required: row.bool("note_required")?,
        },
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

fn reason_json(r: &CashReasonRecord) -> Value {
    json!({
        "id": r.reason.id.to_string(),
        "name": r.reason.name,
        "direction": r.reason.direction.as_str(),
        "note_required": r.reason.note_required,
        "active": r.reason.active,
        "sort": r.reason.sort,
        "archived_at": r.archived_at.map(Timestamp::unix_millis),
    })
}

/// One cash reason, archived or not.
///
/// # Errors
/// The database error.
pub async fn get_cash_reason<'c>(
    conn: impl Into<Conn<'c>>,
    id: CashReasonId,
) -> Result<Option<CashReasonRecord>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// The cash reasons in register order (in before out, then sort, then id): only the active,
/// non-archived ones (`GET /cash-reasons`, what registers offer), or with `all` every one
/// including inactive and archived ones (`?archived=true`, the back office).
///
/// # Errors
/// The database error.
pub async fn list_cash_reasons<'c>(
    conn: impl Into<Conn<'c>>,
    all: bool,
) -> Result<Vec<CashReasonRecord>> {
    let filter = if all {
        ""
    } else {
        " WHERE active = 1 AND archived_at IS NULL"
    };
    let sql = format!("{SELECT}{filter} ORDER BY direction, sort_order, id");
    sql::fetch_all(conn.into(), &sql, args![], map).await
}

/// Creates or overwrites a cash reason (`PUT /cash-reasons/{id}`, `tender.manage`, HQ). The name
/// is trimmed (1 to [`MAX_REASON_NAME`] characters). Audit and outbox `cash_reason.put` unless
/// nothing changed (an idempotent replay answers [`Written::Unchanged`]).
///
/// # Errors
/// [`CashDbError::Invalid`] (`name`; `direction` when an existing reason's direction changes);
/// [`CashDbError::Archived`]; or the database error.
pub async fn put_cash_reason(
    tx: &mut Tx,
    reason: &CashReason,
    actor: &Actor,
    now: Timestamp,
) -> CashResult<Written> {
    let name = reason.name.trim();
    if name.is_empty() || name.chars().count() > MAX_REASON_NAME {
        return Err(CashDbError::invalid(
            "name",
            format!("must be 1 to {MAX_REASON_NAME} characters"),
        ));
    }
    let new = CashReasonRecord {
        reason: CashReason {
            name: name.to_owned(),
            ..reason.clone()
        },
        archived_at: None,
    };
    let r = &new.reason;
    let old = get_cash_reason(Conn::Tx(&mut *tx), r.id).await?;
    let written = match &old {
        Some(old) if old.archived_at.is_some() => return Err(CashDbError::Archived),
        Some(old) if *old == new => return Ok(Written::Unchanged),
        Some(old) if old.reason.direction != r.direction => {
            return Err(CashDbError::invalid(
                "direction",
                "the direction of a cash reason never changes",
            ));
        }
        Some(_) => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "UPDATE cash_reason SET name = ?, note_required = ?, active = ?, sort_order = ?, \
                 updated_at = ? WHERE id = ?",
                args![
                    r.name.as_str(),
                    r.note_required,
                    r.active,
                    i64::from(r.sort),
                    now,
                    r.id,
                ],
            )
            .await?;
            Written::Updated
        }
        None => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "INSERT INTO cash_reason (id, name, direction, note_required, active, sort_order, \
                 archived_at, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, NULL, ?, ?)",
                args![
                    r.id,
                    r.name.as_str(),
                    r.direction.as_str(),
                    r.note_required,
                    r.active,
                    i64::from(r.sort),
                    now,
                    now,
                ],
            )
            .await?;
            Written::Created
        }
    };
    let after = reason_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "cash_reason.put",
            target: format!("cash_reason:{}", r.id),
            before: old.as_ref().map(reason_json),
            after: after.clone(),
            outbox: ("cash_reason.put", &after),
        },
    )
    .await?;
    Ok(written)
}

/// Archives a cash reason (`DELETE /cash-reasons/{id}`): registers no longer offer it, stored
/// movements keep it. Audit and outbox `cash_reason.archive`; an already archived one answers
/// [`Written::Unchanged`].
///
/// # Errors
/// [`CashDbError::NotFound`], or the database error.
pub async fn archive_cash_reason(
    tx: &mut Tx,
    id: CashReasonId,
    actor: &Actor,
    now: Timestamp,
) -> CashResult<Written> {
    let old = get_cash_reason(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CashDbError::NotFound("cash reason"))?;
    if old.archived_at.is_some() {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE cash_reason SET archived_at = ?, updated_at = ? WHERE id = ?",
        args![now, now, id],
    )
    .await?;
    let new = CashReasonRecord {
        archived_at: Some(now),
        ..old.clone()
    };
    let after = reason_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "cash_reason.archive",
            target: format!("cash_reason:{id}"),
            before: Some(reason_json(&old)),
            after: after.clone(),
            outbox: ("cash_reason.archive", &after),
        },
    )
    .await?;
    Ok(Written::Updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_ids_are_v7_and_distinct() {
        assert_eq!(
            SEED_REASON_IDS[0].to_string(),
            "00000000-0000-7000-8000-000000000401"
        );
        assert_eq!(
            SEED_REASON_IDS[5].to_string(),
            "00000000-0000-7000-8000-000000000406"
        );
        for id in SEED_REASON_IDS {
            assert_eq!(id.as_uuid().get_version_num(), 7);
        }
        let mut ids = SEED_REASON_IDS.to_vec();
        ids.dedup();
        assert_eq!(ids.len(), SEED_REASONS.len());
    }
}
