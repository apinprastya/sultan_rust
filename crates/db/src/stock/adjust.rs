//! Adjustments and opening stock (spec 2.8 §7, I11, I12): both post at once (`stock.adjust`)
//! and are never voided; a mistake is corrected by another adjustment.

use sultan_core::catalog::Qty;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{
    AdjustmentId, BranchId, LocationId, LotId, OpeningStockId, StockReasonId, UnitId, UserId,
    VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::{
    DocKind, MovementKind, StockError, check_adjust_line, check_lot, check_note, line_value,
    to_base,
};
use sultan_core::time::Timestamp;

use super::ledger::{DocRef, LotRule, Move, Valuation, post_moves, total_value};
use super::numbers::allocate;
use super::{
    StockDbError, StockResult, Written, check_lines, doc_location, get_stock_reason, line_no,
    opt_day, variant_info,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// An adjustment line to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdjustmentLine {
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// Signed quantity in the unit (≠ 0).
    pub qty: Qty,
    /// A reason whose direction allows the sign.
    pub reason: StockReasonId,
    /// A note (required by Koreksi).
    pub note: Option<String>,
    /// A unit cost per unit of the line, used for an increase while there is no cost yet.
    pub unit_cost: Option<Money>,
}

/// An adjustment to post (`PUT /adjustments/{id}`, posts at once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdjustment {
    /// The client's `UUIDv7`.
    pub id: AdjustmentId,
    /// The branch.
    pub branch: BranchId,
    /// The location; `None` = the receiving default.
    pub location: Option<LocationId>,
    /// A note.
    pub note: Option<String>,
    /// Who posts it.
    pub user: UserId,
    /// The lines.
    pub lines: Vec<NewAdjustmentLine>,
}

/// A stored adjustment line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdjustmentLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
    /// The unit's factor.
    pub factor: Qty,
    /// Signed quantity in the unit.
    pub qty: Qty,
    /// The reason.
    pub reason: StockReasonId,
    /// The note.
    pub note: Option<String>,
    /// The typed unit cost.
    pub unit_cost: Option<Money>,
    /// Signed base units.
    pub base_qty: Qty,
    /// The movement's value, signed; `None` = no cost.
    pub value: Option<Money>,
}

/// A stored adjustment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adjustment {
    /// The ID.
    pub id: AdjustmentId,
    /// `JKT01-ADJ-261126-01`.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// The location.
    pub location: LocationId,
    /// The note.
    pub note: Option<String>,
    /// Who posted it.
    pub user: UserId,
    /// When.
    pub posted_at: Timestamp,
    /// The lines.
    pub lines: Vec<AdjustmentLine>,
}

fn map_head<T>(row: &dyn Row, f: impl FnOnce(HeadFields) -> T) -> Result<T> {
    Ok(f(HeadFields {
        number: row.text("number")?,
        branch: sql::id(row, "branch_id")?,
        location: sql::id(row, "location_id")?,
        note: row.opt_text("note")?,
        user: sql::id(row, "user_id")?,
        posted_at: sql::ts(row, "posted_at")?,
    }))
}

struct HeadFields {
    number: String,
    branch: BranchId,
    location: LocationId,
    note: Option<String>,
    user: UserId,
    posted_at: Timestamp,
}

const ADJ_SELECT: &str = "SELECT id, number, branch_id, location_id, note, user_id, posted_at \
                          FROM stock_adjustment";

fn map_adjustment(row: &dyn Row) -> Result<Adjustment> {
    let id = sql::id(row, "id")?;
    map_head(row, |h| Adjustment {
        id,
        number: h.number,
        branch: h.branch,
        location: h.location,
        note: h.note,
        user: h.user,
        posted_at: h.posted_at,
        lines: Vec::new(),
    })
}

async fn adjustment_lines(mut conn: Conn<'_>, mut a: Adjustment) -> Result<Adjustment> {
    a.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, reason_id, note, unit_cost, \
         base_qty, value FROM stock_adjustment_line WHERE adjustment_id = ? ORDER BY line_no",
        args![a.id],
        |row| {
            Ok(AdjustmentLine {
                line_no: line_no(row, "line_no")?,
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
                factor: sql::qty(row, "factor")?,
                qty: sql::qty(row, "qty")?,
                reason: sql::id(row, "reason_id")?,
                note: row.opt_text("note")?,
                unit_cost: sql::opt_money(row, "unit_cost")?,
                base_qty: sql::qty(row, "base_qty")?,
                value: sql::opt_money(row, "value")?,
            })
        },
    )
    .await?;
    Ok(a)
}

/// One adjustment with its lines.
///
/// # Errors
/// The database error.
pub async fn get_adjustment<'c>(
    conn: impl Into<Conn<'c>>,
    id: AdjustmentId,
) -> Result<Option<Adjustment>> {
    let mut conn = conn.into();
    let sql = format!("{ADJ_SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map_adjustment).await? {
        Some(a) => adjustment_lines(conn, a).await.map(Some),
        None => Ok(None),
    }
}

/// The adjustments of a branch, newest first (at most 500).
///
/// # Errors
/// The database error.
pub async fn list_adjustments<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
) -> Result<Vec<Adjustment>> {
    let mut conn = conn.into();
    let sql = format!("{ADJ_SELECT} WHERE branch_id = ? ORDER BY id DESC LIMIT 500");
    let heads = sql::fetch_all(conn.reborrow(), &sql, args![branch], map_adjustment).await?;
    let mut out = Vec::with_capacity(heads.len());
    for a in heads {
        out.push(adjustment_lines(conn.reborrow(), a).await?);
    }
    Ok(out)
}

fn abs(q: Qty) -> StockResult<Qty> {
    if q.thousandths() < 0 {
        q.checked_neg()
            .ok_or(StockDbError::Rule(StockError::Overflow))
    } else {
        Ok(q)
    }
}

/// Posts an adjustment at once (`stock.adjust`, §7): `-ADJ-`, then an `adjust` movement per
/// line. A decrease is valued at the average (tracked products FEFO); an increase at the
/// average, or, while the variant has no cost yet, at the typed unit cost (required:
/// `invalid_field`, `unit_cost`). The same ID again returns the stored adjustment unchanged.
///
/// # Errors
/// [`StockDbError::Rule`] (`qty`, `reason`, `note`, `unit_cost`); [`StockDbError::NotFound`]
/// (`reason`, `location`, `variant`).
pub async fn post_adjustment(
    tx: &mut Tx,
    a: &NewAdjustment,
    at: Timestamp,
) -> StockResult<(Written, Adjustment)> {
    if let Some(stored) = get_adjustment(Conn::Tx(&mut *tx), a.id).await? {
        return Ok((Written::Unchanged, stored));
    }
    check_lines(a.lines.len())?;
    let location = doc_location(tx, a.branch, a.location).await?;
    let note = check_note(a.note.as_deref())?;
    let mut lines = Vec::with_capacity(a.lines.len());
    for l in &a.lines {
        let reason = get_stock_reason(Conn::Tx(&mut *tx), l.reason)
            .await?
            .ok_or(StockDbError::NotFound("stock reason"))?;
        let line_note = check_adjust_line(l.qty, reason.for_rules(), l.note.as_deref())?;
        if l.unit_cost.is_some_and(Money::is_negative) {
            return Err(StockDbError::invalid("unit_cost", "must not be below 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        lines.push((l, line_note, info.factor, to_base(l.qty, info.factor)?));
    }
    let n = allocate(tx, DocKind::Adjustment, a.branch, at).await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO stock_adjustment (id, number, branch_id, location_id, status, note, \
         local_day, seq, user_id, posted_at, posted_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'posted', ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            a.id, n.number, a.branch, location, note, n.day, n.seq, a.user, at, a.user, at, at,
        ],
    )
    .await?;
    for (i, (l, line_note, factor, base)) in lines.into_iter().enumerate() {
        let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let typed = l
            .unit_cost
            .map(|c| line_value(abs(l.qty)?, c).map_err(StockDbError::from))
            .transpose()?;
        let increase = base.is_positive();
        let posted = post_moves(
            tx,
            &[Move {
                branch: a.branch,
                location: Some(location),
                variant: l.variant,
                kind: MovementKind::Adjust,
                qty: base,
                valuation: if increase {
                    Valuation::Increase {
                        typed,
                        need_cost: true,
                    }
                } else {
                    Valuation::Average
                },
                lot: if increase {
                    LotRule::None
                } else {
                    LotRule::Fefo
                },
                doc: DocRef {
                    kind: DocKind::Adjustment.as_str(),
                    id: *a.id.as_uuid(),
                    line: Some(no),
                },
                at,
            }],
        )
        .await?
        .remove(0);
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO stock_adjustment_line (adjustment_id, line_no, variant_id, unit_id, \
             factor, qty, reason_id, note, unit_cost, base_qty, value) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                a.id,
                i64::from(no),
                l.variant,
                l.unit,
                factor,
                l.qty,
                l.reason,
                line_note,
                l.unit_cost,
                base,
                total_value(&posted),
            ],
        )
        .await?;
    }
    Ok((
        Written::Created,
        get_adjustment(Conn::Tx(&mut *tx), a.id)
            .await?
            .ok_or(DbError::NotFound("adjustment"))?,
    ))
}

/// An opening stock line to post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOpeningLine {
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// Quantity in the unit (> 0).
    pub qty: Qty,
    /// Cost per unit of the line (≥ 0).
    pub unit_cost: Money,
    /// Batch (tracked products).
    pub batch: Option<String>,
    /// Expiry (required for tracked products).
    pub expiry: Option<LocalDay>,
}

/// An opening stock document to post (`PUT /opening-stock/{id}`, posts at once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewOpeningStock {
    /// The client's `UUIDv7`.
    pub id: OpeningStockId,
    /// The branch.
    pub branch: BranchId,
    /// The location; `None` = the receiving default.
    pub location: Option<LocationId>,
    /// A note.
    pub note: Option<String>,
    /// Who posts it.
    pub user: UserId,
    /// The lines.
    pub lines: Vec<NewOpeningLine>,
}

/// A stored opening stock line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpeningLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
    /// The unit's factor.
    pub factor: Qty,
    /// Quantity in the unit.
    pub qty: Qty,
    /// Cost per unit.
    pub unit_cost: Money,
    /// Batch.
    pub batch: Option<String>,
    /// Expiry.
    pub expiry: Option<LocalDay>,
    /// Base units.
    pub base_qty: Qty,
    /// The `opening` movement's value.
    pub value: Option<Money>,
    /// The lot it went into.
    pub lot: Option<LotId>,
}

/// A stored opening stock document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpeningStock {
    /// The ID.
    pub id: OpeningStockId,
    /// `JKT01-OS-261126-01`.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// The location.
    pub location: LocationId,
    /// The note.
    pub note: Option<String>,
    /// Who posted it.
    pub user: UserId,
    /// When.
    pub posted_at: Timestamp,
    /// The lines.
    pub lines: Vec<OpeningLine>,
}

const OS_SELECT: &str = "SELECT id, number, branch_id, location_id, note, user_id, posted_at \
                         FROM opening_stock";

fn map_opening(row: &dyn Row) -> Result<OpeningStock> {
    let id = sql::id(row, "id")?;
    map_head(row, |h| OpeningStock {
        id,
        number: h.number,
        branch: h.branch,
        location: h.location,
        note: h.note,
        user: h.user,
        posted_at: h.posted_at,
        lines: Vec::new(),
    })
}

async fn opening_lines(mut conn: Conn<'_>, mut o: OpeningStock) -> Result<OpeningStock> {
    o.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, unit_cost, batch, expiry, base_qty, \
         value, lot_id FROM opening_stock_line WHERE opening_id = ? ORDER BY line_no",
        args![o.id],
        |row| {
            Ok(OpeningLine {
                line_no: line_no(row, "line_no")?,
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
                factor: sql::qty(row, "factor")?,
                qty: sql::qty(row, "qty")?,
                unit_cost: sql::money(row, "unit_cost")?,
                batch: row.opt_text("batch")?,
                expiry: opt_day(row, "expiry")?,
                base_qty: sql::qty(row, "base_qty")?,
                value: sql::opt_money(row, "value")?,
                lot: sql::opt_id(row, "lot_id")?,
            })
        },
    )
    .await?;
    Ok(o)
}

/// One opening stock document with its lines.
///
/// # Errors
/// The database error.
pub async fn get_opening_stock<'c>(
    conn: impl Into<Conn<'c>>,
    id: OpeningStockId,
) -> Result<Option<OpeningStock>> {
    let mut conn = conn.into();
    let sql = format!("{OS_SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map_opening).await? {
        Some(o) => opening_lines(conn, o).await.map(Some),
        None => Ok(None),
    }
}

/// The opening stock documents of a branch, newest first (at most 500).
///
/// # Errors
/// The database error.
pub async fn list_opening_stock<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
) -> Result<Vec<OpeningStock>> {
    let mut conn = conn.into();
    let sql = format!("{OS_SELECT} WHERE branch_id = ? ORDER BY id DESC LIMIT 500");
    let heads = sql::fetch_all(conn.reborrow(), &sql, args![branch], map_opening).await?;
    let mut out = Vec::with_capacity(heads.len());
    for o in heads {
        out.push(opening_lines(conn.reborrow(), o).await?);
    }
    Ok(out)
}

/// Posts opening stock at once (`stock.adjust`, §7, I12; the importer writes the same
/// document): `-OS-`, then an `opening` movement per line at qty × unit cost, into its lot for a
/// tracked product (expiry required). Allowed at any time. The same ID again returns the stored
/// document unchanged.
///
/// # Errors
/// [`StockDbError::Rule`] / [`StockDbError::Invalid`] (`qty`, `unit_cost`, `batch`, `expiry`);
/// [`StockDbError::NotFound`].
pub async fn post_opening_stock(
    tx: &mut Tx,
    o: &NewOpeningStock,
    at: Timestamp,
) -> StockResult<(Written, OpeningStock)> {
    if let Some(stored) = get_opening_stock(Conn::Tx(&mut *tx), o.id).await? {
        return Ok((Written::Unchanged, stored));
    }
    check_lines(o.lines.len())?;
    let location = doc_location(tx, o.branch, o.location).await?;
    let note = check_note(o.note.as_deref())?;
    let mut lines = Vec::with_capacity(o.lines.len());
    for l in &o.lines {
        if !l.qty.is_positive() {
            return Err(StockDbError::invalid("qty", "must be above 0"));
        }
        if l.unit_cost.is_negative() {
            return Err(StockDbError::invalid("unit_cost", "must not be below 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        let lot = check_lot(info.track_expiry, l.batch.as_deref(), l.expiry)?;
        lines.push((
            l,
            info.factor,
            to_base(l.qty, info.factor)?,
            lot,
            line_value(l.qty, l.unit_cost)?,
        ));
    }
    let n = allocate(tx, DocKind::OpeningStock, o.branch, at).await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO opening_stock (id, number, branch_id, location_id, status, note, \
         local_day, seq, user_id, posted_at, posted_by, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'posted', ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            o.id, n.number, o.branch, location, note, n.day, n.seq, o.user, at, o.user, at, at,
        ],
    )
    .await?;
    for (i, (l, factor, base, lot, value)) in lines.into_iter().enumerate() {
        let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let (batch, expiry) = match &lot {
            Some((b, e)) => (Some(b.clone()), Some(*e)),
            None => (None, None),
        };
        let posted = post_moves(
            tx,
            &[Move {
                branch: o.branch,
                location: Some(location),
                variant: l.variant,
                kind: MovementKind::Opening,
                qty: base,
                valuation: Valuation::Receive(value),
                lot: lot.map_or(LotRule::None, |(b, e)| LotRule::Into(b, e)),
                doc: DocRef {
                    kind: DocKind::OpeningStock.as_str(),
                    id: *o.id.as_uuid(),
                    line: Some(no),
                },
                at,
            }],
        )
        .await?
        .remove(0);
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO opening_stock_line (opening_id, line_no, variant_id, unit_id, factor, \
             qty, unit_cost, batch, expiry, base_qty, value, lot_id) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                o.id,
                i64::from(no),
                l.variant,
                l.unit,
                factor,
                l.qty,
                l.unit_cost,
                batch,
                expiry.map(LocalDay::key),
                base,
                Some(value),
                posted.first().and_then(|p| p.lot),
            ],
        )
        .await?;
    }
    Ok((
        Written::Created,
        get_opening_stock(Conn::Tx(&mut *tx), o.id)
            .await?
            .ok_or(DbError::NotFound("opening stock"))?,
    ))
}
