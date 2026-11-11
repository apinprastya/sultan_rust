//! Tax rates per tax class (spec 2.3 §7) and the company pricing settings (§7, §8).
//!
//! A rate is keyed by (tax class, `valid_from`): one rate per class and day. Rates for future
//! days may be added ahead of time.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sultan_core::ids::TaxClassId;
use sultan_core::pricing::CashRounding;
use sultan_core::tax::{TaxRate, TaxSettings};
use sultan_core::time::Timestamp;

use super::{Actor, Change, PricingDbError, PricingResult, Written, date_from, date_value, record};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

fn map_rate(row: &dyn Row) -> Result<TaxRate> {
    let bp = row.i64("rate_bp")?;
    Ok(TaxRate {
        rate_bp: u16::try_from(bp)
            .map_err(|_| DbError::Corrupt(format!("tax rate of {bp} basis points")))?,
        valid_from: date_from(row.i64("valid_from")?, "valid_from")?,
    })
}

fn rate_json(class: TaxClassId, rate: TaxRate) -> Value {
    json!({
        "tax_class_id": class.to_string(),
        "rate_bp": rate.rate_bp,
        "valid_from": date_value(rate.valid_from),
    })
}

/// The rates of a tax class, oldest first (`GET /tax-classes/{id}/rates`).
///
/// # Errors
/// The database error.
pub async fn list_tax_rates<'c>(
    conn: impl Into<Conn<'c>>,
    class: TaxClassId,
) -> Result<Vec<TaxRate>> {
    sql::fetch_all(
        conn.into(),
        "SELECT rate_bp, valid_from FROM tax_rate WHERE tax_class_id = ? ORDER BY valid_from",
        args![class],
        map_rate,
    )
    .await
}

/// Every class's rates, oldest first, for pricing a sale with
/// [`sultan_core::tax::rate_on`].
///
/// # Errors
/// The database error.
pub async fn tax_rates_by_class<'c>(
    conn: impl Into<Conn<'c>>,
) -> Result<BTreeMap<TaxClassId, Vec<TaxRate>>> {
    let rows = sql::fetch_all(
        conn.into(),
        "SELECT tax_class_id, rate_bp, valid_from FROM tax_rate ORDER BY tax_class_id, valid_from",
        args![],
        |row| Ok((sql::id::<TaxClassId>(row, "tax_class_id")?, map_rate(row)?)),
    )
    .await?;
    let mut by_class: BTreeMap<TaxClassId, Vec<TaxRate>> = BTreeMap::new();
    for (class, rate) in rows {
        by_class.entry(class).or_default().push(rate);
    }
    Ok(by_class)
}

/// Adds a rate to a tax class, or changes the rate of an existing (class, `valid_from`)
/// (`PUT /tax-classes/{id}/rates`). A future `valid_from` is allowed. Audit and outbox
/// `tax_rate.put` unless nothing changed.
///
/// Spec 2.3 §7: a rate already used by a posted sale is never edited or deleted. There are no
/// sales yet (2.6), so any rate may still be changed.
///
/// # Errors
/// [`PricingDbError::NotFound`] (`tax_class`) or the database error.
pub async fn put_tax_rate(
    tx: &mut Tx,
    class: TaxClassId,
    rate: TaxRate,
    actor: &Actor,
    now: Timestamp,
) -> PricingResult<Written> {
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT 1 FROM tax_class WHERE id = ?",
        args![class],
        |_| Ok(()),
    )
    .await?
    .ok_or(PricingDbError::NotFound("tax_class"))?;
    let day = date_value(rate.valid_from);
    let old = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT rate_bp, valid_from FROM tax_rate WHERE tax_class_id = ? AND valid_from = ?",
        args![class, day],
        map_rate,
    )
    .await?;
    let created = match old {
        Some(o) if o == rate => return Ok(Written::Unchanged),
        // Hook for 2.6: refuse (`409 in_use`) to change a rate a posted sale used.
        Some(_) => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "UPDATE tax_rate SET rate_bp = ?, updated_at = ? \
                 WHERE tax_class_id = ? AND valid_from = ?",
                args![i64::from(rate.rate_bp), now, class, day],
            )
            .await?;
            false
        }
        None => {
            sql::execute(
                Conn::Tx(&mut *tx),
                "INSERT INTO tax_rate (tax_class_id, valid_from, rate_bp, created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?)",
                args![class, day, i64::from(rate.rate_bp), now, now],
            )
            .await?;
            true
        }
    };
    let after = rate_json(class, rate);
    record(
        tx,
        actor,
        now,
        Change {
            action: "tax_rate.put",
            target: format!("tax_class:{class}"),
            before: old.map(|o| rate_json(class, o)),
            after: Some(after.clone()),
            payload: after,
        },
    )
    .await?;
    Ok(if created {
        Written::Created
    } else {
        Written::Updated
    })
}

/// The company pricing settings (`GET /settings/pricing`, one row).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PricingSettings {
    /// Tax-inclusive prices (default on) and tax-registered (default on).
    pub tax: TaxSettings,
    /// Cash rounding (default off).
    pub cash_rounding: CashRounding,
}

fn settings_json(s: PricingSettings) -> Value {
    json!({
        "tax_inclusive": s.tax.inclusive,
        "tax_registered": s.tax.registered,
        "cash_rounding": s.cash_rounding.as_str(),
    })
}

/// The pricing settings.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for a missing row or an unknown rounding.
pub async fn get_pricing_settings<'c>(conn: impl Into<Conn<'c>>) -> Result<PricingSettings> {
    sql::fetch_optional(
        conn.into(),
        "SELECT tax_inclusive, tax_registered, cash_rounding FROM pricing_settings WHERE id = 1",
        args![],
        |row| {
            let rounding = row.text("cash_rounding")?;
            Ok(PricingSettings {
                tax: TaxSettings {
                    inclusive: row.bool("tax_inclusive")?,
                    registered: row.bool("tax_registered")?,
                },
                cash_rounding: CashRounding::ALL
                    .into_iter()
                    .find(|r| r.as_str() == rounding)
                    .ok_or_else(|| DbError::Corrupt(format!("cash rounding `{rounding}`")))?,
            })
        },
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("pricing_settings row is missing".into()))
}

/// Replaces the pricing settings (`PUT /settings/pricing`). Changing `tax_inclusive` does not
/// convert stored prices (spec 2.3 §7). Audit and outbox `pricing.settings.put` unless nothing
/// changed.
///
/// # Errors
/// The database error.
pub async fn put_pricing_settings(
    tx: &mut Tx,
    settings: PricingSettings,
    actor: &Actor,
    now: Timestamp,
) -> PricingResult<Written> {
    let old = get_pricing_settings(Conn::Tx(&mut *tx)).await?;
    if old == settings {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE pricing_settings SET tax_inclusive = ?, tax_registered = ?, cash_rounding = ?, \
         updated_at = ? WHERE id = 1",
        args![
            settings.tax.inclusive,
            settings.tax.registered,
            settings.cash_rounding.as_str(),
            now,
        ],
    )
    .await?;
    let after = settings_json(settings);
    record(
        tx,
        actor,
        now,
        Change {
            action: "pricing.settings.put",
            target: "pricing_settings".to_owned(),
            before: Some(settings_json(old)),
            after: Some(after.clone()),
            payload: after,
        },
    )
    .await?;
    Ok(Written::Updated)
}
