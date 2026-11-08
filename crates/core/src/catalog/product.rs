//! The product document, product types and the top-level validation (spec 2.2 §2, §5, §12).

use std::fmt;
use std::str::FromStr;

use super::CatalogError;
use super::bundle::{BundleComponent, check_component_lines};
use super::code::{check_barcode, check_product_code};
use super::unit::{ProductUnits, UnitKind};
use super::variant::{ProductOption, Variant, validate_options, validate_variants};
use crate::ids::{BarcodeId, ProductId, UnitId, VariantId};

/// Maximum length of a product name.
pub const MAX_NAME_LEN: usize = 120;
/// Maximum length of the receipt name (32/42-column receipts).
pub const MAX_RECEIPT_NAME_LEN: usize = 32;
/// Most tags per product.
pub const MAX_TAGS: usize = 20;
/// Maximum length of one tag.
pub const MAX_TAG_LEN: usize = 40;

/// The product type (spec 2.2 §5). Fixed once any variant has been used.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProductType {
    /// A normal stocked item.
    Stock,
    /// Sold by weight: decimal base unit only, PLU, scale barcodes.
    Weighed,
    /// No stock, no cost.
    Service,
    /// A kit of other products' variants; no stock of its own.
    Bundle,
}

impl ProductType {
    /// Every type, in display order.
    pub const ALL: [Self; 4] = [Self::Stock, Self::Weighed, Self::Service, Self::Bundle];

    /// The stored / wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Weighed => "weighed",
            Self::Service => "service",
            Self::Bundle => "bundle",
        }
    }

    /// The default of the `purchasable` flag: false for `service` and `bundle` (spec 2.2 §2).
    #[must_use]
    pub const fn default_purchasable(self) -> bool {
        matches!(self, Self::Stock | Self::Weighed)
    }

    /// Whether selling or receiving the product moves its own stock.
    #[must_use]
    pub const fn has_stock(self) -> bool {
        matches!(self, Self::Stock | Self::Weighed)
    }
}

impl fmt::Display for ProductType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ProductType {
    type Err = CatalogError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or(CatalogError::InvalidField {
                field: "type",
                reason: "must be stock, weighed, service or bundle",
            })
    }
}

/// Batch / serial tracking (spec 2.2 §2). Only [`Tracking::None`] is accepted in P1; the other
/// values exist so the column and the wire format are ready for P2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Tracking {
    /// No tracking.
    #[default]
    None,
    /// Batch / expiry tracking (P2).
    Batch,
    /// Serial-number tracking (P2).
    Serial,
}

impl Tracking {
    /// The stored / wire name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Batch => "batch",
            Self::Serial => "serial",
        }
    }
}

impl fmt::Display for Tracking {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A barcode of a variant + unit (spec 2.2 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Barcode {
    /// Barcode ID (client-generated).
    pub id: BarcodeId,
    /// The variant it identifies.
    pub variant: VariantId,
    /// The unit it identifies (e.g. the box barcode).
    pub unit: UnitId,
    /// The code as scanned.
    pub code: String,
    /// Skip EAN/UPC check-digit validation (importer, odd vendor codes).
    pub allow_invalid_check_digit: bool,
}

/// The product document written by `PUT /products/{id}` (spec 2.2 §12): the product with its
/// options, units, variants, barcodes and bundle components. References to other entities
/// (category, brand, tax class) and company-wide uniqueness are checked by the repository.
#[allow(clippy::struct_excessive_bools)] // independent product flags from spec 2.2 §2
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductDraft {
    /// Product ID (client-generated).
    pub id: ProductId,
    /// Product code, upper case, `[A-Z0-9._-]{1,24}`.
    pub code: String,
    /// Name, 1–120 chars, trimmed.
    pub name: String,
    /// Receipt name, 1–32 chars; see [`default_receipt_name`].
    pub receipt_name: String,
    /// Product type.
    pub product_type: ProductType,
    /// Tracking; only [`Tracking::None`] in P1.
    pub tracking: Tracking,
    /// Free labels, lower-cased, ≤20.
    pub tags: Vec<String>,
    /// Can be sold.
    pub sellable: bool,
    /// Can be bought.
    pub purchasable: bool,
    /// The cashier types the price.
    pub open_price: bool,
    /// The register asks for a line note.
    pub requires_note: bool,
    /// Options, in order (≤3).
    pub options: Vec<ProductOption>,
    /// Units.
    pub units: ProductUnits,
    /// Variants (≥1).
    pub variants: Vec<Variant>,
    /// Barcodes of the variants.
    pub barcodes: Vec<Barcode>,
    /// Bundle components (bundle type only).
    pub components: Vec<BundleComponent>,
    /// Archived (soft-deleted).
    pub archived: bool,
}

/// The default receipt name: `name` trimmed and cut to 32 characters (spec 2.2 §2).
#[must_use]
pub fn default_receipt_name(name: &str) -> String {
    name.trim()
        .chars()
        .take(MAX_RECEIPT_NAME_LEN)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

impl ProductDraft {
    /// Normalises user input before validation: trims names, upper-cases the product code,
    /// SKUs and option-value codes, trims barcodes, lower-cases and de-duplicates tags, and
    /// fills an empty receipt name from the name.
    #[must_use]
    pub fn normalized(mut self) -> Self {
        self.code = self.code.trim().to_ascii_uppercase();
        self.name = self.name.trim().to_owned();
        self.receipt_name = self.receipt_name.trim().to_owned();
        if self.receipt_name.is_empty() {
            self.receipt_name = default_receipt_name(&self.name);
        }
        let mut tags: Vec<String> = Vec::new();
        for tag in &self.tags {
            let tag = tag.trim().to_lowercase();
            if !tag.is_empty() && !tags.contains(&tag) {
                tags.push(tag);
            }
        }
        self.tags = tags;
        for option in &mut self.options {
            option.name = option.name.trim().to_owned();
            for value in &mut option.values {
                value.name = value.name.trim().to_owned();
                value.code = value.code.trim().to_ascii_uppercase();
            }
        }
        for unit in &mut self.units.units {
            unit.name = unit.name.trim().to_owned();
            unit.short_name = unit.short_name.trim().to_owned();
        }
        for variant in &mut self.variants {
            variant.sku = variant.sku.trim().to_ascii_uppercase();
        }
        for barcode in &mut self.barcodes {
            barcode.code = barcode.code.trim().to_owned();
        }
        self
    }
}

/// Validates a (normalised) product document against every rule core can check without the
/// database: fields, tracking, options, units, variants, PLUs, barcodes, the product-type
/// rules and the bundle component lines. Collects one error per area.
///
/// Not checked here: company-wide uniqueness (code, SKU, barcode, PLU), references to other
/// entities, bundle components' own products ([`super::validate_bundle_components`]) and the
/// "frozen after use" rules ([`super::check_frozen`]).
///
/// # Errors
/// Every error found, in field order.
pub fn validate_product(draft: &ProductDraft) -> Result<(), Vec<CatalogError>> {
    let mut errors = Vec::new();
    let mut push = |result: Result<(), CatalogError>| {
        if let Err(e) = result {
            errors.push(e);
        }
    };
    push(check_product_code(&draft.code));
    push(check_fields(draft));
    if draft.tracking != Tracking::None {
        push(Err(CatalogError::TrackingNotSupported {
            tracking: draft.tracking,
        }));
    }
    let options = validate_options(&draft.options);
    let options_ok = options.is_ok();
    push(options);
    push(draft.units.validate());
    if options_ok {
        push(validate_variants(&draft.options, &draft.variants));
    }
    push(check_type_rules(draft));
    push(check_plus(draft));
    push(check_barcodes(draft));
    if draft.product_type == ProductType::Bundle {
        push(check_component_lines(&draft.components));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_fields(draft: &ProductDraft) -> Result<(), CatalogError> {
    let invalid = |field, reason| Err(CatalogError::InvalidField { field, reason });
    let name_len = draft.name.chars().count();
    if name_len == 0 || name_len > MAX_NAME_LEN || draft.name.trim() != draft.name {
        return invalid("name", "1–120 characters without surrounding spaces");
    }
    let receipt_len = draft.receipt_name.chars().count();
    if receipt_len == 0 || receipt_len > MAX_RECEIPT_NAME_LEN {
        return invalid("receipt_name", "1–32 characters");
    }
    if draft.tags.len() > MAX_TAGS {
        return invalid("tags", "at most 20 tags");
    }
    let tag_ok = |t: &String| {
        !t.is_empty() && t.chars().count() <= MAX_TAG_LEN && t.trim() == t && *t == t.to_lowercase()
    };
    if !draft.tags.iter().all(tag_ok) {
        return invalid("tags", "1–40 lower-case characters each");
    }
    Ok(())
}

fn check_type_rules(draft: &ProductDraft) -> Result<(), CatalogError> {
    let product_type = draft.product_type;
    let rule = |reason| {
        Err(CatalogError::ProductTypeRule {
            product_type,
            reason,
        })
    };
    let base_kind = draft.units.base_unit().map(|u| u.kind);
    let single_unit = draft.units.units.len() == 1;
    match product_type {
        ProductType::Weighed => {
            if base_kind == Some(UnitKind::Integer) {
                return rule("the base unit must be decimal");
            }
            if !single_unit {
                return rule("no units besides the base unit");
            }
        }
        ProductType::Bundle => {
            if !draft.options.is_empty() {
                return rule("a bundle has no options");
            }
            if !single_unit {
                return rule("no units besides the base unit");
            }
            if base_kind == Some(UnitKind::Decimal) {
                return rule("the base unit must be integer");
            }
        }
        ProductType::Stock | ProductType::Service => {}
    }
    if product_type != ProductType::Bundle && !draft.components.is_empty() {
        return rule("only a bundle has components");
    }
    Ok(())
}

fn check_plus(draft: &ProductDraft) -> Result<(), CatalogError> {
    let mut seen = Vec::new();
    for plu in draft.variants.iter().filter_map(|v| v.plu) {
        if draft.product_type != ProductType::Weighed {
            return Err(CatalogError::ProductTypeRule {
                product_type: draft.product_type,
                reason: "only weighed variants have a PLU",
            });
        }
        if seen.contains(&plu) {
            return Err(CatalogError::DuplicatePlu { plu: plu.value() });
        }
        seen.push(plu);
    }
    Ok(())
}

fn check_barcodes(draft: &ProductDraft) -> Result<(), CatalogError> {
    for (i, barcode) in draft.barcodes.iter().enumerate() {
        let earlier = &draft.barcodes[..i];
        if earlier.iter().any(|b| b.id == barcode.id) {
            return Err(CatalogError::DuplicateId { what: "barcode" });
        }
        check_barcode(&barcode.code, barcode.allow_invalid_check_digit)?;
        if !draft.variants.iter().any(|v| v.id == barcode.variant) {
            return Err(CatalogError::InvalidField {
                field: "barcodes.variant",
                reason: "unknown variant",
            });
        }
        if draft.units.get(barcode.unit).is_none() {
            return Err(CatalogError::UnknownUnit);
        }
        if earlier.iter().any(|b| b.code == barcode.code) {
            return Err(CatalogError::DuplicateBarcode {
                code: barcode.code.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::catalog::code::Plu;
    use crate::catalog::unit::tests::{kg, pcs, unit};
    use crate::catalog::variant::tests::{size_color, variant};
    use crate::catalog::{CodeKind, Qty};
    use uuid::Uuid;

    pub(crate) fn pid(n: u128) -> ProductId {
        ProductId::from_uuid(Uuid::from_u128(n))
    }

    /// A valid stock product `KAOS01` with Size × Color, 6 variants, unit pcs + box(24).
    pub(crate) fn kaos() -> ProductDraft {
        let mut units = ProductUnits::single(pcs());
        units.units.push(unit(2, "box", UnitKind::Integer, 24_000));
        let variants = [(11, 21), (11, 22), (12, 21), (12, 22), (13, 21), (13, 22)]
            .iter()
            .enumerate()
            .map(|(i, (a, b))| variant(100 + i as u128, &format!("KAOS01-{i}"), &[*a, *b]))
            .collect();
        ProductDraft {
            id: pid(1),
            code: "KAOS01".to_owned(),
            name: "Kaos Polos".to_owned(),
            receipt_name: "Kaos Polos".to_owned(),
            product_type: ProductType::Stock,
            tracking: Tracking::None,
            tags: vec!["apparel".to_owned()],
            sellable: true,
            purchasable: true,
            open_price: false,
            requires_note: false,
            options: size_color(),
            units,
            variants,
            barcodes: vec![Barcode {
                id: BarcodeId::from_uuid(Uuid::from_u128(500)),
                variant: VariantId::from_uuid(Uuid::from_u128(100)),
                unit: pcs().id,
                code: "4006381333931".to_owned(),
                allow_invalid_check_digit: false,
            }],
            components: vec![],
            archived: false,
        }
    }

    /// A valid weighed product with one variant (PLU 123) and base unit kg.
    pub(crate) fn cheese() -> ProductDraft {
        let mut v = variant(200, "KEJU", &[]);
        v.plu = Plu::new(123);
        ProductDraft {
            code: "KEJU".to_owned(),
            name: "Keju".to_owned(),
            receipt_name: "Keju".to_owned(),
            product_type: ProductType::Weighed,
            options: vec![],
            units: ProductUnits::single(kg()),
            variants: vec![v],
            barcodes: vec![],
            ..kaos()
        }
    }

    fn errors(draft: &ProductDraft) -> Vec<CatalogError> {
        validate_product(draft).err().unwrap_or_default()
    }

    fn type_rule(product_type: ProductType, reason: &'static str) -> CatalogError {
        CatalogError::ProductTypeRule {
            product_type,
            reason,
        }
    }

    #[test]
    fn valid_products() {
        assert_eq!(validate_product(&kaos()), Ok(()));
        assert_eq!(validate_product(&cheese()), Ok(()));
    }

    #[test]
    fn field_rules() {
        type Edit = fn(&mut ProductDraft);
        let field = |field, reason| vec![CatalogError::InvalidField { field, reason }];
        let name_reason = "1–120 characters without surrounding spaces";
        let cases: Vec<(Edit, Vec<CatalogError>)> = vec![
            (|d| d.name = String::new(), field("name", name_reason)),
            (|d| d.name = "x".repeat(121), field("name", name_reason)),
            (
                |d| d.receipt_name = "x".repeat(33),
                field("receipt_name", "1–32 characters"),
            ),
            (
                |d| d.tags = (0..21).map(|i| format!("t{i}")).collect(),
                field("tags", "at most 20 tags"),
            ),
            (
                |d| d.tags = vec!["Big".to_owned()],
                field("tags", "1–40 lower-case characters each"),
            ),
            (
                |d| d.code = "kaos01".to_owned(),
                vec![CatalogError::InvalidCode {
                    kind: CodeKind::ProductCode,
                    value: "kaos01".into(),
                }],
            ),
            (
                |d| d.tracking = Tracking::Batch,
                vec![CatalogError::TrackingNotSupported {
                    tracking: Tracking::Batch,
                }],
            ),
            (
                |d| d.tracking = Tracking::Serial,
                vec![CatalogError::TrackingNotSupported {
                    tracking: Tracking::Serial,
                }],
            ),
        ];
        for (edit, expected) in cases {
            let mut d = kaos();
            edit(&mut d);
            assert_eq!(errors(&d), expected, "{:?}", (&d.name, &d.tags, &d.code));
        }
    }

    #[test]
    fn collects_errors_from_several_areas() {
        let mut d = kaos();
        d.code = String::new();
        d.options.push(d.options[0].clone());
        d.options[2].id = crate::ids::OptionId::from_uuid(Uuid::from_u128(3));
        d.options[2].name = "Material".into();
        d.units.units[1].factor = Qty::ONE;
        let errs = errors(&d);
        assert_eq!(errs.len(), 3, "{errs:?}");
        assert_eq!(errs[0].code(), "invalid_code");
        // Duplicate value IDs across options; variants are not checked against broken options.
        assert_eq!(
            errs[1],
            CatalogError::DuplicateId {
                what: "option value"
            }
        );
        assert_eq!(errs[2].code(), "invalid_unit_factor");

        let mut no_variants = kaos();
        no_variants.variants.clear();
        no_variants.barcodes.clear();
        assert_eq!(errors(&no_variants)[0].code(), "invalid_variant");
    }

    #[test]
    fn weighed_rules() {
        let mut integer_base = cheese();
        integer_base.units = ProductUnits::single(pcs());
        assert_eq!(
            errors(&integer_base),
            vec![type_rule(
                ProductType::Weighed,
                "the base unit must be decimal"
            )]
        );

        let mut extra_unit = cheese();
        extra_unit
            .units
            .units
            .push(unit(2, "sack", UnitKind::Decimal, 25_000));
        assert_eq!(
            errors(&extra_unit),
            vec![type_rule(
                ProductType::Weighed,
                "no units besides the base unit"
            )]
        );

        let mut dup_plu = cheese();
        dup_plu.options = vec![crate::catalog::variant::tests::option(
            1,
            "Kind",
            vec![
                crate::catalog::variant::tests::value(11, "Gouda", "G"),
                crate::catalog::variant::tests::value(12, "Edam", "E"),
            ],
        )];
        dup_plu.variants = vec![variant(1, "KEJU-G", &[11]), variant(2, "KEJU-E", &[12])];
        dup_plu.variants[0].plu = Plu::new(7);
        dup_plu.variants[1].plu = Plu::new(8);
        assert_eq!(validate_product(&dup_plu), Ok(()));
        dup_plu.variants[1].plu = Plu::new(7);
        assert_eq!(
            errors(&dup_plu),
            vec![CatalogError::DuplicatePlu { plu: 7 }]
        );

        let mut stock_plu = kaos();
        stock_plu.variants[0].plu = Plu::new(1);
        assert_eq!(
            errors(&stock_plu),
            vec![type_rule(
                ProductType::Stock,
                "only weighed variants have a PLU"
            )]
        );
    }

    #[test]
    fn bundle_and_component_rules() {
        let mut bundle = cheese();
        bundle.product_type = ProductType::Bundle;
        bundle.variants[0].plu = None;
        bundle.units = ProductUnits::single(pcs());
        bundle.components = vec![crate::catalog::bundle::BundleComponent {
            variant: VariantId::from_uuid(Uuid::from_u128(900)),
            unit: pcs().id,
            qty: Qty::ONE,
        }];
        assert_eq!(validate_product(&bundle), Ok(()));

        let mut decimal = bundle.clone();
        decimal.units = ProductUnits::single(kg());
        assert_eq!(
            errors(&decimal),
            vec![type_rule(
                ProductType::Bundle,
                "the base unit must be integer"
            )]
        );

        let mut extra = bundle.clone();
        extra
            .units
            .units
            .push(unit(2, "box", UnitKind::Integer, 6_000));
        assert_eq!(
            errors(&extra),
            vec![type_rule(
                ProductType::Bundle,
                "no units besides the base unit"
            )]
        );

        let mut options = kaos();
        options.product_type = ProductType::Bundle;
        options.components = bundle.components.clone();
        assert!(
            errors(&options).contains(&type_rule(ProductType::Bundle, "a bundle has no options"))
        );

        let mut empty = bundle.clone();
        empty.components.clear();
        assert_eq!(
            errors(&empty),
            vec![CatalogError::BundleComponentCount { count: 0, max: 50 }]
        );

        let mut stock_with_components = kaos();
        stock_with_components.components = bundle.components;
        assert_eq!(
            errors(&stock_with_components),
            vec![type_rule(
                ProductType::Stock,
                "only a bundle has components"
            )]
        );

        let mut service = kaos();
        service.product_type = ProductType::Service;
        assert_eq!(validate_product(&service), Ok(()));
    }

    #[test]
    fn barcode_rules() {
        let bc = |n: u128, code: &str| Barcode {
            id: BarcodeId::from_uuid(Uuid::from_u128(n)),
            variant: VariantId::from_uuid(Uuid::from_u128(100)),
            unit: pcs().id,
            code: code.to_owned(),
            allow_invalid_check_digit: false,
        };
        let mut dup = kaos();
        dup.barcodes.push(bc(501, "4006381333931"));
        assert_eq!(
            errors(&dup),
            vec![CatalogError::DuplicateBarcode {
                code: "4006381333931".into()
            }]
        );

        let mut dup_id = kaos();
        dup_id.barcodes.push(bc(500, "ABC"));
        assert_eq!(
            errors(&dup_id),
            vec![CatalogError::DuplicateId { what: "barcode" }]
        );

        let mut bad_check = kaos();
        bad_check.barcodes[0].code = "4006381333932".into();
        assert_eq!(
            errors(&bad_check),
            vec![CatalogError::InvalidCheckDigit {
                code: "4006381333932".into()
            }]
        );
        bad_check.barcodes[0].allow_invalid_check_digit = true;
        assert_eq!(validate_product(&bad_check), Ok(()));

        let mut unknown_variant = kaos();
        unknown_variant.barcodes[0].variant = VariantId::from_uuid(Uuid::from_u128(999));
        assert_eq!(
            errors(&unknown_variant),
            vec![CatalogError::InvalidField {
                field: "barcodes.variant",
                reason: "unknown variant"
            }]
        );

        let mut unknown_unit = kaos();
        unknown_unit.barcodes[0].unit = UnitId::from_uuid(Uuid::from_u128(999));
        assert_eq!(errors(&unknown_unit), vec![CatalogError::UnknownUnit]);
    }

    #[test]
    fn normalization() {
        let mut d = kaos();
        d.code = " kaos01 ".into();
        d.name = "  Kaos Polos Lengan Panjang Warna Merah Muda  ".into();
        d.receipt_name = "  ".into();
        d.tags = vec![" Apparel ".into(), "apparel".into(), String::new()];
        d.options[0].name = " Size ".into();
        d.options[0].values[0].code = " s ".into();
        d.options[0].values[0].name = " Small ".into();
        d.units.units[1].name = " box ".into();
        d.units.units[1].short_name = " BX ".into();
        d.variants[0].sku = " kaos01-0 ".into();
        d.barcodes[0].code = " 4006381333931 ".into();
        let n = d.normalized();
        assert_eq!(n.code, "KAOS01");
        assert_eq!(n.name, "Kaos Polos Lengan Panjang Warna Merah Muda");
        assert_eq!(n.receipt_name, "Kaos Polos Lengan Panjang Warna");
        assert_eq!(n.tags, vec!["apparel".to_owned()]);
        assert_eq!(
            (
                n.options[0].name.as_str(),
                n.options[0].values[0].code.as_str()
            ),
            ("Size", "S")
        );
        assert_eq!(n.options[0].values[0].name, "Small");
        assert_eq!(
            (
                n.units.units[1].name.as_str(),
                n.units.units[1].short_name.as_str()
            ),
            ("box", "BX")
        );
        assert_eq!(n.variants[0].sku, "KAOS01-0");
        assert_eq!(n.barcodes[0].code, "4006381333931");
        assert_eq!(validate_product(&n), Ok(()));
        assert_eq!(default_receipt_name(" Teh "), "Teh");
    }

    #[test]
    fn type_and_tracking_names() {
        for t in ProductType::ALL {
            assert_eq!(t.as_str().parse::<ProductType>(), Ok(t));
            assert_eq!(t.to_string(), t.as_str());
        }
        assert!("kit".parse::<ProductType>().is_err());
        let purchasable: Vec<bool> = ProductType::ALL
            .iter()
            .map(|t| t.default_purchasable())
            .collect();
        assert_eq!(purchasable, [true, true, false, false]);
        let stock: Vec<bool> = ProductType::ALL.iter().map(|t| t.has_stock()).collect();
        assert_eq!(stock, [true, true, false, false]);
        assert_eq!(Tracking::default(), Tracking::None);
        let names: Vec<String> = [Tracking::None, Tracking::Batch, Tracking::Serial]
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(names, ["none", "batch", "serial"]);
    }
}
