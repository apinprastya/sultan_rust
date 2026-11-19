//! Reads across documents: the customer purchase history (spec 2.5 §9, 2.6 §12) and what a
//! cashier session's sales, returns and post-voids feed into `core::sale::session_report`
//! (spec 2.6 §10).

use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, CashierSessionId};
use sultan_core::money::Money;
use sultan_core::pricing::line_amount;
use sultan_core::sale::{SessionEvent, TenderKey};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::sale::{SaleHeader, list_recent};
use super::{CustomerId, clamp_limit, tender_kind};
use crate::conn::Conn;
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// Which document a history entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HistoryDoc {
    /// A sale (posted, or voided later: see [`HistoryEntry::voided`]).
    Sale,
    /// A posted return.
    Return,
}

/// One entry of a customer's purchase history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Sale or return.
    pub doc: HistoryDoc,
    /// The document's ID.
    pub id: Uuid,
    /// Its number.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// The customer it was posted for (the customer asked about, or one merged into it).
    pub customer: CustomerId,
    /// That customer's name (joined when read; phase S6).
    pub customer_name: String,
    /// That customer's member code.
    pub customer_code: String,
    /// When it was posted.
    pub posted_at: Timestamp,
    /// A sale: what was paid (`total + cash_rounding`); a return: the refund.
    pub amount: Money,
    /// A sale that was voided (shown marked VOID, spec 2.6 §19 item 30). Always false for a
    /// return.
    pub voided: bool,
}

/// Where a history page ends: the last entry's posting time and ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCursor {
    /// Its posting time.
    pub posted_at: Timestamp,
    /// Its ID.
    pub id: Uuid,
}

impl From<&HistoryEntry> for HistoryCursor {
    fn from(e: &HistoryEntry) -> Self {
        Self {
            posted_at: e.posted_at,
            id: e.id,
        }
    }
}

/// The largest page of [`purchase_history`].
pub const MAX_HISTORY_PAGE: u32 = 100;

/// `customer_id IN (the customer and those merged into it)`. A merge repoints customers merged
/// into the merged one (spec 2.5 §8), so one level is enough.
const CUSTOMERS: &str =
    "customer_id IN (SELECT id FROM customer WHERE id = ? OR merged_into_id = ?)";

/// The customer's sales (posted and voided, marked by [`HistoryEntry::voided`]) and posted
/// returns, including those of customers merged into it, newest first
/// (`GET /customers/{id}/history`, spec 2.5 §9, 2.6 §19 item 30).
/// `before` = the last entry of the previous page; `limit` 0 = 20, at most [`MAX_HISTORY_PAGE`].
///
/// # Errors
/// The database error.
pub async fn purchase_history<'c>(
    conn: impl Into<Conn<'c>>,
    customer: CustomerId,
    before: Option<HistoryCursor>,
    limit: u32,
) -> Result<Vec<HistoryEntry>> {
    let mut values: Vec<Arg> = args![customer, customer, customer, customer];
    let cursor = match before {
        Some(c) => {
            values.extend(args![c.posted_at, c.posted_at, c.id]);
            " WHERE h.posted_at < ? OR (h.posted_at = ? AND h.id < ?)"
        }
        None => "",
    };
    values.push(clamp_limit(limit, 20, MAX_HISTORY_PAGE).into());
    let sql = format!(
        "SELECT h.doc, h.id, h.number, h.branch_id, h.customer_id, h.posted_at, h.amount, \
         h.voided, c.name AS customer_name, c.code AS customer_code FROM (\
         SELECT 1 AS doc, id, number, branch_id, customer_id, posted_at, \
         total + cash_rounding AS amount, \
         CASE WHEN status = 'voided' THEN 1 ELSE 0 END AS voided \
         FROM sale WHERE {CUSTOMERS} \
         UNION ALL \
         SELECT 2 AS doc, id, number, branch_id, customer_id, posted_at, refund AS amount, \
         0 AS voided FROM sale_return WHERE status = 'posted' AND {CUSTOMERS}) h \
         JOIN customer c ON c.id = h.customer_id{cursor} \
         ORDER BY h.posted_at DESC, h.id DESC LIMIT ?"
    );
    sql::fetch_all(conn.into(), &sql, values, |row| {
        Ok(HistoryEntry {
            doc: match row.i64("doc")? {
                1 => HistoryDoc::Sale,
                _ => HistoryDoc::Return,
            },
            id: sql::id(row, "id")?,
            number: row.text("number")?,
            branch: sql::id(row, "branch_id")?,
            customer: sql::id(row, "customer_id")?,
            customer_name: row.text("customer_name")?,
            customer_code: row.text("customer_code")?,
            posted_at: sql::ts(row, "posted_at")?,
            amount: sql::money(row, "amount")?,
            voided: row.i64("voided")? != 0,
        })
    })
    .await
}

/// The till's recent receipts of a customer (spec 2.5 §9): its posted sales (and those of
/// customers merged into it) posted at or after `since` (the caller passes 90 days ago), newest
/// first, at most `limit` (0 = 5).
///
/// # Errors
/// The database error.
pub async fn recent_receipts<'c>(
    conn: impl Into<Conn<'c>>,
    customer: CustomerId,
    since: Timestamp,
    limit: u32,
) -> Result<Vec<SaleHeader>> {
    let condition = format!("status = 'posted' AND {CUSTOMERS} AND posted_at >= ?");
    list_recent(
        conn.into(),
        &condition,
        args![customer, customer, since, clamp_limit(limit, 5, 20)],
    )
    .await
}

fn key(row: &dyn Row) -> Result<TenderKey> {
    Ok(TenderKey {
        kind: tender_kind(row, "kind")?,
        tender_type: sql::opt_id(row, "tender_type_id")?,
    })
}

fn nonzero(m: Money) -> Option<Money> {
    (m != Money::ZERO).then_some(m)
}

/// What the sales documents of cashier session `session` contribute to its report (spec 2.6
/// §10), as `core::sale::SessionEvent`s for `core::sale::session_report`:
///
/// - every tender of the sales posted in it (also of sales voided later: the void is a refund of
///   the session that voided it, D10), their cash rounding, line discounts, cart discounts and
///   price overrides (how much each lowered the line);
/// - every return posted in it: `Return` with its refund, a `Refund` per tender (cash, balance
///   and the exchange tender), its cash rounding;
/// - every post-void paid in it: `PostVoid` with the sale's total, a `Refund` per tender, its cash
///   rounding.
///
/// Line voids, cart voids, cash in/out/drops and balance payouts come from elsewhere (the audit
/// log, [`crate::cash::session_cash_events`], [`crate::customers::session_payouts`]) and are
/// added by the caller.
///
/// # Errors
/// The database error.
pub async fn session_events<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> Result<Vec<SessionEvent>> {
    let mut conn = conn.into();
    let mut events = Vec::new();
    let tenders = sql::fetch_all(
        conn.reborrow(),
        "SELECT t.tender_type_id, t.kind, t.amount, t.cash_received, t.change_given, \
         t.change_saved FROM sale_tender t JOIN sale s ON s.id = t.sale_id \
         WHERE s.cashier_session_id = ? ORDER BY s.id, t.tender_no",
        args![session],
        |row| {
            Ok(SessionEvent::Tender {
                key: key(row)?,
                amount: sql::money(row, "amount")?,
                cash_received: row.opt_i64("cash_received")?.map(Money::from_minor),
                change_given: sql::money(row, "change_given")?,
                change_saved: sql::money(row, "change_saved")?,
            })
        },
    )
    .await?;
    events.extend(tenders);
    let sales = sql::fetch_all(
        conn.reborrow(),
        "SELECT cash_rounding, cart_discount FROM sale WHERE cashier_session_id = ? ORDER BY id",
        args![session],
        |row| {
            Ok((
                sql::money(row, "cash_rounding")?,
                sql::money(row, "cart_discount")?,
            ))
        },
    )
    .await?;
    for (rounding, cart_discount) in sales {
        events.extend(nonzero(rounding).map(SessionEvent::CashRounding));
        events.extend(nonzero(cart_discount).map(SessionEvent::CartDiscount));
    }
    let lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT l.line_discount, l.price_mode, l.resolved_price, l.qty, l.gross \
         FROM sale_line l JOIN sale s ON s.id = l.sale_id WHERE s.cashier_session_id = ? \
         ORDER BY s.id, l.line_no",
        args![session],
        |row| {
            Ok((
                sql::money(row, "line_discount")?,
                row.text("price_mode")? == "overridden",
                row.opt_i64("resolved_price")?.map(Money::from_minor),
                sql::qty(row, "qty")?,
                sql::money(row, "gross")?,
            ))
        },
    )
    .await?;
    for (discount, overridden, resolved, qty, gross) in lines {
        events.extend(nonzero(discount).map(SessionEvent::LineDiscount));
        if overridden {
            events.push(SessionEvent::PriceOverride(lowered(resolved, qty, gross)?));
        }
    }
    refund_events(conn, session, &mut events).await?;
    Ok(events)
}

/// The returns and post-voids paid in `session` (see [`session_events`]).
async fn refund_events(
    mut conn: Conn<'_>,
    session: CashierSessionId,
    events: &mut Vec<SessionEvent>,
) -> Result<()> {
    let returns = sql::fetch_all(
        conn.reborrow(),
        "SELECT refund, cash_rounding FROM sale_return \
         WHERE cashier_session_id = ? AND status = 'posted' ORDER BY id",
        args![session],
        |row| {
            Ok((
                sql::money(row, "refund")?,
                sql::money(row, "cash_rounding")?,
            ))
        },
    )
    .await?;
    for (refund, rounding) in returns {
        events.push(SessionEvent::Return(refund));
        events.extend(nonzero(rounding).map(SessionEvent::CashRounding));
    }
    let refunds = sql::fetch_all(
        conn.reborrow(),
        "SELECT t.tender_type_id, t.kind, t.amount FROM sale_return_tender t \
         JOIN sale_return r ON r.id = t.return_id \
         WHERE r.cashier_session_id = ? AND r.status = 'posted' ORDER BY r.id, t.tender_no",
        args![session],
        |row| {
            Ok(SessionEvent::Refund {
                key: key(row)?,
                amount: sql::money(row, "amount")?,
            })
        },
    )
    .await?;
    events.extend(refunds);
    let voids = sql::fetch_all(
        conn.reborrow(),
        "SELECT s.total, v.cash_rounding FROM sale_void v JOIN sale s ON s.id = v.sale_id \
         WHERE v.cashier_session_id = ? ORDER BY v.sale_id",
        args![session],
        |row| Ok((sql::money(row, "total")?, sql::money(row, "cash_rounding")?)),
    )
    .await?;
    for (total, rounding) in voids {
        events.push(SessionEvent::PostVoid(total));
        events.extend(nonzero(rounding).map(SessionEvent::CashRounding));
    }
    let void_refunds = sql::fetch_all(
        conn,
        "SELECT t.tender_type_id, t.kind, t.amount FROM sale_void_tender t \
         JOIN sale_void v ON v.sale_id = t.sale_id WHERE v.cashier_session_id = ? \
         ORDER BY t.sale_id, t.tender_no",
        args![session],
        |row| {
            Ok(SessionEvent::Refund {
                key: key(row)?,
                amount: sql::money(row, "amount")?,
            })
        },
    )
    .await?;
    events.extend(void_refunds);
    Ok(())
}

/// How much an override lowered a line: the resolved price × qty − the gross, at least 0 (0
/// without a resolved price).
fn lowered(resolved: Option<Money>, qty: Qty, gross: Money) -> Result<Money> {
    let Some(resolved) = resolved else {
        return Ok(Money::ZERO);
    };
    let listed = line_amount(resolved, qty)
        .map_err(|e| DbError::Corrupt(format!("override amount: {e}")))?;
    Ok(listed.saturating_sub(gross).max(Money::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowered_by_an_override() {
        let rp = Money::from_minor;
        let two = Qty::from_thousandths(2_000);
        assert_eq!(lowered(None, two, rp(10)).unwrap(), Money::ZERO);
        assert_eq!(lowered(Some(rp(10)), two, rp(15)).unwrap(), rp(5));
        assert_eq!(lowered(Some(rp(10)), two, rp(25)).unwrap(), Money::ZERO);
        assert!(lowered(Some(rp(i64::MAX)), two, rp(1)).is_err());
        assert_eq!(nonzero(Money::ZERO), None);
        assert_eq!(nonzero(rp(1)), Some(rp(1)));
    }
}
