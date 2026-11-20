//! The sales documents of a cashier session that a Z needs beyond its close figures (spec 2.7 §6,
//! §16 reading 8, phase K3): `core::cash::SessionSales`.
//!
//! - Sales **posted in** the session (later voided ones too: a post-void counts in the session
//!   that paid it, spec 2.6 D10): their count, Σ line gross, the first and last by posting time
//!   (ties by number), and their tax per rate as stored on the lines (`net` is the base).
//! - `returns_tax`: the tax per rate of the returns and post-voids **paid in** the session. A
//!   post-void takes the voided sale's stored lines; a return line takes its refund through
//!   `core::tax::line_tax` at the rate of the sold line (tax-inclusive, since a refund is what the
//!   customer paid; no tax when the sold line had none). A return without a receipt has no sold
//!   line and so no known rate: it is left out of the tax summary.

use std::collections::BTreeMap;

use sultan_core::cash::{SaleRef, SessionSales};
use sultan_core::ids::CashierSessionId;
use sultan_core::money::Money;
use sultan_core::sale::TaxLine;
use sultan_core::tax::{TaxSettings, line_tax};

use crate::conn::Conn;
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

fn rate(row: &dyn Row) -> Result<u16> {
    let value = row.i64("tax_rate_bp")?;
    u16::try_from(value).map_err(|_| DbError::Corrupt(format!("tax rate {value} out of range")))
}

fn add_line(by_rate: &mut BTreeMap<u16, TaxLine>, rate_bp: u16, base: Money, tax: Money) {
    let line = by_rate.entry(rate_bp).or_insert(TaxLine {
        rate_bp,
        base: Money::ZERO,
        tax: Money::ZERO,
    });
    line.base = line.base.saturating_add(base);
    line.tax = line.tax.saturating_add(tax);
}

/// The sales figures of `session` for its Z (see the module docs).
///
/// # Errors
/// The database error.
pub async fn session_sales<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> Result<SessionSales> {
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
        add_line(&mut returned, t.rate_bp, t.base, t.tax);
    }
    let lines = sql::fetch_all(
        conn,
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
    for (refund, rate_bp, taxed) in lines {
        let settings = TaxSettings {
            inclusive: true,
            registered: taxed,
        };
        let t = line_tax(refund, rate_bp, settings);
        add_line(&mut returned, rate_bp, t.net, t.tax);
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
