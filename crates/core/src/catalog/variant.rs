//! Options, option values and the variant grid (spec 2.2 §3).

use super::code::{Plu, check_sku, check_value_code};
use super::search::search_key;
use super::{CatalogError, CodeKind};
use crate::ids::{OptionId, OptionValueId, VariantId};

/// Most options per product (decided 2026-11-08).
pub const MAX_OPTIONS: usize = 3;
/// Most variants per product *(proposed in the spec)*.
pub const MAX_VARIANTS: usize = 500;
/// Maximum length of an option or option-value name.
pub const MAX_OPTION_NAME_LEN: usize = 40;

/// One value of an option, e.g. `Medium` / `M`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OptionValue {
    /// Value ID (client-generated).
    pub id: OptionValueId,
    /// Display name, e.g. `Medium`.
    pub name: String,
    /// Short code used in default SKUs, `[A-Z0-9]{1,8}`, e.g. `M`.
    pub code: String,
}

/// An option attribute with its ordered values, e.g. Size: S, M, L.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductOption {
    /// Option ID (client-generated).
    pub id: OptionId,
    /// Name, e.g. `Size`.
    pub name: String,
    /// Ordered values; at least one.
    pub values: Vec<OptionValue>,
}

impl ProductOption {
    /// The value with this ID.
    #[must_use]
    pub fn value(&self, id: OptionValueId) -> Option<&OptionValue> {
        self.values.iter().find(|v| v.id == id)
    }
}

/// A variant: the thing that is sold, stocked and priced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// Variant ID (client-generated).
    pub id: VariantId,
    /// SKU, `[A-Z0-9._-]{1,32}`, unique company-wide.
    pub sku: String,
    /// One value per option, in option order (empty for a product without options).
    pub values: Vec<OptionValueId>,
    /// PLU, weighed products only (spec 2.2 §5).
    pub plu: Option<Plu>,
    /// Archived variants cannot be sold or bought (spec 2.2 §9).
    pub archived: bool,
}

/// One cell of the variant grid: a combination of option values and its default SKU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GridEntry {
    /// One value per option, in option order.
    pub values: Vec<OptionValueId>,
    /// The default SKU, e.g. `KAOS01-M-RED`.
    pub sku: String,
}

/// The default SKU: `<product code>-<value codes joined by ->`, or the product code alone
/// without options (spec 2.2 §3).
#[must_use]
pub fn default_sku(product_code: &str, value_codes: &[&str]) -> String {
    let mut sku = product_code.to_owned();
    for code in value_codes {
        sku.push('-');
        sku.push_str(code);
    }
    sku
}

/// The variant display name: `<product name> – <values joined by " / ">`, or the product name
/// alone without options (spec 2.2 §3).
#[must_use]
pub fn variant_display_name(product_name: &str, value_names: &[&str]) -> String {
    if value_names.is_empty() {
        product_name.to_owned()
    } else {
        format!("{product_name} – {}", value_names.join(" / "))
    }
}

fn check_name(name: &str, field: &'static str) -> Result<(), CatalogError> {
    let len = name.chars().count();
    if len == 0 || len > MAX_OPTION_NAME_LEN || name.trim() != name {
        return Err(CatalogError::InvalidField {
            field,
            reason: "1–40 characters without surrounding spaces",
        });
    }
    Ok(())
}

/// Validates the options: at most [`MAX_OPTIONS`]; unique IDs; names 1–40 chars, unique
/// (case- and accent-insensitive); each option has ≥1 value; value names and short codes are
/// valid and unique within the option (spec 2.2 §3).
///
/// # Errors
/// The first broken rule.
pub fn validate_options(options: &[ProductOption]) -> Result<(), CatalogError> {
    if options.len() > MAX_OPTIONS {
        return Err(CatalogError::TooManyOptions { max: MAX_OPTIONS });
    }
    let mut value_ids = Vec::new();
    for (i, option) in options.iter().enumerate() {
        check_name(&option.name, "options.name")?;
        if options[..i].iter().any(|o| o.id == option.id) {
            return Err(CatalogError::DuplicateId { what: "option" });
        }
        if options[..i]
            .iter()
            .any(|o| search_key(&o.name) == search_key(&option.name))
        {
            return Err(CatalogError::DuplicateOption {
                name: option.name.clone(),
            });
        }
        if option.values.is_empty() {
            return Err(CatalogError::InvalidField {
                field: "options.values",
                reason: "an option needs at least one value",
            });
        }
        for (j, value) in option.values.iter().enumerate() {
            check_name(&value.name, "options.values.name")?;
            check_value_code(&value.code)?;
            if value_ids.contains(&value.id) {
                return Err(CatalogError::DuplicateId {
                    what: "option value",
                });
            }
            value_ids.push(value.id);
            let earlier = &option.values[..j];
            let duplicate = if earlier.iter().any(|v| v.code == value.code) {
                Some(&value.code)
            } else if earlier
                .iter()
                .any(|v| search_key(&v.name) == search_key(&value.name))
            {
                Some(&value.name)
            } else {
                None
            };
            if let Some(dup) = duplicate {
                return Err(CatalogError::DuplicateOptionValue {
                    option: option.name.clone(),
                    value: dup.clone(),
                });
            }
        }
    }
    Ok(())
}

/// Generates the variant grid: the cartesian product of the option values (in option and value
/// order) minus the `switched_off` combinations, each with its default SKU. No options → exactly
/// one entry with SKU = product code (spec 2.2 §3).
///
/// # Errors
/// See [`validate_options`]; [`CatalogError::TooManyVariants`] above [`MAX_VARIANTS`];
/// [`CatalogError::InvalidCode`] when a default SKU is longer than 32 characters.
pub fn variant_grid(
    product_code: &str,
    options: &[ProductOption],
    switched_off: &[Vec<OptionValueId>],
) -> Result<Vec<GridEntry>, CatalogError> {
    validate_options(options)?;
    // Option count ≤ 3, so the product is computed without overflow for any realistic sizes.
    let total = options
        .iter()
        .try_fold(1usize, |acc, o| acc.checked_mul(o.values.len()))
        .unwrap_or(usize::MAX);
    if total.saturating_sub(switched_off.len()) > MAX_VARIANTS {
        return Err(CatalogError::TooManyVariants {
            count: total.saturating_sub(switched_off.len()),
            max: MAX_VARIANTS,
        });
    }
    let mut combos: Vec<Vec<&OptionValue>> = vec![Vec::new()];
    for option in options {
        combos = combos
            .into_iter()
            .flat_map(|prefix| {
                option.values.iter().map(move |v| {
                    let mut combo = prefix.clone();
                    combo.push(v);
                    combo
                })
            })
            .collect();
    }
    let mut grid = Vec::new();
    for combo in combos {
        let values: Vec<OptionValueId> = combo.iter().map(|v| v.id).collect();
        if switched_off.contains(&values) {
            continue;
        }
        let codes: Vec<&str> = combo.iter().map(|v| v.code.as_str()).collect();
        let sku = default_sku(product_code, &codes);
        if sku.len() > super::code::MAX_SKU_LEN {
            return Err(CatalogError::InvalidCode {
                kind: CodeKind::Sku,
                value: sku,
            });
        }
        grid.push(GridEntry { values, sku });
    }
    if grid.len() > MAX_VARIANTS {
        return Err(CatalogError::TooManyVariants {
            count: grid.len(),
            max: MAX_VARIANTS,
        });
    }
    Ok(grid)
}

/// Validates the variants against the (already valid) options: at least one variant, at most
/// [`MAX_VARIANTS`]; a product without options has exactly one; unique IDs; each variant has one
/// value per option, from that option, in order; no two variants share a combination (archived
/// ones included); SKUs are valid and unique within the product (spec 2.2 §3).
///
/// # Errors
/// The first broken rule.
pub fn validate_variants(
    options: &[ProductOption],
    variants: &[Variant],
) -> Result<(), CatalogError> {
    if variants.is_empty() {
        return Err(CatalogError::VariantCount {
            reason: "a product needs at least one variant",
        });
    }
    if options.is_empty() && variants.len() > 1 {
        return Err(CatalogError::VariantCount {
            reason: "a product without options has exactly one variant",
        });
    }
    if variants.len() > MAX_VARIANTS {
        return Err(CatalogError::TooManyVariants {
            count: variants.len(),
            max: MAX_VARIANTS,
        });
    }
    for (i, variant) in variants.iter().enumerate() {
        let earlier = &variants[..i];
        if earlier.iter().any(|v| v.id == variant.id) {
            return Err(CatalogError::DuplicateId { what: "variant" });
        }
        let matches_options = variant.values.len() == options.len()
            && options
                .iter()
                .zip(&variant.values)
                .all(|(o, v)| o.value(*v).is_some());
        if !matches_options {
            return Err(CatalogError::VariantValuesMismatch);
        }
        if earlier.iter().any(|v| v.values == variant.values) {
            return Err(CatalogError::DuplicateCombination);
        }
        check_sku(&variant.sku)?;
        if earlier.iter().any(|v| v.sku == variant.sku) {
            return Err(CatalogError::DuplicateSku {
                sku: variant.sku.clone(),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use uuid::Uuid;

    pub(crate) fn value(n: u128, name: &str, code: &str) -> OptionValue {
        OptionValue {
            id: OptionValueId::from_uuid(Uuid::from_u128(n)),
            name: name.to_owned(),
            code: code.to_owned(),
        }
    }

    pub(crate) fn option(n: u128, name: &str, values: Vec<OptionValue>) -> ProductOption {
        ProductOption {
            id: OptionId::from_uuid(Uuid::from_u128(n)),
            name: name.to_owned(),
            values,
        }
    }

    /// Size: S, M, L (values 11–13) and Color: Red, Blue (values 21–22).
    pub(crate) fn size_color() -> Vec<ProductOption> {
        vec![
            option(
                1,
                "Size",
                vec![
                    value(11, "Small", "S"),
                    value(12, "Medium", "M"),
                    value(13, "Large", "L"),
                ],
            ),
            option(
                2,
                "Color",
                vec![value(21, "Red", "RED"), value(22, "Blue", "BLUE")],
            ),
        ]
    }

    fn vid(n: u128) -> OptionValueId {
        OptionValueId::from_uuid(Uuid::from_u128(n))
    }

    pub(crate) fn variant(n: u128, sku: &str, values: &[u128]) -> Variant {
        Variant {
            id: VariantId::from_uuid(Uuid::from_u128(n)),
            sku: sku.to_owned(),
            values: values.iter().map(|v| vid(*v)).collect(),
            plu: None,
            archived: false,
        }
    }

    #[test]
    fn grid_with_switched_off_combination() {
        let grid = variant_grid("KAOS01", &size_color(), &[vec![vid(13), vid(22)]]).unwrap();
        let skus: Vec<&str> = grid.iter().map(|e| e.sku.as_str()).collect();
        assert_eq!(
            skus,
            [
                "KAOS01-S-RED",
                "KAOS01-S-BLUE",
                "KAOS01-M-RED",
                "KAOS01-M-BLUE",
                "KAOS01-L-RED"
            ]
        );
        assert_eq!(grid[2].values, vec![vid(12), vid(21)]);
    }

    #[test]
    fn grid_without_options_is_one_variant() {
        let grid = variant_grid("KAOS01", &[], &[]).unwrap();
        assert_eq!(
            grid,
            vec![GridEntry {
                values: vec![],
                sku: "KAOS01".to_owned()
            }]
        );
    }

    #[test]
    fn grid_limits() {
        let values = |base: u128, n: u128| {
            (0..n)
                .map(|i| value(base + i, &format!("v{i}"), &format!("V{i}")))
                .collect()
        };
        // 8 × 8 × 8 = 512 > 500.
        let big = vec![
            option(1, "A", values(100, 8)),
            option(2, "B", values(200, 8)),
            option(3, "C", values(300, 8)),
        ];
        assert_eq!(
            variant_grid("P", &big, &[]),
            Err(CatalogError::TooManyVariants {
                count: 512,
                max: MAX_VARIANTS
            })
        );
        // Switching 12 off brings it to 500.
        let off: Vec<Vec<OptionValueId>> = (0..12)
            .map(|i| vec![vid(100), vid(200 + i / 8), vid(300 + i % 8)])
            .collect();
        assert_eq!(variant_grid("P", &big, &off).unwrap().len(), 500);
        // Switching off combinations that do not exist does not count.
        let bogus: Vec<Vec<OptionValueId>> = (0..12).map(|i| vec![vid(900 + i)]).collect();
        assert_eq!(
            variant_grid("P", &big, &bogus),
            Err(CatalogError::TooManyVariants {
                count: 512,
                max: MAX_VARIANTS
            })
        );
        let long_code = "X".repeat(24);
        let long_value = vec![option(1, "A", vec![value(1, "a", "ABCDEFGH")])];
        assert_eq!(
            variant_grid(&long_code, &long_value, &[]),
            Err(CatalogError::InvalidCode {
                kind: CodeKind::Sku,
                value: format!("{long_code}-ABCDEFGH")
            })
        );
    }

    #[test]
    fn option_rules() {
        let four = (1..=4)
            .map(|i| option(i, &format!("O{i}"), vec![value(10 * i, "x", "X")]))
            .collect::<Vec<_>>();
        let dup_value_code = vec![option(
            1,
            "Size",
            vec![value(11, "Small", "S"), value(12, "Smaller", "S")],
        )];
        let dup_value_name = vec![option(
            1,
            "Size",
            vec![value(11, "Small", "S"), value(12, "SMALL", "SM")],
        )];
        let dup_option = vec![
            option(1, "Size", vec![value(11, "a", "A")]),
            option(2, "size", vec![value(21, "b", "B")]),
        ];
        let dup_option_id = vec![
            option(1, "Size", vec![value(11, "a", "A")]),
            option(1, "Color", vec![value(21, "b", "B")]),
        ];
        let dup_value_id = vec![
            option(1, "Size", vec![value(11, "a", "A")]),
            option(2, "Color", vec![value(11, "b", "B")]),
        ];
        let empty = vec![option(1, "Size", vec![])];
        let bad_name = vec![option(1, " Size", vec![value(11, "a", "A")])];
        let bad_value_name = vec![option(1, "Size", vec![value(11, "", "A")])];
        let bad_code = vec![option(1, "Size", vec![value(11, "a", "a-1")])];
        let cases: Vec<(Vec<ProductOption>, Result<(), CatalogError>)> = vec![
            (size_color(), Ok(())),
            (vec![], Ok(())),
            (four, Err(CatalogError::TooManyOptions { max: 3 })),
            (
                dup_value_code,
                Err(CatalogError::DuplicateOptionValue {
                    option: "Size".into(),
                    value: "S".into(),
                }),
            ),
            (
                dup_value_name,
                Err(CatalogError::DuplicateOptionValue {
                    option: "Size".into(),
                    value: "SMALL".into(),
                }),
            ),
            (
                dup_option,
                Err(CatalogError::DuplicateOption {
                    name: "size".into(),
                }),
            ),
            (
                dup_option_id,
                Err(CatalogError::DuplicateId { what: "option" }),
            ),
            (
                dup_value_id,
                Err(CatalogError::DuplicateId {
                    what: "option value",
                }),
            ),
            (
                empty,
                Err(CatalogError::InvalidField {
                    field: "options.values",
                    reason: "an option needs at least one value",
                }),
            ),
            (
                bad_name,
                Err(CatalogError::InvalidField {
                    field: "options.name",
                    reason: "1–40 characters without surrounding spaces",
                }),
            ),
            (
                bad_value_name,
                Err(CatalogError::InvalidField {
                    field: "options.values.name",
                    reason: "1–40 characters without surrounding spaces",
                }),
            ),
            (
                bad_code,
                Err(CatalogError::InvalidCode {
                    kind: CodeKind::OptionValueCode,
                    value: "a-1".into(),
                }),
            ),
        ];
        for (options, expected) in cases {
            assert_eq!(validate_options(&options), expected, "{options:?}");
        }
    }

    #[test]
    fn variant_rules() {
        type Case<'a> = (&'a [ProductOption], Vec<Variant>, Result<(), CatalogError>);
        let opts = size_color();
        let ok = vec![
            variant(1, "K-S-RED", &[11, 21]),
            variant(2, "K-M-RED", &[12, 21]),
        ];
        let cases: Vec<Case> = vec![
            (&opts, ok.clone(), Ok(())),
            (&[], vec![variant(1, "K", &[])], Ok(())),
            (
                &[],
                vec![],
                Err(CatalogError::VariantCount {
                    reason: "a product needs at least one variant",
                }),
            ),
            (
                &[],
                vec![variant(1, "K", &[]), variant(2, "K2", &[])],
                Err(CatalogError::VariantCount {
                    reason: "a product without options has exactly one variant",
                }),
            ),
            (
                &opts,
                vec![variant(1, "A", &[11, 21]), variant(2, "B", &[11, 21])],
                Err(CatalogError::DuplicateCombination),
            ),
            (
                &opts,
                vec![variant(1, "A", &[11, 21]), variant(1, "B", &[12, 21])],
                Err(CatalogError::DuplicateId { what: "variant" }),
            ),
            (
                &opts,
                vec![variant(1, "A", &[11])],
                Err(CatalogError::VariantValuesMismatch),
            ),
            (
                &opts,
                vec![variant(1, "A", &[21, 11])],
                Err(CatalogError::VariantValuesMismatch),
            ),
            (
                &opts,
                vec![variant(1, "A", &[11, 21]), variant(2, "A", &[12, 21])],
                Err(CatalogError::DuplicateSku { sku: "A".into() }),
            ),
            (
                &opts,
                vec![variant(1, "a", &[11, 21])],
                Err(CatalogError::InvalidCode {
                    kind: CodeKind::Sku,
                    value: "a".into(),
                }),
            ),
        ];
        for (options, variants, expected) in cases {
            assert_eq!(
                validate_variants(options, &variants),
                expected,
                "{variants:?}"
            );
        }
        let many: Vec<Variant> = (0..501)
            .map(|i| variant(i, &format!("S{i}"), &[11, 21]))
            .collect();
        assert_eq!(
            validate_variants(&opts, &many),
            Err(CatalogError::TooManyVariants {
                count: 501,
                max: MAX_VARIANTS
            })
        );
    }

    #[test]
    fn names_and_skus() {
        assert_eq!(default_sku("KAOS01", &["M", "RED"]), "KAOS01-M-RED");
        assert_eq!(default_sku("KAOS01", &[]), "KAOS01");
        assert_eq!(
            variant_display_name("Kaos Polos", &["M", "Red"]),
            "Kaos Polos – M / Red"
        );
        assert_eq!(variant_display_name("Kaos Polos", &[]), "Kaos Polos");
        let opts = size_color();
        assert_eq!(
            opts[0].value(vid(12)).map(|v| v.name.as_str()),
            Some("Medium")
        );
        assert_eq!(opts[0].value(vid(21)), None);
    }
}
