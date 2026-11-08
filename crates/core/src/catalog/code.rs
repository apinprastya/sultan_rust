//! Product codes, SKUs, option-value codes, barcodes, PLUs, GS1 check digits and the code
//! generators (spec 2.2 §2, §3, §5, §6, §7).

use std::fmt;

use super::scale::ScaleLayout;
use super::{CatalogError, CodeKind};

/// Maximum length of a product code.
pub const MAX_PRODUCT_CODE_LEN: usize = 24;
/// Maximum length of a variant SKU.
pub const MAX_SKU_LEN: usize = 32;
/// Maximum length of an option-value or category short code.
pub const MAX_SHORT_CODE_LEN: usize = 8;
/// Maximum length of a barcode.
pub const MAX_BARCODE_LEN: usize = 64;
/// Digits of the counter in a generated product code (`P000123`).
pub const PRODUCT_CODE_COUNTER_WIDTH: usize = 6;
/// Default prefix of generated product codes.
pub const DEFAULT_PRODUCT_CODE_PREFIX: &str = "P";
/// Default prefix of generated internal EAN-13 barcodes.
pub const DEFAULT_INTERNAL_BARCODE_PREFIX: &str = "29";
/// Largest counter of an internal barcode (10 digits).
pub const MAX_INTERNAL_BARCODE_COUNTER: u64 = 9_999_999_999;

/// `[A-Z0-9._-]`: the characters of product codes and SKUs.
const fn is_code_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
}

/// `[A-Z0-9]`: the characters of short codes.
const fn is_short_code_byte(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

fn check(
    value: &str,
    kind: CodeKind,
    min: usize,
    max: usize,
    allowed: fn(u8) -> bool,
) -> Result<(), CatalogError> {
    if (min..=max).contains(&value.len()) && value.bytes().all(allowed) {
        Ok(())
    } else {
        Err(CatalogError::InvalidCode {
            kind,
            value: value.to_owned(),
        })
    }
}

fn normalize(
    value: &str,
    kind: CodeKind,
    min: usize,
    max: usize,
    allowed: fn(u8) -> bool,
) -> Result<String, CatalogError> {
    let upper = value.trim().to_ascii_uppercase();
    check(&upper, kind, min, max, allowed)?;
    Ok(upper)
}

/// Checks a stored product code: `[A-Z0-9._-]{1,24}`, already upper case.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::ProductCode`].
pub fn check_product_code(code: &str) -> Result<(), CatalogError> {
    check(
        code,
        CodeKind::ProductCode,
        1,
        MAX_PRODUCT_CODE_LEN,
        is_code_byte,
    )
}

/// Trims and upper-cases user input into a product code, then checks it.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::ProductCode`].
pub fn normalize_product_code(input: &str) -> Result<String, CatalogError> {
    normalize(
        input,
        CodeKind::ProductCode,
        1,
        MAX_PRODUCT_CODE_LEN,
        is_code_byte,
    )
}

/// Trims and upper-cases a prefix of generated product codes, then checks it:
/// `[A-Z0-9._-]{0,17}`, leaving room for the 6-digit counter (spec 2.2 §7).
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::ProductCodePrefix`].
pub fn normalize_product_code_prefix(input: &str) -> Result<String, CatalogError> {
    normalize(
        input,
        CodeKind::ProductCodePrefix,
        0,
        MAX_PRODUCT_CODE_LEN - PRODUCT_CODE_COUNTER_WIDTH - 1,
        is_code_byte,
    )
}

/// Checks a stored SKU: `[A-Z0-9._-]{1,32}`, already upper case.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::Sku`].
pub fn check_sku(sku: &str) -> Result<(), CatalogError> {
    check(sku, CodeKind::Sku, 1, MAX_SKU_LEN, is_code_byte)
}

/// Trims and upper-cases user input into a SKU, then checks it.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::Sku`].
pub fn normalize_sku(input: &str) -> Result<String, CatalogError> {
    normalize(input, CodeKind::Sku, 1, MAX_SKU_LEN, is_code_byte)
}

/// Checks a stored option-value short code: `[A-Z0-9]{1,8}`.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::OptionValueCode`].
pub fn check_value_code(code: &str) -> Result<(), CatalogError> {
    check(
        code,
        CodeKind::OptionValueCode,
        1,
        MAX_SHORT_CODE_LEN,
        is_short_code_byte,
    )
}

/// Trims and upper-cases user input into an option-value short code, then checks it.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::OptionValueCode`].
pub fn normalize_value_code(input: &str) -> Result<String, CatalogError> {
    normalize(
        input,
        CodeKind::OptionValueCode,
        1,
        MAX_SHORT_CODE_LEN,
        is_short_code_byte,
    )
}

/// Checks a category short code: empty (none) or `[A-Z0-9]{1,8}`.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::CategoryCode`].
pub fn check_category_code(code: &str) -> Result<(), CatalogError> {
    check(
        code,
        CodeKind::CategoryCode,
        0,
        MAX_SHORT_CODE_LEN,
        is_short_code_byte,
    )
}

/// The barcode symbologies whose check digit is validated (spec 2.2 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GtinFormat {
    /// 8 digits.
    Ean8,
    /// 12 digits.
    UpcA,
    /// 13 digits.
    Ean13,
}

impl GtinFormat {
    /// The format of an all-digit code of that length, if any.
    #[must_use]
    pub fn of(code: &str) -> Option<Self> {
        if !code.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        match code.len() {
            8 => Some(Self::Ean8),
            12 => Some(Self::UpcA),
            13 => Some(Self::Ean13),
            _ => None,
        }
    }
}

/// The result of [`check_digit_status`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CheckDigitStatus {
    /// Not an all-digit EAN-8 / UPC-A / EAN-13 code: nothing to validate.
    NotApplicable,
    /// The check digit matches.
    Valid(GtinFormat),
    /// The check digit does not match.
    Invalid(GtinFormat),
}

/// The GS1 mod-10 check digit of `body` (all digits, without the check digit): weights 3 and 1
/// alternate from the rightmost digit. `None` if `body` is empty or not all digits.
#[must_use]
pub fn gtin_check_digit(body: &str) -> Option<u8> {
    if body.is_empty() || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sum: u32 = body
        .bytes()
        .rev()
        .enumerate()
        .map(|(i, b)| u32::from(b - b'0') * if i % 2 == 0 { 3 } else { 1 })
        .sum();
    // `sum % 10` is below 10, so the subtraction fits a `u8`.
    Some(u8::try_from((10 - sum % 10) % 10).unwrap_or(0))
}

/// Validates the check digit of an EAN-8 / UPC-A / EAN-13 code; any other code (letters,
/// other lengths) is [`CheckDigitStatus::NotApplicable`].
#[must_use]
pub fn check_digit_status(code: &str) -> CheckDigitStatus {
    let Some(format) = GtinFormat::of(code) else {
        return CheckDigitStatus::NotApplicable;
    };
    let (body, last) = code.split_at(code.len() - 1);
    let expected = gtin_check_digit(body);
    if expected == Some(last.as_bytes()[0] - b'0') {
        CheckDigitStatus::Valid(format)
    } else {
        CheckDigitStatus::Invalid(format)
    }
}

/// Checks a stored barcode: 1–64 printable ASCII characters without spaces, kept as scanned
/// (letters are not upper-cased: Code-128 is case-sensitive), and a valid check digit for
/// EAN-8 / UPC-A / EAN-13 unless `allow_invalid_check_digit` (spec 2.2 §6).
///
/// # Errors
/// [`CatalogError::InvalidCode`] or [`CatalogError::InvalidCheckDigit`].
pub fn check_barcode(code: &str, allow_invalid_check_digit: bool) -> Result<(), CatalogError> {
    check(code, CodeKind::Barcode, 1, MAX_BARCODE_LEN, |b| {
        b.is_ascii_graphic()
    })?;
    if !allow_invalid_check_digit
        && matches!(check_digit_status(code), CheckDigitStatus::Invalid(_))
    {
        return Err(CatalogError::InvalidCheckDigit {
            code: code.to_owned(),
        });
    }
    Ok(())
}

/// A PLU (price look-up number) of a weighed variant: 1–6 digits, so 0–999 999. Leading zeros
/// are not significant (`00123` = `123`), matching how scale barcodes embed it (spec 2.2 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Plu(u32);

impl Plu {
    /// The largest PLU.
    pub const MAX: u32 = 999_999;

    /// A PLU from its number, or `None` above [`Plu::MAX`].
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        if value <= Self::MAX {
            Some(Self(value))
        } else {
            None
        }
    }

    /// The number.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }

    /// Parses 1–6 ASCII digits.
    ///
    /// # Errors
    /// [`CatalogError::InvalidCode`] with [`CodeKind::Plu`].
    pub fn parse(input: &str) -> Result<Self, CatalogError> {
        check(input, CodeKind::Plu, 1, 6, |b| b.is_ascii_digit())?;
        // At most 6 digits always fit and are ≤ MAX.
        Ok(Self(input.parse().unwrap_or(0)))
    }
}

impl fmt::Display for Plu {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A value produced by a generator and the counter to store for the next call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generated {
    /// The generated code.
    pub value: String,
    /// The counter value after the one that produced `value`.
    pub next_counter: u64,
}

/// The next free product code: `<prefix><counter zero-padded to 6>`, e.g. `P000123`, starting at
/// `counter` and skipping values for which `is_taken` returns true (e.g. imported codes)
/// (spec 2.2 §7). The prefix is upper-cased; counters above 999 999 simply get more digits.
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::ProductCodePrefix`] for a prefix that is not
/// `[A-Z0-9._-]{0,17}`; [`CatalogError::GeneratorExhausted`] when the code would exceed 24
/// characters.
pub fn next_product_code(
    prefix: &str,
    counter: u64,
    is_taken: impl Fn(&str) -> bool,
) -> Result<Generated, CatalogError> {
    let prefix = normalize_product_code_prefix(prefix)?;
    let exhausted = || CatalogError::GeneratorExhausted {
        kind: CodeKind::ProductCode,
    };
    let mut n = counter;
    loop {
        let value = format!("{prefix}{n:0PRODUCT_CODE_COUNTER_WIDTH$}");
        if value.len() > MAX_PRODUCT_CODE_LEN {
            return Err(exhausted());
        }
        let next = n.checked_add(1).ok_or_else(exhausted)?;
        if !is_taken(&value) {
            return Ok(Generated {
                value,
                next_counter: next,
            });
        }
        n = next;
    }
}

/// Checks an internal barcode prefix: exactly 2 digits and not the prefix of a scale layout
/// (spec 2.2 §7).
///
/// # Errors
/// [`CatalogError::InvalidCode`] with [`CodeKind::BarcodePrefix`];
/// [`CatalogError::ScalePrefixOverlap`] when a layout uses it.
pub fn check_internal_barcode_prefix(
    prefix: &str,
    scale_layouts: &[ScaleLayout],
) -> Result<u8, CatalogError> {
    check(prefix, CodeKind::BarcodePrefix, 2, 2, |b| {
        b.is_ascii_digit()
    })?;
    let value: u8 = prefix.parse().unwrap_or(0);
    if scale_layouts.iter().any(|l| l.prefix == value) {
        return Err(CatalogError::ScalePrefixOverlap { prefix: value });
    }
    Ok(value)
}

/// The next free internal EAN-13 barcode: `<2-digit prefix><10-digit counter><check digit>`,
/// e.g. `2900000001234C`, starting at `counter` and skipping taken values (spec 2.2 §7).
///
/// # Errors
/// See [`check_internal_barcode_prefix`]; [`CatalogError::GeneratorExhausted`] past
/// [`MAX_INTERNAL_BARCODE_COUNTER`].
pub fn next_internal_barcode(
    prefix: &str,
    counter: u64,
    scale_layouts: &[ScaleLayout],
    is_taken: impl Fn(&str) -> bool,
) -> Result<Generated, CatalogError> {
    let prefix = check_internal_barcode_prefix(prefix, scale_layouts)?;
    let mut n = counter;
    while n <= MAX_INTERNAL_BARCODE_COUNTER {
        let body = format!("{prefix:02}{n:010}");
        let check_digit = gtin_check_digit(&body).unwrap_or(0);
        let value = format!("{body}{check_digit}");
        n += 1;
        if !is_taken(&value) {
            return Ok(Generated {
                value,
                next_counter: n,
            });
        }
    }
    Err(CatalogError::GeneratorExhausted {
        kind: CodeKind::Barcode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::scale::ScaleLayout;

    fn invalid(kind: CodeKind, value: &str) -> CatalogError {
        CatalogError::InvalidCode {
            kind,
            value: value.to_owned(),
        }
    }

    #[test]
    fn product_code_prefixes() {
        assert_eq!(normalize_product_code_prefix(" p "), Ok("P".to_owned()));
        assert_eq!(normalize_product_code_prefix(""), Ok(String::new()));
        assert_eq!(
            normalize_product_code_prefix(&"A".repeat(17)),
            Ok("A".repeat(17))
        );
        for bad in ["A".repeat(18), "P 1".to_owned()] {
            assert_eq!(
                normalize_product_code_prefix(&bad),
                Err(invalid(CodeKind::ProductCodePrefix, &bad))
            );
        }
    }

    #[test]
    fn product_codes_and_skus() {
        assert_eq!(normalize_product_code(" kaos01 "), Ok("KAOS01".to_owned()));
        assert_eq!(normalize_product_code("a.b_c-1"), Ok("A.B_C-1".to_owned()));
        assert_eq!(normalize_sku("kaos01-m-red"), Ok("KAOS01-M-RED".to_owned()));
        let long24 = "A".repeat(24);
        let long25 = "A".repeat(25);
        assert_eq!(check_product_code(&long24), Ok(()));
        let cases = [
            ("", CodeKind::ProductCode),
            ("A B", CodeKind::ProductCode),
            ("KAOS/1", CodeKind::ProductCode),
            ("ÀB", CodeKind::ProductCode),
            (long25.as_str(), CodeKind::ProductCode),
        ];
        for (input, kind) in cases {
            assert_eq!(
                normalize_product_code(input),
                Err(invalid(kind, &input.trim().to_uppercase()))
            );
        }
        // Stored values must already be upper case.
        assert_eq!(
            check_product_code("kaos"),
            Err(invalid(CodeKind::ProductCode, "kaos"))
        );
        assert_eq!(check_sku(&"S".repeat(32)), Ok(()));
        assert_eq!(
            check_sku(&"S".repeat(33)),
            Err(invalid(CodeKind::Sku, &"S".repeat(33)))
        );
    }

    #[test]
    fn short_codes() {
        assert_eq!(normalize_value_code("red"), Ok("RED".to_owned()));
        assert_eq!(check_value_code("XL2"), Ok(()));
        for bad in ["", "M-L", "ABCDEFGHI", "a"] {
            assert_eq!(
                check_value_code(bad),
                Err(invalid(CodeKind::OptionValueCode, bad))
            );
        }
        assert_eq!(check_category_code(""), Ok(()));
        assert_eq!(check_category_code("FOOD"), Ok(()));
        assert_eq!(
            check_category_code("fo od"),
            Err(invalid(CodeKind::CategoryCode, "fo od"))
        );
    }

    #[test]
    fn check_digits() {
        let cases = [
            (
                "8991234567890",
                CheckDigitStatus::Invalid(GtinFormat::Ean13),
            ),
            ("4006381333931", CheckDigitStatus::Valid(GtinFormat::Ean13)),
            (
                "4006381333932",
                CheckDigitStatus::Invalid(GtinFormat::Ean13),
            ),
            ("96385074", CheckDigitStatus::Valid(GtinFormat::Ean8)),
            ("96385075", CheckDigitStatus::Invalid(GtinFormat::Ean8)),
            ("036000291452", CheckDigitStatus::Valid(GtinFormat::UpcA)),
            ("036000291453", CheckDigitStatus::Invalid(GtinFormat::UpcA)),
            ("ABC-123", CheckDigitStatus::NotApplicable),
            ("40063813339A", CheckDigitStatus::NotApplicable),
            ("12345", CheckDigitStatus::NotApplicable),
            ("18991234567897", CheckDigitStatus::NotApplicable),
        ];
        for (code, status) in cases {
            assert_eq!(check_digit_status(code), status, "{code}");
        }
        assert_eq!(gtin_check_digit("400638133393"), Some(1));
        assert_eq!(gtin_check_digit("0"), Some(0));
        assert_eq!(gtin_check_digit(""), None);
        assert_eq!(gtin_check_digit("12a"), None);
    }

    #[test]
    fn barcodes() {
        assert_eq!(check_barcode("4006381333931", false), Ok(()));
        assert_eq!(check_barcode("abc-XYZ_01", false), Ok(()));
        assert_eq!(check_barcode("4006381333932", true), Ok(()));
        assert_eq!(
            check_barcode("4006381333932", false),
            Err(CatalogError::InvalidCheckDigit {
                code: "4006381333932".to_owned()
            })
        );
        let long = "1".repeat(65);
        for bad in ["", "AB CD", "é", long.as_str(), "a\tb"] {
            assert_eq!(
                check_barcode(bad, true),
                Err(invalid(CodeKind::Barcode, bad))
            );
        }
    }

    #[test]
    fn plu() {
        assert_eq!(Plu::parse("00123").map(Plu::value), Ok(123));
        assert_eq!(Plu::parse("999999").map(Plu::value), Ok(999_999));
        for bad in ["", "1234567", "12a", "-1"] {
            assert_eq!(Plu::parse(bad), Err(invalid(CodeKind::Plu, bad)));
        }
        assert_eq!(
            Plu::new(999_999).map(|p| p.to_string()),
            Some("999999".to_owned())
        );
        assert_eq!(Plu::new(1_000_000), None);
    }

    #[test]
    fn next_product_code_skips_taken_values() {
        let taken = ["P000123", "P000124"];
        let generated = next_product_code("p", 123, |c| taken.contains(&c)).unwrap();
        assert_eq!(
            generated,
            Generated {
                value: "P000125".to_owned(),
                next_counter: 126
            }
        );
        assert_eq!(next_product_code("", 7, |_| false).unwrap().value, "000007");
        assert_eq!(
            next_product_code("X", 1_234_567, |_| false).unwrap().value,
            "X1234567"
        );
        assert_eq!(
            next_product_code("P P", 1, |_| false),
            Err(invalid(CodeKind::ProductCodePrefix, "P P"))
        );
        let exhausted = Err(CatalogError::GeneratorExhausted {
            kind: CodeKind::ProductCode,
        });
        let prefix17 = "A".repeat(17);
        assert_eq!(
            next_product_code(&prefix17, 9_999_999, |_| false)
                .unwrap()
                .value
                .len(),
            24
        );
        assert_eq!(
            next_product_code(&prefix17, 10_000_000, |_| false),
            exhausted
        );
        assert_eq!(next_product_code(&prefix17, 9_999_999, |_| true), exhausted);
        assert_eq!(next_product_code("", u64::MAX, |_| false), exhausted);
    }

    #[test]
    fn internal_barcode_is_valid_ean13_and_avoids_scale_prefixes() {
        let layouts = ScaleLayout::default_layouts();
        let first = next_internal_barcode("29", 1, &layouts, |_| false).unwrap();
        assert_eq!(first.next_counter, 2);
        assert_eq!(first.value.len(), 13);
        assert!(first.value.starts_with("290000000001"));
        assert_eq!(
            check_digit_status(&first.value),
            CheckDigitStatus::Valid(GtinFormat::Ean13)
        );
        assert!(
            layouts
                .iter()
                .all(|l| !first.value.starts_with(&l.prefix.to_string()))
        );

        let taken = first.value.clone();
        let second = next_internal_barcode("29", 1, &layouts, |c| c == taken).unwrap();
        assert!(second.value.starts_with("290000000002"));
        assert_eq!(second.next_counter, 3);

        assert_eq!(
            next_internal_barcode("21", 1, &layouts, |_| false),
            Err(CatalogError::ScalePrefixOverlap { prefix: 21 })
        );
        assert_eq!(
            next_internal_barcode("2", 1, &layouts, |_| false),
            Err(invalid(CodeKind::BarcodePrefix, "2"))
        );
        assert_eq!(
            next_internal_barcode("29", MAX_INTERNAL_BARCODE_COUNTER, &layouts, |_| true),
            Err(CatalogError::GeneratorExhausted {
                kind: CodeKind::Barcode
            })
        );
    }
}
