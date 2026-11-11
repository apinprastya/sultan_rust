//! Rounding: money to Rp 1 (spec 2.3 §4.6–4.7) and cash rounding (§8).

use std::fmt;
use std::str::FromStr;

use super::PricingError;
use crate::catalog::Qty;
use crate::money::Money;

/// `n / d` rounded half up to a whole number, for a positive `d`. Negative values round half
/// away from zero (−2.5 → −3), so a return mirrors its sale.
pub(crate) const fn div_round_half_up(n: i128, d: i128) -> i128 {
    let q = n / d;
    let r = n % d;
    if r.abs() * 2 >= d {
        if n < 0 { q - 1 } else { q + 1 }
    } else {
        q
    }
}

/// Converts an `i128` result back to money.
pub(crate) fn to_money(value: i128) -> Result<Money, PricingError> {
    i64::try_from(value)
        .map(Money::from_minor)
        .map_err(|_| PricingError::Overflow)
}

/// The line amount: unit price × qty (3 dp), rounded half up to Rp 1 (spec 2.3 §4.7).
/// E.g. 1.255 kg × 12 990 = 16 302.45 → 16 302.
///
/// # Errors
/// [`PricingError::Overflow`] when the result does not fit.
pub fn line_amount(unit_price: Money, qty: Qty) -> Result<Money, PricingError> {
    let product = i128::from(unit_price.minor()) * i128::from(qty.thousandths());
    to_money(div_round_half_up(product, 1_000))
}

/// The company's cash rounding setting (spec 2.3 §8). Off by default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum CashRounding {
    /// No rounding.
    #[default]
    None,
    /// To the nearest Rp 100, half up.
    Nearest100,
    /// Down to a multiple of Rp 100.
    Down100,
    /// To the nearest Rp 500, half up.
    Nearest500,
    /// Down to a multiple of Rp 500.
    Down500,
}

/// The result of [`CashRounding::apply`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CashRounded {
    /// The amount the customer pays in cash.
    pub amount: Money,
    /// `amount − exact`, stored on the sale as `cash_rounding` (+/−).
    pub difference: Money,
}

impl CashRounding {
    /// All settings, in display order.
    pub const ALL: [Self; 5] = [
        Self::None,
        Self::Nearest100,
        Self::Down100,
        Self::Nearest500,
        Self::Down500,
    ];

    /// The stable key stored in the settings (`none`, `nearest_100`, …).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Nearest100 => "nearest_100",
            Self::Down100 => "down_100",
            Self::Nearest500 => "nearest_500",
            Self::Down500 => "down_500",
        }
    }

    /// Rounds the rest of a sale that cash settles. `cash` is `false` for card, QRIS and other
    /// tenders, which pay the exact amount (difference 0).
    ///
    /// "Down" rounds towards zero, so a cash refund is rounded in the shop's favour as well;
    /// "nearest" rounds half away from zero.
    #[must_use]
    pub fn apply(self, exact: Money, cash: bool) -> CashRounded {
        let step: i64 = match self {
            Self::None => 1,
            Self::Nearest100 | Self::Down100 => 100,
            Self::Nearest500 | Self::Down500 => 500,
        };
        let value = exact.minor();
        let rounded = if !cash {
            value
        } else if matches!(self, Self::Down100 | Self::Down500) {
            value - value % step
        } else {
            let r = div_round_half_up(i128::from(value), i128::from(step)) * i128::from(step);
            // Only rounding past the `i64` range fails; clamp to the last multiple inside it.
            i64::try_from(r).unwrap_or(if r > 0 {
                i64::MAX - i64::MAX % step
            } else {
                i64::MIN - i64::MIN % step
            })
        };
        CashRounded {
            amount: Money::from_minor(rounded),
            difference: Money::from_minor(rounded.wrapping_sub(value)),
        }
    }
}

impl fmt::Display for CashRounding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for CashRounding {
    type Err = PricingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or(PricingError::InvalidField {
                field: "cash_rounding",
                reason: "unknown setting",
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    #[test]
    fn div_round_half_up_cases() {
        let cases = [
            (5, 2, 3),
            (4, 2, 2),
            (7, 3, 2),
            (-5, 2, -3),
            (-7, 3, -2),
            (299_700, 1_000, 300),
            (16_302_450, 1_000, 16_302),
        ];
        for (n, d, expected) in cases {
            assert_eq!(div_round_half_up(n, d), expected, "{n}/{d}");
        }
    }

    #[test]
    fn line_amounts() {
        let cases = [
            (12_990, 1_255, Some(16_302)), // §12.17
            (10_000, 15_000, Some(150_000)),
            (333, 500, Some(167)), // 166.5 → 167
            (333, -500, Some(-167)),
            (0, 3_000, Some(0)),
            (i64::MAX, 2_000, None),
        ];
        for (price, qty, expected) in cases {
            let got = line_amount(m(price), Qty::from_thousandths(qty));
            assert_eq!(
                got,
                expected.map(m).ok_or(PricingError::Overflow),
                "{price} × {qty}"
            );
        }
    }

    /// §12.19 and more.
    #[test]
    fn cash_rounding() {
        use CashRounding::{Down100, Down500, Nearest100, Nearest500, None};
        let cases = [
            (None, 47_830, true, 47_830),
            (Nearest100, 47_830, true, 47_800),
            (Nearest100, 47_850, true, 47_900),
            (Down100, 47_890, true, 47_800),
            (Nearest500, 47_830, true, 48_000),
            (Nearest500, 47_740, true, 47_500),
            (Down500, 47_830, true, 47_500),
            (Down500, 47_500, true, 47_500),
            (Nearest100, 47_830, false, 47_830), // card
            (Down500, 47_830, false, 47_830),
            (Down100, -47_890, true, -47_800),
            (Nearest100, -47_850, true, -47_900),
            (Nearest500, i64::MAX, true, i64::MAX - i64::MAX % 500),
            (Nearest500, i64::MIN, true, i64::MIN - i64::MIN % 500),
        ];
        for (setting, exact, cash, expected) in cases {
            let got = setting.apply(m(exact), cash);
            assert_eq!(got.amount, m(expected), "{setting} {exact} {cash}");
            assert_eq!(
                got.difference,
                m(expected.wrapping_sub(exact)),
                "{setting} {exact}"
            );
        }
        assert_eq!(CashRounding::default(), None);
    }

    #[test]
    fn cash_rounding_keys_round_trip() {
        for r in CashRounding::ALL {
            assert_eq!(r.to_string().parse::<CashRounding>(), Ok(r));
        }
        assert_eq!(
            "nearest_1000".parse::<CashRounding>().unwrap_err().code(),
            "invalid_field"
        );
    }
}
