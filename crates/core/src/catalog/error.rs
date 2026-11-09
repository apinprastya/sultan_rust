//! The catalog error type and its stable codes (spec 2.2 §12).

use std::fmt;

use crate::ids::VariantId;

/// Which kind of code a [`CatalogError::InvalidCode`] is about (spec 2.2 §2, §3, §6, §7, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CodeKind {
    /// Product code, `[A-Z0-9._-]{1,24}`.
    ProductCode,
    /// Variant SKU, `[A-Z0-9._-]{1,32}`.
    Sku,
    /// Option-value short code, `[A-Z0-9]{1,8}`.
    OptionValueCode,
    /// Category short code, `[A-Z0-9]{1,8}` (optional).
    CategoryCode,
    /// Barcode, 1–64 printable ASCII characters without spaces.
    Barcode,
    /// PLU of a weighed variant, 1–6 digits.
    Plu,
    /// Prefix of generated product codes, `[A-Z0-9._-]{0,17}`.
    ProductCodePrefix,
    /// Prefix of generated internal barcodes, exactly 2 digits.
    BarcodePrefix,
}

impl CodeKind {
    /// A short name for messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProductCode => "product code",
            Self::Sku => "SKU",
            Self::OptionValueCode => "option value code",
            Self::CategoryCode => "category code",
            Self::Barcode => "barcode",
            Self::Plu => "PLU",
            Self::ProductCodePrefix => "product code prefix",
            Self::BarcodePrefix => "internal barcode prefix",
        }
    }
}

impl fmt::Display for CodeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Errors of the catalog rules. Each variant maps to a stable problem+json code through
/// [`CatalogError::code`] (spec 2.2 §12; generic field errors use the existing `invalid_field`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CatalogError {
    /// A field breaks a simple rule (empty, too long, too many items…).
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// The field, e.g. `name`, `receipt_name`, `tags`.
        field: &'static str,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// A quantity string is not a decimal number with at most 3 decimal places (spec 2.2 §4).
    #[error("`{input}` is not a quantity with at most 3 decimal places")]
    InvalidQty {
        /// The rejected input.
        input: String,
    },
    /// A fractional quantity for an integer unit (spec 2.2 §4).
    #[error("unit `{unit}` takes whole quantities only")]
    FractionalQty {
        /// The unit name.
        unit: String,
    },
    /// A quantity that must be above zero is not (bundle components, unit prices).
    #[error("{field} must be greater than zero")]
    NonPositive {
        /// The field.
        field: &'static str,
    },
    /// A quantity does not fit in `i64` thousandths.
    #[error("quantity overflow")]
    QtyOverflow,
    /// A code breaks its format rule.
    #[error("invalid {kind} `{value}`")]
    InvalidCode {
        /// Which code.
        kind: CodeKind,
        /// The rejected value.
        value: String,
    },
    /// An EAN-13 / EAN-8 / UPC-A barcode whose check digit does not match (spec 2.2 §6).
    #[error("barcode `{code}` has an invalid check digit")]
    InvalidCheckDigit {
        /// The barcode.
        code: String,
    },
    /// The product code is already used by another product (checked by the repository).
    #[error("product code `{code}` is already used")]
    DuplicateCode {
        /// The code.
        code: String,
    },
    /// The SKU is already used by another variant (spec 2.2 §3).
    #[error("SKU `{sku}` is already used")]
    DuplicateSku {
        /// The SKU.
        sku: String,
    },
    /// The barcode is already used by another variant + unit (spec 2.2 §6).
    #[error("barcode `{code}` is already used")]
    DuplicateBarcode {
        /// The barcode.
        code: String,
    },
    /// The PLU is already used by another weighed variant (spec 2.2 §5).
    #[error("PLU {plu} is already used")]
    DuplicatePlu {
        /// The PLU.
        plu: u32,
    },
    /// Two rows of one product share an ID.
    #[error("duplicate {what} id")]
    DuplicateId {
        /// Which kind of row (`unit`, `option`, `option value`, `variant`, `barcode`).
        what: &'static str,
    },
    /// A unit factor breaks the rules of spec 2.2 §4.
    #[error("invalid factor for unit `{unit}`: {reason}")]
    InvalidUnitFactor {
        /// The unit name.
        unit: String,
        /// Why.
        reason: &'static str,
    },
    /// Two active units of one product have the same name (spec 2.2 §4).
    #[error("unit name `{name}` is used twice")]
    DuplicateUnitName {
        /// The name.
        name: String,
    },
    /// More than [`super::MAX_EXTRA_UNITS`] units besides the base unit (spec 2.2 §4).
    #[error("at most {max} units besides the base unit")]
    TooManyUnits {
        /// The limit.
        max: usize,
    },
    /// A reference to a unit that the product does not have.
    #[error("unknown unit")]
    UnknownUnit,
    /// The default sale or purchase unit is archived or lacks the matching flag (spec 2.2 §4).
    #[error("default {which} unit: {reason}")]
    InvalidDefaultUnit {
        /// `sale` or `purchase`.
        which: &'static str,
        /// Why.
        reason: &'static str,
    },
    /// More than [`super::MAX_OPTIONS`] options (spec 2.2 §3).
    #[error("at most {max} options per product")]
    TooManyOptions {
        /// The limit.
        max: usize,
    },
    /// Two options of one product have the same name.
    #[error("option `{name}` is used twice")]
    DuplicateOption {
        /// The option name.
        name: String,
    },
    /// Two values of one option have the same name or short code (spec 2.2 §3).
    #[error("option `{option}` has value `{value}` twice")]
    DuplicateOptionValue {
        /// The option name.
        option: String,
        /// The repeated value name or code.
        value: String,
    },
    /// More than [`super::MAX_VARIANTS`] variants (spec 2.2 §3).
    #[error("{count} variants, at most {max} allowed")]
    TooManyVariants {
        /// The number of variants requested.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// The product has no variant, or a product without options has more than one.
    #[error("invalid variant count: {reason}")]
    VariantCount {
        /// Why.
        reason: &'static str,
    },
    /// A variant's option values do not match the product's options (one value per option,
    /// in option order).
    #[error("variant option values do not match the product options")]
    VariantValuesMismatch,
    /// Two variants have the same option-value combination (spec 2.2 §3).
    #[error("two variants have the same option values")]
    DuplicateCombination,
    /// A rule of the product type is broken (spec 2.2 §5).
    #[error("{product_type} product: {reason}")]
    ProductTypeRule {
        /// The product type.
        product_type: super::ProductType,
        /// Which rule.
        reason: &'static str,
    },
    /// Batch / serial tracking is reserved for P2 (spec 2.2 §2).
    #[error("tracking `{tracking}` is not supported yet")]
    TrackingNotSupported {
        /// The rejected tracking mode.
        tracking: super::Tracking,
    },
    /// A bundle has fewer than 1 or more than [`super::MAX_BUNDLE_COMPONENTS`] components.
    #[error("a bundle needs 1 to {max} components, got {count}")]
    BundleComponentCount {
        /// Components given.
        count: usize,
        /// The limit.
        max: usize,
    },
    /// A bundle component is itself a bundle (spec 2.2 §5).
    #[error("variant {variant} is a bundle and cannot be a bundle component")]
    BundleNesting {
        /// The offending component.
        variant: VariantId,
    },
    /// A bundle component is archived (spec 2.2 §5, §9).
    #[error("variant {variant} is archived and cannot be a bundle component")]
    BundleComponentArchived {
        /// The offending component.
        variant: VariantId,
    },
    /// A bundle component belongs to the bundle product itself (spec 2.2 §5).
    #[error("variant {variant} belongs to the bundle itself")]
    BundleComponentSelf {
        /// The offending component.
        variant: VariantId,
    },
    /// The same variant + unit appears twice in a bundle (spec 2.2 §5).
    #[error("variant {variant} with the same unit appears twice in the bundle")]
    DuplicateBundleComponent {
        /// The repeated component.
        variant: VariantId,
    },
    /// A bundle component references a variant or unit that does not exist.
    #[error("bundle component {variant} not found")]
    UnknownBundleComponent {
        /// The component.
        variant: VariantId,
    },
    /// A change that is not allowed once the product has been used (spec 2.2 §3, §4).
    #[error("`{field}` cannot change after the product has been used")]
    FrozenAfterUse {
        /// The frozen field: `type`, `base_unit`, `units`, `units.factor`, `units.kind`,
        /// `options` or `options.values`.
        field: &'static str,
    },
    /// A scale barcode layout breaks the rules of spec 2.2 §6.
    #[error("invalid scale layout with prefix {prefix}: {reason}")]
    InvalidScaleLayout {
        /// The layout prefix.
        prefix: u8,
        /// Why.
        reason: &'static str,
    },
    /// Two scale layouts share a prefix, or a layout uses the internal barcode prefix.
    #[error("scale prefix {prefix} overlaps another prefix")]
    ScalePrefixOverlap {
        /// The overlapping prefix.
        prefix: u8,
    },
    /// A scale barcode with an embedded weight or price of zero (spec 2.2 §6).
    #[error("scale barcode carries a zero value")]
    ScaleValueZero,
    /// A code generator ran out of values (the counter no longer fits the code format).
    #[error("{kind} generator exhausted")]
    GeneratorExhausted {
        /// Which generator.
        kind: CodeKind,
    },
    /// Moving the category would make it its own ancestor (spec 2.2 §8).
    #[error("category move would create a cycle")]
    CategoryCycle,
    /// The category tree would be deeper than [`super::MAX_CATEGORY_DEPTH`] (spec 2.2 §8).
    #[error("category tree depth {depth} exceeds {max}")]
    CategoryTooDeep {
        /// The resulting depth.
        depth: usize,
        /// The limit.
        max: usize,
    },
    /// A sibling category already has this name (spec 2.2 §8).
    #[error("a sibling category is already named `{name}`")]
    DuplicateCategoryName {
        /// The name.
        name: String,
    },
    /// Another brand already has this name (spec 2.2 §8).
    #[error("a brand is already named `{name}`")]
    DuplicateBrandName {
        /// The name.
        name: String,
    },
    /// The parent category does not exist.
    #[error("unknown parent category")]
    UnknownCategory,
    /// No product matches the scanned or typed code (spec 2.2 §6).
    #[error("not found")]
    NotFound,
    /// The product or variant is archived (spec 2.2 §6, §9).
    #[error("item is archived")]
    Archived,
    /// The product is blocked at this branch (spec 2.2 §10).
    #[error("item is not sold at this branch")]
    NotSoldHere,
    /// The product is not sellable (spec 2.2 §2).
    #[error("item is not sellable")]
    NotSellable,
}

impl CatalogError {
    /// The field of the request body this error is about (`variants.sku`,
    /// `units.factor_thousandths`, …), when there is one. The server sends it as `field` in
    /// problem+json; the back office uses it to put the error next to the right input.
    #[must_use]
    pub fn field(&self) -> Option<&'static str> {
        Some(match self {
            Self::InvalidField { field, .. }
            | Self::NonPositive { field }
            | Self::FrozenAfterUse { field } => field,
            Self::InvalidCode { kind, .. } => match kind {
                CodeKind::ProductCode | CodeKind::CategoryCode => "code",
                CodeKind::Sku => "variants.sku",
                CodeKind::OptionValueCode => "options.values.code",
                CodeKind::Barcode => "barcodes.code",
                CodeKind::Plu => "variants.plu",
                CodeKind::ProductCodePrefix => "product_code_prefix",
                CodeKind::BarcodePrefix => "internal_barcode_prefix",
            },
            Self::InvalidCheckDigit { .. } | Self::DuplicateBarcode { .. } => "barcodes.code",
            Self::DuplicateCode { .. } => "code",
            Self::DuplicateSku { .. } => "variants.sku",
            Self::DuplicatePlu { .. } => "variants.plu",
            Self::InvalidUnitFactor { .. } => "units.factor_thousandths",
            Self::DuplicateUnitName { .. } => "units.name",
            Self::TooManyUnits { .. } | Self::UnknownUnit => "units",
            Self::InvalidDefaultUnit { which, .. } => {
                if *which == "sale" {
                    "default_sale_unit_id"
                } else {
                    "default_purchase_unit_id"
                }
            }
            Self::TooManyOptions { .. }
            | Self::DuplicateOption { .. }
            | Self::DuplicateOptionValue { .. } => "options",
            Self::TooManyVariants { .. }
            | Self::VariantCount { .. }
            | Self::VariantValuesMismatch
            | Self::DuplicateCombination => "variants",
            Self::TrackingNotSupported { .. } => "tracking",
            Self::ProductTypeRule { .. } => "type",
            Self::BundleComponentCount { .. }
            | Self::BundleNesting { .. }
            | Self::BundleComponentArchived { .. }
            | Self::BundleComponentSelf { .. }
            | Self::DuplicateBundleComponent { .. }
            | Self::UnknownBundleComponent { .. } => "components",
            Self::InvalidScaleLayout { .. } | Self::ScalePrefixOverlap { .. } => "scale_layouts",
            Self::CategoryCycle | Self::CategoryTooDeep { .. } | Self::UnknownCategory => {
                "parent_id"
            }
            Self::DuplicateCategoryName { .. } | Self::DuplicateBrandName { .. } => "name",
            _ => return None,
        })
    }

    /// The stable error code used in problem+json responses (spec 2.2 §12).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidField { .. } | Self::DuplicateId { .. } => "invalid_field",
            Self::InvalidQty { .. } | Self::FractionalQty { .. } | Self::NonPositive { .. } => {
                "invalid_qty"
            }
            Self::QtyOverflow => "qty_overflow",
            Self::InvalidCode { .. } => "invalid_code",
            Self::InvalidCheckDigit { .. } => "invalid_check_digit",
            Self::DuplicateCode { .. } => "duplicate_code",
            Self::DuplicateSku { .. } => "duplicate_sku",
            Self::DuplicateBarcode { .. } => "duplicate_barcode",
            Self::DuplicatePlu { .. } => "duplicate_plu",
            Self::InvalidUnitFactor { .. } => "invalid_unit_factor",
            Self::DuplicateUnitName { .. } => "duplicate_unit_name",
            Self::TooManyUnits { .. } => "too_many_units",
            Self::UnknownUnit => "unknown_unit",
            Self::InvalidDefaultUnit { .. } => "invalid_default_unit",
            Self::TooManyOptions { .. } => "too_many_options",
            Self::DuplicateOption { .. } => "duplicate_option",
            Self::DuplicateOptionValue { .. } => "duplicate_option_value",
            Self::TooManyVariants { .. } => "too_many_variants",
            Self::VariantCount { .. } | Self::VariantValuesMismatch => "invalid_variant",
            Self::DuplicateCombination => "duplicate_combination",
            Self::ProductTypeRule { .. } => "product_type_rule",
            Self::TrackingNotSupported { .. } => "tracking_not_supported",
            Self::BundleComponentCount { .. }
            | Self::BundleComponentSelf { .. }
            | Self::UnknownBundleComponent { .. } => "invalid_bundle",
            Self::BundleNesting { .. } => "bundle_nesting",
            Self::BundleComponentArchived { .. } => "component_archived",
            Self::DuplicateBundleComponent { .. } => "duplicate_component",
            Self::FrozenAfterUse { .. } => "frozen_after_use",
            Self::InvalidScaleLayout { .. } => "invalid_scale_layout",
            Self::ScalePrefixOverlap { .. } => "scale_prefix_overlap",
            Self::ScaleValueZero => "scale_value_zero",
            Self::GeneratorExhausted { .. } => "generator_exhausted",
            Self::CategoryCycle => "category_cycle",
            Self::CategoryTooDeep { .. } => "category_too_deep",
            Self::DuplicateCategoryName { .. } => "duplicate_category_name",
            Self::DuplicateBrandName { .. } => "duplicate_brand_name",
            Self::UnknownCategory => "unknown_category",
            Self::NotFound => "not_found",
            Self::Archived => "archived",
            Self::NotSoldHere => "not_sold_here",
            Self::NotSellable => "not_sellable",
        }
    }
}
