//! Products (spec 2.2 §2–§5, §9, §10, §12): `GET /products`, `GET|PUT /products/{id}`,
//! `POST /products/{id}/archive|unarchive`, `GET|PUT /products/{id}/branch-blocks`.
//!
//! **The product document.** `PUT /products/{id}` writes the product with its options, units,
//! variants, barcodes and bundle components in one body, with client-generated IDs (`UUIDv7`
//! for the product; any UUID for the child rows). The server diffs it against the stored
//! version ("frozen after use", archive-instead-of-delete for used variants; see
//! `sultan_db::catalog::put_product`). `201` on create, `200` on update **and** on a replay of
//! the same body (nothing is written then: no audit, no outbox, no event). The response is the
//! stored document plus `used`, `archived_at`, `archived_barcodes`, `created_at`, `updated_at`.
//!
//! **JSON conventions** (decided in C3): quantities and unit factors are **integer thousandths**
//! in fields named `*_thousandths` (`factor_thousandths: 24000` = ×24, `qty_thousandths: 500` =
//! 0.5), money is integer minor units (Rupiah), times are unix milliseconds, IDs are UUID
//! strings. An empty `code` is generated (`P000123`) for a new product and kept for an existing
//! one; an empty variant `sku` becomes `<code>-<value codes>`; an empty `receipt_name` is the
//! name cut to 32 characters. `purchasable` defaults by type (false for `service` / `bundle`);
//! `default_sale_unit_id` / `default_purchase_unit_id` default to `base_unit_id`.
//!
//! **Errors.** Rule failures are `422` with `code` (the first failure), `field` and `errors`
//! (all failures: `{code, field?, message}`). A product code, SKU, barcode or PLU used by another
//! product is `409 duplicate_code|_sku|_barcode|_plu` with the conflicting `product_id`,
//! `variant_id` and `value`.
//!
//! **Archived products** (decided in C3): `GET /products/{id}` of an archived product and
//! `GET /products?archived=true` need `catalog.view_archived` for all branches, otherwise
//! `403 permission_denied` (a device token never has it). Without `archived=true` the list holds
//! active products only.
//!
//! Reads: a session or a device token; branch blocks: a session. Writes: see
//! [`super::catalog`]. Every write that changed something publishes
//! `catalog.changed {scope: "product", product_id}`.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::catalog::{
    Barcode, BundleComponent, OptionValue, Plu, ProductDraft, ProductOption, ProductUnits, Qty,
    Tracking, Unit, UnitKind, Variant,
};
use sultan_core::ids::{BranchId, ProductId, VariantId};
use sultan_core::time::Timestamp;
use sultan_db::catalog::{
    self, MAX_LIST_LIMIT, ProductDoc, ProductFilter, ProductRecord, ProductSummary, Written,
};
use utoipa::{IntoParams, ToSchema};

use super::catalog::{
    actor, not_found, parse_type, publish_changed, require_manage, require_view_archived, retrying,
    unit_kind_str, written_status,
};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, CatalogReader, RequireHq, UserSession};
use crate::state::AppState;

const fn yes() -> bool {
    true
}

/// A value of an option, e.g. `Medium` / `M`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OptionValueDto {
    /// ID (client-generated).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Short code `[A-Z0-9]{1,8}`, used in default SKUs (stored upper case).
    pub code: String,
}

/// An option, e.g. `Size` with its ordered values (at most 3 options per product).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct OptionDto {
    /// ID (client-generated).
    pub id: String,
    /// Name.
    pub name: String,
    /// Values, in order.
    pub values: Vec<OptionValueDto>,
}

/// A unit of the product (shared by all variants).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UnitDto {
    /// ID (client-generated).
    pub id: String,
    /// Name, e.g. `box`.
    pub name: String,
    /// Short name for the receipt.
    #[serde(default)]
    pub short_name: String,
    /// `integer` or `decimal` (the base unit's kind decides how quantities are entered).
    pub kind: String,
    /// Factor to the base unit in thousandths: `1000` for the base unit, `24000` for a box of 24.
    pub factor_thousandths: i64,
    /// Can be sold in this unit (default true).
    #[serde(default = "yes")]
    pub sellable: bool,
    /// Can be bought in this unit (default true).
    #[serde(default = "yes")]
    pub purchasable: bool,
    /// Archived (replaced packaging).
    #[serde(default)]
    pub archived: bool,
}

/// A variant: one sellable combination of option values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct VariantDto {
    /// ID (client-generated).
    pub id: String,
    /// SKU `[A-Z0-9._-]{1,32}`, unique company-wide; empty = `<code>-<value codes>`.
    #[serde(default)]
    pub sku: String,
    /// One value ID per option, in option order (empty without options).
    #[serde(default)]
    pub option_value_ids: Vec<String>,
    /// PLU of a weighed variant, 1–6 digits as a number.
    #[serde(default)]
    pub plu: Option<u32>,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A barcode of a variant + unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BarcodeDto {
    /// ID (client-generated).
    pub id: String,
    /// The variant.
    pub variant_id: String,
    /// The unit (e.g. the box barcode).
    pub unit_id: String,
    /// The code as scanned, 1–64 printable ASCII characters.
    pub code: String,
    /// Skip the EAN/UPC check-digit validation (odd vendor codes).
    #[serde(default)]
    pub allow_invalid_check_digit: bool,
}

/// A component line of a bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ComponentDto {
    /// Variant of another (non-bundle) product.
    pub variant_id: String,
    /// Its unit.
    pub unit_id: String,
    /// Quantity in thousandths (`1000` = 1, `500` = 0.5 kg).
    pub qty_thousandths: i64,
    /// Answer only (ignored in a body): the component's product, for the back office.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_id: Option<String>,
    /// Answer only: the variant's display name `<product> – <values>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Answer only: the variant's SKU.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sku: Option<String>,
    /// Answer only: the unit's name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit_name: Option<String>,
}

/// Body of `PUT /products/{id}`: the whole product document.
#[allow(clippy::struct_excessive_bools)] // independent product flags from spec 2.2 §2
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ProductBody {
    /// Product code `[A-Z0-9._-]{1,24}` (stored upper case); empty = generated / kept.
    #[serde(default)]
    pub code: String,
    /// Name, 1–120 characters.
    pub name: String,
    /// Receipt name, ≤ 32 characters; empty = the name cut to 32.
    #[serde(default)]
    pub receipt_name: String,
    /// `stock`, `weighed`, `service` or `bundle`.
    #[serde(rename = "type")]
    pub product_type: String,
    /// `none` (default); `batch` / `serial` are reserved for P2 and rejected.
    #[serde(default)]
    pub tracking: Option<String>,
    /// Category; `null` = Uncategorised.
    #[serde(default)]
    pub category_id: Option<String>,
    /// Brand.
    #[serde(default)]
    pub brand_id: Option<String>,
    /// Tax class (see `GET /tax-classes`).
    pub tax_class_id: String,
    /// Free labels (≤ 20, stored lower case).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Can be sold (default true).
    #[serde(default = "yes")]
    pub sellable: bool,
    /// Can be bought (default: true for `stock` / `weighed`, false otherwise).
    #[serde(default)]
    pub purchasable: Option<bool>,
    /// The cashier types the price.
    #[serde(default)]
    pub open_price: bool,
    /// The register asks for a line note.
    #[serde(default)]
    pub requires_note: bool,
    /// Options (≤ 3).
    #[serde(default)]
    pub options: Vec<OptionDto>,
    /// Units, the base unit included.
    pub units: Vec<UnitDto>,
    /// The base (stock) unit.
    pub base_unit_id: String,
    /// Default sale unit (default: the base unit).
    #[serde(default)]
    pub default_sale_unit_id: Option<String>,
    /// Default purchase unit (default: the base unit).
    #[serde(default)]
    pub default_purchase_unit_id: Option<String>,
    /// Variants (≥ 1; exactly one without options).
    pub variants: Vec<VariantDto>,
    /// Active barcodes. A barcode left out is deleted (or archived when its variant is used).
    #[serde(default)]
    pub barcodes: Vec<BarcodeDto>,
    /// Bundle components (`bundle` only).
    #[serde(default)]
    pub components: Vec<ComponentDto>,
    /// `true` archives the product (same as `POST /products/{id}/archive`).
    #[serde(default)]
    pub archived: bool,
}

/// A stored product document.
#[allow(clippy::struct_excessive_bools)] // independent product flags from spec 2.2 §2
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProductDto {
    /// ID.
    pub id: String,
    /// Product code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Receipt name.
    pub receipt_name: String,
    /// Type.
    #[serde(rename = "type")]
    pub product_type: String,
    /// Tracking (`none`).
    pub tracking: String,
    /// Category.
    pub category_id: Option<String>,
    /// Brand.
    pub brand_id: Option<String>,
    /// Tax class.
    pub tax_class_id: String,
    /// Tags.
    pub tags: Vec<String>,
    /// Can be sold.
    pub sellable: bool,
    /// Can be bought.
    pub purchasable: bool,
    /// Open price.
    pub open_price: bool,
    /// Requires a line note.
    pub requires_note: bool,
    /// Options.
    pub options: Vec<OptionDto>,
    /// Units.
    pub units: Vec<UnitDto>,
    /// Base unit.
    pub base_unit_id: String,
    /// Default sale unit.
    pub default_sale_unit_id: String,
    /// Default purchase unit.
    pub default_purchase_unit_id: String,
    /// Variants (archived ones included).
    pub variants: Vec<VariantDto>,
    /// Active barcodes.
    pub barcodes: Vec<BarcodeDto>,
    /// Barcodes removed from used variants: kept so old scans still resolve for returns.
    pub archived_barcodes: Vec<BarcodeDto>,
    /// Bundle components.
    pub components: Vec<ComponentDto>,
    /// Archived.
    pub archived: bool,
    /// When archived (unix ms).
    pub archived_at: Option<i64>,
    /// A variant has been used (sale, stock movement, price list, bundle): the "frozen after
    /// use" rules apply (spec 2.2 §3–§5).
    pub used: bool,
    /// Created (unix ms).
    pub created_at: i64,
    /// Last changed (unix ms).
    pub updated_at: i64,
}

fn barcode_dto(b: &Barcode) -> BarcodeDto {
    BarcodeDto {
        id: b.id.to_string(),
        variant_id: b.variant.to_string(),
        unit_id: b.unit.to_string(),
        code: b.code.clone(),
        allow_invalid_check_digit: b.allow_invalid_check_digit,
    }
}

impl From<&ProductRecord> for ProductDto {
    fn from(r: &ProductRecord) -> Self {
        let d = &r.doc.draft;
        Self {
            id: d.id.to_string(),
            code: d.code.clone(),
            name: d.name.clone(),
            receipt_name: d.receipt_name.clone(),
            product_type: d.product_type.as_str().to_owned(),
            tracking: d.tracking.as_str().to_owned(),
            category_id: r.doc.category.map(|c| c.to_string()),
            brand_id: r.doc.brand.map(|b| b.to_string()),
            tax_class_id: r.doc.tax_class.to_string(),
            tags: d.tags.clone(),
            sellable: d.sellable,
            purchasable: d.purchasable,
            open_price: d.open_price,
            requires_note: d.requires_note,
            options: d
                .options
                .iter()
                .map(|o| OptionDto {
                    id: o.id.to_string(),
                    name: o.name.clone(),
                    values: o
                        .values
                        .iter()
                        .map(|v| OptionValueDto {
                            id: v.id.to_string(),
                            name: v.name.clone(),
                            code: v.code.clone(),
                        })
                        .collect(),
                })
                .collect(),
            units: d
                .units
                .units
                .iter()
                .map(|u| UnitDto {
                    id: u.id.to_string(),
                    name: u.name.clone(),
                    short_name: u.short_name.clone(),
                    kind: unit_kind_str(u.kind).to_owned(),
                    factor_thousandths: u.factor.thousandths(),
                    sellable: u.sellable,
                    purchasable: u.purchasable,
                    archived: u.archived,
                })
                .collect(),
            base_unit_id: d.units.base.to_string(),
            default_sale_unit_id: d.units.default_sale.to_string(),
            default_purchase_unit_id: d.units.default_purchase.to_string(),
            variants: d
                .variants
                .iter()
                .map(|v| VariantDto {
                    id: v.id.to_string(),
                    sku: v.sku.clone(),
                    option_value_ids: v.values.iter().map(ToString::to_string).collect(),
                    plu: v.plu.map(Plu::value),
                    archived: v.archived,
                })
                .collect(),
            barcodes: d.barcodes.iter().map(barcode_dto).collect(),
            archived_barcodes: r.archived_barcodes.iter().map(barcode_dto).collect(),
            components: d
                .components
                .iter()
                .map(|c| ComponentDto {
                    variant_id: c.variant.to_string(),
                    unit_id: c.unit.to_string(),
                    qty_thousandths: c.qty.thousandths(),
                    product_id: None,
                    name: None,
                    sku: None,
                    unit_name: None,
                })
                .collect(),
            archived: d.archived,
            archived_at: r.archived_at.map(Timestamp::unix_millis),
            used: r.used,
            created_at: r.created_at.unix_millis(),
            updated_at: r.updated_at.unix_millis(),
        }
    }
}

/// The answer for a stored product: the document with its component lines labelled (product,
/// display name, SKU and unit name of each component, for the back office).
async fn product_answer(state: &AppState, record: &ProductRecord) -> Result<ProductDto, ApiError> {
    let mut dto = ProductDto::from(record);
    let variants: Vec<VariantId> = record
        .doc
        .draft
        .components
        .iter()
        .map(|c| c.variant)
        .collect();
    if variants.is_empty() {
        return Ok(dto);
    }
    let labels = catalog::variant_labels(&state.db, &variants).await?;
    for (line, c) in dto.components.iter_mut().zip(&record.doc.draft.components) {
        if let Some(label) = labels.get(&c.variant) {
            line.product_id = Some(label.product.to_string());
            line.name = Some(label.name.clone());
            line.sku = Some(label.sku.clone());
            line.unit_name = label
                .units
                .iter()
                .find(|(id, _)| *id == c.unit)
                .map(|(_, name)| name.clone());
        }
    }
    Ok(dto)
}

fn parse_kind(value: &str) -> Result<UnitKind, ApiError> {
    match value {
        "integer" => Ok(UnitKind::Integer),
        "decimal" => Ok(UnitKind::Decimal),
        _ => Err(ApiError::invalid_field(
            "units.kind",
            "`kind` must be integer or decimal",
        )),
    }
}

fn parse_tracking(value: Option<&str>) -> Result<Tracking, ApiError> {
    match value.unwrap_or("none") {
        "none" => Ok(Tracking::None),
        "batch" => Ok(Tracking::Batch),
        "serial" => Ok(Tracking::Serial),
        _ => Err(ApiError::invalid_field(
            "tracking",
            "`tracking` must be none, batch or serial",
        )),
    }
}

fn option_of(o: &OptionDto) -> Result<ProductOption, ApiError> {
    Ok(ProductOption {
        id: parse_id(&o.id, "options.id")?,
        name: o.name.clone(),
        values: o
            .values
            .iter()
            .map(|v| {
                Ok(OptionValue {
                    id: parse_id(&v.id, "options.values.id")?,
                    name: v.name.clone(),
                    code: v.code.clone(),
                })
            })
            .collect::<Result<_, ApiError>>()?,
    })
}

fn unit_of(u: &UnitDto) -> Result<Unit, ApiError> {
    Ok(Unit {
        id: parse_id(&u.id, "units.id")?,
        name: u.name.clone(),
        short_name: u.short_name.clone(),
        kind: parse_kind(&u.kind)?,
        factor: Qty::from_thousandths(u.factor_thousandths),
        sellable: u.sellable,
        purchasable: u.purchasable,
        archived: u.archived,
    })
}

fn variant_of(v: &VariantDto) -> Result<Variant, ApiError> {
    let plu = v
        .plu
        .map(|n| {
            Plu::new(n).ok_or_else(|| {
                ApiError::invalid_field("variants.plu", "`plu` must be 1 to 6 digits")
            })
        })
        .transpose()?;
    Ok(Variant {
        id: parse_id(&v.id, "variants.id")?,
        sku: v.sku.clone(),
        values: v
            .option_value_ids
            .iter()
            .map(|id| parse_id(id, "variants.option_value_ids"))
            .collect::<Result<_, _>>()?,
        plu,
        archived: v.archived,
    })
}

fn barcode_of(b: &BarcodeDto) -> Result<Barcode, ApiError> {
    Ok(Barcode {
        id: parse_id(&b.id, "barcodes.id")?,
        variant: parse_id(&b.variant_id, "barcodes.variant_id")?,
        unit: parse_id(&b.unit_id, "barcodes.unit_id")?,
        code: b.code.clone(),
        allow_invalid_check_digit: b.allow_invalid_check_digit,
    })
}

fn component_of(c: &ComponentDto) -> Result<BundleComponent, ApiError> {
    Ok(BundleComponent {
        variant: parse_id(&c.variant_id, "components.variant_id")?,
        unit: parse_id(&c.unit_id, "components.unit_id")?,
        qty: Qty::from_thousandths(c.qty_thousandths),
    })
}

fn all<T, U>(items: &[T], f: fn(&T) -> Result<U, ApiError>) -> Result<Vec<U>, ApiError> {
    items.iter().map(f).collect()
}

impl ProductBody {
    /// The repository document; `400 invalid_body` for an ID that is not a UUID, `422
    /// invalid_field` for an unknown enum value or PLU out of range.
    fn into_doc(self, id: ProductId) -> Result<ProductDoc, ApiError> {
        let product_type = parse_type("type", &self.product_type)?;
        let base = parse_id(&self.base_unit_id, "base_unit_id")?;
        let unit_or_base = |value: Option<&str>, field: &str| {
            parse_opt_id(value, field).map(|u| u.unwrap_or(base))
        };
        let units = ProductUnits {
            base,
            units: all(&self.units, unit_of)?,
            default_sale: unit_or_base(
                self.default_sale_unit_id.as_deref(),
                "default_sale_unit_id",
            )?,
            default_purchase: unit_or_base(
                self.default_purchase_unit_id.as_deref(),
                "default_purchase_unit_id",
            )?,
        };
        let draft = ProductDraft {
            id,
            code: self.code,
            name: self.name,
            receipt_name: self.receipt_name,
            product_type,
            tracking: parse_tracking(self.tracking.as_deref())?,
            tags: self.tags,
            sellable: self.sellable,
            purchasable: self
                .purchasable
                .unwrap_or_else(|| product_type.default_purchasable()),
            open_price: self.open_price,
            requires_note: self.requires_note,
            options: all(&self.options, option_of)?,
            units,
            variants: all(&self.variants, variant_of)?,
            barcodes: all(&self.barcodes, barcode_of)?,
            components: all(&self.components, component_of)?,
            archived: self.archived,
        };
        Ok(ProductDoc {
            draft,
            category: parse_opt_id(self.category_id.as_deref(), "category_id")?,
            brand: parse_opt_id(self.brand_id.as_deref(), "brand_id")?,
            tax_class: parse_id(&self.tax_class_id, "tax_class_id")?,
        })
    }
}

/// Query of `GET /products`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ProductQuery {
    /// Contains-match on name, receipt name, code and brand (case and accents ignored).
    pub q: Option<String>,
    /// Only this category.
    pub category_id: Option<String>,
    /// Only this brand.
    pub brand_id: Option<String>,
    /// Only this type.
    #[serde(rename = "type")]
    #[param(rename = "type")]
    pub product_type: Option<String>,
    /// `true`: archived products only (needs `catalog.view_archived`); default: active only.
    #[serde(default)]
    pub archived: bool,
    /// Page cursor: `next_before_id` of the previous page.
    pub before_id: Option<String>,
    /// Page size, 1–50 (default 50).
    pub limit: Option<u32>,
}

/// A row of `GET /products`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProductSummaryDto {
    /// ID.
    pub id: String,
    /// Product code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Type.
    #[serde(rename = "type")]
    pub product_type: String,
    /// Category.
    pub category_id: Option<String>,
    /// Brand.
    pub brand_id: Option<String>,
    /// Can be sold.
    pub sellable: bool,
    /// When archived (unix ms).
    pub archived_at: Option<i64>,
    /// Number of active variants.
    pub variant_count: u32,
}

impl From<ProductSummary> for ProductSummaryDto {
    fn from(p: ProductSummary) -> Self {
        Self {
            id: p.id.to_string(),
            code: p.code,
            name: p.name,
            product_type: p.product_type.as_str().to_owned(),
            category_id: p.category.map(|c| c.to_string()),
            brand_id: p.brand.map(|b| b.to_string()),
            sellable: p.sellable,
            archived_at: p.archived_at.map(Timestamp::unix_millis),
            variant_count: p.variant_count,
        }
    }
}

/// One page of `GET /products`, newest first (by ID).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProductPage {
    /// Products.
    pub items: Vec<ProductSummaryDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

/// `GET /api/v1/products`: a page of products.
///
/// # Errors
/// `401`, `403 permission_denied` (`archived=true` without `catalog.view_archived`),
/// `400 invalid_body` (bad ID), `422 invalid_field` (`type`, `limit`).
#[utoipa::path(get, path = "/api/v1/products", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(ProductQuery),
    responses((status = 200, body = ProductPage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiQuery(query): ApiQuery<ProductQuery>,
) -> Result<Json<ProductPage>, ApiError> {
    let limit = query.limit.unwrap_or(MAX_LIST_LIMIT);
    if !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {MAX_LIST_LIMIT}"),
        ));
    }
    if query.archived {
        require_view_archived(&state, &reader).await?;
    }
    let filter = ProductFilter {
        q: query.q,
        category: parse_opt_id(query.category_id.as_deref(), "category_id")?,
        brand: parse_opt_id(query.brand_id.as_deref(), "brand_id")?,
        product_type: query
            .product_type
            .as_deref()
            .map(|t| parse_type("type", t))
            .transpose()?,
        archived: query.archived,
        before_id: parse_opt_id(query.before_id.as_deref(), "before_id")?,
        limit,
    };
    let products = catalog::list_products(&state.db, &filter).await?;
    let full = products.len() == limit as usize;
    let next_before_id = products.last().filter(|_| full).map(|p| p.id.to_string());
    Ok(Json(ProductPage {
        items: products.into_iter().map(ProductSummaryDto::from).collect(),
        next_before_id,
    }))
}

/// `GET /api/v1/products/{id}`: the product document.
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied` (archived, see the module docs),
/// `404 not_found`.
#[utoipa::path(get, path = "/api/v1/products/{id}", tag = "catalog",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Product ID")),
    responses((status = 200, body = ProductDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    reader: CatalogReader,
    Path(id): Path<String>,
) -> Result<Json<ProductDto>, ApiError> {
    let id: ProductId = parse_id(&id, "id")?;
    let record = catalog::get_product(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("product"))?;
    if record.archived_at.is_some() {
        require_view_archived(&state, &reader).await?;
    }
    Ok(Json(product_answer(&state, &record).await?))
}

/// A product write.
#[derive(Debug, Clone, Copy)]
enum ProductWrite<'a> {
    Put(&'a ProductDoc),
    Archive,
    Unarchive,
}

/// Runs a product write and reads the stored record back, in one (retried) transaction, then
/// publishes `catalog.changed` unless nothing changed.
async fn write_product(
    state: &AppState,
    session: &UserSession,
    id: ProductId,
    op: ProductWrite<'_>,
) -> Result<(Written, ProductRecord), ApiError> {
    let (actor, now) = (&actor(session), state.now());
    let (written, record) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = match op {
            ProductWrite::Put(doc) => catalog::put_product(&mut tx, doc, actor, now).await?.0,
            ProductWrite::Archive => catalog::archive_product(&mut tx, id, actor, now).await?,
            ProductWrite::Unarchive => catalog::unarchive_product(&mut tx, id, actor, now).await?,
        };
        let record = catalog::get_product(&mut tx, id)
            .await?
            .ok_or_else(|| not_found("product"))?;
        tx.commit().await?;
        Ok((written, record))
    })
    .await?;
    publish_changed(
        state,
        written,
        "product",
        json!({ "product_id": id.to_string() }),
    );
    Ok((written, record))
}

/// `PUT /api/v1/products/{id}`: create (`201`) or replace (`200`) the product document.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422` rule errors (with
/// `errors`), `409 duplicate_*` (with `product_id`, `variant_id`, `value`).
#[utoipa::path(put, path = "/api/v1/products/{id}", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = ProductBody,
    responses((status = 201, body = ProductDto), (status = 200, body = ProductDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ProductBody>,
) -> Result<(StatusCode, Json<ProductDto>), ApiError> {
    let id: ProductId = parse_put_id(&id)?;
    require_manage(&state, &session).await?;
    let doc = body.into_doc(id)?;
    let (written, record) = write_product(&state, &session, id, ProductWrite::Put(&doc)).await?;
    Ok((
        written_status(written),
        Json(product_answer(&state, &record).await?),
    ))
}

/// `POST /api/v1/products/{id}/archive`: archives the product and all its variants (§9).
/// Archiving twice is a no-op.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(post, path = "/api/v1/products/{id}/archive", tag = "catalog",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Product ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = ProductDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ProductDto>, ApiError> {
    set_archived(&state, &session, &id, true).await
}

/// `POST /api/v1/products/{id}/unarchive`: allowed while its SKUs, PLUs and barcodes are free.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`, `409 duplicate_*`.
#[utoipa::path(post, path = "/api/v1/products/{id}/unarchive", tag = "catalog",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Product ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = ProductDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn unarchive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ProductDto>, ApiError> {
    set_archived(&state, &session, &id, false).await
}

async fn set_archived(
    state: &AppState,
    session: &UserSession,
    id: &str,
    archived: bool,
) -> Result<Json<ProductDto>, ApiError> {
    let id: ProductId = parse_id(id, "id")?;
    require_manage(state, session).await?;
    let op = if archived {
        ProductWrite::Archive
    } else {
        ProductWrite::Unarchive
    };
    let (_, record) = write_product(state, session, id, op).await?;
    Ok(Json(product_answer(state, &record).await?))
}

/// The branches where a product is not sold (spec 2.2 §10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BranchBlocksDto {
    /// Branch IDs, ordered; empty = sold everywhere.
    pub branch_ids: Vec<String>,
}

/// `GET /api/v1/products/{id}/branch-blocks`.
///
/// # Errors
/// `401 invalid_session`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/products/{id}/branch-blocks", tag = "catalog",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Product ID")),
    responses((status = 200, body = BranchBlocksDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_branch_blocks(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<BranchBlocksDto>, ApiError> {
    let id: ProductId = parse_id(&id, "id")?;
    if catalog::get_product(&state.db, id).await?.is_none() {
        return Err(ApiError::not_found("product"));
    }
    let blocks = catalog::get_branch_blocks(&state.db, id).await?;
    Ok(Json(BranchBlocksDto {
        branch_ids: blocks.iter().map(ToString::to_string).collect(),
    }))
}

/// `PUT /api/v1/products/{id}/branch-blocks`: replaces the set of blocked branches; always `200`
/// (decided in C3: the set always exists, empty by default).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `404 not_found`,
/// `422 invalid_field` (`branch_ids`: unknown branch).
#[utoipa::path(put, path = "/api/v1/products/{id}/branch-blocks", tag = "catalog",
    security(("bearer" = [])), params(("id" = String, Path, description = "Product ID")),
    request_body = BranchBlocksDto,
    responses((status = 200, body = BranchBlocksDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_branch_blocks(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<BranchBlocksDto>,
) -> Result<Json<BranchBlocksDto>, ApiError> {
    let id: ProductId = parse_id(&id, "id")?;
    require_manage(&state, &session).await?;
    let branches: Vec<BranchId> = body
        .branch_ids
        .iter()
        .map(|b| parse_id(b, "branch_ids"))
        .collect::<Result<BTreeSet<_>, _>>()?
        .into_iter()
        .collect();
    let (state, actor, branches, now) = (&state, &actor(&session), &branches, state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = catalog::put_branch_blocks(&mut tx, id, branches, actor, now).await?;
        let stored = catalog::get_branch_blocks(&mut tx, id).await?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish_changed(
        state,
        written,
        "product",
        json!({ "product_id": id.to_string() }),
    );
    Ok(Json(BranchBlocksDto {
        branch_ids: stored.iter().map(ToString::to_string).collect(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_fields_are_checked() {
        assert_eq!(parse_kind("integer").unwrap(), UnitKind::Integer);
        assert_eq!(parse_kind("decimal").unwrap(), UnitKind::Decimal);
        assert_eq!(parse_kind("x").unwrap_err().code(), "invalid_field");
        assert_eq!(parse_tracking(None).unwrap(), Tracking::None);
        assert_eq!(parse_tracking(Some("batch")).unwrap(), Tracking::Batch);
        assert_eq!(parse_tracking(Some("serial")).unwrap(), Tracking::Serial);
        assert_eq!(
            parse_tracking(Some("lot")).unwrap_err().code(),
            "invalid_field"
        );
    }
}
