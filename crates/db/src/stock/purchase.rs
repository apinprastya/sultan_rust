//! Purchase orders (spec 2.8 §6): optional, `draft → ordered → closed`, or `draft | ordered →
//! cancelled` while nothing was received. A draft is written whole by [`put_purchase_order`];
//! [`order_purchase_order`] numbers it (`-PO-`). Receipts fill its lines
//! ([`super::post_goods_receipt`]); it closes by itself once every line is fully received, or by
//! hand ([`close_purchase_order`]).

use sultan_core::catalog::Qty;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{
    BranchId, LocationId, PurchaseOrderId, SupplierId, UnitId, UserId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::{DocKind, check_note, to_base};
use sultan_core::time::Timestamp;

use super::json::{doc_actor, purchase_order_json, record_doc};
use super::numbers::allocate;
use super::{
    StockDbError, StockResult, Written, check_lines, doc_location, line_no, opt_day, variant_info,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The state of a purchase order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PurchaseOrderStatus {
    /// Being written.
    Draft,
    /// Sent to the supplier (numbered).
    Ordered,
    /// Fully received, or closed by hand.
    Closed,
    /// Cancelled.
    Cancelled,
}

impl PurchaseOrderStatus {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Ordered => "ordered",
            Self::Closed => "closed",
            Self::Cancelled => "cancelled",
        }
    }

    /// The value of a stored name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Draft, Self::Ordered, Self::Closed, Self::Cancelled]
            .into_iter()
            .find(|s| s.as_str() == key)
    }
}

/// A line of a purchase order to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewPurchaseOrderLine {
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// Quantity in the unit (> 0).
    pub qty: Qty,
    /// Price per unit (≥ 0), from the supplier's last price, editable.
    pub price: Money,
}

/// A purchase order draft to write (`PUT /purchase-orders/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPurchaseOrder {
    /// The client's `UUIDv7`.
    pub id: PurchaseOrderId,
    /// The ordering branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The location to receive into; `None` = the receiving default.
    pub location: Option<LocationId>,
    /// The order date.
    pub order_date: Option<LocalDay>,
    /// The expected delivery date.
    pub expected_date: Option<LocalDay>,
    /// A note (0–500 characters).
    pub note: Option<String>,
    /// Who writes it.
    pub user: UserId,
    /// The lines (1–1000).
    pub lines: Vec<NewPurchaseOrderLine>,
}

/// A stored purchase order line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PurchaseOrderLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
    /// The unit's factor when written.
    pub factor: Qty,
    /// Quantity in the unit.
    pub qty: Qty,
    /// Price per unit.
    pub price: Money,
    /// Ordered, in base units.
    pub base_qty: Qty,
    /// Received so far, in base units (may exceed the order: over-received).
    pub received_qty: Qty,
}

/// A stored purchase order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurchaseOrder {
    /// The ID.
    pub id: PurchaseOrderId,
    /// `JKT01-PO-261126-01` once ordered.
    pub number: Option<String>,
    /// The branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The location to receive into.
    pub location: LocationId,
    /// The state.
    pub status: PurchaseOrderStatus,
    /// The order date.
    pub order_date: Option<LocalDay>,
    /// The expected date.
    pub expected_date: Option<LocalDay>,
    /// The note.
    pub note: Option<String>,
    /// Who wrote it.
    pub user: UserId,
    /// When it was ordered.
    pub ordered_at: Option<Timestamp>,
    /// When it closed.
    pub closed_at: Option<Timestamp>,
    /// When it was cancelled.
    pub cancelled_at: Option<Timestamp>,
    /// The lines.
    pub lines: Vec<PurchaseOrderLine>,
}

const SELECT: &str = "SELECT id, number, branch_id, supplier_id, location_id, status, order_date, \
                      expected_date, note, user_id, ordered_at, closed_at, cancelled_at \
                      FROM purchase_order";

fn map(row: &dyn Row) -> Result<PurchaseOrder> {
    let status = row.text("status")?;
    Ok(PurchaseOrder {
        id: sql::id(row, "id")?,
        number: row.opt_text("number")?,
        branch: sql::id(row, "branch_id")?,
        supplier: sql::id(row, "supplier_id")?,
        location: sql::id(row, "location_id")?,
        status: PurchaseOrderStatus::from_key(&status)
            .ok_or_else(|| DbError::Corrupt(format!("unknown PO status `{status}`")))?,
        order_date: opt_day(row, "order_date")?,
        expected_date: opt_day(row, "expected_date")?,
        note: row.opt_text("note")?,
        user: sql::id(row, "user_id")?,
        ordered_at: sql::opt_ts(row, "ordered_at")?,
        closed_at: sql::opt_ts(row, "closed_at")?,
        cancelled_at: sql::opt_ts(row, "cancelled_at")?,
        lines: Vec::new(),
    })
}

fn map_line(row: &dyn Row) -> Result<PurchaseOrderLine> {
    Ok(PurchaseOrderLine {
        line_no: line_no(row, "line_no")?,
        variant: sql::id(row, "variant_id")?,
        unit: sql::id(row, "unit_id")?,
        factor: sql::qty(row, "factor")?,
        qty: sql::qty(row, "qty")?,
        price: sql::money(row, "price")?,
        base_qty: sql::qty(row, "base_qty")?,
        received_qty: sql::qty(row, "received_qty")?,
    })
}

async fn with_lines(mut conn: Conn<'_>, mut po: PurchaseOrder) -> Result<PurchaseOrder> {
    po.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, price, base_qty, received_qty \
         FROM purchase_order_line WHERE order_id = ? ORDER BY line_no",
        args![po.id],
        map_line,
    )
    .await?;
    Ok(po)
}

/// One purchase order with its lines.
///
/// # Errors
/// The database error.
pub async fn get_purchase_order<'c>(
    conn: impl Into<Conn<'c>>,
    id: PurchaseOrderId,
) -> Result<Option<PurchaseOrder>> {
    let mut conn = conn.into();
    let sql = format!("{SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map).await? {
        Some(po) => with_lines(conn, po).await.map(Some),
        None => Ok(None),
    }
}

/// The purchase orders of a branch and / or supplier, optionally of one status, newest first
/// (at most 500).
///
/// # Errors
/// The database error.
pub async fn list_purchase_orders<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    supplier: Option<SupplierId>,
    status: Option<PurchaseOrderStatus>,
) -> Result<Vec<PurchaseOrder>> {
    let mut conn = conn.into();
    let mut sql = format!("{SELECT} WHERE 1 = 1");
    let mut a: Vec<Arg> = Vec::new();
    if let Some(b) = branch {
        sql.push_str(" AND branch_id = ?");
        a.push(b.into());
    }
    if let Some(s) = supplier {
        sql.push_str(" AND supplier_id = ?");
        a.push(s.into());
    }
    if let Some(s) = status {
        sql.push_str(" AND status = ?");
        a.push(s.as_str().into());
    }
    sql.push_str(" ORDER BY id DESC LIMIT 500");
    let heads = sql::fetch_all(conn.reborrow(), &sql, a, map).await?;
    let mut out = Vec::with_capacity(heads.len());
    for po in heads {
        out.push(with_lines(conn.reborrow(), po).await?);
    }
    Ok(out)
}

/// Checks that the supplier exists, is not archived and is active.
pub(crate) async fn check_supplier_live(tx: &mut Tx, supplier: SupplierId) -> StockResult<()> {
    let found = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT active, archived_at FROM supplier WHERE id = ?",
        args![supplier],
        |row| Ok((row.bool("active")?, sql::opt_ts(row, "archived_at")?)),
    )
    .await?;
    match found {
        None => Err(StockDbError::NotFound("supplier")),
        Some((_, Some(_))) => Err(StockDbError::Archived("supplier")),
        Some((false, None)) => Err(StockDbError::invalid("supplier", "is not active")),
        Some((true, None)) => Ok(()),
    }
}

/// Writes a draft purchase order whole (`PUT /purchase-orders/{id}`): header and lines. Only a
/// draft can be written.
///
/// # Errors
/// [`StockDbError::State`] for an ordered, closed or cancelled order; [`StockDbError::NotFound`]
/// (`supplier`, `location`, `variant`); [`StockDbError::Invalid`] / [`StockDbError::Rule`] for a
/// line (`qty`, `price`, `unit`, `variant`, `lines`, `note`).
#[allow(clippy::too_many_lines)] // one draft write: header, then its lines, checked first
pub async fn put_purchase_order(
    tx: &mut Tx,
    po: &NewPurchaseOrder,
    now: Timestamp,
) -> StockResult<(Written, PurchaseOrder)> {
    let before = get_purchase_order(Conn::Tx(&mut *tx), po.id).await?;
    if let Some(b) = &before {
        if b.status != PurchaseOrderStatus::Draft {
            return Err(StockDbError::state("purchase order", b.status.as_str()));
        }
        if b.branch != po.branch {
            return Err(StockDbError::invalid("branch", "cannot change"));
        }
    }
    check_lines(po.lines.len())?;
    check_supplier_live(tx, po.supplier).await?;
    let location = doc_location(tx, po.branch, po.location).await?;
    let note = check_note(po.note.as_deref())?;
    let mut lines = Vec::with_capacity(po.lines.len());
    for (i, l) in po.lines.iter().enumerate() {
        if !l.qty.is_positive() {
            return Err(StockDbError::invalid("qty", "must be above 0"));
        }
        if l.price.is_negative() {
            return Err(StockDbError::invalid("price", "must not be below 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        lines.push(PurchaseOrderLine {
            line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
            variant: l.variant,
            unit: l.unit,
            factor: info.factor,
            qty: l.qty,
            price: l.price,
            base_qty: to_base(l.qty, info.factor)?,
            received_qty: Qty::ZERO,
        });
    }
    let values: Vec<Arg> = args![
        po.branch,
        po.supplier,
        location,
        po.order_date.map(LocalDay::key),
        po.expected_date.map(LocalDay::key),
        note,
        po.user,
        now,
    ];
    if before.is_some() {
        let mut v = values;
        v.push(po.id.into());
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE purchase_order SET branch_id = ?, supplier_id = ?, location_id = ?, \
             order_date = ?, expected_date = ?, note = ?, user_id = ?, updated_at = ? \
             WHERE id = ?",
            v,
        )
        .await?;
        sql::execute(
            Conn::Tx(&mut *tx),
            "DELETE FROM purchase_order_line WHERE order_id = ?",
            args![po.id],
        )
        .await?;
    } else {
        let mut v: Vec<Arg> = args![po.id];
        v.extend(values);
        v.push(now.into());
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO purchase_order (id, branch_id, supplier_id, location_id, order_date, \
             expected_date, note, user_id, updated_at, created_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft')",
            v,
        )
        .await?;
    }
    for l in &lines {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO purchase_order_line (order_id, line_no, variant_id, unit_id, factor, \
             qty, price, base_qty, received_qty) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0)",
            args![
                po.id,
                i64::from(l.line_no),
                l.variant,
                l.unit,
                l.factor,
                l.qty,
                l.price,
                l.base_qty,
            ],
        )
        .await?;
    }
    let stored = get_purchase_order(Conn::Tx(&mut *tx), po.id)
        .await?
        .ok_or(DbError::NotFound("purchase order"))?;
    if before.as_ref() == Some(&stored) {
        return Ok((Written::Unchanged, stored));
    }
    recorded(tx, po.user, "purchase_order.put", &stored, now).await?;
    Ok((
        if before.is_some() {
            Written::Updated
        } else {
            Written::Created
        },
        stored,
    ))
}

async fn load(tx: &mut Tx, id: PurchaseOrderId) -> StockResult<PurchaseOrder> {
    get_purchase_order(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("purchase order"))
}

/// Loads the order and records `action` on it (`purchase_order.changed`).
async fn changed(
    tx: &mut Tx,
    id: PurchaseOrderId,
    user: UserId,
    action: &str,
    at: Timestamp,
) -> StockResult<(Written, PurchaseOrder)> {
    let po = load(tx, id).await?;
    recorded(tx, user, action, &po, at).await?;
    Ok((Written::Updated, po))
}

async fn recorded(
    tx: &mut Tx,
    user: UserId,
    action: &str,
    po: &PurchaseOrder,
    at: Timestamp,
) -> Result<()> {
    record_doc(
        tx,
        &doc_actor(user, po.branch),
        at,
        (action, "purchase_order.changed"),
        *po.id.as_uuid(),
        purchase_order_json(po),
    )
    .await
}

/// Orders a draft (`POST /purchase-orders/{id}/order`, `purchase.order`): numbers it `-PO-` on
/// the branch-local day of `at`. An ordered or closed order is returned unchanged.
///
/// # Errors
/// [`StockDbError::NotFound`]; [`StockDbError::State`] for a cancelled order; a retryable
/// conflict when another order took the number.
pub async fn order_purchase_order(
    tx: &mut Tx,
    id: PurchaseOrderId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, PurchaseOrder)> {
    let po = load(tx, id).await?;
    match po.status {
        PurchaseOrderStatus::Ordered | PurchaseOrderStatus::Closed => {
            return Ok((Written::Unchanged, po));
        }
        PurchaseOrderStatus::Cancelled => {
            return Err(StockDbError::state("purchase order", po.status.as_str()));
        }
        PurchaseOrderStatus::Draft => {}
    }
    let n = allocate(tx, DocKind::PurchaseOrder, po.branch, at).await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE purchase_order SET status = 'ordered', number = ?, local_day = ?, seq = ?, \
         ordered_at = ?, ordered_by = ?, updated_at = ? WHERE id = ? AND status = 'draft'",
        args![n.number, n.day, n.seq, at, user, at, id],
    )
    .await?;
    changed(tx, id, user, "purchase_order.order", at).await
}

/// Closes an ordered purchase order by hand ("close the rest", `purchase.order`). A closed one
/// is returned unchanged.
///
/// # Errors
/// [`StockDbError::NotFound`]; [`StockDbError::State`] for a draft or cancelled order.
pub async fn close_purchase_order(
    tx: &mut Tx,
    id: PurchaseOrderId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, PurchaseOrder)> {
    let po = load(tx, id).await?;
    match po.status {
        PurchaseOrderStatus::Closed => Ok((Written::Unchanged, po)),
        PurchaseOrderStatus::Ordered => {
            close(tx, id, Some(user), at).await?;
            changed(tx, id, user, "purchase_order.close", at).await
        }
        s => Err(StockDbError::state("purchase order", s.as_str())),
    }
}

pub(crate) async fn close(
    tx: &mut Tx,
    id: PurchaseOrderId,
    user: Option<UserId>,
    at: Timestamp,
) -> Result<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE purchase_order SET status = 'closed', closed_at = ?, closed_by = ?, \
         updated_at = ? WHERE id = ? AND status = 'ordered'",
        args![at, user, at, id],
    )
    .await?;
    Ok(())
}

/// Cancels a draft, or an ordered purchase order with nothing received (`purchase.order`). A
/// cancelled one is returned unchanged.
///
/// # Errors
/// [`StockDbError::NotFound`]; [`StockDbError::State`] for a closed order or one with a receipt.
pub async fn cancel_purchase_order(
    tx: &mut Tx,
    id: PurchaseOrderId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, PurchaseOrder)> {
    let po = load(tx, id).await?;
    match po.status {
        PurchaseOrderStatus::Cancelled => return Ok((Written::Unchanged, po)),
        PurchaseOrderStatus::Closed => {
            return Err(StockDbError::state("purchase order", "closed"));
        }
        PurchaseOrderStatus::Ordered if po.lines.iter().any(|l| l.received_qty != Qty::ZERO) => {
            return Err(StockDbError::state("purchase order", "partly received"));
        }
        _ => {}
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE purchase_order SET status = 'cancelled', cancelled_at = ?, cancelled_by = ?, \
         updated_at = ? WHERE id = ?",
        args![at, user, at, id],
    )
    .await?;
    changed(tx, id, user, "purchase_order.cancel", at).await
}

/// Adds `qty` base units to the received quantity of a PO line, closes the order once every
/// line is fully received, and says whether the line is now over-received (a warning, §6).
pub(crate) async fn receive_into(
    tx: &mut Tx,
    order: PurchaseOrderId,
    line: u32,
    variant: VariantId,
    qty: Qty,
    at: Timestamp,
) -> StockResult<bool> {
    let po = load(tx, order).await?;
    if po.status != PurchaseOrderStatus::Ordered {
        return Err(StockDbError::invalid(
            "order_line",
            format!("the purchase order is {}", po.status.as_str()),
        ));
    }
    let l = po
        .lines
        .iter()
        .find(|l| l.line_no == line)
        .ok_or(StockDbError::NotFound("purchase order line"))?;
    if l.variant != variant {
        return Err(StockDbError::invalid(
            "order_line",
            "is for another variant",
        ));
    }
    let received = l
        .received_qty
        .checked_add(qty)
        .ok_or(sultan_core::stock::StockError::Overflow)?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE purchase_order_line SET received_qty = ? WHERE order_id = ? AND line_no = ?",
        args![received, order, i64::from(line)],
    )
    .await?;
    let po = load(tx, order).await?;
    if po.lines.iter().all(|l| l.received_qty >= l.base_qty) {
        close(tx, order, None, at).await?;
    }
    Ok(received > l.base_qty)
}

/// Takes `qty` base units back from a PO line's received quantity (a voided receipt); the
/// order reopens when it had closed by itself.
pub(crate) async fn unreceive(
    tx: &mut Tx,
    order: PurchaseOrderId,
    line: u32,
    qty: Qty,
    at: Timestamp,
) -> Result<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE purchase_order_line SET received_qty = received_qty - ? \
         WHERE order_id = ? AND line_no = ?",
        args![qty, order, i64::from(line)],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE purchase_order SET status = 'ordered', closed_at = NULL, updated_at = ? \
         WHERE id = ? AND status = 'closed' AND closed_by IS NULL",
        args![at, order],
    )
    .await?;
    Ok(())
}

/// On order of `variant` at `branch` (§4): ordered − received over its ordered (open) purchase
/// orders, never below 0 per line.
///
/// # Errors
/// The database error.
pub async fn on_order<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    variant: VariantId,
) -> Result<Qty> {
    sql::fetch_optional(
        conn.into(),
        "SELECT CAST(COALESCE(SUM(CASE WHEN l.base_qty > l.received_qty \
         THEN l.base_qty - l.received_qty ELSE 0 END), 0) AS SIGNED) AS qty \
         FROM purchase_order_line l JOIN purchase_order o ON o.id = l.order_id \
         WHERE o.branch_id = ? AND o.status = 'ordered' AND l.variant_id = ?",
        args![branch, variant],
        |row| sql::qty(row, "qty"),
    )
    .await
    .map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses() {
        for s in [
            PurchaseOrderStatus::Draft,
            PurchaseOrderStatus::Ordered,
            PurchaseOrderStatus::Closed,
            PurchaseOrderStatus::Cancelled,
        ] {
            assert_eq!(PurchaseOrderStatus::from_key(s.as_str()), Some(s));
        }
        assert_eq!(PurchaseOrderStatus::from_key("open"), None);
    }
}
