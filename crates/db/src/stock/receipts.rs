//! Goods receipts with the supplier's invoice, their payables, and supplier returns (spec 2.8
//! §6, I2, I3, I5, I6, I14).
//!
//! A receipt is a draft until [`post_goods_receipt`]: the totals and landed cost come from
//! `core::stock::receipt_totals`; posting moves the stock in at the landed cost (with lots),
//! updates the average, the supplier items' last price and the PO lines, and creates the
//! payable. [`void_goods_receipt`] takes it all back while [`receipt_in_use`] says no later
//! movement used its cost and the payable is unpaid. A supplier return
//! ([`post_supplier_return`]) moves stock out at the average and stores the supplier credit.

use std::collections::BTreeMap;

use sultan_core::catalog::Qty;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{
    BranchId, GoodsReceiptId, LocationId, LotId, PayableId, PurchaseOrderId, SupplierId,
    SupplierReturnId, UnitId, UserId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::{
    DocKind, MovementKind, ReceiptInput, ReceiptLineInput, TaxMode, check_lot, check_note,
    receipt_totals, supplier_return_credit, to_base,
};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::ledger::{DocRef, LotRule, Move, Valuation, post_moves, total_value};
use super::numbers::allocate;
use super::purchase::{check_supplier_live, receive_into, unreceive};
use super::suppliers::record_last_price;
use super::{
    StockDbError, StockResult, Written, check_lines, day, doc_location, get_inventory_settings,
    line_no, opt_day, variant_info,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The longest invoice number, in characters.
const MAX_INVOICE_CHARS: usize = 60;

/// The state of a goods receipt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReceiptStatus {
    /// Being entered.
    Draft,
    /// Posted: stock, cost, payable.
    Posted,
    /// Voided.
    Voided,
}

impl ReceiptStatus {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Draft => "draft",
            Self::Posted => "posted",
            Self::Voided => "voided",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        [Self::Draft, Self::Posted, Self::Voided]
            .into_iter()
            .find(|s| s.as_str() == key)
    }
}

/// How a receipt is paid (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaymentKind {
    /// On credit: due on the invoice date + the supplier's terms.
    Credit,
    /// Paid now in cash.
    Cash,
}

impl PaymentKind {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Credit => "credit",
            Self::Cash => "cash",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        [Self::Credit, Self::Cash]
            .into_iter()
            .find(|s| s.as_str() == key)
    }
}

fn tax_mode(row: &dyn Row) -> Result<TaxMode> {
    let key = row.text("tax_mode")?;
    TaxMode::from_key(&key).ok_or_else(|| DbError::Corrupt(format!("unknown tax mode `{key}`")))
}

fn rate(row: &dyn Row) -> Result<u16> {
    let v = row.i64("tax_rate_bp")?;
    u16::try_from(v).map_err(|_| DbError::Corrupt(format!("tax rate {v} out of range")))
}

/// A receipt line to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGoodsReceiptLine {
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// Quantity in the unit (> 0).
    pub qty: Qty,
    /// Price per unit before discount, in the receipt's tax mode.
    pub price: Money,
    /// Line discount (Rp).
    pub discount: Money,
    /// The PO line it fills.
    pub order_line: Option<(PurchaseOrderId, u32)>,
    /// Batch number (tracked products).
    pub batch: Option<String>,
    /// Expiry date (required for tracked products when posted).
    pub expiry: Option<LocalDay>,
}

/// A goods receipt draft to write (`PUT /goods-receipts/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGoodsReceipt {
    /// The client's `UUIDv7`.
    pub id: GoodsReceiptId,
    /// The receiving branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The location; `None` = the receiving default.
    pub location: Option<LocationId>,
    /// The supplier's invoice number (1–60 characters).
    pub invoice_no: String,
    /// The invoice date.
    pub invoice_date: LocalDay,
    /// Credit or cash.
    pub payment: PaymentKind,
    /// The due date of a credit receipt; `None` = invoice date + the supplier's terms.
    pub due_date: Option<LocalDay>,
    /// Included, added or none.
    pub tax_mode: TaxMode,
    /// The PPN rate; `None` = the company's standard rate.
    pub tax_rate_bp: Option<u16>,
    /// Header discount (Rp).
    pub header_discount: Money,
    /// Other costs (Rp).
    pub other_costs: Money,
    /// A note.
    pub note: Option<String>,
    /// Who enters it.
    pub user: UserId,
    /// The lines.
    pub lines: Vec<NewGoodsReceiptLine>,
}

/// A stored receipt line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoodsReceiptLine {
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
    /// Price per unit.
    pub price: Money,
    /// Line discount.
    pub discount: Money,
    /// The PO line it fills.
    pub order_line: Option<(PurchaseOrderId, u32)>,
    /// Batch.
    pub batch: Option<String>,
    /// Expiry.
    pub expiry: Option<LocalDay>,
    /// Base units.
    pub base_qty: Qty,
    /// Its share of the header discount (posted).
    pub header_discount: Option<Money>,
    /// Its PPN (posted).
    pub tax: Option<Money>,
    /// Its share of the other costs (posted).
    pub other_costs: Option<Money>,
    /// Its landed cost, the `receipt` movement's value (posted).
    pub landed: Option<Money>,
    /// The lot it went into.
    pub lot: Option<LotId>,
}

/// A stored goods receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoodsReceipt {
    /// The ID.
    pub id: GoodsReceiptId,
    /// `JKT01-GR-261126-01` once posted.
    pub number: Option<String>,
    /// The branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The location.
    pub location: LocationId,
    /// The state.
    pub status: ReceiptStatus,
    /// The invoice number.
    pub invoice_no: String,
    /// The invoice date.
    pub invoice_date: LocalDay,
    /// Credit or cash.
    pub payment: PaymentKind,
    /// The due date (set when posted).
    pub due_date: Option<LocalDay>,
    /// The tax mode.
    pub tax_mode: TaxMode,
    /// The PPN rate.
    pub tax_rate_bp: u16,
    /// Header discount.
    pub header_discount: Money,
    /// Other costs.
    pub other_costs: Money,
    /// The company's PKP setting when posted.
    pub pkp: Option<bool>,
    /// Σ line nets (posted).
    pub net: Option<Money>,
    /// PPN (posted).
    pub tax: Option<Money>,
    /// The invoice total, the payable's amount (posted).
    pub total: Option<Money>,
    /// Σ landed costs (posted).
    pub landed: Option<Money>,
    /// The note.
    pub note: Option<String>,
    /// Who entered it.
    pub user: UserId,
    /// When it was posted.
    pub posted_at: Option<Timestamp>,
    /// When it was voided.
    pub voided_at: Option<Timestamp>,
    /// The manager who approved the void.
    pub approved_by: Option<UserId>,
    /// The lines.
    pub lines: Vec<GoodsReceiptLine>,
}

/// A posted receipt and its over-received PO lines (a warning, §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptPosted {
    /// The receipt.
    pub receipt: GoodsReceipt,
    /// PO lines now received beyond what was ordered.
    pub over_received: Vec<(PurchaseOrderId, u32)>,
}

const SELECT: &str = "SELECT id, number, branch_id, supplier_id, location_id, status, invoice_no, \
                      invoice_date, payment, due_date, tax_mode, tax_rate_bp, header_discount, \
                      other_costs, pkp, net, tax, total, landed, note, user_id, posted_at, \
                      voided_at, approved_by FROM goods_receipt";

fn map(row: &dyn Row) -> Result<GoodsReceipt> {
    let status = row.text("status")?;
    let payment = row.text("payment")?;
    Ok(GoodsReceipt {
        id: sql::id(row, "id")?,
        number: row.opt_text("number")?,
        branch: sql::id(row, "branch_id")?,
        supplier: sql::id(row, "supplier_id")?,
        location: sql::id(row, "location_id")?,
        status: ReceiptStatus::from_key(&status)
            .ok_or_else(|| DbError::Corrupt(format!("unknown receipt status `{status}`")))?,
        invoice_no: row.text("invoice_no")?,
        invoice_date: day(row, "invoice_date")?,
        payment: PaymentKind::from_key(&payment)
            .ok_or_else(|| DbError::Corrupt(format!("unknown payment `{payment}`")))?,
        due_date: opt_day(row, "due_date")?,
        tax_mode: tax_mode(row)?,
        tax_rate_bp: rate(row)?,
        header_discount: sql::money(row, "header_discount")?,
        other_costs: sql::money(row, "other_costs")?,
        pkp: row.opt_i64("pkp")?.map(|v| v != 0),
        net: sql::opt_money(row, "net")?,
        tax: sql::opt_money(row, "tax")?,
        total: sql::opt_money(row, "total")?,
        landed: sql::opt_money(row, "landed")?,
        note: row.opt_text("note")?,
        user: sql::id(row, "user_id")?,
        posted_at: sql::opt_ts(row, "posted_at")?,
        voided_at: sql::opt_ts(row, "voided_at")?,
        approved_by: sql::opt_id(row, "approved_by")?,
        lines: Vec::new(),
    })
}

fn map_line(row: &dyn Row) -> Result<GoodsReceiptLine> {
    let order: Option<PurchaseOrderId> = sql::opt_id(row, "order_id")?;
    let order_line = match order {
        Some(o) => Some((o, line_no(row, "order_line_no")?)),
        None => None,
    };
    Ok(GoodsReceiptLine {
        line_no: line_no(row, "line_no")?,
        variant: sql::id(row, "variant_id")?,
        unit: sql::id(row, "unit_id")?,
        factor: sql::qty(row, "factor")?,
        qty: sql::qty(row, "qty")?,
        price: sql::money(row, "price")?,
        discount: sql::money(row, "discount")?,
        order_line,
        batch: row.opt_text("batch")?,
        expiry: opt_day(row, "expiry")?,
        base_qty: sql::qty(row, "base_qty")?,
        header_discount: sql::opt_money(row, "header_discount")?,
        tax: sql::opt_money(row, "tax")?,
        other_costs: sql::opt_money(row, "other_costs")?,
        landed: sql::opt_money(row, "landed")?,
        lot: sql::opt_id(row, "lot_id")?,
    })
}

async fn receipt_lines(mut conn: Conn<'_>, mut r: GoodsReceipt) -> Result<GoodsReceipt> {
    r.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, price, discount, order_id, \
         order_line_no, batch, expiry, base_qty, header_discount, tax, other_costs, landed, \
         lot_id FROM goods_receipt_line WHERE receipt_id = ? ORDER BY line_no",
        args![r.id],
        map_line,
    )
    .await?;
    Ok(r)
}

/// One goods receipt with its lines.
///
/// # Errors
/// The database error.
pub async fn get_goods_receipt<'c>(
    conn: impl Into<Conn<'c>>,
    id: GoodsReceiptId,
) -> Result<Option<GoodsReceipt>> {
    let mut conn = conn.into();
    let sql = format!("{SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map).await? {
        Some(r) => receipt_lines(conn, r).await.map(Some),
        None => Ok(None),
    }
}

/// The receipts of a branch and / or supplier, optionally of one status, newest first (at most
/// 500).
///
/// # Errors
/// The database error.
pub async fn list_goods_receipts<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    supplier: Option<SupplierId>,
    status: Option<ReceiptStatus>,
) -> Result<Vec<GoodsReceipt>> {
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
    for r in heads {
        out.push(receipt_lines(conn.reborrow(), r).await?);
    }
    Ok(out)
}

fn clean_invoice(no: &str) -> StockResult<String> {
    let no = no.trim();
    let n = no.chars().count();
    if n == 0 || n > MAX_INVOICE_CHARS {
        return Err(StockDbError::invalid(
            "invoice_no",
            "must be 1–60 characters",
        ));
    }
    Ok(no.to_owned())
}

/// Writes a draft receipt whole (`PUT /goods-receipts/{id}`). Lines are checked for what can
/// be checked while entering (`qty > 0`, `price ≥ 0`, discount ≥ 0, the unit, the batch length);
/// the totals, the expiry of tracked lines and the PO lines are checked when posting.
///
/// # Errors
/// [`StockDbError::State`] for a posted or voided receipt; [`StockDbError::NotFound`]
/// (`supplier`, `location`, `variant`); [`StockDbError::Invalid`] / [`StockDbError::Rule`].
#[allow(clippy::too_many_lines)] // one draft write: header, then its lines, checked first
pub async fn put_goods_receipt(
    tx: &mut Tx,
    r: &NewGoodsReceipt,
    now: Timestamp,
) -> StockResult<(Written, GoodsReceipt)> {
    let before = get_goods_receipt(Conn::Tx(&mut *tx), r.id).await?;
    if let Some(b) = &before {
        if b.status != ReceiptStatus::Draft {
            return Err(StockDbError::state("goods receipt", b.status.as_str()));
        }
        if b.branch != r.branch {
            return Err(StockDbError::invalid("branch", "cannot change"));
        }
    }
    check_lines(r.lines.len())?;
    check_supplier_live(tx, r.supplier).await?;
    let location = doc_location(tx, r.branch, r.location).await?;
    let invoice = clean_invoice(&r.invoice_no)?;
    let note = check_note(r.note.as_deref())?;
    let rate = match r.tax_rate_bp {
        Some(rate) if rate > 10_000 => {
            return Err(StockDbError::invalid("tax_rate", "must be at most 100%"));
        }
        Some(rate) => rate,
        None => {
            get_inventory_settings(Conn::Tx(&mut *tx))
                .await?
                .standard_ppn_rate_bp
        }
    };
    if r.header_discount.is_negative() || r.other_costs.is_negative() {
        return Err(StockDbError::invalid(
            "header_discount",
            "discounts and costs must not be below 0",
        ));
    }
    let mut lines = Vec::with_capacity(r.lines.len());
    for l in &r.lines {
        if !l.qty.is_positive() {
            return Err(StockDbError::invalid("qty", "must be above 0"));
        }
        if l.price.is_negative() || l.discount.is_negative() {
            return Err(StockDbError::invalid("price", "must not be below 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        check_lot(false, l.batch.as_deref(), None)?;
        lines.push((l, info.factor, to_base(l.qty, info.factor)?));
    }
    let header: Vec<Arg> = args![
        r.branch,
        r.supplier,
        location,
        invoice,
        r.invoice_date.key(),
        r.payment.as_str(),
        r.due_date.map(LocalDay::key),
        r.tax_mode.as_str(),
        i64::from(rate),
        r.header_discount,
        r.other_costs,
        note,
        r.user,
        now,
    ];
    if before.is_some() {
        let mut v = header;
        v.push(r.id.into());
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE goods_receipt SET branch_id = ?, supplier_id = ?, location_id = ?, \
             invoice_no = ?, invoice_date = ?, payment = ?, due_date = ?, tax_mode = ?, \
             tax_rate_bp = ?, header_discount = ?, other_costs = ?, note = ?, user_id = ?, \
             updated_at = ? WHERE id = ?",
            v,
        )
        .await?;
        sql::execute(
            Conn::Tx(&mut *tx),
            "DELETE FROM goods_receipt_line WHERE receipt_id = ?",
            args![r.id],
        )
        .await?;
    } else {
        let mut v: Vec<Arg> = args![r.id];
        v.extend(header);
        v.push(now.into());
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO goods_receipt (id, branch_id, supplier_id, location_id, invoice_no, \
             invoice_date, payment, due_date, tax_mode, tax_rate_bp, header_discount, \
             other_costs, note, user_id, updated_at, created_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft')",
            v,
        )
        .await?;
    }
    for (i, (l, factor, base)) in lines.iter().enumerate() {
        let batch = l
            .batch
            .as_deref()
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .map(str::to_owned);
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO goods_receipt_line (receipt_id, line_no, variant_id, unit_id, factor, \
             qty, price, discount, order_id, order_line_no, batch, expiry, base_qty) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                r.id,
                i64::try_from(i + 1).unwrap_or(i64::MAX),
                l.variant,
                l.unit,
                *factor,
                l.qty,
                l.price,
                l.discount,
                l.order_line.map(|o| o.0),
                l.order_line.map(|o| i64::from(o.1)),
                batch,
                l.expiry.map(LocalDay::key),
                *base,
            ],
        )
        .await?;
    }
    let stored = get_goods_receipt(Conn::Tx(&mut *tx), r.id)
        .await?
        .ok_or(DbError::NotFound("goods receipt"))?;
    Ok((
        if before.is_some() {
            Written::Updated
        } else {
            Written::Created
        },
        stored,
    ))
}

async fn load_receipt(tx: &mut Tx, id: GoodsReceiptId) -> StockResult<GoodsReceipt> {
    get_goods_receipt(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("goods receipt"))
}

async fn supplier_terms(tx: &mut Tx, supplier: SupplierId) -> Result<i64> {
    Ok(sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT terms_days FROM supplier WHERE id = ?",
        args![supplier],
        |row| row.i64("terms_days"),
    )
    .await?
    .unwrap_or(0))
}

/// Posts a draft receipt (`POST /goods-receipts/{id}/post`, `purchase.receive`, §6): numbers it
/// `-GR-`, computes the totals and landed cost (PPN in the cost unless the company is PKP),
/// moves each line in at its landed cost into its lot (a tracked product needs an expiry),
/// updates the average, the supplier items' last price (per base unit, before discounts) and
/// the PO lines (closing a fully received PO), and creates the payable (amount = invoice total,
/// due on the given date or invoice date + the supplier's terms; a cash receipt is paid). A
/// posted receipt is returned unchanged.
///
/// # Errors
/// [`StockDbError::NotFound`]; [`StockDbError::State`] for a voided receipt;
/// [`StockDbError::DuplicateInvoice`] when another posted receipt of the supplier has the
/// invoice number; [`StockDbError::Rule`] / [`StockDbError::Invalid`] for totals, lots or PO
/// lines; a retryable conflict on a lost race.
#[allow(clippy::too_many_lines)] // one posting, its steps in the order of spec §6
pub async fn post_goods_receipt(
    tx: &mut Tx,
    id: GoodsReceiptId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, ReceiptPosted)> {
    let r = load_receipt(tx, id).await?;
    match r.status {
        ReceiptStatus::Posted => {
            return Ok((
                Written::Unchanged,
                ReceiptPosted {
                    receipt: r,
                    over_received: Vec::new(),
                },
            ));
        }
        ReceiptStatus::Voided => return Err(StockDbError::state("goods receipt", "voided")),
        ReceiptStatus::Draft => {}
    }
    check_supplier_live(tx, r.supplier).await?;
    let dup = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 AS x FROM goods_receipt WHERE supplier_id = ? AND invoice_active = ?",
        args![r.supplier, r.invoice_no.clone()],
        |_| Ok(()),
    )
    .await?;
    if dup.is_some() {
        return Err(StockDbError::DuplicateInvoice(r.invoice_no));
    }
    let settings = get_inventory_settings(Conn::Tx(&mut *tx)).await?;
    let inputs: Vec<ReceiptLineInput> = r
        .lines
        .iter()
        .map(|l| ReceiptLineInput {
            qty: l.qty,
            price: l.price,
            discount: l.discount,
            factor: l.factor,
        })
        .collect();
    let totals = receipt_totals(&ReceiptInput {
        tax_mode: r.tax_mode,
        rate_bp: r.tax_rate_bp,
        header_discount: r.header_discount,
        other_costs: r.other_costs,
        pkp: settings.pkp,
        lines: &inputs,
    })?;
    let n = allocate(tx, DocKind::GoodsReceipt, r.branch, at).await?;
    let mut over_received = Vec::new();
    for (l, t) in r.lines.iter().zip(&totals.lines) {
        let info = variant_info(tx, l.variant, l.unit).await?;
        let lot = check_lot(info.track_expiry, l.batch.as_deref(), l.expiry)?;
        if let Some((order, line)) = l.order_line {
            let po = super::get_purchase_order(Conn::Tx(&mut *tx), order)
                .await?
                .ok_or(StockDbError::NotFound("purchase order"))?;
            if po.supplier != r.supplier || po.branch != r.branch {
                return Err(StockDbError::invalid(
                    "order_line",
                    "is of another supplier or branch",
                ));
            }
            if receive_into(tx, order, line, l.variant, l.base_qty, at).await? {
                over_received.push((order, line));
            }
        }
        let posted = post_moves(
            tx,
            &[Move {
                branch: r.branch,
                location: Some(r.location),
                variant: l.variant,
                kind: MovementKind::Receipt,
                qty: l.base_qty,
                valuation: Valuation::Receive(t.landed),
                lot: lot.map_or(LotRule::None, |(b, e)| LotRule::Into(b, e)),
                doc: DocRef {
                    kind: DocKind::GoodsReceipt.as_str(),
                    id: *id.as_uuid(),
                    line: Some(l.line_no),
                },
                at,
            }],
        )
        .await?;
        let lot_id = posted.first().and_then(|p| p.first()).and_then(|p| p.lot);
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE goods_receipt_line SET header_discount = ?, tax = ?, other_costs = ?, \
             landed = ?, lot_id = ? WHERE receipt_id = ? AND line_no = ?",
            args![
                t.header_discount,
                t.tax,
                t.other_costs,
                t.landed,
                lot_id,
                id,
                i64::from(l.line_no),
            ],
        )
        .await?;
        let per_base = sultan_core::stock::per_base_price(l.price, l.factor)?;
        record_last_price(tx, r.supplier, l.variant, per_base, l.unit, at).await?;
    }
    let due = match (r.payment, r.due_date) {
        (PaymentKind::Cash, _) => r.invoice_date,
        (PaymentKind::Credit, Some(d)) => d,
        (PaymentKind::Credit, None) => {
            let terms = supplier_terms(tx, r.supplier).await?;
            r.invoice_date
                .add_days(terms)
                .ok_or_else(|| StockDbError::invalid("due_date", "out of range"))?
        }
    };
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE goods_receipt SET status = 'posted', number = ?, local_day = ?, seq = ?, \
         invoice_active = invoice_no, due_date = ?, pkp = ?, net = ?, tax = ?, total = ?, \
         landed = ?, posted_at = ?, posted_by = ?, updated_at = ? \
         WHERE id = ? AND status = 'draft'",
        args![
            n.number,
            n.day,
            n.seq,
            due.key(),
            settings.pkp,
            totals.net,
            totals.tax,
            totals.total,
            totals.landed,
            at,
            user,
            at,
            id,
        ],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO payable (id, branch_id, supplier_id, receipt_id, amount, due_date, paid, \
         status, created_at, voided_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'open', ?, NULL)",
        args![
            PayableId::generate(),
            r.branch,
            r.supplier,
            id,
            totals.total,
            due.key(),
            r.payment == PaymentKind::Cash,
            at,
        ],
    )
    .await?;
    Ok((
        Written::Updated,
        ReceiptPosted {
            receipt: load_receipt(tx, id).await?,
            over_received,
        },
    ))
}

/// Whether a posted receipt can no longer be voided (§6): a later movement of any of its
/// variants at its branch used the cost it brought (a sale, transfer, return, adjustment or any
/// other posting since), or its payable is paid (a cash receipt is paid when posted).
///
/// # Errors
/// [`StockDbError::NotFound`]; the database error.
pub async fn receipt_in_use<'c>(
    conn: impl Into<Conn<'c>>,
    id: GoodsReceiptId,
) -> StockResult<bool> {
    let mut conn = conn.into();
    let paid = sql::fetch_optional(
        conn.reborrow(),
        "SELECT paid FROM payable WHERE receipt_id = ?",
        args![id],
        |row| row.bool("paid"),
    )
    .await?
    .ok_or(StockDbError::NotFound("payable"))?;
    if paid {
        return Ok(true);
    }
    let later = sql::fetch_optional(
        conn,
        "SELECT 1 AS x FROM stock_movement r JOIN stock_movement m \
         ON m.branch_id = r.branch_id AND m.variant_id = r.variant_id AND m.id > r.id \
         WHERE r.doc_id = ? AND r.kind = 'receipt' AND m.doc_id <> r.doc_id LIMIT 1",
        args![id],
        |_| Ok(()),
    )
    .await?;
    Ok(later.is_some())
}

/// Voids a posted receipt (`POST /goods-receipts/{id}/void`, `purchase.receive` + a manager
/// override, §6): `receipt_void` movements take each line back at its landed cost from its lot,
/// the PO lines lose what it received, the payable is voided and the invoice number is free
/// again. A voided receipt is returned unchanged.
///
/// # Errors
/// [`StockDbError::ReceiptInUse`]; [`StockDbError::State`] for a draft;
/// [`StockDbError::NotFound`].
pub async fn void_goods_receipt(
    tx: &mut Tx,
    id: GoodsReceiptId,
    user: UserId,
    approved_by: Option<UserId>,
    at: Timestamp,
) -> StockResult<(Written, GoodsReceipt)> {
    let r = load_receipt(tx, id).await?;
    match r.status {
        ReceiptStatus::Voided => return Ok((Written::Unchanged, r)),
        ReceiptStatus::Draft => return Err(StockDbError::state("goods receipt", "draft")),
        ReceiptStatus::Posted => {}
    }
    if receipt_in_use(Conn::Tx(&mut *tx), id).await? {
        return Err(StockDbError::ReceiptInUse);
    }
    for l in r.lines.iter().rev() {
        let qty = l
            .base_qty
            .checked_neg()
            .ok_or(sultan_core::stock::StockError::Overflow)?;
        post_moves(
            tx,
            &[Move {
                branch: r.branch,
                location: Some(r.location),
                variant: l.variant,
                kind: MovementKind::ReceiptVoid,
                qty,
                valuation: Valuation::Reverse(l.landed.unwrap_or(Money::ZERO)),
                lot: l.lot.map_or(LotRule::None, LotRule::Lot),
                doc: DocRef {
                    kind: DocKind::GoodsReceipt.as_str(),
                    id: *id.as_uuid(),
                    line: Some(l.line_no),
                },
                at,
            }],
        )
        .await?;
        if let Some((order, line)) = l.order_line {
            unreceive(tx, order, line, l.base_qty, at).await?;
        }
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE payable SET status = 'voided', voided_at = ? WHERE receipt_id = ?",
        args![at, id],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE goods_receipt SET status = 'voided', invoice_active = NULL, voided_at = ?, \
         voided_by = ?, approved_by = ?, updated_at = ? WHERE id = ?",
        args![at, user, approved_by, at, id],
    )
    .await?;
    Ok((Written::Updated, load_receipt(tx, id).await?))
}

/// A payable (§6, I3; payments are 2.9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payable {
    /// The ID.
    pub id: PayableId,
    /// The branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The receipt.
    pub receipt: GoodsReceiptId,
    /// The receipt's number.
    pub receipt_number: Option<String>,
    /// The amount (the invoice total).
    pub amount: Money,
    /// The due date.
    pub due_date: LocalDay,
    /// Paid (a cash receipt).
    pub paid: bool,
    /// Voided with its receipt.
    pub voided: bool,
}

/// The payables by due date (`GET /payables`, read-only until 2.9): of a branch and / or
/// supplier; `open` = not paid and not voided.
///
/// # Errors
/// The database error.
pub async fn list_payables<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    supplier: Option<SupplierId>,
    open: bool,
) -> Result<Vec<Payable>> {
    let mut sql = String::from(
        "SELECT p.id, p.branch_id, p.supplier_id, p.receipt_id, r.number, p.amount, \
         p.due_date, p.paid, p.status FROM payable p JOIN goods_receipt r ON r.id = p.receipt_id \
         WHERE 1 = 1",
    );
    let mut a: Vec<Arg> = Vec::new();
    if let Some(b) = branch {
        sql.push_str(" AND p.branch_id = ?");
        a.push(b.into());
    }
    if let Some(s) = supplier {
        sql.push_str(" AND p.supplier_id = ?");
        a.push(s.into());
    }
    if open {
        sql.push_str(" AND p.paid = 0 AND p.status = 'open'");
    }
    sql.push_str(" ORDER BY p.due_date, p.id");
    sql::fetch_all(conn.into(), &sql, a, |row| {
        Ok(Payable {
            id: sql::id(row, "id")?,
            branch: sql::id(row, "branch_id")?,
            supplier: sql::id(row, "supplier_id")?,
            receipt: sql::id(row, "receipt_id")?,
            receipt_number: row.opt_text("number")?,
            amount: sql::money(row, "amount")?,
            due_date: day(row, "due_date")?,
            paid: row.bool("paid")?,
            voided: row.text("status")? == "voided",
        })
    })
    .await
}

/// A supplier return line to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewSupplierReturnLine {
    /// The variant.
    pub variant: VariantId,
    /// Any unit of its product.
    pub unit: UnitId,
    /// Quantity in the unit (> 0).
    pub qty: Qty,
    /// Price per unit (from the receipt line when taken from one).
    pub price: Money,
    /// The receipt line it returns.
    pub receipt_line: Option<(GoodsReceiptId, u32)>,
}

/// A supplier return draft to write (`PUT /supplier-returns/{id}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSupplierReturn {
    /// The client's `UUIDv7`.
    pub id: SupplierReturnId,
    /// The branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The location; `None` = the receiving default.
    pub location: Option<LocationId>,
    /// The tax mode of the credit (a line taken from a receipt uses the receipt's).
    pub tax_mode: TaxMode,
    /// The PPN rate; `None` = the company's standard rate.
    pub tax_rate_bp: Option<u16>,
    /// A note.
    pub note: Option<String>,
    /// Who enters it.
    pub user: UserId,
    /// The lines.
    pub lines: Vec<NewSupplierReturnLine>,
}

/// A stored supplier return line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupplierReturnLine {
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
    /// Price per unit.
    pub price: Money,
    /// The receipt line it returns.
    pub receipt_line: Option<(GoodsReceiptId, u32)>,
    /// Base units.
    pub base_qty: Qty,
    /// The movement's value at the average (posted; `None` = no cost).
    pub value: Option<Money>,
}

/// A stored supplier return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplierReturn {
    /// The ID.
    pub id: SupplierReturnId,
    /// `JKT01-RS-261126-01` once posted.
    pub number: Option<String>,
    /// The branch.
    pub branch: BranchId,
    /// The supplier.
    pub supplier: SupplierId,
    /// The location.
    pub location: LocationId,
    /// Posted or not.
    pub posted: bool,
    /// The tax mode of the credit.
    pub tax_mode: TaxMode,
    /// The PPN rate.
    pub tax_rate_bp: u16,
    /// The supplier credit (posted), for 2.9.
    pub credit: Option<Money>,
    /// The note.
    pub note: Option<String>,
    /// Who entered it.
    pub user: UserId,
    /// When it was posted.
    pub posted_at: Option<Timestamp>,
    /// The lines.
    pub lines: Vec<SupplierReturnLine>,
}

const RETURN_SELECT: &str = "SELECT id, number, branch_id, supplier_id, location_id, status, \
                             tax_mode, tax_rate_bp, credit, note, user_id, posted_at \
                             FROM supplier_return";

fn map_return(row: &dyn Row) -> Result<SupplierReturn> {
    Ok(SupplierReturn {
        id: sql::id(row, "id")?,
        number: row.opt_text("number")?,
        branch: sql::id(row, "branch_id")?,
        supplier: sql::id(row, "supplier_id")?,
        location: sql::id(row, "location_id")?,
        posted: row.text("status")? == "posted",
        tax_mode: tax_mode(row)?,
        tax_rate_bp: rate(row)?,
        credit: sql::opt_money(row, "credit")?,
        note: row.opt_text("note")?,
        user: sql::id(row, "user_id")?,
        posted_at: sql::opt_ts(row, "posted_at")?,
        lines: Vec::new(),
    })
}

fn map_return_line(row: &dyn Row) -> Result<SupplierReturnLine> {
    let receipt: Option<GoodsReceiptId> = sql::opt_id(row, "receipt_id")?;
    Ok(SupplierReturnLine {
        line_no: line_no(row, "line_no")?,
        variant: sql::id(row, "variant_id")?,
        unit: sql::id(row, "unit_id")?,
        factor: sql::qty(row, "factor")?,
        qty: sql::qty(row, "qty")?,
        price: sql::money(row, "price")?,
        receipt_line: match receipt {
            Some(r) => Some((r, line_no(row, "receipt_line_no")?)),
            None => None,
        },
        base_qty: sql::qty(row, "base_qty")?,
        value: sql::opt_money(row, "value")?,
    })
}

async fn return_lines(mut conn: Conn<'_>, mut r: SupplierReturn) -> Result<SupplierReturn> {
    r.lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, variant_id, unit_id, factor, qty, price, receipt_id, receipt_line_no, \
         base_qty, value FROM supplier_return_line WHERE return_id = ? ORDER BY line_no",
        args![r.id],
        map_return_line,
    )
    .await?;
    Ok(r)
}

/// One supplier return with its lines.
///
/// # Errors
/// The database error.
pub async fn get_supplier_return<'c>(
    conn: impl Into<Conn<'c>>,
    id: SupplierReturnId,
) -> Result<Option<SupplierReturn>> {
    let mut conn = conn.into();
    let sql = format!("{RETURN_SELECT} WHERE id = ?");
    match sql::fetch_optional(conn.reborrow(), &sql, args![id], map_return).await? {
        Some(r) => return_lines(conn, r).await.map(Some),
        None => Ok(None),
    }
}

/// The supplier returns of a branch and / or supplier, newest first (at most 500).
///
/// # Errors
/// The database error.
pub async fn list_supplier_returns<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
    supplier: Option<SupplierId>,
) -> Result<Vec<SupplierReturn>> {
    let mut conn = conn.into();
    let mut sql = format!("{RETURN_SELECT} WHERE 1 = 1");
    let mut a: Vec<Arg> = Vec::new();
    if let Some(b) = branch {
        sql.push_str(" AND branch_id = ?");
        a.push(b.into());
    }
    if let Some(s) = supplier {
        sql.push_str(" AND supplier_id = ?");
        a.push(s.into());
    }
    sql.push_str(" ORDER BY id DESC LIMIT 500");
    let heads = sql::fetch_all(conn.reborrow(), &sql, a, map_return).await?;
    let mut out = Vec::with_capacity(heads.len());
    for r in heads {
        out.push(return_lines(conn.reborrow(), r).await?);
    }
    Ok(out)
}

/// Writes a draft supplier return whole (`PUT /supplier-returns/{id}`).
///
/// # Errors
/// [`StockDbError::State`] for a posted return; [`StockDbError::NotFound`];
/// [`StockDbError::Invalid`] / [`StockDbError::Rule`].
#[allow(clippy::too_many_lines)] // one draft write: header, then its lines, checked first
pub async fn put_supplier_return(
    tx: &mut Tx,
    r: &NewSupplierReturn,
    now: Timestamp,
) -> StockResult<(Written, SupplierReturn)> {
    let before = get_supplier_return(Conn::Tx(&mut *tx), r.id).await?;
    if let Some(b) = &before {
        if b.posted {
            return Err(StockDbError::state("supplier return", "posted"));
        }
        if b.branch != r.branch {
            return Err(StockDbError::invalid("branch", "cannot change"));
        }
    }
    check_lines(r.lines.len())?;
    check_supplier_live(tx, r.supplier).await?;
    let location = doc_location(tx, r.branch, r.location).await?;
    let note = check_note(r.note.as_deref())?;
    let rate = match r.tax_rate_bp {
        Some(rate) if rate > 10_000 => {
            return Err(StockDbError::invalid("tax_rate", "must be at most 100%"));
        }
        Some(rate) => rate,
        None => {
            get_inventory_settings(Conn::Tx(&mut *tx))
                .await?
                .standard_ppn_rate_bp
        }
    };
    let mut lines = Vec::with_capacity(r.lines.len());
    for l in &r.lines {
        if !l.qty.is_positive() {
            return Err(StockDbError::invalid("qty", "must be above 0"));
        }
        if l.price.is_negative() {
            return Err(StockDbError::invalid("price", "must not be below 0"));
        }
        let info = variant_info(tx, l.variant, l.unit).await?;
        lines.push((l, info.factor, to_base(l.qty, info.factor)?));
    }
    let header: Vec<Arg> = args![
        r.branch,
        r.supplier,
        location,
        r.tax_mode.as_str(),
        i64::from(rate),
        note,
        r.user,
        now,
    ];
    if before.is_some() {
        let mut v = header;
        v.push(r.id.into());
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE supplier_return SET branch_id = ?, supplier_id = ?, location_id = ?, \
             tax_mode = ?, tax_rate_bp = ?, note = ?, user_id = ?, updated_at = ? WHERE id = ?",
            v,
        )
        .await?;
        sql::execute(
            Conn::Tx(&mut *tx),
            "DELETE FROM supplier_return_line WHERE return_id = ?",
            args![r.id],
        )
        .await?;
    } else {
        let mut v: Vec<Arg> = args![r.id];
        v.extend(header);
        v.push(now.into());
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO supplier_return (id, branch_id, supplier_id, location_id, tax_mode, \
             tax_rate_bp, note, user_id, updated_at, created_at, status) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'draft')",
            v,
        )
        .await?;
    }
    for (i, (l, factor, base)) in lines.iter().enumerate() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO supplier_return_line (return_id, line_no, variant_id, unit_id, factor, \
             qty, price, receipt_id, receipt_line_no, base_qty) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                r.id,
                i64::try_from(i + 1).unwrap_or(i64::MAX),
                l.variant,
                l.unit,
                *factor,
                l.qty,
                l.price,
                l.receipt_line.map(|x| x.0),
                l.receipt_line.map(|x| i64::from(x.1)),
                *base,
            ],
        )
        .await?;
    }
    let stored = get_supplier_return(Conn::Tx(&mut *tx), r.id)
        .await?
        .ok_or(DbError::NotFound("supplier return"))?;
    Ok((
        if before.is_some() {
            Written::Updated
        } else {
            Written::Created
        },
        stored,
    ))
}

/// The base quantity of a receipt line already returned by posted supplier returns.
async fn returned_so_far(tx: &mut Tx, receipt: GoodsReceiptId, line: u32) -> Result<Qty> {
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT CAST(COALESCE(SUM(l.base_qty), 0) AS SIGNED) AS qty \
         FROM supplier_return_line l JOIN supplier_return r ON r.id = l.return_id \
         WHERE r.status = 'posted' AND l.receipt_id = ? AND l.receipt_line_no = ?",
        args![receipt, i64::from(line)],
        |row| sql::qty(row, "qty"),
    )
    .await
    .map(Option::unwrap_or_default)
}

/// Posts a draft supplier return (`POST /supplier-returns/{id}/post`, `purchase.receive`, §6):
/// numbers it `-RS-`, moves each line out at the average (the average does not change; a line
/// taken from a receipt leaves from that line's lot, else FEFO) and stores the supplier credit
/// (Σ qty × price, PPN added when the tax mode is `added`; the mode and rate of the first
/// receipt the lines come from). A line taken from a receipt returns at most what the line
/// received minus earlier returns. A posted return is returned unchanged.
///
/// # Errors
/// [`StockDbError::NotFound`]; [`StockDbError::Invalid`] (`qty` above what is left,
/// `receipt_line` of another supplier, branch or variant, or not posted).
#[allow(clippy::too_many_lines)] // one posting, its steps in the order of spec §6
pub async fn post_supplier_return(
    tx: &mut Tx,
    id: SupplierReturnId,
    user: UserId,
    at: Timestamp,
) -> StockResult<(Written, SupplierReturn)> {
    let r = get_supplier_return(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(StockDbError::NotFound("supplier return"))?;
    if r.posted {
        return Ok((Written::Unchanged, r));
    }
    let mut mode = (r.tax_mode, r.tax_rate_bp);
    let mut first_receipt = true;
    let mut taking: BTreeMap<(Uuid, u32), Qty> = BTreeMap::new();
    let mut lots: Vec<Option<LotId>> = Vec::with_capacity(r.lines.len());
    for l in &r.lines {
        let Some((receipt, line)) = l.receipt_line else {
            lots.push(None);
            continue;
        };
        let gr = get_goods_receipt(Conn::Tx(&mut *tx), receipt)
            .await?
            .ok_or(StockDbError::NotFound("goods receipt"))?;
        if gr.status != ReceiptStatus::Posted || gr.supplier != r.supplier || gr.branch != r.branch
        {
            return Err(StockDbError::invalid(
                "receipt_line",
                "must be of a posted receipt of this supplier and branch",
            ));
        }
        let gl = gr
            .lines
            .iter()
            .find(|g| g.line_no == line)
            .ok_or(StockDbError::NotFound("goods receipt line"))?;
        if gl.variant != l.variant {
            return Err(StockDbError::invalid(
                "receipt_line",
                "is for another variant",
            ));
        }
        if first_receipt {
            mode = (gr.tax_mode, gr.tax_rate_bp);
            first_receipt = false;
        }
        let key = (*receipt.as_uuid(), line);
        let so_far = match taking.get(&key) {
            Some(q) => *q,
            None => returned_so_far(tx, receipt, line).await?,
        };
        let total = so_far
            .checked_add(l.base_qty)
            .ok_or(sultan_core::stock::StockError::Overflow)?;
        if total > gl.base_qty {
            return Err(StockDbError::invalid(
                "qty",
                "is more than the receipt line received minus earlier returns",
            ));
        }
        taking.insert(key, total);
        lots.push(gl.lot);
    }
    let credit = supplier_return_credit(
        &r.lines.iter().map(|l| (l.qty, l.price)).collect::<Vec<_>>(),
        mode.0,
        mode.1,
    )?;
    let n = allocate(tx, DocKind::SupplierReturn, r.branch, at).await?;
    for (l, lot) in r.lines.iter().zip(lots) {
        let qty = l
            .base_qty
            .checked_neg()
            .ok_or(sultan_core::stock::StockError::Overflow)?;
        let posted = post_moves(
            tx,
            &[Move {
                branch: r.branch,
                location: Some(r.location),
                variant: l.variant,
                kind: MovementKind::SupplierReturn,
                qty,
                valuation: Valuation::Average,
                lot: lot.map_or(LotRule::Fefo, LotRule::Lot),
                doc: DocRef {
                    kind: DocKind::SupplierReturn.as_str(),
                    id: *id.as_uuid(),
                    line: Some(l.line_no),
                },
                at,
            }],
        )
        .await?;
        sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE supplier_return_line SET value = ? WHERE return_id = ? AND line_no = ?",
            args![
                posted.first().and_then(|p| total_value(p)),
                id,
                i64::from(l.line_no)
            ],
        )
        .await?;
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE supplier_return SET status = 'posted', number = ?, local_day = ?, seq = ?, \
         tax_mode = ?, tax_rate_bp = ?, credit = ?, posted_at = ?, posted_by = ?, \
         updated_at = ? WHERE id = ? AND status = 'draft'",
        args![
            n.number,
            n.day,
            n.seq,
            mode.0.as_str(),
            i64::from(mode.1),
            credit,
            at,
            user,
            at,
            id,
        ],
    )
    .await?;
    Ok((
        Written::Updated,
        get_supplier_return(Conn::Tx(&mut *tx), id)
            .await?
            .ok_or(DbError::NotFound("supplier return"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys() {
        for s in [
            ReceiptStatus::Draft,
            ReceiptStatus::Posted,
            ReceiptStatus::Voided,
        ] {
            assert_eq!(ReceiptStatus::from_key(s.as_str()), Some(s));
        }
        for p in [PaymentKind::Credit, PaymentKind::Cash] {
            assert_eq!(PaymentKind::from_key(p.as_str()), Some(p));
        }
        assert_eq!(ReceiptStatus::from_key("x"), None);
        assert_eq!(PaymentKind::from_key("x"), None);
        assert!(clean_invoice(" ").is_err());
        assert!(clean_invoice(&"x".repeat(61)).is_err());
        assert_eq!(clean_invoice(" INV-1 ").unwrap(), "INV-1");
    }
}
