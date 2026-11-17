//! Tender types (spec 2.6 D4, §1, §12): HQ master data. `PUT /tender-types/{id}` creates or
//! overwrites one ([`put_tender_type`]), `DELETE` archives it ([`archive_tender_type`]), and the
//! registers read the active ones ([`list_tender_types`]). The kind is fixed once created.

use serde_json::{Value, json};
use sultan_core::ids::TenderTypeId;
use sultan_core::sale::TenderType;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::{Actor, Change, SaleDbError, SaleResult, Written, count, record, tender_kind};
use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Row, args};

/// The seeded "Cash" tender type (migration `20261117000001_sales.sql`), so a fresh install can
/// sell.
pub const CASH_TENDER_TYPE: TenderTypeId =
    TenderTypeId::from_uuid(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0301));

/// The seeded "Customer balance" tender type (saved change spent at the till, 2.5 §11).
pub const CUSTOMER_BALANCE_TENDER_TYPE: TenderTypeId =
    TenderTypeId::from_uuid(Uuid::from_u128(0x0000_0000_0000_7000_8000_0000_0000_0304));

/// A stored tender type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenderTypeRecord {
    /// The tender type (name, kind, reference and drawer flags, active).
    pub tender: TenderType,
    /// The position in the register's list, ascending.
    pub sort: u32,
    /// When it was archived (deleted); `None` while it exists.
    pub archived_at: Option<Timestamp>,
}

const SELECT: &str = "SELECT id, name, kind, needs_reference, opens_drawer, active, sort_order, \
                      archived_at FROM tender_type";

fn map(row: &dyn Row) -> Result<TenderTypeRecord> {
    Ok(TenderTypeRecord {
        tender: TenderType {
            id: sql::id(row, "id")?,
            name: row.text("name")?,
            kind: tender_kind(row, "kind")?,
            needs_reference: row.bool("needs_reference")?,
            opens_drawer: row.bool("opens_drawer")?,
            active: row.bool("active")?,
        },
        sort: count(row, "sort_order")?,
        archived_at: sql::opt_ts(row, "archived_at")?,
    })
}

fn tender_json(r: &TenderTypeRecord) -> Value {
    json!({
        "id": r.tender.id.to_string(),
        "name": r.tender.name,
        "kind": r.tender.kind.as_str(),
        "needs_reference": r.tender.needs_reference,
        "opens_drawer": r.tender.opens_drawer,
        "active": r.tender.active,
        "sort": r.sort,
        "archived_at": r.archived_at.map(Timestamp::unix_millis),
    })
}

/// One tender type, archived or not.
///
/// # Errors
/// The database error.
pub async fn get_tender_type<'c>(
    conn: impl Into<Conn<'c>>,
    id: TenderTypeId,
) -> Result<Option<TenderTypeRecord>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map).await
}

/// The tender types in register order (sort, then id): only the active, non-archived ones
/// (`GET /tender-types`, what registers offer), or with `all` every one including inactive and
/// archived ones (the back office).
///
/// # Errors
/// The database error.
pub async fn list_tender_types<'c>(
    conn: impl Into<Conn<'c>>,
    all: bool,
) -> Result<Vec<TenderTypeRecord>> {
    let filter = if all {
        ""
    } else {
        " WHERE active = 1 AND archived_at IS NULL"
    };
    let sql = format!("{SELECT}{filter} ORDER BY sort_order, id");
    sql::fetch_all(conn.into(), &sql, args![], map).await
}

/// Creates or overwrites a tender type (`PUT /tender-types/{id}`, `tender.manage`, HQ). The
/// name is trimmed and checked by `core` (1–40 characters, not the exchange kind). Audit and
/// outbox `tender_type.put` unless nothing changed (an idempotent replay answers
/// [`Written::Unchanged`]).
///
/// # Errors
/// [`SaleDbError::Rule`] (`invalid_field`: name, kind); [`SaleDbError::Invalid`] (`kind`) when
/// the kind of an existing tender type changes; [`SaleDbError::Archived`]; or the database
/// error.
pub async fn put_tender_type(
    tx: &mut Tx,
    tender: &TenderType,
    sort: u32,
    actor: &Actor,
    now: Timestamp,
) -> SaleResult<Written> {
    let tender = tender.clone().validate()?;
    let new = TenderTypeRecord {
        tender,
        sort,
        archived_at: None,
    };
    let old = get_tender_type(Conn::Tx(&mut *tx), new.tender.id).await?;
    let written = match &old {
        Some(old) if old.archived_at.is_some() => return Err(SaleDbError::Archived),
        Some(old) if *old == new => return Ok(Written::Unchanged),
        Some(old) if old.tender.kind != new.tender.kind => {
            return Err(SaleDbError::invalid(
                "kind",
                "the kind of a tender type never changes",
            ));
        }
        Some(_) => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "UPDATE tender_type SET name = ?, needs_reference = ?, opens_drawer = ?, \
                 active = ?, sort_order = ?, updated_at = ? WHERE id = ?",
                args![
                    new.tender.name.as_str(),
                    new.tender.needs_reference,
                    new.tender.opens_drawer,
                    new.tender.active,
                    i64::from(sort),
                    now,
                    new.tender.id,
                ],
            )
            .await?;
            Written::Updated
        }
        None => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "INSERT INTO tender_type (id, name, kind, needs_reference, opens_drawer, active, \
                 sort_order, archived_at, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, NULL, ?, ?)",
                args![
                    new.tender.id,
                    new.tender.name.as_str(),
                    new.tender.kind.as_str(),
                    new.tender.needs_reference,
                    new.tender.opens_drawer,
                    new.tender.active,
                    i64::from(sort),
                    now,
                    now,
                ],
            )
            .await?;
            Written::Created
        }
    };
    let after = tender_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "tender_type.put",
            target: format!("tender_type:{}", new.tender.id),
            before: old.as_ref().map(tender_json),
            after: Some(after.clone()),
            outbox: Some(("tender_type.put", &after)),
        },
    )
    .await?;
    Ok(written)
}

/// Archives a tender type (`DELETE /tender-types/{id}`): registers no longer offer it, stored
/// sales keep it. Audit and outbox `tender_type.archive`; an already archived one answers
/// [`Written::Unchanged`].
///
/// # Errors
/// [`SaleDbError::NotFound`], or the database error.
pub async fn archive_tender_type(
    tx: &mut Tx,
    id: TenderTypeId,
    actor: &Actor,
    now: Timestamp,
) -> SaleResult<Written> {
    let old = get_tender_type(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(SaleDbError::NotFound("tender type"))?;
    if old.archived_at.is_some() {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE tender_type SET archived_at = ?, updated_at = ? WHERE id = ?",
        args![now, now, id],
    )
    .await?;
    let new = TenderTypeRecord {
        archived_at: Some(now),
        ..old.clone()
    };
    let after = tender_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "tender_type.archive",
            target: format!("tender_type:{id}"),
            before: Some(tender_json(&old)),
            after: Some(after.clone()),
            outbox: Some(("tender_type.archive", &after)),
        },
    )
    .await?;
    Ok(Written::Updated)
}
