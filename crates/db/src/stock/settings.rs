//! The inventory settings (spec 2.8 §13: `pkp`, `standard_ppn_rate`, `expiry_warn_days`,
//! `velocity_days`, `cover_days`, one company row, HQ data) and the product's expiry fields
//! (§9: `track_expiry`, `expiry_warn_days`).

use sultan_core::ids::ProductId;
use sultan_core::stock::InventorySettings;
use sultan_core::time::Timestamp;

use super::{StockDbError, StockResult, Written};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

fn small(row: &dyn Row, col: &str) -> Result<u32> {
    let v = row.i64(col)?;
    u32::try_from(v).map_err(|_| DbError::Corrupt(format!("`{col}` {v} out of range")))
}

/// The stored settings (`GET /settings/inventory`).
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] when the row is missing.
pub async fn get_inventory_settings<'c>(conn: impl Into<Conn<'c>>) -> Result<InventorySettings> {
    sql::fetch_optional(
        conn.into(),
        "SELECT pkp, standard_ppn_rate_bp, expiry_warn_days, velocity_days, cover_days \
         FROM inventory_settings WHERE id = 1",
        args![],
        |row| {
            let rate = small(row, "standard_ppn_rate_bp")?;
            Ok(InventorySettings {
                pkp: row.bool("pkp")?,
                standard_ppn_rate_bp: u16::try_from(rate)
                    .map_err(|_| DbError::Corrupt(format!("rate {rate} out of range")))?,
                expiry_warn_days: small(row, "expiry_warn_days")?,
                velocity_days: small(row, "velocity_days")?,
                cover_days: small(row, "cover_days")?,
            })
        },
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("inventory_settings row is missing".into()))
}

/// Stores the settings (`PUT /settings/inventory`) after `InventorySettings::check`; nothing
/// when nothing changed.
///
/// # Errors
/// [`StockDbError::Rule`] naming the field; the database error.
pub async fn put_inventory_settings(
    tx: &mut Tx,
    s: &InventorySettings,
    now: Timestamp,
) -> StockResult<Written> {
    s.check()?;
    if get_inventory_settings(Conn::Tx(&mut *tx)).await? == *s {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE inventory_settings SET pkp = ?, standard_ppn_rate_bp = ?, expiry_warn_days = ?, \
         velocity_days = ?, cover_days = ?, updated_at = ? WHERE id = 1",
        args![
            s.pkp,
            i64::from(s.standard_ppn_rate_bp),
            i64::from(s.expiry_warn_days),
            i64::from(s.velocity_days),
            i64::from(s.cover_days),
            now,
        ],
    )
    .await?;
    Ok(Written::Updated)
}

/// A product's expiry fields (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductExpiry {
    /// Receipts and opening stock record lots; other movements take them FEFO.
    pub track_expiry: bool,
    /// The warning days; `None` = the company setting.
    pub expiry_warn_days: Option<u32>,
}

/// The expiry fields of a product; `None` when it does not exist.
///
/// # Errors
/// The database error.
pub async fn get_product_expiry<'c>(
    conn: impl Into<Conn<'c>>,
    product: ProductId,
) -> Result<Option<ProductExpiry>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT track_expiry, expiry_warn_days FROM product WHERE id = ?",
        args![product],
        |row| {
            Ok(ProductExpiry {
                track_expiry: row.bool("track_expiry")?,
                expiry_warn_days: row
                    .opt_i64("expiry_warn_days")?
                    .map(|v| {
                        u32::try_from(v)
                            .map_err(|_| DbError::Corrupt(format!("warn days {v} out of range")))
                    })
                    .transpose()?,
            })
        },
    )
    .await
}

/// Sets a product's expiry fields (catalog, HQ). Switching tracking off keeps the lots for
/// history; the ledger then stops using them (§9). The catalog's product `PUT` carries these
/// fields from N3 on.
///
/// # Errors
/// [`StockDbError::NotFound`] (`product`); [`StockDbError::Invalid`] (`expiry_warn_days`,
/// 0–365).
pub async fn set_product_expiry(
    tx: &mut Tx,
    product: ProductId,
    e: ProductExpiry,
    now: Timestamp,
) -> StockResult<Written> {
    if e.expiry_warn_days.is_some_and(|d| d > 365) {
        return Err(StockDbError::invalid("expiry_warn_days", "must be 0–365"));
    }
    let before = get_product_expiry(Conn::Tx(&mut *tx), product)
        .await?
        .ok_or(StockDbError::NotFound("product"))?;
    if before == e {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE product SET track_expiry = ?, expiry_warn_days = ?, updated_at = ? WHERE id = ?",
        args![
            e.track_expiry,
            e.expiry_warn_days.map(i64::from),
            now,
            product
        ],
    )
    .await?;
    Ok(Written::Updated)
}
