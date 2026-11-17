//! Posted sales and post-voids (spec 2.6 §1, §5, §7, §12).

use serde_json::{Value, json};
use sultan_core::catalog::{ComponentSnapshot, ProductType, Qty};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, SaleId, UnitId, UserId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::PriceListId;
use sultan_core::sale::{
    CartTotals, LineTotals, MAX_LINES, PaidSale, PriceMode, RefundMethod, ReturnedSoFar, SaleError,
    SaleStatus, SaleTender, ScaleLabel, StockLine, StockMovementKind, TenderKind, VoidPlan,
    VoidReason, void_movements,
};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::stock;
use super::{
    Change, CustomerId, NumberKind, Place, RefundTender, SaleDbError, SaleResult, Written,
    allocate, append_balance, clamp_limit, components, components_json, count, product_type,
    record, tender_kind,
};
use crate::TokenHash;
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The totals of a sale (spec 2.6 §6), as posted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SaleTotals {
    /// Σ gross.
    pub subtotal: Money,
    /// Σ line discounts.
    pub line_discount: Money,
    /// The cart discount (= Σ shares).
    pub cart_discount: Money,
    /// Σ tax.
    pub tax: Money,
    /// What the customer pays before cash rounding.
    pub total: Money,
}

impl From<&CartTotals> for SaleTotals {
    fn from(t: &CartTotals) -> Self {
        Self {
            subtotal: t.subtotal,
            line_discount: t.line_discount,
            cart_discount: t.cart_discount,
            tax: t.tax,
            total: t.total,
        }
    }
}

/// One line of a sale as posted (spec 2.6 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleLine {
    /// The variant sold.
    pub variant: VariantId,
    /// The unit sold in.
    pub unit: UnitId,
    /// The receipt name as sold (name + variant).
    pub name: String,
    /// The product type (stock, weighed, service, bundle).
    pub product_type: ProductType,
    /// The unit's factor to the base unit.
    pub factor: Qty,
    /// A bundle's component snapshot; empty otherwise.
    pub components: Vec<ComponentSnapshot>,
    /// The quantity in the line's unit (0 for a price label without a derived quantity).
    pub qty: Qty,
    /// The unit price charged.
    pub unit_price: Money,
    /// How the unit price was set (listed, open price, override with the price it replaced).
    pub price_mode: PriceMode,
    /// The price list that set a listed price.
    pub price_list: Option<PriceListId>,
    /// Who approved a price override (a manager override), if not the cashier.
    pub override_by: Option<UserId>,
    /// The scale label the line was scanned from.
    pub scale_label: Option<ScaleLabel>,
    /// The computed values (gross, discounts, amount, tax, total).
    pub totals: LineTotals,
}

impl SaleLine {
    /// The line as `core::sale` stock functions need it.
    #[must_use]
    pub fn stock_line(&self) -> StockLine<'_> {
        StockLine {
            variant: self.variant,
            product_type: self.product_type,
            qty: self.qty,
            factor: self.factor,
            components: &self.components,
        }
    }
}

/// A stored sale line: its number (from 1) and what posted returns took from it so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSaleLine {
    /// The line number, from 1.
    pub line_no: u32,
    /// The line.
    pub line: SaleLine,
    /// Returned quantity and refunded amount so far (spec 2.6 §8).
    pub returned: ReturnedSoFar,
}

/// A sale to post (`PUT /sales/{id}`): everything the server computed with `core`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSale {
    /// The client's `UUIDv7`.
    pub id: SaleId,
    /// A hash of the posted body, so a replay can be told from another body.
    pub request_hash: TokenHash,
    /// The branch.
    pub branch: BranchId,
    /// The register.
    pub register: RegisterId,
    /// The cashier's open session.
    pub cashier_session: CashierSessionId,
    /// The cashier.
    pub user: UserId,
    /// The attached customer.
    pub customer: Option<CustomerId>,
    /// Printed as a tax invoice (D13).
    pub tax_invoice: bool,
    /// The company's tax mode at posting (2.3 §7).
    pub tax_inclusive: bool,
    /// The totals.
    pub totals: SaleTotals,
    /// The lines, in order (1–300).
    pub lines: Vec<SaleLine>,
    /// Tenders, cash rounding and balance entries (`payment`, `change_saved`).
    pub payment: PaidSale,
    /// The `sale` stock movements (`core::sale::sale_movements`).
    pub movements: Vec<sultan_core::sale::StockMovement>,
    /// When it was posted.
    pub posted_at: Timestamp,
}

impl NewSale {
    fn place(&self) -> Place {
        Place {
            branch: self.branch,
            register: self.register,
            session: self.cashier_session,
            user: self.user,
        }
    }
}

/// A sale's header: everything but its lines and tenders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleHeader {
    /// The ID.
    pub id: SaleId,
    /// The number, e.g. `JKT01-R2-261116-0001`.
    pub number: String,
    /// The old-Sultan number of an imported sale (§14): such a sale is read-only.
    pub legacy_number: Option<String>,
    /// Posted or voided.
    pub status: SaleStatus,
    /// The branch.
    pub branch: BranchId,
    /// The register (`None` for an imported sale).
    pub register: Option<RegisterId>,
    /// The cashier session it was posted in.
    pub cashier_session: Option<CashierSessionId>,
    /// The cashier.
    pub user: Option<UserId>,
    /// The customer.
    pub customer: Option<CustomerId>,
    /// Printed as a tax invoice.
    pub tax_invoice: bool,
    /// Tax-inclusive prices.
    pub tax_inclusive: bool,
    /// The totals.
    pub totals: SaleTotals,
    /// The cash rounding (+/−): what was paid is `total + cash_rounding`.
    pub cash_rounding: Money,
    /// Σ change handed back.
    pub change_given: Money,
    /// Σ change saved to the customer balance.
    pub change_saved: Money,
    /// The number of lines.
    pub line_count: u32,
    /// The number of posted returns of this sale.
    pub return_count: u32,
    /// When it was posted.
    pub posted_at: Timestamp,
    /// When it was voided.
    pub voided_at: Option<Timestamp>,
}

impl SaleHeader {
    /// What the customer paid: `total + cash_rounding`.
    #[must_use]
    pub fn paid(&self) -> Money {
        self.totals.total.saturating_add(self.cash_rounding)
    }
}

/// How a sale was post-voided (spec 2.6 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleVoid {
    /// The branch where it was voided.
    pub branch: BranchId,
    /// The register that paid back.
    pub register: RegisterId,
    /// The (current) session whose drawer paid back (D10).
    pub cashier_session: CashierSessionId,
    /// Who voided it.
    pub user: UserId,
    /// Why.
    pub reason: VoidReason,
    /// Everything the customer paid: Σ tender amounts + saved change.
    pub refund: Money,
    /// Saved change taken back from the balance.
    pub change_saved_reversed: Money,
    /// The part paid with the balance, back to the balance.
    pub payment_reversed: Money,
    /// Refunded to the balance (method balance).
    pub to_balance: Money,
    /// Cash paid out of the drawer (after cash rounding).
    pub cash: Money,
    /// The cash rounding of the cash part (+/−).
    pub cash_rounding: Money,
    /// How it was paid back.
    pub tenders: Vec<RefundTender>,
    /// When.
    pub voided_at: Timestamp,
}

/// A stored sale with its lines, tenders and post-void.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sale {
    /// The header.
    pub header: SaleHeader,
    /// The lines, in order.
    pub lines: Vec<StoredSaleLine>,
    /// The tenders, in order.
    pub tenders: Vec<SaleTender>,
    /// The post-void, if voided.
    pub void: Option<SaleVoid>,
}

/// A post-void to store (`POST /sales/{id}/void`): the plan from `core::sale::plan_post_void`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSaleVoid {
    /// The sale.
    pub sale: SaleId,
    /// The branch.
    pub branch: BranchId,
    /// The register paying back.
    pub register: RegisterId,
    /// The voiding user's open session on it (D10).
    pub cashier_session: CashierSessionId,
    /// The voiding user.
    pub user: UserId,
    /// Why.
    pub reason: VoidReason,
    /// Cash or balance for the rest (D11).
    pub method: RefundMethod,
    /// The plan: balance entries, cash and rounding.
    pub plan: VoidPlan,
    /// How it is paid back: one cash tender for `plan.refund.cash` and one balance tender for
    /// `payment_reversed + to_balance`, each only when not 0.
    pub tenders: Vec<RefundTender>,
    /// When.
    pub voided_at: Timestamp,
}

const HEADER_SELECT: &str = "SELECT id, number, legacy_number, status, branch_id, register_id, \
     cashier_session_id, user_id, customer_id, tax_invoice, tax_inclusive, subtotal, \
     line_discount, cart_discount, tax, total, cash_rounding, change_given, change_saved, \
     line_count, return_count, posted_at, voided_at FROM sale";

fn status(row: &dyn Row) -> Result<SaleStatus> {
    match row.text("status")?.as_str() {
        "posted" => Ok(SaleStatus::Posted),
        "voided" => Ok(SaleStatus::Voided),
        other => Err(DbError::Corrupt(format!("unknown sale status `{other}`"))),
    }
}

fn map_header(row: &dyn Row) -> Result<SaleHeader> {
    Ok(SaleHeader {
        id: sql::id(row, "id")?,
        number: row.text("number")?,
        legacy_number: row.opt_text("legacy_number")?,
        status: status(row)?,
        branch: sql::id(row, "branch_id")?,
        register: sql::opt_id(row, "register_id")?,
        cashier_session: sql::opt_id(row, "cashier_session_id")?,
        user: sql::opt_id(row, "user_id")?,
        customer: sql::opt_id(row, "customer_id")?,
        tax_invoice: row.bool("tax_invoice")?,
        tax_inclusive: row.bool("tax_inclusive")?,
        totals: SaleTotals {
            subtotal: sql::money(row, "subtotal")?,
            line_discount: sql::money(row, "line_discount")?,
            cart_discount: sql::money(row, "cart_discount")?,
            tax: sql::money(row, "tax")?,
            total: sql::money(row, "total")?,
        },
        cash_rounding: sql::money(row, "cash_rounding")?,
        change_given: sql::money(row, "change_given")?,
        change_saved: sql::money(row, "change_saved")?,
        line_count: count(row, "line_count")?,
        return_count: count(row, "return_count")?,
        posted_at: sql::ts(row, "posted_at")?,
        voided_at: sql::opt_ts(row, "voided_at")?,
    })
}

fn price_mode_parts(mode: PriceMode) -> (&'static str, Option<Money>) {
    match mode {
        PriceMode::Listed => ("listed", None),
        PriceMode::OpenPrice => ("open_price", None),
        PriceMode::Overridden { resolved } => ("overridden", resolved),
    }
}

fn map_line(row: &dyn Row) -> Result<StoredSaleLine> {
    let price_mode = match row.text("price_mode")?.as_str() {
        "listed" => PriceMode::Listed,
        "open_price" => PriceMode::OpenPrice,
        "overridden" => PriceMode::Overridden {
            resolved: row.opt_i64("resolved_price")?.map(Money::from_minor),
        },
        other => return Err(DbError::Corrupt(format!("unknown price mode `{other}`"))),
    };
    let scale_label = match row.opt_text("scale_label")?.as_deref() {
        None => None,
        Some("weight") => Some(ScaleLabel::Weight),
        Some("price") => Some(ScaleLabel::Price {
            amount: sql::money(row, "label_amount")?,
        }),
        Some(other) => return Err(DbError::Corrupt(format!("unknown scale label `{other}`"))),
    };
    let tax_rate = row.i64("tax_rate_bp")?;
    Ok(StoredSaleLine {
        line_no: count(row, "line_no")?,
        line: SaleLine {
            variant: sql::id(row, "variant_id")?,
            unit: sql::id(row, "unit_id")?,
            name: row.text("name")?,
            product_type: product_type(row, "product_type")?,
            factor: sql::qty(row, "factor")?,
            components: components(row, "components")?,
            qty: sql::qty(row, "qty")?,
            unit_price: sql::money(row, "unit_price")?,
            price_mode,
            price_list: sql::opt_id::<Uuid>(row, "price_list_id")?.map(PriceListId::from_uuid),
            override_by: sql::opt_id(row, "override_by")?,
            scale_label,
            totals: LineTotals {
                gross: sql::money(row, "gross")?,
                line_discount: sql::money(row, "line_discount")?,
                after_line: sql::money(row, "after_line")?,
                cart_discount_share: sql::money(row, "cart_discount_share")?,
                amount: sql::money(row, "amount")?,
                tax_rate_bp: u16::try_from(tax_rate)
                    .map_err(|_| DbError::Corrupt(format!("tax rate {tax_rate} out of range")))?,
                tax: sql::money(row, "tax")?,
                net: sql::money(row, "net")?,
                total: sql::money(row, "total")?,
            },
        },
        returned: ReturnedSoFar {
            qty: sql::qty(row, "returned_qty")?,
            refunded: sql::money(row, "refunded")?,
        },
    })
}

fn map_tender(row: &dyn Row) -> Result<SaleTender> {
    Ok(SaleTender {
        tender_type: sql::opt_id(row, "tender_type_id")?,
        kind: tender_kind(row, "kind")?,
        amount: sql::money(row, "amount")?,
        cash_received: row.opt_i64("cash_received")?.map(Money::from_minor),
        change_given: sql::money(row, "change_given")?,
        change_saved: sql::money(row, "change_saved")?,
        reference: row.opt_text("reference")?,
    })
}

pub(super) fn map_refund_tender(row: &dyn Row) -> Result<RefundTender> {
    Ok(RefundTender {
        tender_type: sql::opt_id(row, "tender_type_id")?,
        kind: tender_kind(row, "kind")?,
        amount: sql::money(row, "amount")?,
    })
}

/// The header of sale `id`.
pub(super) async fn header(conn: Conn<'_>, id: SaleId) -> Result<Option<SaleHeader>> {
    let sql = format!("{HEADER_SELECT} WHERE id = ?");
    sql::fetch_optional(conn, &sql, args![id], map_header).await
}

/// The lines of sale `id`, in order.
pub(super) async fn lines(conn: Conn<'_>, id: SaleId) -> Result<Vec<StoredSaleLine>> {
    sql::fetch_all(
        conn,
        "SELECT line_no, variant_id, unit_id, name, product_type, factor, components, qty, \
         unit_price, price_mode, resolved_price, price_list_id, override_by, scale_label, \
         label_amount, gross, line_discount, after_line, cart_discount_share, amount, \
         tax_rate_bp, tax, net, total, returned_qty, refunded FROM sale_line WHERE sale_id = ? \
         ORDER BY line_no",
        args![id],
        map_line,
    )
    .await
}

async fn load(mut conn: Conn<'_>, header: SaleHeader) -> Result<Sale> {
    let id = header.id;
    let lines = lines(conn.reborrow(), id).await?;
    let tenders = sql::fetch_all(
        conn.reborrow(),
        "SELECT tender_type_id, kind, amount, cash_received, change_given, change_saved, \
         reference FROM sale_tender WHERE sale_id = ? ORDER BY tender_no",
        args![id],
        map_tender,
    )
    .await?;
    let void = if header.status == SaleStatus::Voided {
        let tenders = sql::fetch_all(
            conn.reborrow(),
            "SELECT tender_type_id, kind, amount FROM sale_void_tender WHERE sale_id = ? \
             ORDER BY tender_no",
            args![id],
            map_refund_tender,
        )
        .await?;
        sql::fetch_optional(
            conn,
            "SELECT branch_id, register_id, cashier_session_id, user_id, reason, reason_text, \
             refund, change_saved_reversed, payment_reversed, to_balance, cash, cash_rounding, \
             voided_at FROM sale_void WHERE sale_id = ?",
            args![id],
            |row| {
                let key = row.text("reason")?;
                let text = row.opt_text("reason_text")?;
                Ok(SaleVoid {
                    branch: sql::id(row, "branch_id")?,
                    register: sql::id(row, "register_id")?,
                    cashier_session: sql::id(row, "cashier_session_id")?,
                    user: sql::id(row, "user_id")?,
                    reason: VoidReason::parse(&key, text.as_deref())
                        .map_err(|e| DbError::Corrupt(format!("void reason: {e}")))?,
                    refund: sql::money(row, "refund")?,
                    change_saved_reversed: sql::money(row, "change_saved_reversed")?,
                    payment_reversed: sql::money(row, "payment_reversed")?,
                    to_balance: sql::money(row, "to_balance")?,
                    cash: sql::money(row, "cash")?,
                    cash_rounding: sql::money(row, "cash_rounding")?,
                    tenders: tenders.clone(),
                    voided_at: sql::ts(row, "voided_at")?,
                })
            },
        )
        .await?
    } else {
        None
    };
    Ok(Sale {
        header,
        lines,
        tenders,
        void,
    })
}

/// One sale with its lines, tenders and post-void.
///
/// # Errors
/// The database error.
pub async fn get_sale<'c>(conn: impl Into<Conn<'c>>, id: SaleId) -> Result<Option<Sale>> {
    let mut conn = conn.into();
    match header(conn.reborrow(), id).await? {
        Some(h) => load(conn, h).await.map(Some),
        None => Ok(None),
    }
}

/// The sale with this number (`GET /sales/by-number/{number}`, the receipt lookup of a return),
/// or with this old-Sultan number.
///
/// # Errors
/// The database error.
pub async fn get_sale_by_number<'c>(
    conn: impl Into<Conn<'c>>,
    number: &str,
) -> Result<Option<Sale>> {
    let mut conn = conn.into();
    let sql = format!("{HEADER_SELECT} WHERE number = ? OR legacy_number = ? ORDER BY id LIMIT 1");
    match sql::fetch_optional(conn.reborrow(), &sql, args![number, number], map_header).await? {
        Some(h) => load(conn, h).await.map(Some),
        None => Ok(None),
    }
}

/// Filters for [`list_sales`] and [`super::list_returns`] (spec 2.6 §12). All set fields must
/// match; newest first by ID (keyset paging: `before` = the last ID of the previous page).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocFilter {
    /// Only this branch.
    pub branch: Option<BranchId>,
    /// Only this register.
    pub register: Option<RegisterId>,
    /// Only this cashier session.
    pub cashier_session: Option<CashierSessionId>,
    /// Only this customer.
    pub customer: Option<CustomerId>,
    /// Posted at or after.
    pub from: Option<Timestamp>,
    /// Posted before.
    pub until: Option<Timestamp>,
    /// Exactly this number (for sales also the old-Sultan number).
    pub number: Option<String>,
    /// Page cursor.
    pub before: Option<Uuid>,
    /// Page size; `0` = 50, at most [`MAX_DOC_PAGE`].
    pub limit: u32,
}

/// The largest page of [`list_sales`] and [`super::list_returns`].
pub const MAX_DOC_PAGE: u32 = 200;

/// `WHERE` conditions and values of `filter`; `legacy` adds the old-Sultan number.
pub(super) fn filter_sql(filter: &DocFilter, legacy: bool) -> (String, Vec<Arg>) {
    let mut conditions: Vec<&str> = Vec::new();
    let mut values: Vec<Arg> = Vec::new();
    let mut add = |condition: &'static str, value: Arg| {
        conditions.push(condition);
        values.push(value);
    };
    if let Some(b) = filter.branch {
        add("branch_id = ?", b.into());
    }
    if let Some(r) = filter.register {
        add("register_id = ?", r.into());
    }
    if let Some(s) = filter.cashier_session {
        add("cashier_session_id = ?", s.into());
    }
    if let Some(c) = filter.customer {
        add("customer_id = ?", c.into());
    }
    if let Some(from) = filter.from {
        add("posted_at >= ?", from.into());
    }
    if let Some(until) = filter.until {
        add("posted_at < ?", until.into());
    }
    if let Some(before) = filter.before {
        add("id < ?", before.into());
    }
    if let Some(number) = &filter.number {
        if legacy {
            conditions.push("(number = ? OR legacy_number = ?)");
            values.push(number.clone().into());
            values.push(number.clone().into());
        } else {
            conditions.push("number = ?");
            values.push(number.clone().into());
        }
    }
    let clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    values.push(clamp_limit(filter.limit, 50, MAX_DOC_PAGE).into());
    (clause, values)
}

/// Sale headers matching `filter`, newest first (`GET /sales?…`).
///
/// # Errors
/// The database error.
pub async fn list_sales<'c>(
    conn: impl Into<Conn<'c>>,
    filter: &DocFilter,
) -> Result<Vec<SaleHeader>> {
    let (clause, values) = filter_sql(filter, true);
    let sql = format!("{HEADER_SELECT}{clause} ORDER BY id DESC LIMIT ?");
    sql::fetch_all(conn.into(), &sql, values, map_header).await
}

/// Sale headers matching `condition` (a `WHERE` clause on `sale`), newest posted first; the last
/// value is the limit.
pub(super) async fn list_recent(
    conn: Conn<'_>,
    condition: &str,
    values: Vec<Arg>,
) -> Result<Vec<SaleHeader>> {
    let sql = format!("{HEADER_SELECT} WHERE {condition} ORDER BY posted_at DESC, id DESC LIMIT ?");
    sql::fetch_all(conn, &sql, values, map_header).await
}

fn line_json(no: usize, l: &SaleLine) -> Value {
    let (mode, resolved) = price_mode_parts(l.price_mode);
    json!({
        "line_no": no,
        "variant_id": l.variant.to_string(),
        "unit_id": l.unit.to_string(),
        "name": l.name,
        "product_type": l.product_type.as_str(),
        "qty": l.qty.thousandths(),
        "factor": l.factor.thousandths(),
        "unit_price": l.unit_price.minor(),
        "price_mode": mode,
        "resolved_price": resolved.map(Money::minor),
        "price_list_id": l.price_list.map(|p| p.to_string()),
        "override_by": l.override_by.map(|u| u.to_string()),
        "gross": l.totals.gross.minor(),
        "line_discount": l.totals.line_discount.minor(),
        "cart_discount_share": l.totals.cart_discount_share.minor(),
        "amount": l.totals.amount.minor(),
        "tax_rate_bp": l.totals.tax_rate_bp,
        "tax": l.totals.tax.minor(),
        "total": l.totals.total.minor(),
    })
}

fn tender_json(t: &SaleTender) -> Value {
    json!({
        "tender_type_id": t.tender_type.map(|t| t.to_string()),
        "kind": t.kind.as_str(),
        "amount": t.amount.minor(),
        "cash_received": t.cash_received.map(Money::minor),
        "change_given": t.change_given.minor(),
        "change_saved": t.change_saved.minor(),
        "reference": t.reference,
    })
}

pub(super) fn refund_tender_json(t: &RefundTender) -> Value {
    json!({
        "tender_type_id": t.tender_type.map(|t| t.to_string()),
        "kind": t.kind.as_str(),
        "amount": t.amount.minor(),
    })
}

pub(super) fn movements_json(movements: &[sultan_core::sale::StockMovement]) -> Value {
    Value::Array(
        movements
            .iter()
            .map(|m| {
                json!({
                    "variant_id": m.variant.to_string(),
                    "kind": m.kind.as_str(),
                    "base_qty": m.base_qty.thousandths(),
                })
            })
            .collect(),
    )
}

fn sale_json(sale: &NewSale, number: &str, location: Option<Uuid>) -> Value {
    json!({
        "id": sale.id.to_string(),
        "number": number,
        "branch_id": sale.branch.to_string(),
        "register_id": sale.register.to_string(),
        "cashier_session_id": sale.cashier_session.to_string(),
        "user_id": sale.user.to_string(),
        "customer_id": sale.customer.map(|c| c.to_string()),
        "tax_invoice": sale.tax_invoice,
        "tax_inclusive": sale.tax_inclusive,
        "subtotal": sale.totals.subtotal.minor(),
        "line_discount": sale.totals.line_discount.minor(),
        "cart_discount": sale.totals.cart_discount.minor(),
        "tax": sale.totals.tax.minor(),
        "total": sale.totals.total.minor(),
        "cash_rounding": sale.payment.cash_rounding.minor(),
        "posted_at": sale.posted_at.unix_millis(),
        "lines": sale.lines.iter().enumerate().map(|(i, l)| line_json(i + 1, l)).collect::<Vec<_>>(),
        "tenders": sale.payment.tenders.iter().map(tender_json).collect::<Vec<_>>(),
        "location_id": location.map(|l| l.to_string()),
        "movements": movements_json(&sale.movements),
    })
}

fn sum(values: impl IntoIterator<Item = Money>) -> SaleResult<Money> {
    values.into_iter().try_fold(Money::ZERO, |acc, v| {
        acc.checked_add(v)
            .ok_or(SaleDbError::Rule(SaleError::AmountOverflow))
    })
}

/// Checks what the database relies on: the line count, the sums, the tender kinds and the
/// movement kinds. `exchange` allows exchange tenders (a sale posted by an exchange, D12).
fn check_sale(sale: &NewSale, exchange: bool) -> SaleResult<()> {
    if sale.lines.is_empty() || sale.lines.len() > MAX_LINES {
        return Err(SaleDbError::invalid(
            "lines",
            format!("a sale has 1 to {MAX_LINES} lines"),
        ));
    }
    if sum(sale.lines.iter().map(|l| l.totals.total))? != sale.totals.total {
        return Err(SaleDbError::invalid(
            "total",
            "is not the sum of the line totals",
        ));
    }
    let paid = sum(sale.payment.tenders.iter().map(|t| t.amount))?;
    let due = sale
        .totals
        .total
        .checked_add(sale.payment.cash_rounding)
        .ok_or(SaleDbError::Rule(SaleError::AmountOverflow))?;
    if paid != due {
        return Err(SaleDbError::invalid(
            "tenders",
            format!("pay {paid}, not total + cash rounding {due}"),
        ));
    }
    for t in &sale.payment.tenders {
        if (t.kind == TenderKind::Exchange) != t.tender_type.is_none() {
            return Err(SaleDbError::invalid(
                "tenders",
                "only an exchange tender has no tender type",
            ));
        }
        if t.kind == TenderKind::Exchange && !exchange {
            return Err(SaleDbError::invalid(
                "tenders",
                "an exchange tender is posted with its return",
            ));
        }
    }
    if sale
        .movements
        .iter()
        .any(|m| m.kind != StockMovementKind::Sale)
    {
        return Err(SaleDbError::invalid("movements", "a sale moves stock out"));
    }
    Ok(())
}

/// Posts a sale in `tx` (`PUT /sales/{id}`, spec 2.6 §5, §12): allocates its number, stores the
/// sale, its lines and tenders, appends its stock movements and customer balance entries (with
/// the cached balance), marks its variants used, and appends the audit row `sale.post` and the
/// outbox event `sale.posted`. Replaying the same id with the same `request_hash` writes nothing
/// and returns the stored sale ([`Written::Unchanged`]).
///
/// Prices, permissions, tender rules and balance limits are the caller's job (`core::sale`).
///
/// # Errors
/// [`SaleDbError::Conflict`] when the id exists with another body; [`SaleDbError::Invalid`] for
/// sums that do not add up, an empty or oversized sale, a register of another branch or balance
/// entries without a customer; [`SaleDbError::NotFound`] (`register`, `customer`); a retryable
/// [`DbError::Conflict`] when another transaction took the number or the id.
pub async fn post_sale(tx: &mut Tx, sale: &NewSale, now: Timestamp) -> SaleResult<(Written, Sale)> {
    post(tx, sale, false, now).await
}

pub(super) async fn post(
    tx: &mut Tx,
    sale: &NewSale,
    exchange: bool,
    now: Timestamp,
) -> SaleResult<(Written, Sale)> {
    if let Some(stored) = replay(tx, sale.id, &sale.request_hash).await? {
        return Ok((Written::Unchanged, stored));
    }
    check_sale(sale, exchange)?;
    let place = sale.place();
    let number = allocate(
        tx,
        NumberKind::Sale,
        sale.branch,
        sale.register,
        sale.posted_at,
    )
    .await?;
    insert_sale(tx, sale, &number, now).await?;
    let location = stock::register_location(tx, sale.register).await?;
    stock::insert(
        tx,
        sale.branch,
        location,
        "sale",
        *sale.id.as_uuid(),
        &sale.movements,
        sale.posted_at,
    )
    .await?;
    append_balance(
        tx,
        sale.customer,
        &sale.payment.balance_entries,
        place,
        *sale.id.as_uuid(),
        sale.posted_at,
        now,
    )
    .await?;
    let variants: Vec<VariantId> = sale
        .lines
        .iter()
        .map(|l| l.variant)
        .chain(
            sale.lines
                .iter()
                .flat_map(|l| l.components.iter().map(|c| c.variant)),
        )
        .collect();
    crate::catalog::mark_used(Conn::Tx(&mut *tx), &variants, now).await?;
    let payload = sale_json(sale, &number.number, location.map(|l| *l.as_uuid()));
    record(
        tx,
        &place.actor(),
        now,
        Change {
            action: "sale.post",
            target: format!("sale:{}", sale.id),
            before: None,
            after: Some(json!({
                "number": number.number,
                "total": sale.totals.total.minor(),
                "cash_rounding": sale.payment.cash_rounding.minor(),
            })),
            outbox: Some(("sale.posted", &payload)),
        },
    )
    .await?;
    let stored = get_sale(Conn::Tx(&mut *tx), sale.id)
        .await?
        .ok_or(SaleDbError::NotFound("sale"))?;
    Ok((Written::Created, stored))
}

/// Inserts the sale row, its lines and its tenders.
async fn insert_sale(
    tx: &mut Tx,
    sale: &NewSale,
    number: &super::Allocated,
    now: Timestamp,
) -> SaleResult<()> {
    let change_given = sum(sale.payment.tenders.iter().map(|t| t.change_given))?;
    let change_saved = sum(sale.payment.tenders.iter().map(|t| t.change_saved))?;
    let line_count = i64::try_from(sale.lines.len()).unwrap_or(i64::MAX);
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO sale (id, number, legacy_number, status, branch_id, register_id, \
         cashier_session_id, user_id, customer_id, local_day, seq, tax_invoice, tax_inclusive, \
         subtotal, line_discount, cart_discount, tax, total, cash_rounding, change_given, \
         change_saved, line_count, return_count, posted_at, voided_at, request_hash, created_at) \
         VALUES (?, ?, NULL, 'posted', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, \
         ?, NULL, ?, ?)",
        args![
            sale.id,
            number.number.as_str(),
            sale.branch,
            sale.register,
            sale.cashier_session,
            sale.user,
            sale.customer,
            number.day,
            number.seq,
            sale.tax_invoice,
            sale.tax_inclusive,
            sale.totals.subtotal,
            sale.totals.line_discount,
            sale.totals.cart_discount,
            sale.totals.tax,
            sale.totals.total,
            sale.payment.cash_rounding,
            change_given,
            change_saved,
            line_count,
            sale.posted_at,
            &sale.request_hash,
            now,
        ],
    )
    .await?;
    for (i, line) in sale.lines.iter().enumerate() {
        insert_line(tx, sale.id, i + 1, line).await?;
    }
    for (i, t) in sale.payment.tenders.iter().enumerate() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO sale_tender (sale_id, tender_no, tender_type_id, kind, amount, \
             cash_received, change_given, change_saved, reference) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                sale.id,
                i64::try_from(i + 1).unwrap_or(i64::MAX),
                t.tender_type,
                t.kind.as_str(),
                t.amount,
                t.cash_received.map(Money::minor),
                t.change_given,
                t.change_saved,
                t.reference.as_deref(),
            ],
        )
        .await?;
    }
    Ok(())
}

/// The stored sale when `id` exists with the same request hash; [`SaleDbError::Conflict`] when it
/// exists with another one; `None` when it does not exist.
async fn replay(tx: &mut Tx, id: SaleId, hash: &TokenHash) -> SaleResult<Option<Sale>> {
    let stored = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT request_hash FROM sale WHERE id = ?",
        args![id],
        |row| row.opt_bytes("request_hash"),
    )
    .await?;
    match stored {
        None => Ok(None),
        Some(Some(h)) if h.as_slice() == hash.as_slice() => {
            Ok(get_sale(Conn::Tx(&mut *tx), id).await?)
        }
        Some(_) => Err(SaleDbError::Conflict("sale", *id.as_uuid())),
    }
}

async fn insert_line(tx: &mut Tx, sale: SaleId, no: usize, l: &SaleLine) -> Result<()> {
    let (mode, resolved) = price_mode_parts(l.price_mode);
    let (label, label_amount) = match l.scale_label {
        None => (None, None),
        Some(ScaleLabel::Weight) => (Some("weight"), None),
        Some(ScaleLabel::Price { amount }) => (Some("price"), Some(amount.minor())),
    };
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO sale_line (sale_id, line_no, variant_id, unit_id, name, product_type, \
         factor, components, qty, unit_price, price_mode, resolved_price, price_list_id, \
         override_by, scale_label, label_amount, gross, line_discount, after_line, \
         cart_discount_share, amount, tax_rate_bp, tax, net, total, returned_qty, refunded) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0)",
        args![
            sale,
            i64::try_from(no).unwrap_or(i64::MAX),
            l.variant,
            l.unit,
            l.name.as_str(),
            l.product_type.as_str(),
            l.factor,
            components_json(&l.components),
            l.qty,
            l.unit_price,
            mode,
            resolved.map(Money::minor),
            l.price_list,
            l.override_by,
            label,
            label_amount,
            l.totals.gross,
            l.totals.line_discount,
            l.totals.after_line,
            l.totals.cart_discount_share,
            l.totals.amount,
            i64::from(l.totals.tax_rate_bp),
            l.totals.tax,
            l.totals.net,
            l.totals.total,
        ],
    )
    .await?;
    Ok(())
}

/// Checks a void's refund tenders against its plan: cash = `plan.refund.cash`, balance =
/// `payment_reversed + to_balance`, and the plan refunds what the customer paid.
fn check_void(v: &NewSaleVoid, paid_and_saved: Money) -> SaleResult<()> {
    let refund = &v.plan.refund;
    let planned = sum([
        refund.payment_reversed,
        refund.to_balance,
        refund.cash_exact,
    ])?;
    if planned != paid_and_saved {
        return Err(SaleDbError::invalid(
            "plan",
            format!("refunds {planned}, but the customer paid {paid_and_saved}"),
        ));
    }
    let of = |kind| {
        sum(v
            .tenders
            .iter()
            .filter(|t| t.kind == kind)
            .map(|t| t.amount))
    };
    let to_balance = sum([refund.payment_reversed, refund.to_balance])?;
    if v.tenders
        .iter()
        .any(|t| !t.kind.refundable() || t.tender_type.is_none() || t.amount <= Money::ZERO)
    {
        return Err(SaleDbError::invalid(
            "tenders",
            "a post-void pays back in cash or to the balance, with a tender type and an amount",
        ));
    }
    if of(TenderKind::Cash)? != refund.cash || of(TenderKind::CustomerBalance)? != to_balance {
        return Err(SaleDbError::invalid("tenders", "do not match the plan"));
    }
    Ok(())
}

/// Inserts the `sale_void` row and its refund tenders.
async fn insert_void(
    tx: &mut Tx,
    v: &NewSaleVoid,
    paid_and_saved: Money,
    text: Option<&str>,
) -> Result<()> {
    let refund = &v.plan.refund;
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO sale_void (sale_id, branch_id, register_id, cashier_session_id, user_id, \
         reason, reason_text, refund, change_saved_reversed, payment_reversed, to_balance, cash, \
         cash_rounding, voided_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            v.sale,
            v.branch,
            v.register,
            v.cashier_session,
            v.user,
            v.reason.as_str(),
            text,
            paid_and_saved,
            v.plan.change_saved_reversed,
            refund.payment_reversed,
            refund.to_balance,
            refund.cash,
            refund.cash_rounding,
            v.voided_at,
        ],
    )
    .await?;
    insert_refund_tenders(
        tx,
        "sale_void_tender",
        "sale_id",
        *v.sale.as_uuid(),
        &v.tenders,
    )
    .await
}

/// Inserts refund tenders numbered from 1 into `table` (keyed by `key` = `id`).
pub(super) async fn insert_refund_tenders(
    tx: &mut Tx,
    table: &str,
    key: &str,
    id: Uuid,
    tenders: &[RefundTender],
) -> Result<()> {
    for (i, t) in tenders.iter().enumerate() {
        sql::execute(
            Conn::Tx(&mut *tx),
            &format!(
                "INSERT INTO {table} ({key}, tender_no, tender_type_id, kind, amount) \
                 VALUES (?, ?, ?, ?, ?)"
            ),
            args![
                id,
                i64::try_from(i + 1).unwrap_or(i64::MAX),
                t.tender_type,
                t.kind.as_str(),
                t.amount,
            ],
        )
        .await?;
    }
    Ok(())
}

/// Appends the `sale_void` movements that put the sale's `sale` movements back where they were
/// taken from.
async fn put_back_stock(tx: &mut Tx, v: &NewSaleVoid) -> SaleResult<()> {
    let original = stock::document_movements(Conn::Tx(&mut *tx), *v.sale.as_uuid()).await?;
    for m in original
        .iter()
        .filter(|m| m.movement.kind == StockMovementKind::Sale)
    {
        let back = void_movements(&[m.movement])?;
        stock::insert(
            tx,
            m.branch,
            m.location,
            "sale",
            *v.sale.as_uuid(),
            &back,
            v.voided_at,
        )
        .await?;
    }
    Ok(())
}

/// The outbox payload of a post-void.
fn void_json(v: &NewSaleVoid, number: &str, paid_and_saved: Money, text: Option<&str>) -> Value {
    let refund = &v.plan.refund;
    json!({
        "sale_id": v.sale.to_string(),
        "number": number,
        "branch_id": v.branch.to_string(),
        "register_id": v.register.to_string(),
        "cashier_session_id": v.cashier_session.to_string(),
        "user_id": v.user.to_string(),
        "reason": v.reason.as_str(),
        "reason_text": text,
        "refund": paid_and_saved.minor(),
        "change_saved_reversed": v.plan.change_saved_reversed.minor(),
        "payment_reversed": refund.payment_reversed.minor(),
        "to_balance": refund.to_balance.minor(),
        "cash": refund.cash.minor(),
        "cash_rounding": refund.cash_rounding.minor(),
        "tenders": v.tenders.iter().map(refund_tender_json).collect::<Vec<_>>(),
        "voided_at": v.voided_at.unix_millis(),
    })
}

/// Post-voids a sale in `tx` (`POST /sales/{id}/void`, spec 2.6 §7, D10, D11): status `voided`,
/// the `sale_void` row with its refund tenders, `sale_void` stock movements that put back the
/// sale's `sale` movements (same branch and location), the plan's balance entries
/// (`change_saved_reversed`, `payment_reversed`, `refund`) for the sale's customer, the audit row
/// `sale.void` and the outbox event `sale.voided`. Replays are the server's job
/// (`Idempotency-Key`).
///
/// # Errors
/// [`SaleDbError::NotFound`] (`sale`); [`SaleDbError::Rule`]: `conflict` (already voided),
/// `has_returns`, `invalid_field` (an imported sale); [`SaleDbError::Invalid`] when the plan or
/// its tenders do not match the sale; a retryable [`DbError::Conflict`] when a return or another
/// void won a race.
pub async fn void_sale(tx: &mut Tx, v: &NewSaleVoid, now: Timestamp) -> SaleResult<Sale> {
    let h = header(Conn::Tx(&mut *tx), v.sale)
        .await?
        .ok_or(SaleDbError::NotFound("sale"))?;
    if h.status == SaleStatus::Voided {
        return Err(SaleError::AlreadyVoided.into());
    }
    if h.legacy_number.is_some() {
        return Err(SaleError::InvalidField {
            field: "sale",
            reason: "imported sales are read-only",
        }
        .into());
    }
    if h.return_count > 0 {
        return Err(SaleError::HasReturns.into());
    }
    let stored = load(Conn::Tx(&mut *tx), h.clone()).await?;
    let paid_and_saved = sum(stored
        .tenders
        .iter()
        .flat_map(|t| [t.amount, t.change_saved]))?;
    check_void(v, paid_and_saved)?;
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE sale SET status = 'voided', voided_at = ? \
         WHERE id = ? AND status = 'posted' AND return_count = 0",
        args![v.voided_at, v.sale],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::Conflict("the sale changed while voiding it".into()).into());
    }
    let text = match &v.reason {
        VoidReason::Other(t) => Some(t.as_str()),
        _ => None,
    };
    insert_void(tx, v, paid_and_saved, text).await?;
    put_back_stock(tx, v).await?;
    let place = Place {
        branch: v.branch,
        register: v.register,
        session: v.cashier_session,
        user: v.user,
    };
    append_balance(
        tx,
        h.customer,
        &v.plan.balance_entries,
        place,
        *v.sale.as_uuid(),
        v.voided_at,
        now,
    )
    .await?;
    let payload = void_json(v, &h.number, paid_and_saved, text);
    record(
        tx,
        &place.actor(),
        now,
        Change {
            action: "sale.void",
            target: format!("sale:{}", v.sale),
            before: Some(json!({ "status": "posted" })),
            after: Some(json!({
                "status": "voided",
                "reason": v.reason.as_str(),
                "reason_text": text,
                "refund": paid_and_saved.minor(),
            })),
            outbox: Some(("sale.voided", &payload)),
        },
    )
    .await?;
    get_sale(Conn::Tx(&mut *tx), v.sale)
        .await?
        .ok_or(SaleDbError::NotFound("sale"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn price_modes_round_trip_parts() {
        assert_eq!(price_mode_parts(PriceMode::Listed), ("listed", None));
        assert_eq!(price_mode_parts(PriceMode::OpenPrice), ("open_price", None));
        let m = Money::from_minor(5);
        assert_eq!(
            price_mode_parts(PriceMode::Overridden { resolved: Some(m) }),
            ("overridden", Some(m))
        );
    }

    #[test]
    fn sums_report_overflow() {
        assert!(sum([Money::from_minor(i64::MAX), Money::from_minor(1)]).is_err());
        assert_eq!(sum([]).unwrap(), Money::ZERO);
    }
}
