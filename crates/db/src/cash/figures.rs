//! The sales documents of a cashier session that a Z needs beyond its close figures (spec 2.7 §6,
//! §16 reading 8, phase K3): `core::cash::SessionSales`.
//!
//! - Sales **posted in** the session (later voided ones too: a post-void counts in the session
//!   that paid it, spec 2.6 D10): their count, Σ line gross, the first and last by posting time
//!   (ties by number), and their tax per rate as stored on the lines (`net` is the base).
//! - `returns_tax`: the tax per rate of the returns and post-voids **paid in** the session. A
//!   post-void takes the voided sale's stored lines; a return line takes its refund through
//!   `core::tax::line_tax` at the rate of the sold line (tax-inclusive, since a refund is what the
//!   customer paid; no tax when the sold line had none). A return **without a receipt** has no
//!   sold line: its refund takes the returned product's tax class at the rate valid on the
//!   return's local day (`tax_rate.valid_from` ≤ the day; 0% when the class has no rate yet),
//!   tax-inclusive as well, taxed when the company is tax-registered (decided 2026-11-21, spec 2.7
//!   §18 reading 13).
//!
//! Per-rate sums that overflow are [`CashError::AmountOverflow`] (`422 amount_overflow`), never
//! clamped (spec 2.7 §16 reading 8).

use std::collections::BTreeMap;

use sultan_core::cash::{CashError, SaleRef, SessionSales};
use sultan_core::ids::CashierSessionId;
use sultan_core::money::Money;
use sultan_core::sale::TaxLine;
use sultan_core::tax::{TaxSettings, line_tax};

use super::CashResult;
use crate::conn::Conn;
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

fn rate(row: &dyn Row) -> Result<u16> {
    let value = row.i64("tax_rate_bp")?;
    u16::try_from(value).map_err(|_| DbError::Corrupt(format!("tax rate {value} out of range")))
}

fn add_line(
    by_rate: &mut BTreeMap<u16, TaxLine>,
    rate_bp: u16,
    base: Money,
    tax: Money,
) -> CashResult<()> {
    let line = by_rate.entry(rate_bp).or_insert(TaxLine {
        rate_bp,
        base: Money::ZERO,
        tax: Money::ZERO,
    });
    line.base = line
        .base
        .checked_add(base)
        .ok_or(CashError::AmountOverflow)?;
    line.tax = line.tax.checked_add(tax).ok_or(CashError::AmountOverflow)?;
    Ok(())
}

/// The returned lines paid back in `session`: refund, tax rate and whether it was taxed. A line
/// by receipt takes its sold line's rate (taxed when the sold line had tax); a line without a
/// receipt takes the product's tax class at the rate valid on the return's local day (0% when
/// the class has no rate yet), taxed when the company is tax-registered (decided 2026-11-21).
async fn return_lines(
    mut conn: Conn<'_>,
    session: CashierSessionId,
) -> Result<Vec<(Money, u16, bool)>> {
    let lines = sql::fetch_all(
        conn.reborrow(),
        "SELECT rl.refund, l.tax_rate_bp, l.tax FROM sale_return_line rl \
         JOIN sale_return r ON r.id = rl.return_id \
         JOIN sale_line l ON l.sale_id = r.sale_id AND l.line_no = rl.sale_line_no \
         WHERE r.cashier_session_id = ? AND r.status = 'posted'",
        args![session],
        |row| {
            Ok((
                sql::money(row, "refund")?,
                rate(row)?,
                sql::money(row, "tax")? != Money::ZERO,
            ))
        },
    )
    .await?;
    let registered = sql::fetch_optional(
        conn.reborrow(),
        "SELECT tax_registered FROM pricing_settings WHERE id = 1",
        args![],
        |row| row.bool("tax_registered"),
    )
    .await?
    .unwrap_or(true);
    let no_receipt = sql::fetch_all(
        conn,
        "SELECT rl.refund, COALESCE((SELECT t.rate_bp FROM tax_rate t \
         WHERE t.tax_class_id = p.tax_class_id AND t.valid_from <= r.local_day \
         ORDER BY t.valid_from DESC LIMIT 1), 0) AS tax_rate_bp \
         FROM sale_return_line rl JOIN sale_return r ON r.id = rl.return_id \
         JOIN variant v ON v.id = rl.variant_id JOIN product p ON p.id = v.product_id \
         WHERE r.cashier_session_id = ? AND r.status = 'posted' AND rl.sale_line_no IS NULL",
        args![session],
        |row| Ok((sql::money(row, "refund")?, rate(row)?, registered)),
    )
    .await?;
    Ok(lines.into_iter().chain(no_receipt).collect())
}

/// The sales figures of `session` for its Z (see the module docs).
///
/// # Errors
/// The database error, or [`CashError::AmountOverflow`] when a per-rate sum overflows.
pub async fn session_sales<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> CashResult<SessionSales> {
    let mut conn = conn.into();
    let sales = sql::fetch_all(
        conn.reborrow(),
        "SELECT number, posted_at FROM sale WHERE cashier_session_id = ? \
         ORDER BY posted_at, number",
        args![session],
        |row| {
            Ok(SaleRef {
                number: row.text("number")?,
                posted_at: sql::ts(row, "posted_at")?,
            })
        },
    )
    .await?;
    let gross = sql::fetch_optional(
        conn.reborrow(),
        "SELECT CAST(COALESCE(SUM(l.gross), 0) AS SIGNED) AS gross FROM sale_line l \
         JOIN sale s ON s.id = l.sale_id WHERE s.cashier_session_id = ?",
        args![session],
        |row| sql::money(row, "gross"),
    )
    .await?
    .unwrap_or_default();
    let per_rate = "SELECT l.tax_rate_bp, CAST(SUM(l.net) AS SIGNED) AS base, \
         CAST(SUM(l.tax) AS SIGNED) AS tax FROM sale_line l";
    let map_rate = |row: &dyn Row| {
        Ok(TaxLine {
            rate_bp: rate(row)?,
            base: sql::money(row, "base")?,
            tax: sql::money(row, "tax")?,
        })
    };
    let tax = sql::fetch_all(
        conn.reborrow(),
        &format!(
            "{per_rate} JOIN sale s ON s.id = l.sale_id WHERE s.cashier_session_id = ? \
             GROUP BY l.tax_rate_bp ORDER BY l.tax_rate_bp"
        ),
        args![session],
        map_rate,
    )
    .await?;
    let mut returned: BTreeMap<u16, TaxLine> = BTreeMap::new();
    let voided = sql::fetch_all(
        conn.reborrow(),
        &format!(
            "{per_rate} JOIN sale_void v ON v.sale_id = l.sale_id \
             WHERE v.cashier_session_id = ? GROUP BY l.tax_rate_bp"
        ),
        args![session],
        map_rate,
    )
    .await?;
    for t in voided {
        add_line(&mut returned, t.rate_bp, t.base, t.tax)?;
    }
    for (refund, rate_bp, taxed) in return_lines(conn, session).await? {
        let settings = TaxSettings {
            inclusive: true,
            registered: taxed,
        };
        let t = line_tax(refund, rate_bp, settings);
        add_line(&mut returned, rate_bp, t.net, t.tax)?;
    }
    Ok(SessionSales {
        count: u32::try_from(sales.len()).unwrap_or(u32::MAX),
        gross,
        first: sales.first().cloned(),
        last: sales.last().cloned(),
        tax,
        returns_tax: returned.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cash::CashDbError;

    #[test]
    fn per_rate_sums_add_up_and_never_clamp() {
        let mut by_rate = BTreeMap::new();
        add_line(
            &mut by_rate,
            1100,
            Money::from_minor(900),
            Money::from_minor(99),
        )
        .unwrap();
        add_line(
            &mut by_rate,
            1100,
            Money::from_minor(100),
            Money::from_minor(11),
        )
        .unwrap();
        add_line(&mut by_rate, 0, Money::from_minor(5), Money::ZERO).unwrap();
        assert_eq!(
            by_rate[&1100],
            TaxLine {
                rate_bp: 1100,
                base: Money::from_minor(1_000),
                tax: Money::from_minor(110),
            }
        );
        assert_eq!(by_rate.len(), 2);
        for (base, tax) in [(i64::MAX, 0), (0, i64::MAX)] {
            let mut by_rate = by_rate.clone();
            let r = add_line(
                &mut by_rate,
                1100,
                Money::from_minor(base),
                Money::from_minor(tax),
            );
            assert!(
                matches!(r, Err(CashDbError::Rule(CashError::AmountOverflow))),
                "{r:?}"
            );
        }
    }
}
