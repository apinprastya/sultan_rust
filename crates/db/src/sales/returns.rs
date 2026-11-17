//! Returns and exchanges (spec 2.6 §8, D9, D11, D12).

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sultan_core::catalog::{ComponentSnapshot, ProductType, Qty};
use sultan_core::customer::BalanceEntryKind as CoreKind;
use sultan_core::ids::{
    BranchId, CashierSessionId, LocationId, RegisterId, ReturnId, SaleId, UnitId, UserId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::sale::{
    MAX_LINES, ReturnDisposition, ReturnedSoFar, SaleError, SaleStatus, SoldLine, StockLine,
    StockMovementKind, TenderKind, line_refund,
};
use sultan_core::time::Timestamp;

use super::sale::{
    self as sales, DocFilter, NewSale, filter_sql, map_refund_tender, movements_json,
    refund_tender_json,
};
use super::stock;
use super::{
    Change, CustomerId, NumberKind, Place, RefundTender, SaleDbError, SaleResult, Written,
    allocate, append_balance, clean_text, components, components_json, count, product_type, record,
};
use crate::TokenHash;
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// One returned line (spec 2.6 §1, §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnLine {
    /// The sale line it comes from (by receipt); `None` without a receipt.
    pub sale_line: Option<u32>,
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
    /// The receipt name.
    pub name: String,
    /// The product type.
    pub product_type: ProductType,
    /// The unit's factor to the base unit.
    pub factor: Qty,
    /// A bundle's component snapshot (the sale's, by receipt).
    pub components: Vec<ComponentSnapshot>,
    /// The quantity returned, in the line's unit (> 0).
    pub qty: Qty,
    /// The refund for it (`core::sale::line_refund`, or `no_receipt_refund`).
    pub refund: Money,
    /// Restock or write-off (D9).
    pub disposition: ReturnDisposition,
}

impl ReturnLine {
    /// The line as `core::sale::return_movements` needs it.
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

/// A stored return line with its number (from 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReturnLine {
    /// The line number, from 1.
    pub line_no: u32,
    /// The line.
    pub line: ReturnLine,
}

/// A return to post (`PUT /returns/{id}`), with the new sale of an exchange.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewReturn {
    /// The client's `UUIDv7`.
    pub id: ReturnId,
    /// A hash of the posted body.
    pub request_hash: TokenHash,
    /// The original sale (by receipt).
    pub sale: Option<SaleId>,
    /// The branch.
    pub branch: BranchId,
    /// The register.
    pub register: RegisterId,
    /// The cashier's open session (it pays the cash out).
    pub cashier_session: CashierSessionId,
    /// The cashier.
    pub user: UserId,
    /// The customer: the sale's (by receipt) or the attached one.
    pub customer: Option<CustomerId>,
    /// An optional note on why (at most 200 characters).
    pub reason: Option<String>,
    /// The lines (1–300).
    pub lines: Vec<ReturnLine>,
    /// The cash rounding of the cash part (`RefundPlan::cash_rounding`).
    pub cash_rounding: Money,
    /// How it pays back: cash, customer balance, exchange (Σ = Σ refunds + cash rounding).
    pub tenders: Vec<RefundTender>,
    /// The balance entries of the plan (`payment_reversed`, `refund`).
    pub balance_entries: Vec<sultan_core::customer::BalanceEntry>,
    /// `return` and `write_off` movements (`core::sale::return_movements`).
    pub movements: Vec<sultan_core::sale::StockMovement>,
    /// The new sale of an exchange (D12), paid partly by the `exchange` tender.
    pub exchange: Option<NewSale>,
    /// When it was posted.
    pub posted_at: Timestamp,
}

/// A return's header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReturnHeader {
    /// The ID.
    pub id: ReturnId,
    /// The number, e.g. `JKT01-R2-RT-261116-0001`.
    pub number: String,
    /// Always posted for now (no return void yet).
    pub status: SaleStatus,
    /// The original sale.
    pub sale: Option<SaleId>,
    /// The branch.
    pub branch: BranchId,
    /// The register.
    pub register: RegisterId,
    /// The session that paid it.
    pub cashier_session: CashierSessionId,
    /// The cashier.
    pub user: UserId,
    /// The customer.
    pub customer: Option<CustomerId>,
    /// The note.
    pub reason: Option<String>,
    /// Σ line refunds.
    pub refund: Money,
    /// The cash rounding.
    pub cash_rounding: Money,
    /// The new sale of an exchange.
    pub exchange_sale: Option<SaleId>,
    /// When it was posted.
    pub posted_at: Timestamp,
}

/// A stored return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Return {
    /// The header.
    pub header: ReturnHeader,
    /// The lines.
    pub lines: Vec<StoredReturnLine>,
    /// How it paid back.
    pub tenders: Vec<RefundTender>,
}

const HEADER_SELECT: &str = "SELECT id, number, status, sale_id, branch_id, register_id, \
     cashier_session_id, user_id, customer_id, reason, refund, cash_rounding, exchange_sale_id, \
     posted_at FROM sale_return";

fn map_header(row: &dyn Row) -> Result<ReturnHeader> {
    let status = match row.text("status")?.as_str() {
        "posted" => SaleStatus::Posted,
        "voided" => SaleStatus::Voided,
        other => return Err(DbError::Corrupt(format!("unknown return status `{other}`"))),
    };
    Ok(ReturnHeader {
        id: sql::id(row, "id")?,
        number: row.text("number")?,
        status,
        sale: sql::opt_id(row, "sale_id")?,
        branch: sql::id(row, "branch_id")?,
        register: sql::id(row, "register_id")?,
        cashier_session: sql::id(row, "cashier_session_id")?,
        user: sql::id(row, "user_id")?,
        customer: sql::opt_id(row, "customer_id")?,
        reason: row.opt_text("reason")?,
        refund: sql::money(row, "refund")?,
        cash_rounding: sql::money(row, "cash_rounding")?,
        exchange_sale: sql::opt_id(row, "exchange_sale_id")?,
        posted_at: sql::ts(row, "posted_at")?,
    })
}

fn map_line(row: &dyn Row) -> Result<StoredReturnLine> {
    let disposition = match row.text("disposition")?.as_str() {
        "restock" => ReturnDisposition::Restock,
        "write_off" => ReturnDisposition::WriteOff {
            reason: row.opt_text("write_off_reason")?.unwrap_or_default(),
        },
        other => return Err(DbError::Corrupt(format!("unknown disposition `{other}`"))),
    };
    Ok(StoredReturnLine {
        line_no: count(row, "line_no")?,
        line: ReturnLine {
            sale_line: row
                .opt_i64("sale_line_no")?
                .map(|n| u32::try_from(n).map_err(|_| DbError::Corrupt(format!("line {n}"))))
                .transpose()?,
            variant: sql::id(row, "variant_id")?,
            unit: sql::id(row, "unit_id")?,
            name: row.text("name")?,
            product_type: product_type(row, "product_type")?,
            factor: sql::qty(row, "factor")?,
            components: components(row, "components")?,
            qty: sql::qty(row, "qty")?,
            refund: sql::money(row, "refund")?,
            disposition,
        },
    })
}

/// One return with its lines and tenders.
///
/// # Errors
/// The database error.
pub async fn get_return<'c>(conn: impl Into<Conn<'c>>, id: ReturnId) -> Result<Option<Return>> {
    let mut conn = conn.into();
    let sql = format!("{HEADER_SELECT} WHERE id = ?");
    let Some(header) = sql::fetch_optional(conn.reborrow(), &sql, args![id], map_header).await?
    else {
        return Ok(None);
    };
    let lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT line_no, sale_line_no, variant_id, unit_id, name, product_type, factor, \
         components, qty, refund, disposition, write_off_reason FROM sale_return_line \
         WHERE return_id = ? ORDER BY line_no",
        args![id],
        map_line,
    )
    .await?;
    let tenders = sql::fetch_all(
        conn,
        "SELECT tender_type_id, kind, amount FROM sale_return_tender WHERE return_id = ? \
         ORDER BY tender_no",
        args![id],
        map_refund_tender,
    )
    .await?;
    Ok(Some(Return {
        header,
        lines,
        tenders,
    }))
}

/// Return headers matching `filter`, newest first (`GET /returns?…`).
///
/// # Errors
/// The database error.
pub async fn list_returns<'c>(
    conn: impl Into<Conn<'c>>,
    filter: &DocFilter,
) -> Result<Vec<ReturnHeader>> {
    let (clause, values) = filter_sql(filter, false);
    let sql = format!("{HEADER_SELECT}{clause} ORDER BY id DESC LIMIT ?");
    sql::fetch_all(conn.into(), &sql, values, map_header).await
}

fn sum(values: impl IntoIterator<Item = Money>) -> SaleResult<Money> {
    values.into_iter().try_fold(Money::ZERO, |acc, v| {
        acc.checked_add(v)
            .ok_or(SaleDbError::Rule(SaleError::AmountOverflow))
    })
}

/// The storage checks of a return; returns the cleaned reason and lines.
fn check_return(r: &NewReturn) -> SaleResult<(Option<String>, Vec<ReturnLine>)> {
    if r.lines.is_empty() || r.lines.len() > MAX_LINES {
        return Err(SaleDbError::invalid(
            "lines",
            format!("a return has 1 to {MAX_LINES} lines"),
        ));
    }
    let reason = clean_text("reason", r.reason.as_deref(), 200)?;
    let mut lines = Vec::with_capacity(r.lines.len());
    for line in &r.lines {
        if !line.qty.is_positive() {
            return Err(SaleError::InvalidQty {
                reason: "must be greater than 0",
            }
            .into());
        }
        if line.refund.is_negative() {
            return Err(SaleDbError::invalid("refund", "must not be negative"));
        }
        if line.sale_line.is_some() != r.sale.is_some() {
            return Err(SaleDbError::invalid(
                "lines",
                "a line names its sale line exactly when the return has a sale",
            ));
        }
        lines.push(ReturnLine {
            disposition: line.disposition.clone().validate()?,
            ..line.clone()
        });
    }
    let refund = sum(lines.iter().map(|l| l.refund))?;
    let due = refund
        .checked_add(r.cash_rounding)
        .ok_or(SaleDbError::Rule(SaleError::AmountOverflow))?;
    for t in &r.tenders {
        let ok = match t.kind {
            TenderKind::Cash | TenderKind::CustomerBalance => t.tender_type.is_some(),
            TenderKind::Exchange => t.tender_type.is_none(),
            _ => false,
        };
        if !ok || t.amount <= Money::ZERO {
            return Err(SaleDbError::invalid(
                "tenders",
                "a return pays back in cash, to the balance or as an exchange tender",
            ));
        }
    }
    let paid = sum(r.tenders.iter().map(|t| t.amount))?;
    if paid != due {
        return Err(SaleDbError::invalid(
            "tenders",
            format!("pay {paid}, not refund + cash rounding {due}"),
        ));
    }
    let exchange_out = sum(r
        .tenders
        .iter()
        .filter(|t| t.kind == TenderKind::Exchange)
        .map(|t| t.amount))?;
    let exchange_in = match &r.exchange {
        Some(sale) => sum(sale
            .payment
            .tenders
            .iter()
            .filter(|t| t.kind == TenderKind::Exchange)
            .map(|t| t.amount))?,
        None => Money::ZERO,
    };
    if exchange_out != exchange_in {
        return Err(SaleDbError::invalid(
            "exchange",
            "the exchange tender must pay the new sale",
        ));
    }
    if r.movements.iter().any(|m| {
        !matches!(
            m.kind,
            StockMovementKind::Return | StockMovementKind::WriteOff
        )
    }) {
        return Err(SaleDbError::invalid(
            "movements",
            "a return moves stock back",
        ));
    }
    if r.balance_entries
        .iter()
        .any(|e| !matches!(e.kind(), CoreKind::PaymentReversed | CoreKind::Refund))
    {
        return Err(SaleDbError::invalid(
            "balance_entries",
            "a return adds payment_reversed and refund entries only",
        ));
    }
    if r.sale.is_none() && reverses_payment(r) {
        return Err(SaleDbError::invalid(
            "balance_entries",
            "only a return by receipt reverses a balance payment",
        ));
    }
    Ok((reason, lines))
}

/// Whether the return gives back a balance payment of its sale (`payment_reversed`).
fn reverses_payment(r: &NewReturn) -> bool {
    r.balance_entries
        .iter()
        .any(|e| e.kind() == CoreKind::PaymentReversed)
}

/// Checks the lines against the sale (by receipt): the sale is posted and not imported, each line
/// exists with the same variant and unit, the quantity is returnable and the refund is what
/// `core::sale::line_refund` gives. A balance payment goes back to the sale's own customer
/// (spec 2.6 §19, from S2). Returns the new returned quantity and refund per sale line.
async fn check_against_sale(
    tx: &mut Tx,
    r: &NewReturn,
    sale: SaleId,
    lines: &[ReturnLine],
) -> SaleResult<BTreeMap<u32, (Qty, Money)>> {
    let h = sales::header(Conn::Tx(&mut *tx), sale)
        .await?
        .ok_or(SaleDbError::NotFound("sale"))?;
    sultan_core::sale::ensure_returnable(h.status, h.legacy_number.is_some())?;
    // A balance payment goes back to the sale's customer, and when the sale has a customer, a
    // refund to the balance goes only to that customer (spec 2.6 §19 item 34, decided
    // 2026-11-17).
    let to_sale_customer =
        reverses_payment(r) || (h.customer.is_some() && !r.balance_entries.is_empty());
    if to_sale_customer && r.customer != h.customer {
        return Err(SaleDbError::invalid(
            "customer",
            "the balance goes back to the sale's customer",
        ));
    }
    let stored = sales::lines(Conn::Tx(&mut *tx), sale).await?;
    let mut taken: BTreeMap<u32, (Qty, Money)> = BTreeMap::new();
    for line in lines {
        let no = line.sale_line.unwrap_or_default();
        let sold = stored
            .iter()
            .find(|s| s.line_no == no)
            .ok_or_else(|| SaleDbError::invalid("lines", format!("the sale has no line {no}")))?;
        if sold.line.variant != line.variant || sold.line.unit != line.unit {
            return Err(SaleDbError::invalid(
                "lines",
                format!("line {no} sold another variant or unit"),
            ));
        }
        let (qty, refunded) = taken.get(&no).copied().unwrap_or((Qty::ZERO, Money::ZERO));
        let so_far = ReturnedSoFar {
            qty: sold
                .returned
                .qty
                .checked_add(qty)
                .ok_or(SaleError::AmountOverflow)?,
            refunded: sold
                .returned
                .refunded
                .checked_add(refunded)
                .ok_or(SaleError::AmountOverflow)?,
        };
        let expected = line_refund(
            SoldLine {
                qty: sold.line.qty,
                amount: sold.line.totals.total,
            },
            so_far,
            line.qty,
        )?;
        if expected != line.refund {
            return Err(SaleDbError::invalid(
                "refund",
                format!("line {no} refunds {expected}, not {}", line.refund),
            ));
        }
        taken.insert(
            no,
            (
                qty.checked_add(line.qty).ok_or(SaleError::AmountOverflow)?,
                refunded
                    .checked_add(line.refund)
                    .ok_or(SaleError::AmountOverflow)?,
            ),
        );
    }
    Ok(taken)
}

fn line_json(no: usize, l: &ReturnLine) -> Value {
    let reason = match &l.disposition {
        ReturnDisposition::Restock => None,
        ReturnDisposition::WriteOff { reason } => Some(reason.as_str()),
    };
    json!({
        "line_no": no,
        "sale_line_no": l.sale_line,
        "variant_id": l.variant.to_string(),
        "unit_id": l.unit.to_string(),
        "name": l.name,
        "qty": l.qty.thousandths(),
        "refund": l.refund.minor(),
        "disposition": if reason.is_some() { "write_off" } else { "restock" },
        "write_off_reason": reason,
    })
}

async fn insert_lines(tx: &mut Tx, id: ReturnId, lines: &[ReturnLine]) -> Result<()> {
    for (i, l) in lines.iter().enumerate() {
        let write_off = match &l.disposition {
            ReturnDisposition::Restock => None,
            ReturnDisposition::WriteOff { reason } => Some(reason.as_str()),
        };
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO sale_return_line (return_id, line_no, sale_line_no, variant_id, \
             unit_id, name, product_type, factor, components, qty, refund, disposition, \
             write_off_reason) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                id,
                i64::try_from(i + 1).unwrap_or(i64::MAX),
                l.sale_line.map(i64::from),
                l.variant,
                l.unit,
                l.name.as_str(),
                l.product_type.as_str(),
                l.factor,
                components_json(&l.components),
                l.qty,
                l.refund,
                if write_off.is_some() {
                    "write_off"
                } else {
                    "restock"
                },
                write_off,
            ],
        )
        .await?;
    }
    Ok(())
}

/// Records on the sale what this return took: the returned quantity and refund per line and the
/// return count, with guarded updates (a failed guard is a lost race: retry).
async fn take_from_sale(
    tx: &mut Tx,
    sale: SaleId,
    taken: &BTreeMap<u32, (Qty, Money)>,
) -> SaleResult<()> {
    for (no, (qty, refunded)) in taken {
        let changed = sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE sale_line SET returned_qty = returned_qty + ?, refunded = refunded + ? \
             WHERE sale_id = ? AND line_no = ? AND returned_qty + ? <= qty \
             AND refunded + ? <= total",
            args![*qty, *refunded, sale, i64::from(*no), *qty, *refunded],
        )
        .await?;
        if changed == 0 {
            return Err(DbError::Conflict(format!("sale line {no} changed")).into());
        }
    }
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE sale SET return_count = return_count + 1 WHERE id = ? AND status = 'posted'",
        args![sale],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::Conflict("the sale changed while returning it".into()).into());
    }
    Ok(())
}

/// The outbox payload of a return.
fn return_json(
    r: &NewReturn,
    number: &str,
    reason: Option<&str>,
    lines: &[ReturnLine],
    exchange_sale: Option<SaleId>,
    location: Option<LocationId>,
) -> Value {
    json!({
        "id": r.id.to_string(),
        "number": number,
        "sale_id": r.sale.map(|s| s.to_string()),
        "branch_id": r.branch.to_string(),
        "register_id": r.register.to_string(),
        "cashier_session_id": r.cashier_session.to_string(),
        "user_id": r.user.to_string(),
        "customer_id": r.customer.map(|c| c.to_string()),
        "reason": reason,
        "refund": lines.iter().fold(Money::ZERO, |a, l| a.saturating_add(l.refund)).minor(),
        "cash_rounding": r.cash_rounding.minor(),
        "exchange_sale_id": exchange_sale.map(|s| s.to_string()),
        "posted_at": r.posted_at.unix_millis(),
        "lines": lines.iter().enumerate().map(|(i, l)| line_json(i + 1, l)).collect::<Vec<_>>(),
        "tenders": r.tenders.iter().map(refund_tender_json).collect::<Vec<_>>(),
        "location_id": location.map(|l| l.to_string()),
        "movements": movements_json(&r.movements),
    })
}

/// Inserts the `sale_return` row.
async fn insert_return(
    tx: &mut Tx,
    r: &NewReturn,
    number: &super::Allocated,
    reason: Option<&str>,
    refund: Money,
    exchange_sale: Option<SaleId>,
    now: Timestamp,
) -> Result<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO sale_return (id, number, status, sale_id, branch_id, register_id, \
         cashier_session_id, user_id, customer_id, local_day, seq, reason, refund, cash_rounding, \
         exchange_sale_id, posted_at, request_hash, created_at) \
         VALUES (?, ?, 'posted', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            r.id,
            number.number.as_str(),
            r.sale,
            r.branch,
            r.register,
            r.cashier_session,
            r.user,
            r.customer,
            number.day,
            number.seq,
            reason,
            refund,
            r.cash_rounding,
            exchange_sale,
            r.posted_at,
            &r.request_hash,
            now,
        ],
    )
    .await?;
    Ok(())
}

/// Posts a return in `tx` (`PUT /returns/{id}`, spec 2.6 §8), with the new sale of an exchange
/// (D12) when `exchange` is set: the new sale is posted first ([`super::post_sale`] rules, its
/// `exchange` tender allowed), then the return with its number, lines and tenders. By receipt,
/// the sale's lines record the returned quantity and refund and the sale counts the return (so it
/// can no longer be voided, §7). Appends the `return` / `write_off` stock movements, the balance
/// entries (`payment_reversed`, `refund`) for the return's customer, the audit row `return.post`
/// and the outbox event `return.posted`. Replaying the same id with the same `request_hash`
/// writes nothing and returns the stored return.
///
/// # Errors
/// [`SaleDbError::Conflict`] when the id exists with another body; [`SaleDbError::NotFound`]
/// (`sale`, `register`, `customer`); [`SaleDbError::Rule`]: `conflict` (a voided sale),
/// `invalid_field` (an imported sale, a write-off without a reason), `return_qty_exceeded`,
/// `invalid_qty`; [`SaleDbError::Invalid`] for sums that do not add up or lines that do not belong
/// to the sale; a retryable [`DbError::Conflict`] when another document won a race.
pub async fn post_return(
    tx: &mut Tx,
    r: &NewReturn,
    now: Timestamp,
) -> SaleResult<(Written, Return)> {
    if let Some(stored) = replay(tx, r.id, &r.request_hash).await? {
        return Ok((Written::Unchanged, stored));
    }
    let (reason, lines) = check_return(r)?;
    let taken = match r.sale {
        Some(sale) => check_against_sale(tx, r, sale, &lines).await?,
        None => BTreeMap::new(),
    };
    let exchange_sale = match &r.exchange {
        Some(sale) => Some(sales::post(tx, sale, true, now).await?.1.header.id),
        None => None,
    };
    let number = allocate(tx, NumberKind::Return, r.branch, r.register, r.posted_at).await?;
    let refund = sum(lines.iter().map(|l| l.refund))?;
    insert_return(
        tx,
        r,
        &number,
        reason.as_deref(),
        refund,
        exchange_sale,
        now,
    )
    .await?;
    insert_lines(tx, r.id, &lines).await?;
    sales::insert_refund_tenders(
        tx,
        "sale_return_tender",
        "return_id",
        *r.id.as_uuid(),
        &r.tenders,
    )
    .await?;
    if let Some(sale) = r.sale {
        take_from_sale(tx, sale, &taken).await?;
    }
    let location = stock::register_location(tx, r.register).await?;
    stock::insert(
        tx,
        r.branch,
        location,
        "return",
        *r.id.as_uuid(),
        &r.movements,
        r.posted_at,
    )
    .await?;
    let place = Place {
        branch: r.branch,
        register: r.register,
        session: r.cashier_session,
        user: r.user,
    };
    append_balance(
        tx,
        r.customer,
        &r.balance_entries,
        place,
        *r.id.as_uuid(),
        r.posted_at,
        now,
    )
    .await?;
    let payload = return_json(
        r,
        &number.number,
        reason.as_deref(),
        &lines,
        exchange_sale,
        location,
    );
    record(
        tx,
        &place.actor(),
        now,
        Change {
            action: "return.post",
            target: format!("return:{}", r.id),
            before: None,
            after: Some(json!({
                "number": number.number,
                "sale_id": r.sale.map(|s| s.to_string()),
                "refund": refund.minor(),
                "exchange_sale_id": exchange_sale.map(|s| s.to_string()),
            })),
            outbox: Some(("return.posted", &payload)),
        },
    )
    .await?;
    let stored = get_return(Conn::Tx(&mut *tx), r.id)
        .await?
        .ok_or(SaleDbError::NotFound("return"))?;
    Ok((Written::Created, stored))
}

/// Whether return `id` was posted already (spec 2.6 §12): `Some(stored)` when with the same
/// body hash, [`SaleDbError::Conflict`] with another, `None` when the ID is new. The server
/// asks first, so a replay answers the stored document even when the cart went stale since.
///
/// # Errors
/// [`SaleDbError::Conflict`], or the database error.
pub async fn replayed_return(
    tx: &mut Tx,
    id: ReturnId,
    hash: &TokenHash,
) -> SaleResult<Option<Return>> {
    replay(tx, id, hash).await
}

async fn replay(tx: &mut Tx, id: ReturnId, hash: &TokenHash) -> SaleResult<Option<Return>> {
    let stored = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT request_hash FROM sale_return WHERE id = ?",
        args![id],
        |row| row.opt_bytes("request_hash"),
    )
    .await?;
    match stored {
        None => Ok(None),
        Some(Some(h)) if h.as_slice() == hash.as_slice() => {
            Ok(get_return(Conn::Tx(&mut *tx), id).await?)
        }
        Some(_) => Err(SaleDbError::Conflict("return", *id.as_uuid())),
    }
}
