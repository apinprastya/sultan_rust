//! The catalog domain (spec `docs/spec/2.2-catalog.md`, phase C1): quantities and units,
//! options and the variant grid, codes and check digits, scale barcodes, register lookup,
//! bundles, the "frozen after use" rules and the category tree.
//!
//! Pure: no IO. Data that lives elsewhere (other products, company-wide indexes, usage) is
//! passed in as values, closures or the [`CatalogIndex`] trait.

mod brand;
mod bundle;
mod category;
mod code;
mod error;
mod frozen;
mod lookup;
mod product;
mod qty;
mod scale;
mod search;
mod unit;
mod variant;

pub use brand::{Brand, MAX_BRAND_NAME_LEN, validate_brand};
pub use bundle::{
    BundleComponent, ComponentInfo, ComponentMovement, ComponentSnapshot, MAX_BUNDLE_COMPONENTS,
    available_bundles, check_component_lines, explode_bundle, validate_bundle_components,
};
pub use category::{Category, MAX_CATEGORY_DEPTH, MAX_CATEGORY_NAME_LEN, validate_category};
pub use code::{
    CheckDigitStatus, DEFAULT_INTERNAL_BARCODE_PREFIX, DEFAULT_PRODUCT_CODE_PREFIX, Generated,
    GtinFormat, MAX_BARCODE_LEN, MAX_INTERNAL_BARCODE_COUNTER, MAX_PRODUCT_CODE_LEN,
    MAX_SHORT_CODE_LEN, MAX_SKU_LEN, PRODUCT_CODE_COUNTER_WIDTH, Plu, check_barcode,
    check_category_code, check_digit_status, check_internal_barcode_prefix, check_product_code,
    check_sku, check_value_code, gtin_check_digit, next_internal_barcode, next_product_code,
    normalize_product_code, normalize_product_code_prefix, normalize_sku, normalize_value_code,
};
pub use error::{CatalogError, CodeKind};
pub use frozen::check_frozen;
pub use lookup::{
    CatalogIndex, ItemRef, ItemStatus, LookupMatch, MatchSource, check_sellable, resolve_lookup,
};
pub use product::{
    Barcode, MAX_NAME_LEN, MAX_RECEIPT_NAME_LEN, MAX_TAG_LEN, MAX_TAGS, ProductDraft, ProductType,
    Tracking, default_receipt_name, validate_product,
};
pub use qty::Qty;
pub use scale::{
    ScaleCode, ScaleLayout, ScaleValue, ScaleValueKind, decode_scale_barcode, qty_from_price,
    validate_scale_layouts,
};
pub use search::{product_search_key, search_key};
pub use unit::{
    MAX_EXTRA_UNITS, MAX_UNIT_LIST_LEN, MAX_UNIT_NAME_LEN, MAX_UNIT_SHORT_NAME_LEN, ProductUnits,
    Unit, UnitKind, validate_unit_list,
};
pub use variant::{
    GridEntry, MAX_OPTION_NAME_LEN, MAX_OPTIONS, MAX_VARIANTS, OptionValue, ProductOption, Variant,
    default_sku, validate_options, validate_variants, variant_display_name, variant_grid,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::VariantId;

    #[test]
    #[allow(clippy::too_many_lines)] // one table row per error variant
    fn codes_and_messages() {
        let v = VariantId::from_uuid(uuid::Uuid::from_u128(1));
        let s = String::from;
        let cases = [
            (
                CatalogError::InvalidField {
                    field: "name",
                    reason: "required",
                },
                "invalid_field",
                "invalid name: required",
            ),
            (
                CatalogError::DuplicateId { what: "unit" },
                "invalid_field",
                "duplicate unit id",
            ),
            (
                CatalogError::InvalidQty { input: s("1.2345") },
                "invalid_qty",
                "`1.2345` is not a quantity with at most 3 decimal places",
            ),
            (
                CatalogError::FractionalQty { unit: s("pcs") },
                "invalid_qty",
                "unit `pcs` takes whole quantities only",
            ),
            (
                CatalogError::NonPositive { field: "qty" },
                "invalid_qty",
                "qty must be greater than zero",
            ),
            (
                CatalogError::QtyOverflow,
                "qty_overflow",
                "quantity overflow",
            ),
            (
                CatalogError::InvalidCode {
                    kind: CodeKind::Sku,
                    value: s("a b"),
                },
                "invalid_code",
                "invalid SKU `a b`",
            ),
            (
                CatalogError::InvalidCheckDigit { code: s("1") },
                "invalid_check_digit",
                "barcode `1` has an invalid check digit",
            ),
            (
                CatalogError::DuplicateCode { code: s("P1") },
                "duplicate_code",
                "product code `P1` is already used",
            ),
            (
                CatalogError::DuplicateSku { sku: s("S") },
                "duplicate_sku",
                "SKU `S` is already used",
            ),
            (
                CatalogError::DuplicateBarcode { code: s("B") },
                "duplicate_barcode",
                "barcode `B` is already used",
            ),
            (
                CatalogError::DuplicatePlu { plu: 7 },
                "duplicate_plu",
                "PLU 7 is already used",
            ),
            (
                CatalogError::InvalidUnitFactor {
                    unit: s("box"),
                    reason: "must be greater than 1",
                },
                "invalid_unit_factor",
                "invalid factor for unit `box`: must be greater than 1",
            ),
            (
                CatalogError::DuplicateUnitName { name: s("box") },
                "duplicate_unit_name",
                "unit name `box` is used twice",
            ),
            (
                CatalogError::TooManyUnits { max: 5 },
                "too_many_units",
                "at most 5 units besides the base unit",
            ),
            (CatalogError::UnknownUnit, "unknown_unit", "unknown unit"),
            (
                CatalogError::InvalidDefaultUnit {
                    which: "sale",
                    reason: "archived",
                },
                "invalid_default_unit",
                "default sale unit: archived",
            ),
            (
                CatalogError::TooManyOptions { max: 3 },
                "too_many_options",
                "at most 3 options per product",
            ),
            (
                CatalogError::DuplicateOption { name: s("Size") },
                "duplicate_option",
                "option `Size` is used twice",
            ),
            (
                CatalogError::DuplicateOptionValue {
                    option: s("Size"),
                    value: s("M"),
                },
                "duplicate_option_value",
                "option `Size` has value `M` twice",
            ),
            (
                CatalogError::TooManyVariants {
                    count: 501,
                    max: 500,
                },
                "too_many_variants",
                "501 variants, at most 500 allowed",
            ),
            (
                CatalogError::VariantCount { reason: "none" },
                "invalid_variant",
                "invalid variant count: none",
            ),
            (
                CatalogError::VariantValuesMismatch,
                "invalid_variant",
                "variant option values do not match the product options",
            ),
            (
                CatalogError::DuplicateCombination,
                "duplicate_combination",
                "two variants have the same option values",
            ),
            (
                CatalogError::ProductTypeRule {
                    product_type: ProductType::Weighed,
                    reason: "x",
                },
                "product_type_rule",
                "weighed product: x",
            ),
            (
                CatalogError::TrackingNotSupported {
                    tracking: Tracking::Serial,
                },
                "tracking_not_supported",
                "tracking `serial` is not supported yet",
            ),
            (
                CatalogError::BundleComponentCount { count: 0, max: 50 },
                "invalid_bundle",
                "a bundle needs 1 to 50 components, got 0",
            ),
            (
                CatalogError::BundleNesting { variant: v },
                "bundle_nesting",
                "variant 00000000-0000-0000-0000-000000000001 is a bundle and cannot be a bundle component",
            ),
            (
                CatalogError::BundleComponentArchived { variant: v },
                "component_archived",
                "variant 00000000-0000-0000-0000-000000000001 is archived and cannot be a bundle component",
            ),
            (
                CatalogError::BundleComponentSelf { variant: v },
                "invalid_bundle",
                "variant 00000000-0000-0000-0000-000000000001 belongs to the bundle itself",
            ),
            (
                CatalogError::DuplicateBundleComponent { variant: v },
                "duplicate_component",
                "variant 00000000-0000-0000-0000-000000000001 with the same unit appears twice in the bundle",
            ),
            (
                CatalogError::UnknownBundleComponent { variant: v },
                "invalid_bundle",
                "bundle component 00000000-0000-0000-0000-000000000001 not found",
            ),
            (
                CatalogError::FrozenAfterUse { field: "type" },
                "frozen_after_use",
                "`type` cannot change after the product has been used",
            ),
            (
                CatalogError::InvalidScaleLayout {
                    prefix: 21,
                    reason: "x",
                },
                "invalid_scale_layout",
                "invalid scale layout with prefix 21: x",
            ),
            (
                CatalogError::ScalePrefixOverlap { prefix: 21 },
                "scale_prefix_overlap",
                "scale prefix 21 overlaps another prefix",
            ),
            (
                CatalogError::ScaleValueZero,
                "scale_value_zero",
                "scale barcode carries a zero value",
            ),
            (
                CatalogError::GeneratorExhausted {
                    kind: CodeKind::ProductCode,
                },
                "generator_exhausted",
                "product code generator exhausted",
            ),
            (
                CatalogError::CategoryCycle,
                "category_cycle",
                "category move would create a cycle",
            ),
            (
                CatalogError::CategoryTooDeep { depth: 5, max: 4 },
                "category_too_deep",
                "category tree depth 5 exceeds 4",
            ),
            (
                CatalogError::DuplicateCategoryName { name: s("Food") },
                "duplicate_category_name",
                "a sibling category is already named `Food`",
            ),
            (
                CatalogError::DuplicateBrandName { name: s("Aqua") },
                "duplicate_brand_name",
                "a brand is already named `Aqua`",
            ),
            (
                CatalogError::UnknownCategory,
                "unknown_category",
                "unknown parent category",
            ),
            (CatalogError::NotFound, "not_found", "not found"),
            (CatalogError::Archived, "archived", "item is archived"),
            (
                CatalogError::NotSoldHere,
                "not_sold_here",
                "item is not sold at this branch",
            ),
            (
                CatalogError::NotSellable,
                "not_sellable",
                "item is not sellable",
            ),
        ];
        for (error, code, message) in cases {
            assert_eq!(error.code(), code, "{error:?}");
            assert_eq!(error.to_string(), message);
        }
        let kinds = [
            (CodeKind::ProductCode, "product code"),
            (CodeKind::Sku, "SKU"),
            (CodeKind::OptionValueCode, "option value code"),
            (CodeKind::CategoryCode, "category code"),
            (CodeKind::Barcode, "barcode"),
            (CodeKind::Plu, "PLU"),
            (CodeKind::ProductCodePrefix, "product code prefix"),
            (CodeKind::BarcodePrefix, "internal barcode prefix"),
        ];
        for (kind, name) in kinds {
            assert_eq!(kind.to_string(), name);
        }
    }
}
