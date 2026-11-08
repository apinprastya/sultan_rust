//! Company catalog settings (code prefixes, scale layouts, unit list) and the code generators
//! (spec 2.2 §4, §6, §7, §12 `GET/PUT /settings/catalog`, `POST /catalog/generate/…`).
//!
//! The counters live in `catalog_counter` at HQ. A generator reads its counter, skips values that
//! are already taken (e.g. imported codes) and stores the next counter with a compare-and-set
//! (`WHERE next_value = <read value>`), so two racing generators cannot hand out the same value:
//! the loser's update matches no row (or `SQLite` reports a busy snapshot) and the transaction is
//! retried.

use serde_json::json;
use sultan_core::catalog::{
    ScaleLayout, next_internal_barcode, next_product_code, normalize_product_code_prefix,
    validate_scale_layouts, validate_unit_list,
};
use sultan_core::time::Timestamp;

use super::json::{layouts_json, parse_layouts, parse_strings, strings_json};
use super::{Actor, CatalogDbError, CatalogResult, Change, MAX_ATTEMPTS, Written, record};
use crate::conn::{Conn, Db, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, args};

/// The company catalog settings (one row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogSettings {
    /// Prefix of generated product codes, `[A-Z0-9._-]{0,17}` (default `P`).
    pub product_code_prefix: String,
    /// Prefix of generated internal EAN-13 barcodes, 2 digits, not a scale prefix (default `29`).
    pub internal_barcode_prefix: String,
    /// Scale barcode layouts (default `21` weight and `22` price).
    pub scale_layouts: Vec<ScaleLayout>,
    /// Common unit names for autocomplete.
    pub unit_names: Vec<String>,
}

/// The catalog settings.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for unreadable JSON or a missing row.
pub async fn get_catalog_settings<'c>(conn: impl Into<Conn<'c>>) -> Result<CatalogSettings> {
    sql::fetch_optional(
        conn.into(),
        "SELECT product_code_prefix, internal_barcode_prefix, scale_layouts, unit_names \
         FROM catalog_settings WHERE id = 1",
        args![],
        |row| {
            let layouts = sql::opt_json(row, "scale_layouts")?.unwrap_or_default();
            let units = sql::opt_json(row, "unit_names")?.unwrap_or_default();
            Ok(CatalogSettings {
                product_code_prefix: row.text("product_code_prefix")?,
                internal_barcode_prefix: row.text("internal_barcode_prefix")?,
                scale_layouts: parse_layouts(&layouts)?,
                unit_names: parse_strings(&units, "unit_names")?,
            })
        },
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("catalog_settings row is missing".into()))
}

fn settings_json(s: &CatalogSettings) -> serde_json::Value {
    let layouts: serde_json::Value =
        serde_json::from_str(&layouts_json(&s.scale_layouts)).unwrap_or_default();
    json!({
        "product_code_prefix": s.product_code_prefix,
        "internal_barcode_prefix": s.internal_barcode_prefix,
        "scale_layouts": layouts,
        "unit_names": s.unit_names,
    })
}

/// Replaces the catalog settings (`PUT /settings/catalog`) after validating them with `core`:
/// the product code prefix (stored upper case), the scale layouts together with the internal
/// barcode prefix (no overlaps), and the unit list. Audit and outbox `catalog.settings.put`
/// unless nothing changed.
///
/// # Errors
/// [`CatalogDbError::Rule`] or the database error.
pub async fn put_catalog_settings(
    tx: &mut Tx,
    settings: &CatalogSettings,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    let mut new = settings.clone();
    new.product_code_prefix =
        normalize_product_code_prefix(&new.product_code_prefix).map_err(CatalogDbError::rule)?;
    new.internal_barcode_prefix = new.internal_barcode_prefix.trim().to_owned();
    validate_scale_layouts(&new.scale_layouts, &new.internal_barcode_prefix)
        .map_err(CatalogDbError::rule)?;
    validate_unit_list(&new.unit_names).map_err(CatalogDbError::rule)?;
    let old = get_catalog_settings(Conn::Tx(&mut *tx)).await?;
    if old == new {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE catalog_settings SET product_code_prefix = ?, internal_barcode_prefix = ?, \
         scale_layouts = ?, unit_names = ?, updated_at = ? WHERE id = 1",
        args![
            new.product_code_prefix.as_str(),
            new.internal_barcode_prefix.as_str(),
            layouts_json(&new.scale_layouts),
            strings_json(&new.unit_names),
            now,
        ],
    )
    .await?;
    let after = settings_json(&new);
    record(
        tx,
        actor,
        now,
        Change {
            action: "catalog.settings.put",
            target: "catalog_settings".to_owned(),
            before: Some(settings_json(&old)),
            after: Some(after.clone()),
            outbox_kind: "catalog.settings.put",
            payload: after,
        },
    )
    .await?;
    Ok(Written::Updated)
}

const PRODUCT_CODE: &str = "product_code";
const INTERNAL_BARCODE: &str = "internal_barcode";

async fn counter(tx: &mut Tx, name: &str) -> Result<u64> {
    let value = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT next_value FROM catalog_counter WHERE name = ?",
        args![name],
        |row| row.i64("next_value"),
    )
    .await?
    .ok_or_else(|| DbError::Corrupt(format!("catalog_counter `{name}` is missing")))?;
    u64::try_from(value).map_err(|_| DbError::Corrupt(format!("catalog_counter `{name}` < 0")))
}

/// Compare-and-set of a counter: fails with a retryable [`DbError::Conflict`] when another
/// transaction moved it since it was read.
async fn store_counter(tx: &mut Tx, name: &str, read: u64, next: u64) -> Result<()> {
    let as_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE catalog_counter SET next_value = ? WHERE name = ? AND next_value = ?",
        args![as_i64(next), name, as_i64(read)],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::Conflict(format!(
            "catalog counter `{name}` changed concurrently"
        )));
    }
    Ok(())
}

/// The next free product code, advancing the counter in `tx` (spec 2.2 §7).
pub(super) async fn next_product_code_in(tx: &mut Tx) -> CatalogResult<String> {
    let prefix = get_catalog_settings(Conn::Tx(&mut *tx))
        .await?
        .product_code_prefix;
    let read = counter(tx, PRODUCT_CODE).await?;
    let mut n = read;
    loop {
        let generated = next_product_code(&prefix, n, |_| false).map_err(CatalogDbError::rule)?;
        let taken = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT 1 FROM product WHERE code = ?",
            args![generated.value.as_str()],
            |_| Ok(()),
        )
        .await?
        .is_some();
        if !taken {
            store_counter(tx, PRODUCT_CODE, read, generated.next_counter).await?;
            return Ok(generated.value);
        }
        n = generated.next_counter;
    }
}

/// The next free internal EAN-13 barcode, advancing the counter in `tx` (spec 2.2 §7). A code is
/// taken when any barcode row (active or archived) has it.
async fn next_internal_barcode_in(tx: &mut Tx) -> CatalogResult<String> {
    let settings = get_catalog_settings(Conn::Tx(&mut *tx)).await?;
    let read = counter(tx, INTERNAL_BARCODE).await?;
    let mut n = read;
    loop {
        let generated = next_internal_barcode(
            &settings.internal_barcode_prefix,
            n,
            &settings.scale_layouts,
            |_| false,
        )
        .map_err(CatalogDbError::rule)?;
        let taken = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT 1 FROM barcode WHERE code = ? LIMIT 1",
            args![generated.value.as_str()],
            |_| Ok(()),
        )
        .await?
        .is_some();
        if !taken {
            store_counter(tx, INTERNAL_BARCODE, read, generated.next_counter).await?;
            return Ok(generated.value);
        }
        n = generated.next_counter;
    }
}

/// Which generator [`generate`] runs.
#[derive(Clone, Copy)]
enum Generator {
    ProductCode,
    InternalBarcode,
}

async fn generate_once(
    db: &Db,
    generator: Generator,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<String> {
    let mut tx = db.begin().await?;
    let (value, counter_name, action) = match generator {
        Generator::ProductCode => (
            next_product_code_in(&mut tx).await?,
            PRODUCT_CODE,
            "catalog.generate.product_code",
        ),
        Generator::InternalBarcode => (
            next_internal_barcode_in(&mut tx).await?,
            INTERNAL_BARCODE,
            "catalog.generate.barcode",
        ),
    };
    let next = counter(&mut tx, counter_name).await?;
    record(
        &mut tx,
        actor,
        now,
        Change {
            action,
            target: format!("catalog_counter:{counter_name}"),
            before: None,
            after: Some(json!({ "value": value })),
            outbox_kind: "catalog.counter.put",
            payload: json!({ "name": counter_name, "next_value": next }),
        },
    )
    .await?;
    tx.commit().await?;
    Ok(value)
}

async fn generate(
    db: &Db,
    generator: Generator,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<String> {
    let mut attempt = 1;
    loop {
        match generate_once(db, generator, actor, now).await {
            Err(e) if e.is_retryable() && attempt < MAX_ATTEMPTS => attempt += 1,
            result => return result,
        }
    }
}

/// Generates the next free product code (`POST /catalog/generate/product-code`), e.g. `P000123`,
/// in its own transaction, retried after a lost race. The value is reserved (the counter moves
/// past it) but not yet used by a product. Audit `catalog.generate.product_code`, outbox
/// `catalog.counter.put`.
///
/// # Errors
/// [`CatalogDbError::Rule`] (`generator_exhausted`, an invalid prefix) or the database error.
pub async fn generate_product_code(
    db: &Db,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<String> {
    generate(db, Generator::ProductCode, actor, now).await
}

/// Generates the next free internal EAN-13 barcode (`POST /catalog/generate/barcode`), e.g.
/// `2900000000017`, like [`generate_product_code`]. Audit `catalog.generate.barcode`.
///
/// # Errors
/// [`CatalogDbError::Rule`] (`generator_exhausted`, `scale_prefix_overlap`) or the database
/// error.
pub async fn generate_internal_barcode(
    db: &Db,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<String> {
    generate(db, Generator::InternalBarcode, actor, now).await
}
