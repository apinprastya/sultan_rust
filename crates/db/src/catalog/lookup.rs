//! Register lookup and name search (spec 2.2 §6, §10: `GET /catalog/lookup`,
//! `GET /catalog/search`).
//!
//! The resolution order is `core::resolve_lookup`, which reads through the synchronous
//! `CatalogIndex` trait. [`load_index`] therefore prefetches, in a few queries, every row the
//! resolution can ask for (the barcode as entered and normalised, the SKU, the PLUs a scale code
//! or a short number may carry) into a [`DbCatalogIndex`].

use std::cell::Cell;
use std::collections::HashMap;

use sultan_core::catalog::{
    CatalogError, CatalogIndex, ItemRef, ItemStatus, MatchSource, Plu, ProductType, Qty,
    ScaleLayout, check_sellable, resolve_lookup, search_key, variant_display_name,
};
use sultan_core::ids::{BranchId, ProductId, UnitId, VariantId};
use sultan_core::money::Money;

use super::json::parse_product_type;
use super::product::{clamp_limit, escape_like, plu_from};
use super::{CatalogDbError, CatalogResult, settings};
use crate::conn::Conn;
use crate::error::Result;
use crate::sql::{self, Arg, args};

/// The catalog index for one lookup input, prefetched from the database. Active rows win over
/// archived ones with the same code; an archived match still resolves, so the lookup can answer
/// `archived` (old scans stay resolvable for returns, spec 2.2 §6).
#[derive(Debug, Default)]
pub struct DbCatalogIndex {
    barcodes: HashMap<String, (ItemRef, bool)>,
    skus: HashMap<String, ItemRef>,
    plus: HashMap<Plu, ItemRef>,
    last_barcode_archived: Cell<bool>,
}

impl CatalogIndex for DbCatalogIndex {
    fn by_barcode(&self, code: &str) -> Option<ItemRef> {
        let (item, archived) = *self.barcodes.get(code)?;
        self.last_barcode_archived.set(archived);
        Some(item)
    }

    fn by_sku(&self, sku: &str) -> Option<ItemRef> {
        self.skus.get(sku).copied()
    }

    fn by_plu(&self, plu: Plu) -> Option<ItemRef> {
        self.plus.get(&plu).copied()
    }
}

fn in_list(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// The PLUs `resolve_lookup` may ask for: those of matching scale layouts, and the input itself
/// when it is 1–6 digits.
fn plu_candidates(code: &str, all_digits: bool, layouts: &[ScaleLayout]) -> Vec<i64> {
    let mut plus: Vec<i64> = Vec::new();
    if all_digits && code.len() == 13 {
        for layout in layouts {
            let end = 2 + usize::from(layout.plu_digits);
            if code[..2] == layout.prefix.to_string()
                && let Ok(plu) = code[2..end.min(13)].parse::<i64>()
            {
                plus.push(plu);
            }
        }
    }
    if all_digits
        && code.len() <= 6
        && let Ok(plu) = code.parse::<i64>()
    {
        plus.push(plu);
    }
    plus
}

/// Prefetches the index rows `resolve_lookup(input, layouts, …)` can ask for.
///
/// # Errors
/// The database error.
pub async fn load_index<'c>(
    conn: impl Into<Conn<'c>>,
    input: &str,
    layouts: &[ScaleLayout],
) -> Result<DbCatalogIndex> {
    let mut conn = conn.into();
    let mut index = DbCatalogIndex::default();
    let code = input.trim();
    if code.is_empty() {
        return Ok(index);
    }
    let all_digits = code.bytes().all(|b| b.is_ascii_digit());
    let mut codes = vec![code.to_owned()];
    if all_digits && code.len() == 12 {
        codes.push(format!("0{code}"));
    } else if all_digits && code.len() == 13 && code.starts_with('0') {
        codes.push(code[1..].to_owned());
    }
    let sql = format!(
        "SELECT code, variant_id, unit_id, archived FROM barcode WHERE code IN ({}) \
         ORDER BY archived DESC, id",
        in_list(codes.len())
    );
    let rows = sql::fetch_all(
        conn.reborrow(),
        &sql,
        codes.into_iter().map(Arg::from).collect(),
        |row| {
            let item = ItemRef {
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
            };
            Ok((row.text("code")?, item, row.bool("archived")?))
        },
    )
    .await?;
    for (code, item, archived) in rows {
        index.barcodes.insert(code, (item, archived));
    }

    let sku = code.to_ascii_uppercase();
    let rows = sql::fetch_all(
        conn.reborrow(),
        "SELECT v.id, v.sku_active, p.default_sale_unit_id FROM variant v \
         JOIN product p ON p.id = v.product_id WHERE v.sku = ? ORDER BY v.id",
        args![sku.as_str()],
        |row| {
            let item = ItemRef {
                variant: sql::id(row, "id")?,
                unit: sql::id(row, "default_sale_unit_id")?,
            };
            Ok((item, row.opt_text("sku_active")?.is_some()))
        },
    )
    .await?;
    if let Some((item, _)) = rows
        .iter()
        .find(|(_, active)| *active)
        .or_else(|| rows.first())
    {
        index.skus.insert(sku, *item);
    }

    let plus = plu_candidates(code, all_digits, layouts);
    if !plus.is_empty() {
        let sql = format!(
            "SELECT v.id, v.plu, v.plu_active, p.base_unit_id FROM variant v \
             JOIN product p ON p.id = v.product_id \
             WHERE p.type = 'weighed' AND v.plu IN ({}) ORDER BY v.id",
            in_list(plus.len())
        );
        let rows = sql::fetch_all(
            conn,
            &sql,
            plus.into_iter().map(Arg::from).collect(),
            |row| {
                let item = ItemRef {
                    variant: sql::id(row, "id")?,
                    unit: sql::id(row, "base_unit_id")?,
                };
                let plu = plu_from(row.opt_i64("plu")?)?;
                Ok((plu, item, row.opt_i64("plu_active")?.is_some()))
            },
        )
        .await?;
        // Archived first, so an active variant with the same PLU overwrites it.
        for active_pass in [false, true] {
            for (plu, item, active) in &rows {
                if *active == active_pass
                    && let Some(plu) = plu
                {
                    index.plus.insert(*plu, *item);
                }
            }
        }
    }
    Ok(index)
}

/// A register lookup result (spec 2.2 §6 "Result").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupItem {
    /// The product.
    pub product: ProductId,
    /// The variant.
    pub variant: VariantId,
    /// The unit (from the barcode, else the default sale unit; the base unit for a PLU).
    pub unit: UnitId,
    /// Display name, `<product> – <values>`.
    pub name: String,
    /// The variant SKU.
    pub sku: String,
    /// Product type.
    pub product_type: ProductType,
    /// The cashier types the price.
    pub open_price: bool,
    /// The register asks for a line note.
    pub requires_note: bool,
    /// Which lookup step matched.
    pub source: MatchSource,
    /// Quantity: 1, or the weight of a weight scale code; `None` for a price scale code.
    pub qty: Option<Qty>,
    /// Line amount of a price scale code.
    pub amount: Option<Money>,
}

/// What the back office shows for a variant of another product (bundle component lines):
/// its product, display name `<product> – <values>`, SKU and the product's active units.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantLabel {
    /// The product.
    pub product: ProductId,
    /// Display name.
    pub name: String,
    /// SKU.
    pub sku: String,
    /// The product's units (archived ones included) as `(id, name)`, in position order.
    pub units: Vec<(UnitId, String)>,
}

/// Labels of `variants` (unknown IDs are left out).
///
/// # Errors
/// The database error.
pub async fn variant_labels<'c>(
    conn: impl Into<Conn<'c>>,
    variants: &[VariantId],
) -> Result<HashMap<VariantId, VariantLabel>> {
    let mut conn = conn.into();
    let mut labels: HashMap<VariantId, VariantLabel> = HashMap::new();
    if variants.is_empty() {
        return Ok(labels);
    }
    let sql = format!(
        "SELECT v.id AS variant_id, v.sku, p.id AS product_id, p.name, u.id AS unit_id, \
         u.name AS unit_name FROM variant v JOIN product p ON p.id = v.product_id \
         JOIN product_unit u ON u.product_id = p.id WHERE v.id IN ({}) \
         ORDER BY v.id, u.position",
        in_list(variants.len())
    );
    let rows = sql::fetch_all(
        conn.reborrow(),
        &sql,
        variants.iter().map(|v| Arg::from(*v)).collect(),
        |row| {
            Ok((
                sql::id::<VariantId>(row, "variant_id")?,
                sql::id::<ProductId>(row, "product_id")?,
                row.text("name")?,
                row.text("sku")?,
                (sql::id::<UnitId>(row, "unit_id")?, row.text("unit_name")?),
            ))
        },
    )
    .await?;
    for (variant, product, name, sku, unit) in rows {
        labels
            .entry(variant)
            .or_insert_with(|| VariantLabel {
                product,
                name,
                sku,
                units: Vec::new(),
            })
            .units
            .push(unit);
    }
    let found: Vec<VariantId> = labels.keys().copied().collect();
    let names = value_names(conn, &found).await?;
    for (variant, label) in &mut labels {
        label.name = display_name(&label.name, names.get(variant));
    }
    Ok(labels)
}

/// Value names of `variants`, in option order.
async fn value_names(
    conn: Conn<'_>,
    variants: &[VariantId],
) -> Result<HashMap<VariantId, Vec<String>>> {
    let mut names: HashMap<VariantId, Vec<String>> = HashMap::new();
    if variants.is_empty() {
        return Ok(names);
    }
    let sql = format!(
        "SELECT x.variant_id, ov.name FROM variant_option_value x \
         JOIN product_option_value ov ON ov.id = x.option_value_id \
         WHERE x.variant_id IN ({}) ORDER BY x.variant_id, x.position",
        in_list(variants.len())
    );
    let rows = sql::fetch_all(
        conn,
        &sql,
        variants.iter().map(|v| Arg::from(*v)).collect(),
        |row| Ok((sql::id::<VariantId>(row, "variant_id")?, row.text("name")?)),
    )
    .await?;
    for (variant, name) in rows {
        names.entry(variant).or_default().push(name);
    }
    Ok(names)
}

fn display_name(product: &str, values: Option<&Vec<String>>) -> String {
    let values: Vec<&str> = values
        .map(|v| v.iter().map(String::as_str).collect())
        .unwrap_or_default();
    variant_display_name(product, &values)
}

/// Resolves a scanned or typed code at `branch` (`GET /catalog/lookup?code=`): the `core` order
/// (exact barcode, UPC-A ↔ EAN-13, scale layouts from the settings, SKU, PLU), then
/// `core::check_sellable` — archived product / variant / unit or a removed (archived) barcode →
/// `archived`, blocked at the branch → `not_sold_here`, not sellable → `not_sellable`.
///
/// # Errors
/// [`CatalogDbError::Rule`] with `not_found`, `scale_value_zero`, `archived`, `not_sold_here` or
/// `not_sellable`, or the database error.
pub async fn lookup<'c>(
    conn: impl Into<Conn<'c>>,
    code: &str,
    branch: BranchId,
) -> CatalogResult<LookupItem> {
    let mut conn = conn.into();
    let layouts = settings::get_catalog_settings(conn.reborrow())
        .await?
        .scale_layouts;
    let index = load_index(conn.reborrow(), code, &layouts).await?;
    let found = resolve_lookup(code, &layouts, &index).map_err(CatalogDbError::rule)?;
    let barcode_archived = matches!(
        found.source,
        MatchSource::Barcode | MatchSource::NormalizedBarcode
    ) && index.last_barcode_archived.get();
    let row = sql::fetch_optional(
        conn.reborrow(),
        "SELECT p.id, p.name, p.type, p.open_price, p.requires_note, p.sellable, p.archived_at, \
         v.sku, v.archived, u.archived AS unit_archived, \
         (SELECT COUNT(*) FROM product_branch_block b \
          WHERE b.product_id = p.id AND b.branch_id = ?) AS blocked \
         FROM variant v JOIN product p ON p.id = v.product_id \
         JOIN product_unit u ON u.id = ? WHERE v.id = ?",
        args![branch, found.item.unit, found.item.variant],
        |row| {
            let status = ItemStatus {
                product_archived: row.opt_i64("archived_at")?.is_some(),
                variant_archived: row.bool("archived")? || row.bool("unit_archived")?,
                blocked_at_branch: row.i64("blocked")? > 0,
                sellable: row.bool("sellable")?,
            };
            let item = LookupItem {
                product: sql::id(row, "id")?,
                variant: found.item.variant,
                unit: found.item.unit,
                name: row.text("name")?,
                sku: row.text("sku")?,
                product_type: parse_product_type(&row.text("type")?)?,
                open_price: row.bool("open_price")?,
                requires_note: row.bool("requires_note")?,
                source: found.source,
                qty: found.qty,
                amount: found.amount,
            };
            Ok((item, status))
        },
    )
    .await?;
    let (mut item, mut status) = row.ok_or_else(|| CatalogDbError::rule(CatalogError::NotFound))?;
    status.variant_archived |= barcode_archived;
    check_sellable(status).map_err(CatalogDbError::rule)?;
    let names = value_names(conn, &[item.variant]).await?;
    item.name = display_name(&item.name, names.get(&item.variant));
    Ok(item)
}

/// Largest page of [`search`].
pub const MAX_SEARCH_LIMIT: u32 = 50;

/// A register search result: one sellable variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// The product.
    pub product: ProductId,
    /// The variant.
    pub variant: VariantId,
    /// The default sale unit.
    pub unit: UnitId,
    /// Display name, `<product> – <values>`.
    pub name: String,
    /// The variant SKU.
    pub sku: String,
    /// Product type.
    pub product_type: ProductType,
    /// The cashier types the price.
    pub open_price: bool,
    /// The register asks for a line note.
    pub requires_note: bool,
}

/// Name search for the register (`GET /catalog/search?q=`): variants whose product search key
/// contains `q` (folded with `core::search_key`), prefix matches first, then by search key.
/// Archived products and variants, products blocked at `branch` and products that are not
/// sellable are hidden. At most [`MAX_SEARCH_LIMIT`] hits (`0` = the maximum); an empty query
/// finds nothing.
///
/// # Errors
/// The database error.
pub async fn search<'c>(
    conn: impl Into<Conn<'c>>,
    q: &str,
    branch: BranchId,
    limit: u32,
) -> Result<Vec<SearchHit>> {
    let mut conn = conn.into();
    let key = search_key(q);
    if key.is_empty() {
        return Ok(Vec::new());
    }
    let escaped = escape_like(&key);
    let hits = sql::fetch_all(
        conn.reborrow(),
        "SELECT p.id, p.name, p.type, p.open_price, p.requires_note, p.default_sale_unit_id, \
         v.id AS variant_id, v.sku FROM product p JOIN variant v ON v.product_id = p.id \
         WHERE p.search_key LIKE ? ESCAPE '!' AND p.archived_at IS NULL AND v.archived = 0 \
         AND p.sellable = 1 AND NOT EXISTS (SELECT 1 FROM product_branch_block b \
         WHERE b.product_id = p.id AND b.branch_id = ?) \
         ORDER BY CASE WHEN p.search_key LIKE ? ESCAPE '!' THEN 0 ELSE 1 END, p.search_key, \
         p.id, v.position LIMIT ?",
        args![
            format!("%{escaped}%"),
            branch,
            format!("{escaped}%"),
            i64::from(clamp_limit(limit, MAX_SEARCH_LIMIT)),
        ],
        |row| {
            Ok(SearchHit {
                product: sql::id(row, "id")?,
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "default_sale_unit_id")?,
                name: row.text("name")?,
                sku: row.text("sku")?,
                product_type: parse_product_type(&row.text("type")?)?,
                open_price: row.bool("open_price")?,
                requires_note: row.bool("requires_note")?,
            })
        },
    )
    .await?;
    let variants: Vec<VariantId> = hits.iter().map(|h| h.variant).collect();
    let names = value_names(conn, &variants).await?;
    Ok(hits
        .into_iter()
        .map(|mut h| {
            h.name = display_name(&h.name, names.get(&h.variant));
            h
        })
        .collect())
}
