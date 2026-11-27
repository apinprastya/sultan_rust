//! Transfers between branches and inside a branch (spec 2.8 §8, I8).
//!
//! Between branches: `requested → shipped → received`, or `requested → cancelled`; a transfer
//! may start shipped. Shipping posts `transfer_out` at the sender's average (tracked products
//! leave FEFO and their lots travel with the line); receiving posts `transfer_in` at the
//! receiver with the shipped value of the received part (and its lots) and, for a shortage, a
//! transit loss at the sender valued from the shipment. Until sync exists both branches must be
//! in this database (`409 other_node`).
//!
//! Inside a branch: an internal transfer posts `internal_out` / `internal_in` pairs at once; the
//! cost does not change and lots move FEFO.

use sultan_core::catalog::Qty;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{
    BranchId, InternalTransferId, LocationId, LotId, TransferId, UnitId, UserId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::{
    DocKind, LotStock, MovementKind, StockError, check_note, split_lots, split_value, spread,
    to_base,
};
use sultan_core::time::Timestamp;

use super::ledger::{DocRef, LotRule, Move, Posted, Valuation, post_moves, total_value};
use super::numbers::allocate;
use super::{
    StockDbError, StockResult, Written, check_lines, check_location, day, doc_location, line_no,
    receiving_location, variant_info,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The state of a transfer between branches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferStatus {
    /// Asked for by the receiving branch.
    Requested,
    /// Shipped: in transit.
    Shipped,
    /// Received (closed).
    Received,
    /// Cancelled before shipping.
    Cancelled,
}

impl TransferStatus {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Shipped => "shipped",
            Self::Received => "received",
            Self::Cancelled => "cancelled",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        [
            Self::Requested,
            Self::Shipped,
            Self::Received,
            Self::Cancelled,
        ]
        .into_iter()
        .find(|s| s.as_str() == key)
    }
}

/// A transfer line to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewTransferLine {
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// Quantity in the unit (> 0).
    pub qty: Qty,
}

/// A new transfer between branches (`PUT /transfers/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewTransfer {
    /// The client's `UUIDv7`.
    pub id: TransferId,
    /// The sending branch.
    pub from_branch: BranchId,
    /// The receiving branch.
    pub to_branch: BranchId,
    /// Where it is received; `None` = the receiver's receiving default when received.
    pub to_location: Option<LocationId>,
    /// `Some(location)`: created shipped from that location of the sender (no request), every
    /// line shipped in full. `None`: a request of the receiving branch.
    pub ship_from: Option<LocationId>,
    /// A note.
    pub note: Option<String>,
    /// Who writes it.
    pub user: UserId,
    /// The lines.
    pub lines: Vec<NewTransferLine>,
}

/// A stored transfer line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
    /// The unit's factor.
    pub factor: Qty,
    /// Requested quantity in the unit.
    pub qty: Qty,
    /// Shipped, base units.
    pub shipped_qty: Option<Qty>,
    /// The shipped value (≥ 0), the sender's average.
    pub shipped_value: Option<Money>,
    /// Received, base units.
    pub received_qty: Option<Qty>,
    /// The received part of the shipped value.
    pub received_value: Option<Money>,
    /// Lost in transit, base units.
    pub lost_qty: Option<Qty>,
    /// The lost part of the shipped value.
    pub lost_value: Option<Money>,
    /// The lots that travel with it: (batch, expiry, base qty).
    pub lots: Vec<(String, LocalDay, Qty)>,
}

/// A stored transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transfer {
    /// The ID.
    pub id: TransferId,
    /// `JKT01-TR-261126-01`, numbered by the branch that created it.
    pub number: String,
    /// The sender.
    pub from_branch: BranchId,
    /// The sender's location (once shipped).
    pub from_location: Option<LocationId>,
    /// The receiver.
    pub to_branch: BranchId,
    /// The receiver's location.
    pub to_location: Option<LocationId>,
    /// The state.
    pub status: TransferStatus,
    /// The note.
    pub note: Option<String>,
    /// Who created it.
    pub user: UserId,
    /// When it was shipped.
    pub shipped_at: Option<Timestamp>,
    /// When it was received.
    pub received_at: Option<Timestamp>,
    /// The lines.
    pub lines: Vec<TransferLine>,
}

const SELECT: &str = "SELECT id, number, from_branch_id, from_location_id, to_branch_id, \
                      to_location_id, status, note, user_id, shipped_at, received_at \
                      FROM stock_transfer";

fn map(row: &dyn Row) -> Result<Transfer> {
    let status = row.text("status")?;
    Ok(Transfer {
        id: sql::id(row, "id")?,
        number: row.text("number")?,
        from_branch: sql::id(row, "from_branch_id")?,
        from_location: sql::opt_id(row, "from_location_id")?,
        to_branch: sql::id(row, "to_branch_id")?,
        to_location: sql::opt_id(row, "to_location_id")?,
        status: TransferStatus::from_key(&status)
            .ok_or_else(|| DbError::Corrupt(format!("unknown transfer status `{status}`")))?,
        note: row.opt_text("note")?,
        user: sql::id(row, "user_id")?,
        shipped_at: sql::opt_ts(row, "shipped_at")?,
        received_at: sql::opt_ts(row, "received_at")?,
        lines: Vec::new(),
    })
}

fn map_line(row: &dyn Row) -> Result<TransferLine> {
    Ok(TransferLine {
        line_no: line_no(row, "line_no")?,
        variant: sql::id(row, "variant_id")?,
        unit: sql::id(row, "unit_id")?,
        factor: sql::qty(row, "factor")?,
        qty: sql::qty(row, "qty")?,
        shipped_qty: sql::opt_qty(row, "shipped_qty")?,
        shipped_value: sql::opt_money(row, "shipped_value")?,
        received_qty: sql::opt_qty(row, "received_qty")?,
        received_value: sql::opt_money(row, "received_value")?,
        lost_qty: sql::opt_qty(row, "lost_qty")?,
        lost_value: sql::opt_money(row, "lost_value")?,
        lots: Vec::new(),
    })
}

async fn with_lines(mut conn: Conn<'_>, mut t: Transfer) -> Result<Transfer> {
    t.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, shipped_qty, shipped_value, \
         received_qty, received_value, lost_qty, lost_value FROM stock_transfer_line \
         WHERE transfer_id = ? ORDER BY line_no",
        args![t.id],
        map_line,
    )
    .await?;
    let lots = sql::fetch_all(
        conn,
        "SELECT line_no, batch, expiry, qty FROM stock_transfer_lot WHERE transfer_id = ? \
         ORDER BY line_no, expiry, batch",
        args![t.id],
        |row| {
            Ok((
                line_no(row, "line_no")?,
                row.text("batch")?,
                day(row, "expiry")?,
                sql::qty(row, "qty")?,
            ))
        },
    )
    .await?;
    for (no, batch, expiry, qty) in lots {
        if let Some(l) = t.lines.iter_mut().find(|l| l.line_no == no) {
            l.lots.push((batch, expiry, qty));
        }
    }
    Ok(t)
}

/// One transfer with its lines and travelling lots.
///
/// # Errors
/// The database error.
pub async fn get_transfer<'c>(
    conn: impl Into<Conn<'c>>,
    id: TransferId,
) -> Result<Option<Transfer>> {
    let mut conn = conn.into();
    let sql = format!("{SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map).await? {
        Some(t) => with_lines(conn, t).await.map(Some),
        None => Ok(None),
    }
}

/// The transfers of a branch at either end, optionally of one status, newest first (at most
/// 500): requests, to ship, in transit, to receive (§14).
///
/// # Errors
/// The database error.
pub async fn list_transfers<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    status: Option<TransferStatus>,
) -> Result<Vec<Transfer>> {
    let mut conn = conn.into();
    let mut sql = format!("{SELECT} WHERE 1 = 1");
    let mut a: Vec<Arg> = Vec::new();
    if let Some(b) = branch {
        sql.push_str(" AND (from_branch_id = ? OR to_branch_id = ?)");
        a.push(b.into());
        a.push(b.into());
    }
    if let Some(s) = status {
        sql.push_str(" AND status = ?");
        a.push(s.as_str().into());
    }
    sql.push_str(" ORDER BY id DESC LIMIT 500");
    let heads = sql::fetch_all(conn.reborrow(), &sql, a, map).await?;
    let mut out = Vec::with_capacity(heads.len());
    for t in heads {
        out.push(with_lines(conn.reborrow(), t).await?);
    }
    Ok(out)
}

/// In transit of `variant` for `branch` (§4): shipped − (received + lost) over the open
/// transfers it sends or receives (shown at both ends).
///
/// # Errors
/// The database error.
pub async fn in_transit<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    variant: VariantId,
) -> Result<Qty> {
    sql::fetch_optional(
        conn.into(),
        "SELECT CAST(COALESCE(SUM(l.shipped_qty), 0) AS SIGNED) AS qty \
         FROM stock_transfer_line l JOIN stock_transfer t ON t.id = l.transfer_id \
         WHERE t.status = 'shipped' AND l.variant_id = ? \
         AND (t.from_branch_id = ? OR t.to_branch_id = ?)",
        args![variant, branch, branch],
        |row| sql::qty(row, "qty"),
    )
    .await
    .map(Option::unwrap_or_default)
}

async fn branch_exists(tx: &mut Tx, branch: BranchId) -> Result<bool> {
    Ok(sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 AS x FROM branch WHERE id = ?",
        args![branch],
        |_| Ok(()),
    )
    .await?
    .is_some())
}

async fn load(tx: &mut Tx, id: TransferId) -> StockResult<Transfer> {
    get_transfer(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("transfer"))
}

fn neg(q: Qty) -> StockResult<Qty> {
    q.checked_neg()
        .ok_or(StockDbError::Rule(StockError::Overflow))
}

/// Creates a transfer between branches (`PUT /transfers/{id}`, `stock.transfer`): a request of
/// the receiving branch (numbered by it), or with `ship_from` one shipped at once by the sender
/// (numbered by it). The same ID again returns the stored transfer unchanged.
///
/// # Errors
/// [`StockDbError::OtherNode`] when either branch is not in this database;
/// [`StockDbError::Invalid`] (`to_branch` = `from_branch`, lines); [`StockDbError::NotFound`].
pub async fn create_transfer(
    tx: &mut Tx,
    t: &NewTransfer,
    at: Timestamp,
) -> StockResult<(Written, Transfer)> {
    if let Some(stored) = get_transfer(Conn::Tx(&mut *tx), t.id).await? {
        return Ok((Written::Unchanged, stored));
    }
    if t.from_branch == t.to_branch {
        return Err(StockDbError::invalid("to_branch", "must be another branch"));
    }
    if !branch_exists(tx, t.from_branch).await? || !branch_exists(tx, t.to_branch).await? {
        return Err(StockDbError::OtherNode);
    }
    check_lines(t.lines.len())?;
    let note = check_note(t.note.as_deref())?;
    if let Some(l) = t.to_location {
        check_location(tx, t.to_branch, l).await?;
    }
    let numbering = if t.ship_from.is_some() {
        t.from_branch
    } else {
        t.to_branch
    };
    let mut lines = Vec::with_capacity(t.lines.len());
    for l in &t.lines {
        if !l.qty.is_positive() {
            return Err(StockDbError::invalid("qty", "must be above 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        lines.push((l, info.factor));
    }
    let n = allocate(tx, DocKind::Transfer, numbering, at).await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO stock_transfer (id, number, branch_id, from_branch_id, to_branch_id, \
         to_location_id, status, note, local_day, seq, user_id, requested_at, requested_by, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'requested', ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            t.id,
            n.number,
            numbering,
            t.from_branch,
            t.to_branch,
            t.to_location,
            note,
            n.day,
            n.seq,
            t.user,
            t.ship_from.is_none().then_some(at),
            t.ship_from.is_none().then_some(t.user),
            at,
            at,
        ],
    )
    .await?;
    for (i, (l, factor)) in lines.iter().enumerate() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO stock_transfer_line (transfer_id, line_no, variant_id, unit_id, factor, \
             qty) VALUES (?, ?, ?, ?, ?, ?)",
            args![
                t.id,
                i64::try_from(i + 1).unwrap_or(i64::MAX),
                l.variant,
                l.unit,
                *factor,
                l.qty,
            ],
        )
        .await?;
    }
    if let Some(from) = t.ship_from {
        ship_transfer(tx, t.id, Some(from), &[], t.user, at).await?;
    }
    Ok((Written::Created, load(tx, t.id).await?))
}

async fn lot_key(tx: &mut Tx, lot: LotId) -> Result<(String, LocalDay)> {
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT batch, expiry FROM stock_lot WHERE id = ?",
        args![lot],
        |row| Ok((row.text("batch")?, day(row, "expiry")?)),
    )
    .await?
    .ok_or(DbError::NotFound("lot"))
}

/// Ships a requested transfer (`POST /transfers/{id}/ship`, `stock.transfer` at the sender):
/// each line ships the quantity given in `shipped` (`(line_no, qty in the line's unit)`, 0
/// allowed when out of stock), else what was requested; `transfer_out` at the sender's
/// average, tracked products FEFO, the lots recorded on the line. A shipped or received
/// transfer is returned unchanged.
///
/// # Errors
/// [`StockDbError::State`] for a cancelled transfer; [`StockDbError::Invalid`] (`qty` below 0,
/// unknown line); [`StockDbError::NotFound`] (`location`).
pub async fn ship_transfer(
    tx: &mut Tx,
    id: TransferId,
    from_location: Option<LocationId>,
    shipped: &[(u32, Qty)],
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, Transfer)> {
    let t = load(tx, id).await?;
    match t.status {
        TransferStatus::Shipped | TransferStatus::Received => {
            return Ok((Written::Unchanged, t));
        }
        TransferStatus::Cancelled => return Err(StockDbError::state("transfer", "cancelled")),
        TransferStatus::Requested => {}
    }
    if let Some((no, _)) = shipped
        .iter()
        .find(|(no, _)| !t.lines.iter().any(|l| l.line_no == *no))
    {
        return Err(StockDbError::invalid("lines", format!("no line {no}")));
    }
    let location = doc_location(tx, t.from_branch, from_location).await?;
    for l in &t.lines {
        let qty = shipped
            .iter()
            .find(|(no, _)| *no == l.line_no)
            .map_or(l.qty, |(_, q)| *q);
        if qty.thousandths() < 0 {
            return Err(StockDbError::invalid("qty", "must not be below 0"));
        }
        let base = to_base(qty, l.factor)?;
        let mut value = Some(Money::ZERO);
        if base.is_positive() {
            let posted = post_moves(
                tx,
                &[Move {
                    branch: t.from_branch,
                    location: Some(location),
                    variant: l.variant,
                    kind: MovementKind::TransferOut,
                    qty: neg(base)?,
                    valuation: Valuation::Average,
                    lot: LotRule::Fefo,
                    doc: DocRef {
                        kind: DocKind::Transfer.as_str(),
                        id: *id.as_uuid(),
                        line: Some(l.line_no),
                    },
                    at,
                }],
            )
            .await?
            .remove(0);
            value = total_value(&posted).and_then(|v| Money::ZERO.checked_sub(v));
            for p in &posted {
                let Some(lot) = p.lot else { continue };
                let (batch, expiry) = lot_key(tx, lot).await?;
                sql::execute(
                    Conn::Tx(&mut *tx),
                    "INSERT INTO stock_transfer_lot (transfer_id, line_no, batch, expiry, qty) \
                     VALUES (?, ?, ?, ?, ?)",
                    args![id, i64::from(l.line_no), batch, expiry.key(), neg(p.qty)?],
                )
                .await?;
            }
        }
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE stock_transfer_line SET shipped_qty = ?, shipped_value = ? \
             WHERE transfer_id = ? AND line_no = ?",
            args![base, value, id, i64::from(l.line_no)],
        )
        .await?;
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE stock_transfer SET status = 'shipped', from_location_id = ?, shipped_at = ?, \
         shipped_by = ?, updated_at = ? WHERE id = ?",
        args![location, at, user, at, id],
    )
    .await?;
    Ok((Written::Updated, load(tx, id).await?))
}

/// Receives a shipped transfer in one step (`POST /transfers/{id}/receive`, `stock.transfer` at
/// the receiver): each line receives the quantity given in `received` (`(line_no, qty in the
/// line's unit)`, at most what was shipped), else all that was shipped. The receiver gets
/// `transfer_in` with the received part of the shipped value (FEFO from the travelling lots,
/// into the same batch and expiry); a shortage is a transit loss at the sender, valued from the
/// shipment (§20 reading: posted as `transfer_in` + `transit_loss` of the missing quantity at
/// the sender, so its on hand stays the ledger's sum). A received transfer is returned
/// unchanged.
///
/// # Errors
/// [`StockDbError::State`] for a requested or cancelled transfer; [`StockDbError::Rule`]
/// (`qty` above the shipped quantity); [`StockDbError::NotFound`] (`location`).
#[allow(clippy::too_many_lines)] // one posting: the receiver's part, then the sender's loss
pub async fn receive_transfer(
    tx: &mut Tx,
    id: TransferId,
    to_location: Option<LocationId>,
    received: &[(u32, Qty)],
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, Transfer)> {
    let t = load(tx, id).await?;
    match t.status {
        TransferStatus::Received => return Ok((Written::Unchanged, t)),
        TransferStatus::Shipped => {}
        s => return Err(StockDbError::state("transfer", s.as_str())),
    }
    let location = match to_location.or(t.to_location) {
        Some(l) => {
            check_location(tx, t.to_branch, l).await?;
            l
        }
        None => receiving_location(tx, t.to_branch)
            .await?
            .ok_or(StockDbError::NotFound("location"))?,
    };
    let from_location = t.from_location.ok_or(DbError::Corrupt(
        "a shipped transfer has no location".into(),
    ))?;
    for l in &t.lines {
        let shipped = l.shipped_qty.unwrap_or_default();
        let got = match received.iter().find(|(no, _)| *no == l.line_no) {
            Some((_, q)) => to_base(*q, l.factor)?,
            None => shipped,
        };
        let (got_value, lost_value) = split_value(shipped, l.shipped_value, got)?;
        let lost = shipped
            .checked_sub(got)
            .ok_or(StockDbError::Rule(StockError::Overflow))?;
        let doc = DocRef {
            kind: DocKind::Transfer.as_str(),
            id: *id.as_uuid(),
            line: Some(l.line_no),
        };
        if got.is_positive() {
            let lots: Vec<LotStock<usize>> = l
                .lots
                .iter()
                .enumerate()
                .map(|(i, (_, expiry, qty))| LotStock {
                    lot: i,
                    expiry: *expiry,
                    qty: *qty,
                })
                .collect();
            let (in_lots, _) = split_lots(&lots, got)?;
            let mut parts: Vec<(LotRule, Qty)> = in_lots
                .iter()
                .map(|(i, q)| (LotRule::Into(l.lots[*i].0.clone(), l.lots[*i].1), *q))
                .collect();
            let in_lot_total = parts
                .iter()
                .try_fold(Qty::ZERO, |a, (_, q)| a.checked_add(*q))
                .ok_or(StockDbError::Rule(StockError::Overflow))?;
            let rest = got
                .checked_sub(in_lot_total)
                .ok_or(StockDbError::Rule(StockError::Overflow))?;
            if rest.is_positive() {
                parts.push((LotRule::None, rest));
            }
            let values: Vec<Option<Money>> = match got_value {
                Some(v) => {
                    let weights: Vec<Money> = parts
                        .iter()
                        .map(|(_, q)| Money::from_minor(q.thousandths()))
                        .collect();
                    spread(v, &weights)?.into_iter().map(Some).collect()
                }
                None => vec![None; parts.len()],
            };
            let moves: Vec<Move> = parts
                .into_iter()
                .zip(values)
                .map(|((lot, qty), value)| Move {
                    branch: t.to_branch,
                    location: Some(location),
                    variant: l.variant,
                    kind: MovementKind::TransferIn,
                    qty,
                    valuation: Valuation::PutBack(value),
                    lot,
                    doc,
                    at,
                })
                .collect();
            post_moves(tx, &moves).await?;
        }
        if lost.is_positive() {
            let out_value = lost_value.and_then(|v| Money::ZERO.checked_sub(v));
            let pair = [
                Move {
                    branch: t.from_branch,
                    location: Some(from_location),
                    variant: l.variant,
                    kind: MovementKind::TransferIn,
                    qty: lost,
                    valuation: Valuation::Fixed(lost_value),
                    lot: LotRule::None,
                    doc,
                    at,
                },
                Move {
                    branch: t.from_branch,
                    location: Some(from_location),
                    variant: l.variant,
                    kind: MovementKind::TransitLoss,
                    qty: neg(lost)?,
                    valuation: Valuation::Fixed(out_value),
                    lot: LotRule::None,
                    doc,
                    at,
                },
            ];
            post_moves(tx, &pair).await?;
        }
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE stock_transfer_line SET received_qty = ?, received_value = ?, lost_qty = ?, \
             lost_value = ? WHERE transfer_id = ? AND line_no = ?",
            args![got, got_value, lost, lost_value, id, i64::from(l.line_no)],
        )
        .await?;
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE stock_transfer SET status = 'received', to_location_id = ?, received_at = ?, \
         received_by = ?, updated_at = ? WHERE id = ?",
        args![location, at, user, at, id],
    )
    .await?;
    Ok((Written::Updated, load(tx, id).await?))
}

/// Cancels a requested transfer (`POST /transfers/{id}/cancel`). A shipped one cannot be
/// cancelled (it is sent back with a new transfer).
///
/// # Errors
/// [`StockDbError::State`] for a shipped or received transfer; [`StockDbError::NotFound`].
pub async fn cancel_transfer(
    tx: &mut Tx,
    id: TransferId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, Transfer)> {
    let t = load(tx, id).await?;
    match t.status {
        TransferStatus::Cancelled => return Ok((Written::Unchanged, t)),
        TransferStatus::Requested => {}
        s => return Err(StockDbError::state("transfer", s.as_str())),
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE stock_transfer SET status = 'cancelled', cancelled_at = ?, cancelled_by = ?, \
         updated_at = ? WHERE id = ?",
        args![at, user, at, id],
    )
    .await?;
    Ok((Written::Updated, load(tx, id).await?))
}

/// An internal transfer to post (`PUT /internal-transfers/{id}`, posts at once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewInternalTransfer {
    /// The client's `UUIDv7`.
    pub id: InternalTransferId,
    /// The branch.
    pub branch: BranchId,
    /// From this location.
    pub from_location: LocationId,
    /// To this location.
    pub to_location: LocationId,
    /// A note.
    pub note: Option<String>,
    /// Who posts it.
    pub user: UserId,
    /// The lines.
    pub lines: Vec<NewTransferLine>,
}

/// A stored internal transfer line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InternalTransferLine {
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
    /// Base units.
    pub base_qty: Qty,
    /// The value moved (≥ 0), at the average; `None` = no cost.
    pub value: Option<Money>,
}

/// A stored internal transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InternalTransfer {
    /// The ID.
    pub id: InternalTransferId,
    /// `JKT01-IT-261126-01`.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// From.
    pub from_location: LocationId,
    /// To.
    pub to_location: LocationId,
    /// The note.
    pub note: Option<String>,
    /// Who posted it.
    pub user: UserId,
    /// When.
    pub posted_at: Timestamp,
    /// The lines.
    pub lines: Vec<InternalTransferLine>,
}

const INTERNAL_SELECT: &str = "SELECT id, number, branch_id, from_location_id, to_location_id, \
                               note, user_id, posted_at FROM internal_transfer";

fn map_internal(row: &dyn Row) -> Result<InternalTransfer> {
    Ok(InternalTransfer {
        id: sql::id(row, "id")?,
        number: row.text("number")?,
        branch: sql::id(row, "branch_id")?,
        from_location: sql::id(row, "from_location_id")?,
        to_location: sql::id(row, "to_location_id")?,
        note: row.opt_text("note")?,
        user: sql::id(row, "user_id")?,
        posted_at: sql::ts(row, "posted_at")?,
        lines: Vec::new(),
    })
}

async fn internal_lines(mut conn: Conn<'_>, mut t: InternalTransfer) -> Result<InternalTransfer> {
    t.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, base_qty, value \
         FROM internal_transfer_line WHERE transfer_id = ? ORDER BY line_no",
        args![t.id],
        |row| {
            Ok(InternalTransferLine {
                line_no: line_no(row, "line_no")?,
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
                factor: sql::qty(row, "factor")?,
                qty: sql::qty(row, "qty")?,
                base_qty: sql::qty(row, "base_qty")?,
                value: sql::opt_money(row, "value")?,
            })
        },
    )
    .await?;
    Ok(t)
}

/// One internal transfer with its lines.
///
/// # Errors
/// The database error.
pub async fn get_internal_transfer<'c>(
    conn: impl Into<Conn<'c>>,
    id: InternalTransferId,
) -> Result<Option<InternalTransfer>> {
    let mut conn = conn.into();
    let sql = format!("{INTERNAL_SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map_internal).await? {
        Some(t) => internal_lines(conn, t).await.map(Some),
        None => Ok(None),
    }
}

/// The internal transfers of a branch, newest first (at most 500).
///
/// # Errors
/// The database error.
pub async fn list_internal_transfers<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
) -> Result<Vec<InternalTransfer>> {
    let mut conn = conn.into();
    let sql = format!("{INTERNAL_SELECT} WHERE branch_id = ? ORDER BY id DESC LIMIT 500");
    let heads = sql::fetch_all(conn.reborrow(), &sql, args![branch], map_internal).await?;
    let mut out = Vec::with_capacity(heads.len());
    for t in heads {
        out.push(internal_lines(conn.reborrow(), t).await?);
    }
    Ok(out)
}

/// Posts an internal transfer at once (`stock.transfer`, §8): `-IT-`, then per line
/// `internal_out` at the average from the source (FEFO lots) and `internal_in` of the same
/// value into the destination (the same batch and expiry), so the cost does not change. The
/// same ID again returns the stored transfer unchanged.
///
/// # Errors
/// [`StockDbError::Invalid`] (`to_location` = `from_location`, lines);
/// [`StockDbError::NotFound`] (`location`, `variant`).
#[allow(clippy::too_many_lines)] // one posting: each line out, then in
pub async fn post_internal_transfer(
    tx: &mut Tx,
    t: &NewInternalTransfer,
    at: Timestamp,
) -> StockResult<(Written, InternalTransfer)> {
    if let Some(stored) = get_internal_transfer(Conn::Tx(&mut *tx), t.id).await? {
        return Ok((Written::Unchanged, stored));
    }
    if t.from_location == t.to_location {
        return Err(StockDbError::invalid(
            "to_location",
            "must be another location",
        ));
    }
    check_lines(t.lines.len())?;
    check_location(tx, t.branch, t.from_location).await?;
    check_location(tx, t.branch, t.to_location).await?;
    let note = check_note(t.note.as_deref())?;
    let mut lines = Vec::with_capacity(t.lines.len());
    for l in &t.lines {
        if !l.qty.is_positive() {
            return Err(StockDbError::invalid("qty", "must be above 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        lines.push((l, info.factor, to_base(l.qty, info.factor)?));
    }
    let n = allocate(tx, DocKind::InternalTransfer, t.branch, at).await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO internal_transfer (id, number, branch_id, from_location_id, \
         to_location_id, status, note, local_day, seq, user_id, posted_at, posted_by, \
         created_at, updated_at) VALUES (?, ?, ?, ?, ?, 'posted', ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            t.id,
            n.number,
            t.branch,
            t.from_location,
            t.to_location,
            note,
            n.day,
            n.seq,
            t.user,
            at,
            t.user,
            at,
            at,
        ],
    )
    .await?;
    for (i, (l, factor, base)) in lines.iter().enumerate() {
        let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
        let doc = DocRef {
            kind: DocKind::InternalTransfer.as_str(),
            id: *t.id.as_uuid(),
            line: Some(no),
        };
        let out: Vec<Posted> = post_moves(
            tx,
            &[Move {
                branch: t.branch,
                location: Some(t.from_location),
                variant: l.variant,
                kind: MovementKind::InternalOut,
                qty: neg(*base)?,
                valuation: Valuation::Average,
                lot: LotRule::Fefo,
                doc,
                at,
            }],
        )
        .await?
        .remove(0);
        let mut ins = Vec::with_capacity(out.len());
        for p in &out {
            let lot = match p.lot {
                Some(lot) => {
                    let (batch, expiry) = lot_key(tx, lot).await?;
                    LotRule::Into(batch, expiry)
                }
                None => LotRule::None,
            };
            ins.push(Move {
                branch: t.branch,
                location: Some(t.to_location),
                variant: l.variant,
                kind: MovementKind::InternalIn,
                qty: neg(p.qty)?,
                valuation: Valuation::Fixed(p.value.and_then(|v| Money::ZERO.checked_sub(v))),
                lot,
                doc,
                at,
            });
        }
        post_moves(tx, &ins).await?;
        let value = total_value(&out).and_then(|v| Money::ZERO.checked_sub(v));
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO internal_transfer_line (transfer_id, line_no, variant_id, unit_id, \
             factor, qty, base_qty, value) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                t.id,
                i64::from(no),
                l.variant,
                l.unit,
                *factor,
                l.qty,
                *base,
                value,
            ],
        )
        .await?;
    }
    Ok((
        Written::Created,
        get_internal_transfer(Conn::Tx(&mut *tx), t.id)
            .await?
            .ok_or(DbError::NotFound("internal transfer"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        for s in [
            TransferStatus::Requested,
            TransferStatus::Shipped,
            TransferStatus::Received,
            TransferStatus::Cancelled,
        ] {
            assert_eq!(TransferStatus::from_key(s.as_str()), Some(s));
        }
        assert_eq!(TransferStatus::from_key("x"), None);
        assert!(neg(Qty::from_thousandths(i64::MIN)).is_err());
    }
}
