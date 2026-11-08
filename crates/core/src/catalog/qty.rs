//! Quantities as fixed-point thousandths (spec 2.2 §4: `Qty(i64)`, 1.000 = 1000).

use std::fmt;
use std::str::FromStr;

use super::CatalogError;

/// Thousandths per whole unit.
const SCALE: i64 = 1_000;

/// A quantity in thousandths of a unit: `Qty::from_thousandths(1_250)` is 1.250. Can be
/// negative (returns, stock movements out).
///
/// Unit factors use the same representation (`box ×24` is `Qty::from_whole(24)`), so decimal
/// factors work for decimal base units. Like [`crate::money::Money`] there is deliberately no
/// operator overloading: use the `checked_*` methods.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Qty(i64);

impl Qty {
    /// Zero.
    pub const ZERO: Self = Self(0);
    /// One whole unit (1.000).
    pub const ONE: Self = Self(SCALE);

    /// Creates a quantity from thousandths (`1_250` = 1.250).
    #[must_use]
    pub const fn from_thousandths(thousandths: i64) -> Self {
        Self(thousandths)
    }

    /// Creates a whole quantity (`24` = 24.000), or `None` on overflow.
    #[must_use]
    pub const fn from_whole(whole: i64) -> Option<Self> {
        match whole.checked_mul(SCALE) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// The quantity in thousandths.
    #[must_use]
    pub const fn thousandths(self) -> i64 {
        self.0
    }

    /// Whether the quantity has no fractional part.
    #[must_use]
    pub const fn is_whole(self) -> bool {
        self.0 % SCALE == 0
    }

    /// Whether the quantity is above zero.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        self.0 > 0
    }

    /// `self + other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// `self - other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// `-self`, or `None` on overflow.
    #[must_use]
    pub const fn checked_neg(self) -> Option<Self> {
        match self.0.checked_neg() {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// `self × factor` (both fixed point), rounded half away from zero to 3 decimal places,
    /// or `None` on overflow. Exact whenever either side is whole, e.g. 2 × box(24) = 48 and
    /// 0.5 kg × 2 = 1.000.
    #[must_use]
    pub fn times_factor(self, factor: Self) -> Option<Self> {
        let product = i128::from(self.0) * i128::from(factor.0);
        i64::try_from(div_round_half_away(product, i128::from(SCALE)))
            .ok()
            .map(Self)
    }

    /// How many whole times `part` fits into `self` (`⌊self / part⌋`), or `None` when `part` is
    /// not positive. A negative `self` gives 0.
    #[must_use]
    pub const fn whole_times(self, part: Self) -> Option<i64> {
        if part.0 <= 0 {
            return None;
        }
        if self.0 <= 0 {
            return Some(0);
        }
        Some(self.0 / part.0)
    }

    /// Parses a decimal string with at most 3 decimal places: `2`, `0.125`, `-1.5`.
    /// No exponent, no thousands separator, no leading `+`, no bare `.5` or `1.`.
    ///
    /// # Errors
    /// [`CatalogError::InvalidQty`] on any other input, more than 3 decimal places, or a value
    /// that does not fit `i64` thousandths.
    pub fn parse(input: &str) -> Result<Self, CatalogError> {
        let invalid = || CatalogError::InvalidQty {
            input: input.to_owned(),
        };
        let (negative, rest) = match input.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, input),
        };
        let (whole, frac) = match rest.split_once('.') {
            Some((w, f)) => (w, Some(f)),
            None => (rest, None),
        };
        let all_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
        if !all_digits(whole) {
            return Err(invalid());
        }
        let mut thousandths: i64 = 0;
        for b in whole.bytes() {
            thousandths = thousandths
                .checked_mul(10)
                .and_then(|v| v.checked_add(i64::from(b - b'0')))
                .ok_or_else(invalid)?;
        }
        thousandths = thousandths.checked_mul(SCALE).ok_or_else(invalid)?;
        if let Some(frac) = frac {
            if !all_digits(frac) || frac.len() > 3 {
                return Err(invalid());
            }
            let mut f: i64 = 0;
            for b in frac.bytes() {
                f = f * 10 + i64::from(b - b'0');
            }
            for _ in frac.len()..3 {
                f *= 10;
            }
            thousandths = thousandths.checked_add(f).ok_or_else(invalid)?;
        }
        Ok(Self(if negative { -thousandths } else { thousandths }))
    }
}

/// `n / d` rounded half away from zero; `d` must be positive.
pub(super) const fn div_round_half_away(n: i128, d: i128) -> i128 {
    let q = n / d;
    let r = n % d;
    if r.abs() * 2 >= d {
        if n < 0 { q - 1 } else { q + 1 }
    } else {
        q
    }
}

impl FromStr for Qty {
    type Err = CatalogError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Always three decimal places, e.g. `1.250`, `-2.000`.
impl fmt::Display for Qty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let scale = SCALE.unsigned_abs();
        write!(f, "{sign}{}.{:03}", abs / scale, abs % scale)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(t: i64) -> Qty {
        Qty::from_thousandths(t)
    }

    #[test]
    fn parse_accepts_up_to_three_decimals() {
        let cases = [
            ("0", 0),
            ("2", 2_000),
            ("1.5", 1_500),
            ("0.125", 125),
            ("0.250", 250),
            ("-1.5", -1_500),
            ("007.07", 7_070),
            ("9223372036854775.807", i64::MAX),
        ];
        for (input, thousandths) in cases {
            assert_eq!(Qty::parse(input), Ok(q(thousandths)), "{input}");
            assert_eq!(input.parse::<Qty>(), Ok(q(thousandths)), "{input}");
        }
    }

    #[test]
    fn parse_rejects_bad_input() {
        for input in [
            "",
            "-",
            ".",
            ".5",
            "1.",
            "0.1255",
            "1,5",
            "+1",
            "1e3",
            " 1",
            "1.2.3",
            "1.-2",
            "9223372036854775.808",
            "9223372036854776",
            "99999999999999999999",
        ] {
            assert_eq!(
                Qty::parse(input),
                Err(CatalogError::InvalidQty {
                    input: input.to_owned()
                }),
                "{input:?}"
            );
        }
    }

    #[test]
    fn display_has_three_decimals() {
        let cases = [
            (0, "0.000"),
            (1_250, "1.250"),
            (-2_000, "-2.000"),
            (-5, "-0.005"),
            (i64::MIN, "-9223372036854775.808"),
        ];
        for (t, text) in cases {
            assert_eq!(q(t).to_string(), text);
        }
    }

    #[test]
    fn constructors_and_predicates() {
        assert_eq!(Qty::default(), Qty::ZERO);
        assert_eq!(Qty::from_whole(24), Some(q(24_000)));
        assert_eq!(Qty::from_whole(i64::MAX), None);
        assert_eq!(Qty::ONE.thousandths(), 1_000);
        assert!(q(3_000).is_whole());
        assert!(!q(1_500).is_whole());
        assert!(q(1).is_positive());
        assert!(!Qty::ZERO.is_positive());
    }

    #[test]
    fn checked_arithmetic() {
        assert_eq!(q(1_500).checked_add(q(250)), Some(q(1_750)));
        assert_eq!(q(1_500).checked_sub(q(2_000)), Some(q(-500)));
        assert_eq!(q(500).checked_neg(), Some(q(-500)));
        assert_eq!(q(i64::MAX).checked_add(q(1)), None);
        assert_eq!(q(i64::MIN).checked_sub(q(1)), None);
        assert_eq!(q(i64::MIN).checked_neg(), None);
    }

    #[test]
    fn times_factor_rounds_half_away_from_zero() {
        let cases = [
            (2_000, 24_000, Some(48_000)),
            (250, 1_000, Some(250)),
            (500, 2_000, Some(1_000)),
            (333, 2_500, Some(833)),   // 0.8325 → 0.833
            (-333, 2_500, Some(-833)), // -0.8325 → -0.833
            (331, 1_500, Some(497)),   // 0.4965 → 0.497
            (1, 400, Some(0)),         // 0.0004 → 0.000
            (i64::MAX, 2_000, None),
        ];
        for (a, f, expected) in cases {
            assert_eq!(q(a).times_factor(q(f)), expected.map(q), "{a} × {f}");
        }
    }

    #[test]
    fn whole_times() {
        assert_eq!(q(10_000).whole_times(q(3_000)), Some(3));
        assert_eq!(q(-1_000).whole_times(q(3_000)), Some(0));
        assert_eq!(q(1_000).whole_times(Qty::ZERO), None);
    }
}
