//! Scale barcode layouts and decoding (spec 2.2 §6).
//!
//! A layout is an EAN-13 `prefix (2 digits, 20–29) · PLU (n digits) · value (m digits) · check
//! digit` with `2 + n + m = 12`. The value is a weight in grams or a price in Rupiah.

use super::code::{CheckDigitStatus, Plu, check_digit_status, check_internal_barcode_prefix};
use super::qty::div_round_half_away;
use super::{CatalogError, Qty};
use crate::money::Money;

/// What the value digits of a scale barcode carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScaleValueKind {
    /// A weight in grams; the line quantity is grams / 1000 kg.
    Weight,
    /// A price in minor units; the line amount is that price exactly.
    Price,
}

/// One scale barcode layout (company setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScaleLayout {
    /// The 2-digit prefix, 20–29.
    pub prefix: u8,
    /// Number of PLU digits (`n`).
    pub plu_digits: u8,
    /// Number of value digits (`m`).
    pub value_digits: u8,
    /// What the value digits carry.
    pub kind: ScaleValueKind,
}

impl ScaleLayout {
    /// The default layouts *(proposed in the spec)*: `21 PPPPP WWWWW C` (weight) and
    /// `22 PPPPP $$$$$ C` (price).
    #[must_use]
    pub fn default_layouts() -> Vec<Self> {
        vec![
            Self {
                prefix: 21,
                plu_digits: 5,
                value_digits: 5,
                kind: ScaleValueKind::Weight,
            },
            Self {
                prefix: 22,
                plu_digits: 5,
                value_digits: 5,
                kind: ScaleValueKind::Price,
            },
        ]
    }

    /// Checks one layout: prefix 20–29, at least one PLU and one value digit, `2 + n + m = 12`.
    ///
    /// # Errors
    /// [`CatalogError::InvalidScaleLayout`].
    pub fn validate(&self) -> Result<(), CatalogError> {
        let invalid = |reason| CatalogError::InvalidScaleLayout {
            prefix: self.prefix,
            reason,
        };
        if !(20..=29).contains(&self.prefix) {
            return Err(invalid("prefix must be 20–29"));
        }
        if self.plu_digits == 0 || self.value_digits == 0 {
            return Err(invalid("PLU and value need at least one digit each"));
        }
        if u16::from(self.plu_digits) + u16::from(self.value_digits) != 10 {
            return Err(invalid("prefix + PLU + value digits must be 12"));
        }
        Ok(())
    }
}

/// Validates the company's scale layouts: each layout, no two with the same prefix, none using
/// the internal barcode prefix (spec 2.2 §6, §7). Prefixes are all two digits, so "overlap"
/// means equal.
///
/// # Errors
/// [`CatalogError::InvalidScaleLayout`] or [`CatalogError::ScalePrefixOverlap`];
/// [`CatalogError::InvalidCode`] for an internal prefix that is not 2 digits.
pub fn validate_scale_layouts(
    layouts: &[ScaleLayout],
    internal_barcode_prefix: &str,
) -> Result<(), CatalogError> {
    for (i, layout) in layouts.iter().enumerate() {
        layout.validate()?;
        if layouts[..i].iter().any(|l| l.prefix == layout.prefix) {
            return Err(CatalogError::ScalePrefixOverlap {
                prefix: layout.prefix,
            });
        }
    }
    check_internal_barcode_prefix(internal_barcode_prefix, layouts)?;
    Ok(())
}

/// The value embedded in a scale barcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScaleValue {
    /// The weight in kg (grams / 1000).
    Weight(Qty),
    /// The line amount.
    Price(Money),
}

/// A decoded scale barcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScaleCode {
    /// The layout that matched.
    pub layout: ScaleLayout,
    /// The PLU of the weighed variant.
    pub plu: Plu,
    /// The embedded weight or price.
    pub value: ScaleValue,
}

/// Decodes `code` with the first layout whose prefix matches. Returns `Ok(None)` when `code` is
/// not a scale code: not 13 digits, no layout prefix, a wrong check digit, or a PLU above
/// 999 999 (spec 2.2 §6).
///
/// # Errors
/// [`CatalogError::ScaleValueZero`] when the embedded weight or price is 0.
pub fn decode_scale_barcode(
    code: &str,
    layouts: &[ScaleLayout],
) -> Result<Option<ScaleCode>, CatalogError> {
    if code.len() != 13 || !matches!(check_digit_status(code), CheckDigitStatus::Valid(_)) {
        return Ok(None);
    }
    let Some(layout) = layouts.iter().find(|l| code[..2] == l.prefix.to_string()) else {
        return Ok(None);
    };
    let plu_end = 2 + usize::from(layout.plu_digits);
    let value_end = plu_end + usize::from(layout.value_digits);
    // All digits (checked above) and at most 10 of them, so both parse.
    let plu: u64 = code[2..plu_end].parse().unwrap_or(u64::MAX);
    let value: i64 = code[plu_end..value_end].parse().unwrap_or(0);
    let Some(plu) = u32::try_from(plu).ok().and_then(Plu::new) else {
        return Ok(None);
    };
    if value == 0 {
        return Err(CatalogError::ScaleValueZero);
    }
    let value = match layout.kind {
        ScaleValueKind::Weight => ScaleValue::Weight(Qty::from_thousandths(value)),
        ScaleValueKind::Price => ScaleValue::Price(Money::from_minor(value)),
    };
    Ok(Some(ScaleCode {
        layout: *layout,
        plu,
        value,
    }))
}

/// The quantity of a price-embedded label: `amount / unit_price`, rounded half-up to 3 decimal
/// places (spec 2.2 §6). E.g. 15 000 at 120 000/kg → 0.125 kg.
///
/// # Errors
/// [`CatalogError::NonPositive`] when `unit_price` is not above zero;
/// [`CatalogError::QtyOverflow`] when the result does not fit.
pub fn qty_from_price(amount: Money, unit_price: Money) -> Result<Qty, CatalogError> {
    if unit_price.minor() <= 0 {
        return Err(CatalogError::NonPositive {
            field: "unit_price",
        });
    }
    let thousandths = div_round_half_away(
        i128::from(amount.minor()) * 1_000,
        i128::from(unit_price.minor()),
    );
    i64::try_from(thousandths)
        .map(Qty::from_thousandths)
        .map_err(|_| CatalogError::QtyOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::code::gtin_check_digit;

    /// Appends the check digit to a 12-digit body.
    fn ean13(body: &str) -> String {
        format!("{body}{}", gtin_check_digit(body).unwrap())
    }

    fn layout(prefix: u8, plu_digits: u8, value_digits: u8) -> ScaleLayout {
        ScaleLayout {
            prefix,
            plu_digits,
            value_digits,
            kind: ScaleValueKind::Weight,
        }
    }

    #[test]
    fn decodes_weight_and_price() {
        let layouts = ScaleLayout::default_layouts();
        let weight = decode_scale_barcode(&ean13("210012301250"), &layouts)
            .unwrap()
            .unwrap();
        assert_eq!(weight.plu.value(), 123);
        assert_eq!(
            weight.value,
            ScaleValue::Weight(Qty::from_thousandths(1_250))
        );
        assert_eq!(weight.layout, layouts[0]);

        let price = decode_scale_barcode(&ean13("220012315000"), &layouts)
            .unwrap()
            .unwrap();
        assert_eq!(price.plu.value(), 123);
        assert_eq!(price.value, ScaleValue::Price(Money::from_minor(15_000)));
    }

    #[test]
    fn non_scale_codes_decode_to_none() {
        let layouts = ScaleLayout::default_layouts();
        let good = ean13("210012301250");
        let mut bad_check = good[..12].to_owned();
        bad_check.push(if good.ends_with('0') { '1' } else { '0' });
        let wide_plu = [layout(23, 9, 1)];
        for code in [
            bad_check.as_str(),
            "21001230125",
            "ABCDEFGHIJKLM",
            &ean13("250012301250"),
            "4006381333931",
        ] {
            assert_eq!(decode_scale_barcode(code, &layouts), Ok(None), "{code}");
        }
        // A PLU above 999 999 does not fit and is not a scale code.
        assert_eq!(
            decode_scale_barcode(&ean13("231234567891"), &wide_plu),
            Ok(None)
        );
        assert!(
            decode_scale_barcode(&ean13("230001234561"), &wide_plu)
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn zero_value_is_rejected() {
        let layouts = ScaleLayout::default_layouts();
        for body in ["210012300000", "220012300000"] {
            assert_eq!(
                decode_scale_barcode(&ean13(body), &layouts),
                Err(CatalogError::ScaleValueZero)
            );
        }
    }

    #[test]
    fn layout_validation() {
        let invalid = |prefix, reason| Err(CatalogError::InvalidScaleLayout { prefix, reason });
        let cases = [
            (vec![layout(21, 5, 5)], "29", Ok(())),
            (ScaleLayout::default_layouts(), "29", Ok(())),
            (
                vec![layout(21, 4, 5)],
                "29",
                invalid(21, "prefix + PLU + value digits must be 12"),
            ),
            (
                vec![layout(21, 6, 5)],
                "29",
                invalid(21, "prefix + PLU + value digits must be 12"),
            ),
            (
                vec![layout(19, 5, 5)],
                "29",
                invalid(19, "prefix must be 20–29"),
            ),
            (
                vec![layout(30, 5, 5)],
                "29",
                invalid(30, "prefix must be 20–29"),
            ),
            (
                vec![layout(21, 0, 10)],
                "29",
                invalid(21, "PLU and value need at least one digit each"),
            ),
            (
                vec![layout(21, 10, 0)],
                "29",
                invalid(21, "PLU and value need at least one digit each"),
            ),
            (
                vec![layout(21, 5, 5), layout(21, 4, 6)],
                "29",
                Err(CatalogError::ScalePrefixOverlap { prefix: 21 }),
            ),
            (
                vec![layout(21, 5, 5), layout(29, 4, 6)],
                "29",
                Err(CatalogError::ScalePrefixOverlap { prefix: 29 }),
            ),
        ];
        for (layouts, internal, expected) in cases {
            assert_eq!(
                validate_scale_layouts(&layouts, internal),
                expected,
                "{layouts:?}"
            );
        }
    }

    #[test]
    fn qty_from_price_rounds_half_up() {
        let m = Money::from_minor;
        let cases = [
            (15_000, 120_000, Ok(Qty::from_thousandths(125))),
            (10_000, 30_000, Ok(Qty::from_thousandths(333))),
            (20_000, 30_000, Ok(Qty::from_thousandths(667))),
            (1, 2_000, Ok(Qty::from_thousandths(1))), // 0.0005 → 0.001
            (
                15_000,
                0,
                Err(CatalogError::NonPositive {
                    field: "unit_price",
                }),
            ),
            (
                15_000,
                -1,
                Err(CatalogError::NonPositive {
                    field: "unit_price",
                }),
            ),
            (i64::MAX, 1, Err(CatalogError::QtyOverflow)),
        ];
        for (amount, price, expected) in cases {
            assert_eq!(
                qty_from_price(m(amount), m(price)),
                expected,
                "{amount}/{price}"
            );
        }
    }
}
