//! Products as one document: product + options + values + units + variants + barcodes + bundle
//! components (spec 2.2 §2–§5, §9, §12 `PUT /products/{id}`).
//!
//! [`put_product`] diffs the proposed document against the stored one:
//! - the "frozen after use" rules run with the product's used flag (`core::check_frozen`). A
//!   product is used once any variant has `used_at` ([`mark_used`]) or is a component of another
//!   bundle;
//! - a removed variant that is used or is a bundle component is kept, archived; a removed barcode
//!   of such a variant is archived (it stays resolvable for returns, but frees its code); every
//!   other removed row (never-used variants, barcodes, units, options, values) is deleted;
//! - a document equal to the stored one writes nothing ([`Written::Unchanged`]).

use std::collections::{HashMap, HashSet};

use sultan_core::catalog::{
    Barcode, BundleComponent, CatalogError, ComponentInfo, OptionValue, Plu, ProductDraft,
    ProductOption, ProductType, ProductUnits, Qty, Unit, Variant, check_frozen, product_search_key,
    search_key, validate_bundle_components, validate_product,
};
use sultan_core::ids::{
    BarcodeId, BrandId, CategoryId, OptionId, OptionValueId, ProductId, TaxClassId, UnitId,
    VariantId,
};
use sultan_core::time::Timestamp;

use super::json::{
    parse_product_type, parse_strings, parse_tracking, parse_unit_kind, product_json, strings_json,
    unit_kind_str,
};
use super::{Actor, CatalogDbError, CatalogResult, Change, Duplicate, Written, record, settings};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The product document of `PUT /products/{id}`: the `core` draft plus the references `core`
/// does not check (category, brand, tax class).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductDoc {
    /// Product, options, units, variants, barcodes, components.
    pub draft: ProductDraft,
    /// Category; `None` = "Uncategorised".
    pub category: Option<CategoryId>,
    /// Brand.
    pub brand: Option<BrandId>,
    /// Tax class (required).
    pub tax_class: TaxClassId,
}

/// A stored product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductRecord {
    /// The document. Its barcodes are the active ones.
    pub doc: ProductDoc,
    /// Any variant used (sale, stock movement, price list) or a component of another bundle:
    /// the "frozen after use" rules apply.
    pub used: bool,
    /// When the product was archived.
    pub archived_at: Option<Timestamp>,
    /// Barcodes removed from used variants: archived, still resolvable for returns.
    pub archived_barcodes: Vec<Barcode>,
    /// Created.
    pub created_at: Timestamp,
    /// Last changed.
    pub updated_at: Timestamp,
}

/// Filters of [`list_products`] (`GET /products`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProductFilter {
    /// Contains-match on the search key (lower-cased, accents removed by `core::search_key`).
    pub q: Option<String>,
    /// Only this category.
    pub category: Option<CategoryId>,
    /// Only this brand.
    pub brand: Option<BrandId>,
    /// Only this type.
    pub product_type: Option<ProductType>,
    /// `false`: active products only; `true`: archived products only.
    pub archived: bool,
    /// Page cursor: only products with a smaller ID (the last ID of the previous page).
    pub before_id: Option<ProductId>,
    /// Page size, 1–[`MAX_LIST_LIMIT`]; `0` or more means [`MAX_LIST_LIMIT`].
    pub limit: u32,
}

/// Largest page of [`list_products`].
pub const MAX_LIST_LIMIT: u32 = 50;

/// A row of [`list_products`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductSummary {
    /// ID.
    pub id: ProductId,
    /// Product code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Type.
    pub product_type: ProductType,
    /// Category.
    pub category: Option<CategoryId>,
    /// Brand.
    pub brand: Option<BrandId>,
    /// Sellable flag.
    pub sellable: bool,
    /// When archived.
    pub archived_at: Option<Timestamp>,
}

/// A stored product plus what the diff needs.
struct Stored {
    record: ProductRecord,
    /// Variants with `used_at`.
    used: HashSet<VariantId>,
    /// Variants that are components of other bundles.
    referenced: HashSet<VariantId>,
}

impl Stored {
    fn used(&self) -> bool {
        self.record.used
    }

    /// Whether a variant must be archived rather than deleted.
    fn keeps(&self, variant: VariantId) -> bool {
        self.used.contains(&variant) || self.referenced.contains(&variant)
    }

    fn draft(&self) -> &ProductDraft {
        &self.record.doc.draft
    }
}

/// Creates or replaces a product document (`PUT /products/{id}`), atomically and idempotently.
///
/// The document is normalised (`ProductDraft::normalized`); an empty product code keeps the
/// stored one, or is generated for a new product (§7, counter in this transaction). Then:
/// "frozen after use" (stored vs proposed), `core::validate_product`, references (tax class,
/// category, brand; child IDs must not belong to another product), bundle components against the
/// database, and company-wide uniqueness. Writes one audit row (`catalog.product.put`, before and
/// after) and one outbox row (`catalog.product.put`, the full document) unless nothing changed.
/// `draft.archived` archives or unarchives the product. Returns the stored document.
///
/// # Errors
/// [`CatalogDbError::Rule`] (validation, `frozen_after_use`, bundle rules, unknown references),
/// [`CatalogDbError::Duplicate`] (`duplicate_code` / `_sku` / `_barcode` / `_plu` with the
/// conflicting product and variant), or [`CatalogDbError::Db`] — a lost race is retryable.
pub async fn put_product(
    tx: &mut Tx,
    doc: &ProductDoc,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<(Written, ProductDoc)> {
    save(tx, doc.clone(), actor, now, "catalog.product.put").await
}

/// Archives a product and so all its variants (spec 2.2 §9): they cannot be sold, bought or added
/// to bundles, and their SKUs and PLUs are freed. Barcodes stay reserved. Audit action
/// `catalog.product.archive`.
///
/// # Errors
/// [`CatalogDbError::NotFound`], or see [`put_product`].
pub async fn archive_product(
    tx: &mut Tx,
    id: ProductId,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    set_archived(tx, id, true, actor, now, "catalog.product.archive").await
}

/// Unarchives a product (spec 2.2 §9): allowed only while its SKUs, PLUs and barcodes are free.
/// Audit action `catalog.product.unarchive`.
///
/// # Errors
/// [`CatalogDbError::Duplicate`] when another product took a SKU, PLU or barcode;
/// [`CatalogDbError::NotFound`], or see [`put_product`].
pub async fn unarchive_product(
    tx: &mut Tx,
    id: ProductId,
    actor: &Actor,
    now: Timestamp,
) -> CatalogResult<Written> {
    set_archived(tx, id, false, actor, now, "catalog.product.unarchive").await
}

async fn set_archived(
    tx: &mut Tx,
    id: ProductId,
    archived: bool,
    actor: &Actor,
    now: Timestamp,
    action: &str,
) -> CatalogResult<Written> {
    let stored = load(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CatalogDbError::NotFound("product"))?;
    let mut doc = stored.record.doc;
    doc.draft.archived = archived;
    Ok(save(tx, doc, actor, now, action).await?.0)
}

async fn save(
    tx: &mut Tx,
    mut doc: ProductDoc,
    actor: &Actor,
    now: Timestamp,
    action: &str,
) -> CatalogResult<(Written, ProductDoc)> {
    doc.draft = doc.draft.normalized();
    let id = doc.draft.id;
    let stored = load(Conn::Tx(&mut *tx), id).await?;
    if doc.draft.code.is_empty() {
        doc.draft.code = match &stored {
            Some(s) => s.draft().code.clone(),
            None => settings::next_product_code_in(tx).await?,
        };
    }
    if let Some(s) = &stored {
        check_frozen(s.draft(), &doc.draft, s.used()).map_err(CatalogDbError::rule)?;
        for v in &s.draft().variants {
            if s.keeps(v.id) && !doc.draft.variants.iter().any(|n| n.id == v.id) {
                doc.draft.variants.push(Variant {
                    archived: true,
                    ..v.clone()
                });
            }
        }
        if s.record.doc == doc {
            return Ok((Written::Unchanged, doc));
        }
    }
    validate_product(&doc.draft).map_err(CatalogDbError::Rule)?;
    check_references(tx, &doc).await?;
    if doc.draft.product_type == ProductType::Bundle {
        check_components(tx, &doc.draft).await?;
    }
    check_duplicates(tx, &doc.draft).await?;
    let archived_at = match (&stored, doc.draft.archived) {
        (_, false) => None,
        (Some(s), true) => s.record.archived_at.or(Some(now)),
        (None, true) => Some(now),
    };
    write(tx, &doc, stored.as_ref(), archived_at, now).await?;
    let after = product_json(&doc, archived_at);
    record(
        tx,
        actor,
        now,
        Change {
            action,
            target: format!("product:{id}"),
            before: stored
                .as_ref()
                .map(|s| product_json(&s.record.doc, s.record.archived_at)),
            after: Some(after.clone()),
            outbox_kind: "catalog.product.put",
            payload: after,
        },
    )
    .await?;
    let written = if stored.is_some() {
        Written::Updated
    } else {
        Written::Created
    };
    Ok((written, doc))
}

fn invalid(field: &'static str, reason: &'static str) -> CatalogDbError {
    CatalogDbError::rule(CatalogError::InvalidField { field, reason })
}

async fn exists(conn: Conn<'_>, sql: &str, values: Vec<Arg>) -> Result<bool> {
    Ok(sql::fetch_optional(conn, sql, values, |_| Ok(()))
        .await?
        .is_some())
}

/// `?, ?, ?` for `n` values.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

/// Tax class, category and brand exist; no child row ID belongs to another product.
async fn check_references(tx: &mut Tx, doc: &ProductDoc) -> CatalogResult<()> {
    let tax = "SELECT 1 FROM tax_class WHERE id = ?";
    if !exists(Conn::Tx(&mut *tx), tax, args![doc.tax_class]).await? {
        return Err(invalid("tax_class_id", "unknown tax class"));
    }
    if let Some(c) = doc.category {
        let sql = "SELECT 1 FROM category WHERE id = ?";
        if !exists(Conn::Tx(&mut *tx), sql, args![c]).await? {
            return Err(invalid("category_id", "unknown category"));
        }
    }
    if let Some(b) = doc.brand {
        let sql = "SELECT 1 FROM brand WHERE id = ?";
        if !exists(Conn::Tx(&mut *tx), sql, args![b]).await? {
            return Err(invalid("brand_id", "unknown brand"));
        }
    }
    let d = &doc.draft;
    let values: Vec<OptionValueId> = d
        .options
        .iter()
        .flat_map(|o| o.values.iter().map(|v| v.id))
        .collect();
    let checks: [(&'static str, &str, Vec<Arg>); 5] = [
        (
            "units.id",
            "SELECT id FROM product_unit WHERE product_id <> ? AND id IN",
            d.units.units.iter().map(|u| u.id.into()).collect(),
        ),
        (
            "options.id",
            "SELECT id FROM product_option WHERE product_id <> ? AND id IN",
            d.options.iter().map(|o| o.id.into()).collect(),
        ),
        (
            "options.values.id",
            "SELECT v.id FROM product_option_value v JOIN product_option o ON o.id = v.option_id \
             WHERE o.product_id <> ? AND v.id IN",
            values.into_iter().map(Arg::from).collect(),
        ),
        (
            "variants.id",
            "SELECT id FROM variant WHERE product_id <> ? AND id IN",
            d.variants.iter().map(|v| v.id.into()).collect(),
        ),
        (
            "barcodes.id",
            "SELECT id FROM barcode WHERE product_id <> ? AND id IN",
            d.barcodes.iter().map(|b| b.id.into()).collect(),
        ),
    ];
    for (field, prefix, ids) in checks {
        if ids.is_empty() {
            continue;
        }
        let sql = format!("{prefix} ({}) LIMIT 1", placeholders(ids.len()));
        let mut values = args![d.id];
        values.extend(ids);
        if exists(Conn::Tx(&mut *tx), &sql, values).await? {
            return Err(invalid(field, "belongs to another product"));
        }
    }
    Ok(())
}

/// Runs `core::validate_bundle_components` with the components resolved from the database.
async fn check_components(tx: &mut Tx, draft: &ProductDraft) -> CatalogResult<()> {
    let mut infos: HashMap<(VariantId, UnitId), ComponentInfo> = HashMap::new();
    if !draft.components.is_empty() {
        let variants: Vec<Arg> = draft.components.iter().map(|c| c.variant.into()).collect();
        let sql = format!(
            "SELECT v.id AS variant_id, u.id AS unit_id, p.id AS product_id, p.type, \
             p.archived_at, v.archived AS variant_archived, u.archived AS unit_archived, u.kind, \
             u.factor FROM variant v JOIN product p ON p.id = v.product_id \
             JOIN product_unit u ON u.product_id = p.id WHERE v.id IN ({})",
            placeholders(variants.len())
        );
        let rows = sql::fetch_all(Conn::Tx(&mut *tx), &sql, variants, |row| {
            let key = (sql::id(row, "variant_id")?, sql::id(row, "unit_id")?);
            let info = ComponentInfo {
                product: sql::id(row, "product_id")?,
                product_type: parse_product_type(&row.text("type")?)?,
                archived: row.opt_i64("archived_at")?.is_some()
                    || row.bool("variant_archived")?
                    || row.bool("unit_archived")?,
                unit_kind: parse_unit_kind(&row.text("kind")?)?,
                factor: Qty::from_thousandths(row.i64("factor")?),
            };
            Ok((key, info))
        })
        .await?;
        infos.extend(rows);
    }
    for v in &draft.variants {
        for u in &draft.units.units {
            infos.insert(
                (v.id, u.id),
                ComponentInfo {
                    product: draft.id,
                    product_type: draft.product_type,
                    archived: v.archived,
                    unit_kind: u.kind,
                    factor: u.factor,
                },
            );
        }
    }
    validate_bundle_components(draft.id, &draft.components, |v, u| {
        infos.get(&(v, u)).copied()
    })
    .map_err(CatalogDbError::rule)
}

/// Company-wide uniqueness of the product code, active SKUs and PLUs, and barcodes.
async fn check_duplicates(tx: &mut Tx, d: &ProductDraft) -> CatalogResult<()> {
    let other = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT id FROM product WHERE code = ? AND id <> ?",
        args![d.code.as_str(), d.id],
        |row| sql::id::<ProductId>(row, "id"),
    )
    .await?;
    if let Some(product) = other {
        return Err(CatalogDbError::Duplicate(Duplicate {
            error: CatalogError::DuplicateCode {
                code: d.code.clone(),
            },
            product,
            variant: None,
        }));
    }
    if !d.archived {
        let active: Vec<&Variant> = d.variants.iter().filter(|v| !v.archived).collect();
        let skus: Vec<Arg> = active.iter().map(|v| v.sku.as_str().into()).collect();
        let held = taken(tx, "variant", "id", "sku_active", d.id, skus, |r| {
            r.text("sku_active")
        })
        .await?;
        if let Some(v) = active.iter().find(|v| held.contains_key(&v.sku)) {
            let (product, variant) = held[&v.sku];
            return Err(duplicate(
                CatalogError::DuplicateSku { sku: v.sku.clone() },
                product,
                variant,
            ));
        }
        let plus: Vec<Arg> = active
            .iter()
            .filter_map(|v| v.plu.map(|p| i64::from(p.value()).into()))
            .collect();
        let held = taken(tx, "variant", "id", "plu_active", d.id, plus, |r| {
            r.i64("plu_active")
        })
        .await?;
        for plu in active.iter().filter_map(|v| v.plu) {
            if let Some(&(product, variant)) = held.get(&i64::from(plu.value())) {
                return Err(duplicate(
                    CatalogError::DuplicatePlu { plu: plu.value() },
                    product,
                    variant,
                ));
            }
        }
    }
    let codes: Vec<Arg> = d.barcodes.iter().map(|b| b.code.as_str().into()).collect();
    let held = taken(
        tx,
        "barcode",
        "variant_id",
        "code_active",
        d.id,
        codes,
        |r| r.text("code_active"),
    )
    .await?;
    if let Some(b) = d.barcodes.iter().find(|b| held.contains_key(&b.code)) {
        let (product, variant) = held[&b.code];
        return Err(duplicate(
            CatalogError::DuplicateBarcode {
                code: b.code.clone(),
            },
            product,
            variant,
        ));
    }
    Ok(())
}

fn duplicate(error: CatalogError, product: ProductId, variant: VariantId) -> CatalogDbError {
    CatalogDbError::Duplicate(Duplicate {
        error,
        product,
        variant: Some(variant),
    })
}

/// The values of `column` among `values` that other products hold: value → (product, variant).
async fn taken<K: std::hash::Hash + Eq>(
    tx: &mut Tx,
    table: &str,
    variant_col: &str,
    column: &str,
    product: ProductId,
    values: Vec<Arg>,
    key: impl Fn(&dyn Row) -> Result<K>,
) -> Result<HashMap<K, (ProductId, VariantId)>> {
    if values.is_empty() {
        return Ok(HashMap::new());
    }
    let sql = format!(
        "SELECT {column}, product_id, {variant_col} AS variant_id FROM {table} \
         WHERE product_id <> ? AND {column} IN ({})",
        placeholders(values.len())
    );
    let mut all = args![product];
    all.extend(values);
    let rows = sql::fetch_all(Conn::Tx(&mut *tx), &sql, all, |row| {
        Ok((
            key(row)?,
            (sql::id(row, "product_id")?, sql::id(row, "variant_id")?),
        ))
    })
    .await?;
    Ok(rows.into_iter().collect())
}

/// Inserts or updates the row `id` of `table` (child rows without timestamps).
async fn put_row(
    tx: &mut Tx,
    exists: bool,
    table: &str,
    id: Arg,
    columns: &[&str],
    mut values: Vec<Arg>,
) -> Result<()> {
    values.push(id);
    let sql = if exists {
        let sets: Vec<String> = columns.iter().map(|c| format!("{c} = ?")).collect();
        format!("UPDATE {table} SET {} WHERE id = ?", sets.join(", "))
    } else {
        format!(
            "INSERT INTO {table} ({}, id) VALUES ({})",
            columns.join(", "),
            placeholders(columns.len() + 1)
        )
    };
    sql::execute(Conn::Tx(&mut *tx), &sql, values).await?;
    Ok(())
}

async fn delete_by_id(tx: &mut Tx, table: &str, id: Arg) -> Result<()> {
    let sql = format!("DELETE FROM {table} WHERE id = ?");
    sql::execute(Conn::Tx(&mut *tx), &sql, vec![id]).await?;
    Ok(())
}

const PRODUCT_COLUMNS: [&str; 18] = [
    "code",
    "name",
    "receipt_name",
    "type",
    "tracking",
    "category_id",
    "brand_id",
    "tax_class_id",
    "base_unit_id",
    "default_sale_unit_id",
    "default_purchase_unit_id",
    "sellable",
    "purchasable",
    "open_price",
    "requires_note",
    "tags",
    "search_key",
    "archived_at",
];

/// The product's `search_key`, with its brand's name.
pub(super) async fn product_key(conn: Conn<'_>, doc: &ProductDoc) -> Result<String> {
    let brand = match doc.brand {
        Some(b) => {
            sql::fetch_optional(conn, "SELECT name FROM brand WHERE id = ?", args![b], |r| {
                r.text("name")
            })
            .await?
        }
        None => None,
    };
    let d = &doc.draft;
    Ok(product_search_key(
        &d.name,
        &d.receipt_name,
        &d.code,
        brand.as_deref(),
    ))
}

#[allow(clippy::too_many_lines)] // one block per child table, in dependency order
async fn write(
    tx: &mut Tx,
    doc: &ProductDoc,
    stored: Option<&Stored>,
    archived_at: Option<Timestamp>,
    now: Timestamp,
) -> Result<()> {
    let d = &doc.draft;
    let id = d.id;
    let key = product_key(Conn::Tx(&mut *tx), doc).await?;
    // Release this product's unique keys, so values can move between its own rows.
    if stored.is_some() {
        for sql in [
            "UPDATE variant SET sku_active = NULL, plu_active = NULL WHERE product_id = ?",
            "UPDATE barcode SET code_active = NULL WHERE product_id = ?",
        ] {
            sql::execute(Conn::Tx(&mut *tx), sql, args![id]).await?;
        }
    }
    let mut values = args![
        d.code.as_str(),
        d.name.as_str(),
        d.receipt_name.as_str(),
        d.product_type.as_str(),
        d.tracking.as_str(),
        doc.category,
        doc.brand,
        doc.tax_class,
        d.units.base,
        d.units.default_sale,
        d.units.default_purchase,
        d.sellable,
        d.purchasable,
        d.open_price,
        d.requires_note,
        strings_json(&d.tags),
        key,
        archived_at,
        now,
    ];
    let mut columns: Vec<&str> = PRODUCT_COLUMNS.to_vec();
    columns.push("updated_at");
    if stored.is_none() {
        columns.push("created_at");
        values.push(now.into());
    }
    put_row(tx, stored.is_some(), "product", id.into(), &columns, values).await?;
    for sql in [
        "DELETE FROM bundle_component WHERE bundle_id = ?",
        "DELETE FROM variant_option_value WHERE variant_id IN \
         (SELECT id FROM variant WHERE product_id = ?)",
    ] {
        sql::execute(Conn::Tx(&mut *tx), sql, args![id]).await?;
    }

    let mut stored_units = HashSet::new();
    let mut stored_options = HashSet::new();
    let mut stored_values = HashSet::new();
    let mut stored_variants = HashSet::new();
    let mut stored_barcodes = HashSet::new();
    if let Some(s) = stored {
        let old = s.draft();
        let new_barcodes: HashSet<BarcodeId> = d.barcodes.iter().map(|b| b.id).collect();
        let new_variants: HashSet<VariantId> = d.variants.iter().map(|v| v.id).collect();
        for b in &old.barcodes {
            if new_barcodes.contains(&b.id) {
                stored_barcodes.insert(b.id);
            } else if s.keeps(b.variant) {
                sql::execute(
                    Conn::Tx(&mut *tx),
                    "UPDATE barcode SET archived = ? WHERE id = ?",
                    args![true, b.id],
                )
                .await?;
            } else {
                delete_by_id(tx, "barcode", b.id.into()).await?;
            }
        }
        for b in &s.record.archived_barcodes {
            if new_barcodes.contains(&b.id) {
                stored_barcodes.insert(b.id);
            } else if !new_variants.contains(&b.variant) || d.units.get(b.unit).is_none() {
                delete_by_id(tx, "barcode", b.id.into()).await?;
            }
        }
        for v in &old.variants {
            if new_variants.contains(&v.id) {
                stored_variants.insert(v.id);
            } else {
                delete_by_id(tx, "variant", v.id.into()).await?;
            }
        }
        let new_values: HashSet<OptionValueId> = d
            .options
            .iter()
            .flat_map(|o| o.values.iter().map(|v| v.id))
            .collect();
        for o in &old.options {
            for v in &o.values {
                if new_values.contains(&v.id) {
                    stored_values.insert(v.id);
                } else {
                    delete_by_id(tx, "product_option_value", v.id.into()).await?;
                }
            }
        }
        for o in &old.options {
            if d.options.iter().any(|n| n.id == o.id) {
                stored_options.insert(o.id);
            } else {
                delete_by_id(tx, "product_option", o.id.into()).await?;
            }
        }
        for u in &old.units.units {
            if d.units.get(u.id).is_some() {
                stored_units.insert(u.id);
            } else {
                delete_by_id(tx, "product_unit", u.id.into()).await?;
            }
        }
    }

    for (i, u) in d.units.units.iter().enumerate() {
        put_row(
            tx,
            stored_units.contains(&u.id),
            "product_unit",
            u.id.into(),
            &[
                "product_id",
                "position",
                "name",
                "short_name",
                "kind",
                "factor",
                "sellable",
                "purchasable",
                "archived",
            ],
            args![
                id,
                position(i),
                u.name.as_str(),
                u.short_name.as_str(),
                unit_kind_str(u.kind),
                u.factor.thousandths(),
                u.sellable,
                u.purchasable,
                u.archived,
            ],
        )
        .await?;
    }
    for (i, o) in d.options.iter().enumerate() {
        put_row(
            tx,
            stored_options.contains(&o.id),
            "product_option",
            o.id.into(),
            &["product_id", "position", "name"],
            args![id, position(i), o.name.as_str()],
        )
        .await?;
        for (j, v) in o.values.iter().enumerate() {
            put_row(
                tx,
                stored_values.contains(&v.id),
                "product_option_value",
                v.id.into(),
                &["option_id", "position", "name", "code"],
                args![o.id, position(j), v.name.as_str(), v.code.as_str()],
            )
            .await?;
        }
    }
    for (i, v) in d.variants.iter().enumerate() {
        let active = !d.archived && !v.archived;
        let plu = v.plu.map(|p| i64::from(p.value()));
        put_row(
            tx,
            stored_variants.contains(&v.id),
            "variant",
            v.id.into(),
            &[
                "product_id",
                "position",
                "sku",
                "sku_active",
                "plu",
                "plu_active",
                "archived",
            ],
            args![
                id,
                position(i),
                v.sku.as_str(),
                active.then_some(v.sku.as_str()),
                plu,
                plu.filter(|_| active),
                v.archived,
            ],
        )
        .await?;
        for (j, value) in v.values.iter().enumerate() {
            sql::execute(
                Conn::Tx(&mut *tx),
                "INSERT INTO variant_option_value (variant_id, position, option_value_id) \
                 VALUES (?, ?, ?)",
                args![v.id, position(j), *value],
            )
            .await?;
        }
    }
    for (i, b) in d.barcodes.iter().enumerate() {
        put_row(
            tx,
            stored_barcodes.contains(&b.id),
            "barcode",
            b.id.into(),
            &[
                "product_id",
                "variant_id",
                "unit_id",
                "position",
                "code",
                "code_active",
                "allow_invalid_check_digit",
                "archived",
            ],
            args![
                id,
                b.variant,
                b.unit,
                position(i),
                b.code.as_str(),
                b.code.as_str(),
                b.allow_invalid_check_digit,
                false,
            ],
        )
        .await?;
    }
    for (i, c) in d.components.iter().enumerate() {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO bundle_component (bundle_id, position, variant_id, unit_id, qty) \
             VALUES (?, ?, ?, ?, ?)",
            args![id, position(i), c.variant, c.unit, c.qty.thousandths()],
        )
        .await?;
    }
    Ok(())
}

fn position(i: usize) -> i64 {
    i64::try_from(i).unwrap_or(i64::MAX)
}

/// One product with its whole document; `None` if it does not exist.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for unreadable stored data.
pub async fn get_product<'c>(
    conn: impl Into<Conn<'c>>,
    id: ProductId,
) -> Result<Option<ProductRecord>> {
    Ok(load(conn.into(), id).await?.map(|s| s.record))
}

struct ProductRow {
    record: ProductRecord,
    base: UnitId,
    default_sale: UnitId,
    default_purchase: UnitId,
}

fn map_product(row: &dyn Row) -> Result<ProductRow> {
    let tags = sql::opt_json(row, "tags")?.unwrap_or(serde_json::Value::Null);
    let archived_at = sql::opt_ts(row, "archived_at")?;
    let draft = ProductDraft {
        id: sql::id(row, "id")?,
        code: row.text("code")?,
        name: row.text("name")?,
        receipt_name: row.text("receipt_name")?,
        product_type: parse_product_type(&row.text("type")?)?,
        tracking: parse_tracking(&row.text("tracking")?)?,
        tags: parse_strings(&tags, "tags")?,
        sellable: row.bool("sellable")?,
        purchasable: row.bool("purchasable")?,
        open_price: row.bool("open_price")?,
        requires_note: row.bool("requires_note")?,
        options: Vec::new(),
        units: ProductUnits {
            base: sql::id(row, "base_unit_id")?,
            units: Vec::new(),
            default_sale: sql::id(row, "default_sale_unit_id")?,
            default_purchase: sql::id(row, "default_purchase_unit_id")?,
        },
        variants: Vec::new(),
        barcodes: Vec::new(),
        components: Vec::new(),
        archived: archived_at.is_some(),
    };
    Ok(ProductRow {
        base: draft.units.base,
        default_sale: draft.units.default_sale,
        default_purchase: draft.units.default_purchase,
        record: ProductRecord {
            doc: ProductDoc {
                draft,
                category: sql::opt_id(row, "category_id")?,
                brand: sql::opt_id(row, "brand_id")?,
                tax_class: sql::id(row, "tax_class_id")?,
            },
            used: false,
            archived_at,
            archived_barcodes: Vec::new(),
            created_at: sql::ts(row, "created_at")?,
            updated_at: sql::ts(row, "updated_at")?,
        },
    })
}

pub(super) fn plu_from(value: Option<i64>) -> Result<Option<Plu>> {
    value
        .map(|n| {
            u32::try_from(n)
                .ok()
                .and_then(Plu::new)
                .ok_or_else(|| DbError::Corrupt(format!("invalid stored PLU {n}")))
        })
        .transpose()
}

#[allow(clippy::too_many_lines)] // one query per child table
async fn load(mut conn: Conn<'_>, id: ProductId) -> Result<Option<Stored>> {
    let row = sql::fetch_optional(
        conn.reborrow(),
        "SELECT id, code, name, receipt_name, type, tracking, category_id, brand_id, \
         tax_class_id, base_unit_id, default_sale_unit_id, default_purchase_unit_id, sellable, \
         purchasable, open_price, requires_note, tags, archived_at, created_at, updated_at \
         FROM product WHERE id = ?",
        args![id],
        map_product,
    )
    .await?;
    let Some(ProductRow {
        mut record,
        base,
        default_sale,
        default_purchase,
    }) = row
    else {
        return Ok(None);
    };
    let units = sql::fetch_all(
        conn.reborrow(),
        "SELECT id, name, short_name, kind, factor, sellable, purchasable, archived \
         FROM product_unit WHERE product_id = ? ORDER BY position",
        args![id],
        |row| {
            Ok(Unit {
                id: sql::id(row, "id")?,
                name: row.text("name")?,
                short_name: row.text("short_name")?,
                kind: parse_unit_kind(&row.text("kind")?)?,
                factor: Qty::from_thousandths(row.i64("factor")?),
                sellable: row.bool("sellable")?,
                purchasable: row.bool("purchasable")?,
                archived: row.bool("archived")?,
            })
        },
    )
    .await?;
    let mut options = sql::fetch_all(
        conn.reborrow(),
        "SELECT id, name FROM product_option WHERE product_id = ? ORDER BY position",
        args![id],
        |row| {
            Ok(ProductOption {
                id: sql::id(row, "id")?,
                name: row.text("name")?,
                values: Vec::new(),
            })
        },
    )
    .await?;
    let values = sql::fetch_all(
        conn.reborrow(),
        "SELECT v.id, v.option_id, v.name, v.code FROM product_option_value v \
         JOIN product_option o ON o.id = v.option_id WHERE o.product_id = ? \
         ORDER BY o.position, v.position",
        args![id],
        |row| {
            Ok((
                sql::id::<OptionId>(row, "option_id")?,
                OptionValue {
                    id: sql::id(row, "id")?,
                    name: row.text("name")?,
                    code: row.text("code")?,
                },
            ))
        },
    )
    .await?;
    for (option, value) in values {
        if let Some(o) = options.iter_mut().find(|o| o.id == option) {
            o.values.push(value);
        }
    }
    let mut used = HashSet::new();
    let mut variants = sql::fetch_all(
        conn.reborrow(),
        "SELECT id, sku, plu, archived, used_at FROM variant WHERE product_id = ? \
         ORDER BY position",
        args![id],
        |row| {
            let variant = Variant {
                id: sql::id(row, "id")?,
                sku: row.text("sku")?,
                values: Vec::new(),
                plu: plu_from(row.opt_i64("plu")?)?,
                archived: row.bool("archived")?,
            };
            Ok((variant, row.opt_i64("used_at")?.is_some()))
        },
    )
    .await?
    .into_iter()
    .map(|(v, is_used)| {
        if is_used {
            used.insert(v.id);
        }
        v
    })
    .collect::<Vec<_>>();
    let links = sql::fetch_all(
        conn.reborrow(),
        "SELECT x.variant_id, x.option_value_id FROM variant_option_value x \
         JOIN variant v ON v.id = x.variant_id WHERE v.product_id = ? \
         ORDER BY x.variant_id, x.position",
        args![id],
        |row| {
            Ok((
                sql::id::<VariantId>(row, "variant_id")?,
                sql::id::<OptionValueId>(row, "option_value_id")?,
            ))
        },
    )
    .await?;
    for (variant, value) in links {
        if let Some(v) = variants.iter_mut().find(|v| v.id == variant) {
            v.values.push(value);
        }
    }
    let barcodes = sql::fetch_all(
        conn.reborrow(),
        "SELECT id, variant_id, unit_id, code, allow_invalid_check_digit, archived \
         FROM barcode WHERE product_id = ? ORDER BY position, id",
        args![id],
        |row| {
            let barcode = Barcode {
                id: sql::id(row, "id")?,
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
                code: row.text("code")?,
                allow_invalid_check_digit: row.bool("allow_invalid_check_digit")?,
            };
            Ok((barcode, row.bool("archived")?))
        },
    )
    .await?;
    let components = sql::fetch_all(
        conn.reborrow(),
        "SELECT variant_id, unit_id, qty FROM bundle_component WHERE bundle_id = ? \
         ORDER BY position",
        args![id],
        |row| {
            Ok(BundleComponent {
                variant: sql::id(row, "variant_id")?,
                unit: sql::id(row, "unit_id")?,
                qty: Qty::from_thousandths(row.i64("qty")?),
            })
        },
    )
    .await?;
    let referenced: HashSet<VariantId> = sql::fetch_all(
        conn,
        "SELECT DISTINCT c.variant_id FROM bundle_component c \
         JOIN variant v ON v.id = c.variant_id WHERE v.product_id = ? AND c.bundle_id <> ?",
        args![id, id],
        |row| sql::id(row, "variant_id"),
    )
    .await?
    .into_iter()
    .collect();

    let draft = &mut record.doc.draft;
    draft.units = ProductUnits {
        base,
        units,
        default_sale,
        default_purchase,
    };
    draft.options = options;
    draft.variants = variants;
    for (barcode, archived) in barcodes {
        if archived {
            record.archived_barcodes.push(barcode);
        } else {
            draft.barcodes.push(barcode);
        }
    }
    draft.components = components;
    record.used = !used.is_empty() || !referenced.is_empty();
    Ok(Some(Stored {
        record,
        used,
        referenced,
    }))
}

/// Products matching `filter`, newest first (`GET /products`). The text filter matches anywhere
/// in the search key (name, receipt name, code, brand).
///
/// # Errors
/// The database error.
pub async fn list_products<'c>(
    conn: impl Into<Conn<'c>>,
    filter: &ProductFilter,
) -> Result<Vec<ProductSummary>> {
    let mut conditions = vec![if filter.archived {
        "archived_at IS NOT NULL"
    } else {
        "archived_at IS NULL"
    }];
    let mut values: Vec<Arg> = Vec::new();
    if let Some(q) = filter
        .q
        .as_deref()
        .map(search_key)
        .filter(|q| !q.is_empty())
    {
        conditions.push("search_key LIKE ? ESCAPE '!'");
        values.push(format!("%{}%", escape_like(&q)).into());
    }
    if let Some(c) = filter.category {
        conditions.push("category_id = ?");
        values.push(c.into());
    }
    if let Some(b) = filter.brand {
        conditions.push("brand_id = ?");
        values.push(b.into());
    }
    if let Some(t) = filter.product_type {
        conditions.push("type = ?");
        values.push(t.as_str().into());
    }
    if let Some(id) = filter.before_id {
        conditions.push("id < ?");
        values.push(id.into());
    }
    values.push(i64::from(clamp_limit(filter.limit, MAX_LIST_LIMIT)).into());
    let sql = format!(
        "SELECT id, code, name, type, category_id, brand_id, sellable, archived_at FROM product \
         WHERE {} ORDER BY id DESC LIMIT ?",
        conditions.join(" AND ")
    );
    sql::fetch_all(conn.into(), &sql, values, |row| {
        Ok(ProductSummary {
            id: sql::id(row, "id")?,
            code: row.text("code")?,
            name: row.text("name")?,
            product_type: parse_product_type(&row.text("type")?)?,
            category: sql::opt_id(row, "category_id")?,
            brand: sql::opt_id(row, "brand_id")?,
            sellable: row.bool("sellable")?,
            archived_at: sql::opt_ts(row, "archived_at")?,
        })
    })
    .await
}

/// `limit` within 1..=`max`; `0` means `max`.
pub(super) fn clamp_limit(limit: u32, max: u32) -> u32 {
    if limit == 0 { max } else { limit.min(max) }
}

/// Escapes `LIKE` wildcards with `!` (used with `ESCAPE '!'`, which reads the same on both
/// backends; a backslash would need doubling on `MySQL` only).
pub(super) fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(c);
    }
    out
}

/// Marks variants as used (first sale, stock movement or price-list entry; called by later
/// phases in their own transaction). From then on the product's "frozen after use" rules apply
/// and removed variants are archived instead of deleted. Already-used variants keep their first
/// time. Returns the number of variants marked. Not audited and not sent to the outbox: the
/// referencing document (sale, movement) is.
///
/// # Errors
/// The database error.
pub async fn mark_used<'c>(
    conn: impl Into<Conn<'c>>,
    variants: &[VariantId],
    now: Timestamp,
) -> Result<u64> {
    if variants.is_empty() {
        return Ok(0);
    }
    let mut values = args![now];
    values.extend(variants.iter().map(|v| Arg::from(*v)));
    sql::execute(
        conn.into(),
        &format!(
            "UPDATE variant SET used_at = ? WHERE used_at IS NULL AND id IN ({})",
            placeholders(variants.len())
        ),
        values,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_escaping_and_limits() {
        assert_eq!(escape_like("a%b_c!d"), "a!%b!_c!!d");
        assert_eq!(clamp_limit(0, 50), 50);
        assert_eq!(clamp_limit(10, 50), 10);
        assert_eq!(clamp_limit(99, 50), 50);
        assert_eq!(position(3), 3);
        assert!(matches!(plu_from(Some(-1)), Err(DbError::Corrupt(_))));
        assert_eq!(plu_from(None).unwrap(), None);
    }
}
