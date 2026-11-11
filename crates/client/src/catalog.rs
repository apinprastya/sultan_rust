//! The catalog as the back office sees it (spec 2.2 §12, the JSON of
//! `crates/server/src/routes/{products,catalog,taxonomy,product_images}.rs`).
//!
//! Conventions (decided in C3): quantities and unit factors are integer thousandths in fields
//! named `*_thousandths`, times are unix ms, IDs are UUID strings. [`Product`] is both the body of
//! `PUT /products/{id}` and its answer: the answer-only members (`used`, `archived_at`,
//! `archived_barcodes`, …) are ignored by the server in a body.
//!
//! [`Product::to_draft`] turns a document into `sultan_core::catalog::ProductDraft`, so the back
//! office can run the core rules before it saves and the fake service can enforce them.

use std::str::FromStr;

use serde::{Deserialize, Serialize};
use sultan_core::catalog::{
    Barcode as CoreBarcode, BundleComponent, OptionValue as CoreValue, Plu, ProductDraft,
    ProductOption as CoreOption, ProductType, ProductUnits, Qty, Tracking, Unit as CoreUnit,
    UnitKind, Variant as CoreVariant,
};

use crate::Problem;

const fn yes() -> bool {
    true
}

fn none() -> String {
    "none".to_owned()
}

/// `GET /auth/me`: the session and what it may do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Me {
    /// The user.
    pub user_id: String,
    /// Login name.
    #[serde(default)]
    pub username: String,
    /// Display name.
    pub display_name: String,
    /// The session's branch.
    pub branch_id: String,
    /// The register (PIN session) or `None` (back office).
    #[serde(default)]
    pub register_id: Option<String>,
    /// Permission keys at the session's branch.
    #[serde(default)]
    pub permissions: Vec<String>,
    /// Permission keys held through a role for all branches.
    #[serde(default)]
    pub permissions_everywhere: Vec<String>,
    /// Permission keys held through any role assignment (what the catalog needs, spec 2.2 §11).
    #[serde(default)]
    pub permissions_anywhere: Vec<String>,
}

/// `catalog.manage`.
pub const CATALOG_MANAGE: &str = "catalog.manage";
/// `catalog.view_archived`.
pub const CATALOG_VIEW_ARCHIVED: &str = "catalog.view_archived";

impl Me {
    /// Whether `key` is held for all branches.
    #[must_use]
    pub fn holds_everywhere(&self, key: &str) -> bool {
        self.permissions_everywhere.iter().any(|p| p == key)
    }

    /// Whether `key` is held at the session's branch.
    #[must_use]
    pub fn holds_here(&self, key: &str) -> bool {
        self.permissions.iter().any(|p| p == key)
    }

    /// Whether `key` is held through any role assignment: what `catalog.manage` and
    /// `catalog.view_archived` need (spec 2.2 §11, decided 2026-11-11). A key held here or
    /// everywhere is held anywhere too (so an answer without `permissions_anywhere` still works).
    #[must_use]
    pub fn holds_anywhere(&self, key: &str) -> bool {
        self.permissions_anywhere.iter().any(|p| p == key)
            || self.holds_everywhere(key)
            || self.holds_here(key)
    }
}

/// Filters and paging of `GET /products`; unset fields are left out of the query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductQuery {
    /// Contains-match on name, receipt name, code and brand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// Only this category.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    /// Only this brand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand_id: Option<String>,
    /// Only this type.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub product_type: Option<String>,
    /// Archived products only (needs `catalog.view_archived`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub archived: bool,
    /// Page cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    /// Page size, 1–50.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A row of `GET /products`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductSummary {
    /// ID.
    pub id: String,
    /// Code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Type.
    #[serde(rename = "type")]
    pub product_type: String,
    /// Category.
    #[serde(default)]
    pub category_id: Option<String>,
    /// Brand.
    #[serde(default)]
    pub brand_id: Option<String>,
    /// Sellable.
    #[serde(default = "yes")]
    pub sellable: bool,
    /// When archived (unix ms).
    #[serde(default)]
    pub archived_at: Option<i64>,
    /// Active variants.
    #[serde(default)]
    pub variant_count: u32,
}

/// One page of `GET /products`, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductPage {
    /// Rows.
    pub items: Vec<ProductSummary>,
    /// Cursor of the next page.
    #[serde(default)]
    pub next_before_id: Option<String>,
}

/// A value of an option.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OptionValue {
    /// ID.
    pub id: String,
    /// Display name, e.g. `Medium`.
    pub name: String,
    /// Short code, e.g. `M`.
    pub code: String,
}

/// An option, e.g. `Size`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductOption {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Values in order.
    pub values: Vec<OptionValue>,
}

/// A unit of the product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unit {
    /// ID.
    pub id: String,
    /// Name, e.g. `box`.
    pub name: String,
    /// Short name for the receipt.
    #[serde(default)]
    pub short_name: String,
    /// `integer` or `decimal`.
    pub kind: String,
    /// Factor to the base unit, thousandths.
    pub factor_thousandths: i64,
    /// Sellable in this unit.
    #[serde(default = "yes")]
    pub sellable: bool,
    /// Purchasable in this unit.
    #[serde(default = "yes")]
    pub purchasable: bool,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Variant {
    /// ID.
    pub id: String,
    /// SKU; empty = `<code>-<value codes>`.
    #[serde(default)]
    pub sku: String,
    /// One value ID per option.
    #[serde(default)]
    pub option_value_ids: Vec<String>,
    /// PLU (weighed).
    #[serde(default)]
    pub plu: Option<u32>,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A barcode of a variant + unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Barcode {
    /// ID.
    pub id: String,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The code.
    pub code: String,
    /// Skip the check-digit validation.
    #[serde(default)]
    pub allow_invalid_check_digit: bool,
}

/// A bundle component line. The labels are filled in answers only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Component {
    /// Variant of another product.
    pub variant_id: String,
    /// Its unit.
    pub unit_id: String,
    /// Quantity in thousandths.
    pub qty_thousandths: i64,
    /// Answer only: the component's product.
    #[serde(default, skip_serializing)]
    pub product_id: Option<String>,
    /// Answer only: the variant's display name.
    #[serde(default, skip_serializing)]
    pub name: Option<String>,
    /// Answer only: the SKU.
    #[serde(default, skip_serializing)]
    pub sku: Option<String>,
    /// Answer only: the unit's name.
    #[serde(default, skip_serializing)]
    pub unit_name: Option<String>,
}

/// The product document of `GET|PUT /products/{id}`.
#[allow(clippy::struct_excessive_bools)] // independent product flags from spec 2.2 §2
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Product {
    /// ID (answer only; the path carries it).
    #[serde(default)]
    pub id: String,
    /// Code; empty = generated (new) or kept.
    #[serde(default)]
    pub code: String,
    /// Name.
    pub name: String,
    /// Receipt name; empty = the name cut to 32.
    #[serde(default)]
    pub receipt_name: String,
    /// `stock`, `weighed`, `service` or `bundle`.
    #[serde(rename = "type")]
    pub product_type: String,
    /// Tracking (`none`).
    #[serde(default = "none")]
    pub tracking: String,
    /// Category.
    #[serde(default)]
    pub category_id: Option<String>,
    /// Brand.
    #[serde(default)]
    pub brand_id: Option<String>,
    /// Tax class.
    pub tax_class_id: String,
    /// Tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Sellable.
    #[serde(default = "yes")]
    pub sellable: bool,
    /// Purchasable.
    #[serde(default = "yes")]
    pub purchasable: bool,
    /// Open price.
    #[serde(default)]
    pub open_price: bool,
    /// Requires a line note.
    #[serde(default)]
    pub requires_note: bool,
    /// Options.
    #[serde(default)]
    pub options: Vec<ProductOption>,
    /// Units (the base unit included).
    pub units: Vec<Unit>,
    /// The base unit.
    pub base_unit_id: String,
    /// Default sale unit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_sale_unit_id: String,
    /// Default purchase unit.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub default_purchase_unit_id: String,
    /// Variants.
    pub variants: Vec<Variant>,
    /// Active barcodes.
    #[serde(default)]
    pub barcodes: Vec<Barcode>,
    /// Answer only: barcodes removed from used variants.
    #[serde(default, skip_serializing)]
    pub archived_barcodes: Vec<Barcode>,
    /// Bundle components.
    #[serde(default)]
    pub components: Vec<Component>,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
    /// Answer only: when archived.
    #[serde(default, skip_serializing)]
    pub archived_at: Option<i64>,
    /// Answer only: a variant has been used ("frozen after use").
    #[serde(default, skip_serializing)]
    pub used: bool,
    /// Answer only: created (unix ms).
    #[serde(default, skip_serializing)]
    pub created_at: i64,
    /// Answer only: last changed (unix ms).
    #[serde(default, skip_serializing)]
    pub updated_at: i64,
}

fn parse<T: FromStr>(value: &str, field: &str) -> Result<T, Problem> {
    value.parse().map_err(|_| {
        Problem::new(400, "invalid_body", &format!("`{field}` is not a UUID")).with("field", field)
    })
}

fn parse_opt<T: FromStr>(value: Option<&str>, field: &str) -> Result<Option<T>, Problem> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| parse(v, field))
        .transpose()
}

fn invalid(field: &str, detail: &str) -> Problem {
    Problem::new(422, "invalid_field", detail).with("field", field)
}

/// The core unit kind of a wire value.
///
/// # Errors
/// `422 invalid_field` for an unknown kind.
pub fn unit_kind(value: &str) -> Result<UnitKind, Problem> {
    match value {
        "integer" => Ok(UnitKind::Integer),
        "decimal" => Ok(UnitKind::Decimal),
        _ => Err(invalid("units.kind", "`kind` must be integer or decimal")),
    }
}

/// The wire value of a core unit kind.
#[must_use]
pub const fn unit_kind_str(kind: UnitKind) -> &'static str {
    match kind {
        UnitKind::Integer => "integer",
        UnitKind::Decimal => "decimal",
    }
}

impl Product {
    /// The core document, as the server builds it from a body (`ProductBody::into_doc`): IDs
    /// parsed, `purchasable` as given, default units falling back to the base unit. Category,
    /// brand and tax class are references the core rules do not check.
    ///
    /// # Errors
    /// `400 invalid_body` for an ID that is not a UUID, `422 invalid_field` for an unknown type,
    /// unit kind or tracking, or a PLU out of range.
    #[allow(clippy::too_many_lines)] // one block per child list, like the server's `into_doc`
    pub fn to_draft(&self, id: &str) -> Result<ProductDraft, Problem> {
        let product_type: ProductType = self
            .product_type
            .parse()
            .map_err(|_| invalid("type", "`type` must be stock, weighed, service or bundle"))?;
        let tracking = match self.tracking.as_str() {
            "" | "none" => Tracking::None,
            "batch" => Tracking::Batch,
            "serial" => Tracking::Serial,
            _ => return Err(invalid("tracking", "unknown tracking")),
        };
        let base = parse(&self.base_unit_id, "base_unit_id")?;
        let or_base =
            |value: &str, field: &str| parse_opt(Some(value), field).map(|u| u.unwrap_or(base));
        let units = self
            .units
            .iter()
            .map(|u| {
                Ok(CoreUnit {
                    id: parse(&u.id, "units.id")?,
                    name: u.name.clone(),
                    short_name: u.short_name.clone(),
                    kind: unit_kind(&u.kind)?,
                    factor: Qty::from_thousandths(u.factor_thousandths),
                    sellable: u.sellable,
                    purchasable: u.purchasable,
                    archived: u.archived,
                })
            })
            .collect::<Result<Vec<_>, Problem>>()?;
        let options = self
            .options
            .iter()
            .map(|o| {
                Ok(CoreOption {
                    id: parse(&o.id, "options.id")?,
                    name: o.name.clone(),
                    values: o
                        .values
                        .iter()
                        .map(|v| {
                            Ok(CoreValue {
                                id: parse(&v.id, "options.values.id")?,
                                name: v.name.clone(),
                                code: v.code.clone(),
                            })
                        })
                        .collect::<Result<_, Problem>>()?,
                })
            })
            .collect::<Result<Vec<_>, Problem>>()?;
        let variants = self
            .variants
            .iter()
            .map(|v| {
                let plu = v
                    .plu
                    .map(|n| Plu::new(n).ok_or_else(|| invalid("variants.plu", "1 to 6 digits")))
                    .transpose()?;
                Ok(CoreVariant {
                    id: parse(&v.id, "variants.id")?,
                    sku: v.sku.clone(),
                    values: v
                        .option_value_ids
                        .iter()
                        .map(|id| parse(id, "variants.option_value_ids"))
                        .collect::<Result<_, _>>()?,
                    plu,
                    archived: v.archived,
                })
            })
            .collect::<Result<Vec<_>, Problem>>()?;
        let barcodes = self
            .barcodes
            .iter()
            .map(|b| {
                Ok(CoreBarcode {
                    id: parse(&b.id, "barcodes.id")?,
                    variant: parse(&b.variant_id, "barcodes.variant_id")?,
                    unit: parse(&b.unit_id, "barcodes.unit_id")?,
                    code: b.code.clone(),
                    allow_invalid_check_digit: b.allow_invalid_check_digit,
                })
            })
            .collect::<Result<Vec<_>, Problem>>()?;
        let components = self
            .components
            .iter()
            .map(|c| {
                Ok(BundleComponent {
                    variant: parse(&c.variant_id, "components.variant_id")?,
                    unit: parse(&c.unit_id, "components.unit_id")?,
                    qty: Qty::from_thousandths(c.qty_thousandths),
                })
            })
            .collect::<Result<Vec<_>, Problem>>()?;
        Ok(ProductDraft {
            id: parse(id, "id")?,
            code: self.code.clone(),
            name: self.name.clone(),
            receipt_name: self.receipt_name.clone(),
            product_type,
            tracking,
            tags: self.tags.clone(),
            sellable: self.sellable,
            purchasable: self.purchasable,
            open_price: self.open_price,
            requires_note: self.requires_note,
            options,
            units: ProductUnits {
                base,
                units,
                default_sale: or_base(&self.default_sale_unit_id, "default_sale_unit_id")?,
                default_purchase: or_base(
                    &self.default_purchase_unit_id,
                    "default_purchase_unit_id",
                )?,
            },
            variants,
            barcodes,
            components,
            archived: self.archived,
        })
    }
}

/// A category without children.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Category {
    /// ID.
    pub id: String,
    /// Parent; `None` = a root.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Name.
    pub name: String,
    /// Short code.
    #[serde(default)]
    pub code: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A node of `GET /categories`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryNode {
    /// ID.
    pub id: String,
    /// Parent.
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Name.
    pub name: String,
    /// Short code.
    #[serde(default)]
    pub code: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
    /// Children by name.
    #[serde(default)]
    pub children: Vec<CategoryNode>,
}

impl CategoryNode {
    /// The node without its children.
    #[must_use]
    pub fn category(&self) -> Category {
        Category {
            id: self.id.clone(),
            parent_id: self.parent_id.clone(),
            name: self.name.clone(),
            code: self.code.clone(),
            archived: self.archived,
        }
    }
}

/// Body of `PUT /categories/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CategoryBody {
    /// Parent; `None` = a root.
    pub parent_id: Option<String>,
    /// Name.
    pub name: String,
    /// Short code.
    #[serde(default)]
    pub code: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A brand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Brand {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// Body of `PUT /brands/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrandBody {
    /// Name.
    pub name: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A tax class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxClass {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// A scale barcode layout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScaleLayout {
    /// Prefix 20–29.
    pub prefix: u8,
    /// PLU digits.
    pub plu_digits: u8,
    /// Value digits.
    pub value_digits: u8,
    /// `weight` or `price`.
    pub kind: String,
}

/// `GET /settings/catalog`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogSettings {
    /// Prefix of generated product codes.
    pub product_code_prefix: String,
    /// Prefix of generated internal barcodes.
    pub internal_barcode_prefix: String,
    /// Scale layouts.
    #[serde(default)]
    pub scale_layouts: Vec<ScaleLayout>,
    /// Unit names for autocomplete.
    #[serde(default)]
    pub unit_names: Vec<String>,
}

/// What `POST /catalog/generate/{…}` makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generate {
    /// A product code (`P000123`).
    ProductCode,
    /// An internal EAN-13 barcode.
    Barcode,
}

impl Generate {
    /// The path segment.
    #[must_use]
    pub const fn path(self) -> &'static str {
        match self {
            Self::ProductCode => "product-code",
            Self::Barcode => "barcode",
        }
    }
}

/// Answer of `PUT /products/{id}/image`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInfo {
    /// The product.
    pub product_id: String,
    /// Stored media type.
    pub mime: String,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Size in bytes.
    pub bytes: usize,
    /// `ETag`.
    pub etag: String,
}

/// A hit of `GET /catalog/search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchHit {
    /// The product.
    pub product_id: String,
    /// The variant.
    pub variant_id: String,
    /// The default sale unit.
    pub unit_id: String,
    /// Display name.
    pub name: String,
    /// SKU.
    pub sku: String,
    /// Type.
    #[serde(rename = "type")]
    pub product_type: String,
}

/// One page of `GET /catalog/search` (spec 2.2 §6, C3b).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchPage {
    /// Hits: prefix matches first, then by name.
    pub items: Vec<SearchHit>,
    /// Pass as `after_variant_id` for the next page; `None` on the last page.
    #[serde(default)]
    pub next_after_variant_id: Option<String>,
}

/// `GET /catalog/lookup?code=`: what a scanned or typed code resolved to (spec 2.2 §6
/// "Result"). The display fields after `name` arrived with phase C5; an older server leaves them
/// empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lookup {
    /// The product.
    pub product_id: String,
    /// The variant.
    pub variant_id: String,
    /// The unit: the barcode's, else the default sale unit (the base unit for a PLU).
    pub unit_id: String,
    /// Display name `<product> – <values>`.
    pub name: String,
    /// The product name alone.
    #[serde(default)]
    pub product_name: String,
    /// Option values joined by ` / `; empty without options.
    #[serde(default)]
    pub variant_label: String,
    /// The unit's name (`pcs`, `box`, `kg`).
    #[serde(default)]
    pub unit_name: String,
    /// Variant SKU.
    pub sku: String,
    /// The variant's PLU (weighed products).
    #[serde(default)]
    pub plu: Option<u32>,
    /// `stock`, `weighed`, `service` or `bundle`.
    #[serde(rename = "type")]
    pub product_type: String,
    /// The cashier types the price.
    #[serde(default)]
    pub open_price: bool,
    /// The register asks for a line note.
    #[serde(default)]
    pub requires_note: bool,
    /// `barcode`, `normalized_barcode`, `scale_barcode`, `sku` or `plu`.
    pub source: String,
    /// The code that matched, as stored.
    #[serde(default)]
    pub matched_code: String,
    /// Quantity in thousandths; `None` for a price-embedded scale code.
    #[serde(default)]
    pub qty_thousandths: Option<i64>,
    /// Line amount of a price-embedded scale code (minor units).
    #[serde(default)]
    pub amount: Option<i64>,
    /// The branch whose assortment applied (`None`: none).
    #[serde(default)]
    pub branch_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn id(n: u128) -> String {
        uuid_string(n)
    }

    fn uuid_string(n: u128) -> String {
        format!(
            "{:08x}-0000-7000-8000-{:012x}",
            n >> 48,
            n & 0xffff_ffff_ffff
        )
    }

    fn product() -> Product {
        serde_json::from_value(json!({
            "code": "KAOS", "name": "Kaos", "type": "stock", "tax_class_id": id(9),
            "options": [{"id": id(1), "name": "Size", "values": [{"id": id(2), "name": "M", "code": "M"}]}],
            "units": [{"id": id(3), "name": "pcs", "kind": "integer", "factor_thousandths": 1000}],
            "base_unit_id": id(3),
            "variants": [{"id": id(4), "option_value_ids": [id(2)], "plu": null}],
            "barcodes": [{"id": id(5), "variant_id": id(4), "unit_id": id(3), "code": "123"}],
            "components": [{"variant_id": id(6), "unit_id": id(7), "qty_thousandths": 500, "name": "x"}],
            "used": true, "archived_barcodes": [],
        }))
        .unwrap()
    }

    #[test]
    fn product_round_trips_and_answer_fields_stay_out_of_bodies() {
        let p = product();
        assert!(p.used && p.sellable && p.purchasable);
        assert_eq!(p.tracking, "none");
        assert_eq!(p.components[0].name.as_deref(), Some("x"));
        let body = serde_json::to_value(&p).unwrap();
        for answer_only in ["used", "archived_at", "archived_barcodes", "created_at"] {
            assert!(body.get(answer_only).is_none(), "{answer_only}");
        }
        assert!(body["components"][0].get("name").is_none());
        assert_eq!(body["type"], "stock");

        let draft = p.to_draft(&id(10)).unwrap();
        assert_eq!(draft.units.default_sale, draft.units.base);
        assert_eq!(draft.variants[0].values.len(), 1);
        assert_eq!(draft.components[0].qty, Qty::from_thousandths(500));
        assert_eq!(draft.tracking, Tracking::None);
    }

    #[test]
    fn bad_documents_are_problems() {
        type Edit = fn(&mut Product);
        let cases: [(Edit, &str, &str); 7] = [
            (|p| p.product_type = "x".into(), "invalid_field", "type"),
            (|p| p.tracking = "lot".into(), "invalid_field", "tracking"),
            (
                |p| p.units[0].kind = "x".into(),
                "invalid_field",
                "units.kind",
            ),
            (
                |p| p.base_unit_id = "nope".into(),
                "invalid_body",
                "base_unit_id",
            ),
            (
                |p| p.variants[0].plu = Some(1_234_567),
                "invalid_field",
                "variants.plu",
            ),
            (
                |p| p.barcodes[0].unit_id = "x".into(),
                "invalid_body",
                "barcodes.unit_id",
            ),
            (
                |p| p.components[0].unit_id = "x".into(),
                "invalid_body",
                "components.unit_id",
            ),
        ];
        for (edit, code, field) in cases {
            let mut p = product();
            edit(&mut p);
            let problem = p.to_draft(&id(10)).unwrap_err();
            assert_eq!(
                (problem.code.as_str(), problem.field()),
                (code, Some(field))
            );
        }
        let mut p = product();
        p.tracking = "serial".into();
        assert_eq!(p.to_draft(&id(1)).unwrap().tracking, Tracking::Serial);
        p.tracking = "batch".into();
        assert_eq!(p.to_draft(&id(1)).unwrap().tracking, Tracking::Batch);
        assert_eq!(unit_kind_str(unit_kind("decimal").unwrap()), "decimal");
        assert_eq!(unit_kind_str(UnitKind::Integer), "integer");
    }

    #[test]
    fn small_types() {
        let me = Me {
            permissions: vec![CATALOG_MANAGE.into()],
            permissions_everywhere: vec![CATALOG_VIEW_ARCHIVED.into()],
            ..Me::default()
        };
        assert!(me.holds_here(CATALOG_MANAGE) && !me.holds_everywhere(CATALOG_MANAGE));
        assert!(me.holds_everywhere(CATALOG_VIEW_ARCHIVED));
        let query = ProductQuery {
            q: Some("teh".into()),
            product_type: Some("stock".into()),
            ..ProductQuery::default()
        };
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            json!({"q": "teh", "type": "stock"})
        );
        let archived = ProductQuery {
            archived: true,
            ..ProductQuery::default()
        };
        assert_eq!(
            serde_json::to_value(&archived).unwrap(),
            json!({"archived": true})
        );
        assert_eq!(Generate::ProductCode.path(), "product-code");
        assert_eq!(Generate::Barcode.path(), "barcode");
        let node: CategoryNode =
            serde_json::from_value(json!({"id": "c", "name": "Food", "children": []})).unwrap();
        assert_eq!(node.category().parent_id, None);
    }

    #[test]
    fn lookup_from_an_older_server() {
        let old: Lookup = serde_json::from_value(json!({
            "product_id": "p", "variant_id": "v", "unit_id": "u", "name": "Aqua", "sku": "AQ",
            "type": "stock", "source": "barcode", "qty_thousandths": 1000
        }))
        .unwrap();
        assert_eq!(
            (
                old.product_name.as_str(),
                old.unit_name.as_str(),
                old.plu,
                old.amount
            ),
            ("", "", None, None)
        );
        assert!(!old.open_price && !old.requires_note);
    }
}
