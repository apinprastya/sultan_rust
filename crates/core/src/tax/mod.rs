//! Tax (PPN) per sale line (spec 2.3 §7).
//!
//! - A tax class has a history of [`TaxRate`]s; the rate on a sale is the one with the latest
//!   `valid_from` on or before the sale's local date ([`rate_on`]). `Exempt` is simply a rate of 0.
//! - [`TaxSettings`] holds the company switches: prices tax-inclusive (default) or exclusive, and
//!   tax-registered (default) or not. Not registered → no tax at all.
//! - [`line_tax`] computes one line: inclusive `amount × rate / (10 000 + rate)`, exclusive
//!   `amount × rate / 10 000`, rounded half up to Rp 1 (half away from zero for a negative return
//!   amount). Tax is per line and summed by the caller; cart-level discounts come with 2.4.
//!
//! Pure: rates and the sale date are passed in.

use chrono::NaiveDate;

use crate::money::Money;
use crate::pricing::div_round_half_up;

/// Basis points in 100%.
const FULL_BP: i128 = 10_000;

/// One rate of a tax class from a date on (spec 2.3 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaxRate {
    /// The rate in basis points (1 100 = 11%).
    pub rate_bp: u16,
    /// The first local day the rate applies.
    pub valid_from: NaiveDate,
}

/// The rate on `date`: the one with the latest `valid_from ≤ date`, or `None` when every rate
/// starts later (or there is none). Rates may be in any order.
#[must_use]
pub fn rate_on(rates: &[TaxRate], date: NaiveDate) -> Option<u16> {
    rates
        .iter()
        .filter(|r| r.valid_from <= date)
        .max_by_key(|r| r.valid_from)
        .map(|r| r.rate_bp)
}

/// The company's tax settings (spec 2.3 §7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaxSettings {
    /// Prices include tax (default). Otherwise tax is added on top.
    pub inclusive: bool,
    /// The company is tax-registered (PKP, default). Otherwise no tax is computed.
    pub registered: bool,
}

impl Default for TaxSettings {
    fn default() -> Self {
        Self {
            inclusive: true,
            registered: true,
        }
    }
}

/// The tax of one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineTax {
    /// The tax.
    pub tax: Money,
    /// The amount without tax.
    pub net: Money,
    /// What the customer pays for the line: the amount (inclusive), or amount + tax (exclusive).
    pub total: Money,
}

/// The tax of a line `amount` at `rate_bp` (spec 2.3 §7). With the company not registered there
/// is no tax: net and total are the amount.
///
/// Inclusive 11%: 11 100 → tax 1 100, net 10 000; 10 000 → tax 991. Exclusive 11%: 10 000 → tax
/// 1 100, total 11 100. The arithmetic is in `i128`; only an exclusive total near `i64::MAX` can
/// overflow, and it saturates.
#[must_use]
pub fn line_tax(amount: Money, rate_bp: u16, settings: TaxSettings) -> LineTax {
    let a = i128::from(amount.minor());
    let rate = i128::from(rate_bp);
    let tax = if !settings.registered {
        0
    } else if settings.inclusive {
        div_round_half_up(a * rate, FULL_BP + rate)
    } else {
        div_round_half_up(a * rate, FULL_BP)
    };
    // Inclusive tax is at most the amount, so it fits; exclusive tax is at most 6.6 × amount.
    let tax =
        Money::from_minor(i64::try_from(tax).unwrap_or(if tax > 0 { i64::MAX } else { i64::MIN }));
    if settings.inclusive || !settings.registered {
        LineTax {
            tax,
            net: amount.saturating_sub(tax),
            total: amount,
        }
    } else {
        LineTax {
            tax,
            net: amount,
            total: amount.saturating_add(tax),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn d(y: i32, mo: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, mo, day).unwrap()
    }

    const INCL: TaxSettings = TaxSettings {
        inclusive: true,
        registered: true,
    };
    const EXCL: TaxSettings = TaxSettings {
        inclusive: false,
        registered: true,
    };
    const UNREG: TaxSettings = TaxSettings {
        inclusive: true,
        registered: false,
    };
    const UNREG_EXCL: TaxSettings = TaxSettings {
        inclusive: false,
        registered: false,
    };

    /// §12.14, §12.15.
    #[test]
    fn line_taxes() {
        #[rustfmt::skip]
        let cases = [
            // amount, rate, settings, (tax, net, total)
            (11_100, 1_100, INCL, (1_100, 10_000, 11_100)),
            (10_000, 1_100, INCL, (991, 9_009, 10_000)), // 990.99
            (10_000, 0, INCL, (0, 10_000, 10_000)),      // exempt
            (10_000, 1_100, EXCL, (1_100, 10_000, 11_100)),
            (10_000, 0, EXCL, (0, 10_000, 10_000)),
            (10_000, 1_100, UNREG, (0, 10_000, 10_000)),
            (10_000, 1_100, UNREG_EXCL, (0, 10_000, 10_000)),
            (-11_100, 1_100, INCL, (-1_100, -10_000, -11_100)),
            (-10_000, 1_100, INCL, (-991, -9_009, -10_000)),
            (50, 1_100, EXCL, (6, 50, 56)), // 5.5 → 6
            (i64::MAX, 1_100, EXCL, (1_014_570_924_054_025_339, i64::MAX, i64::MAX)),
            (i64::MAX, u16::MAX, EXCL, (i64::MAX, i64::MAX, i64::MAX)),
            (i64::MIN, u16::MAX, EXCL, (i64::MIN, i64::MIN, i64::MIN)),
        ];
        for (amount, rate, settings, (tax, net, total)) in cases {
            assert_eq!(
                line_tax(m(amount), rate, settings),
                LineTax {
                    tax: m(tax),
                    net: m(net),
                    total: m(total)
                },
                "{amount} at {rate} {settings:?}"
            );
        }
        assert_eq!(TaxSettings::default(), INCL);
    }

    /// §12.16.
    #[test]
    fn rate_history() {
        let rates = [
            TaxRate {
                rate_bp: 1_200,
                valid_from: d(2025, 1, 1),
            },
            TaxRate {
                rate_bp: 1_100,
                valid_from: d(2022, 4, 1),
            },
        ];
        let cases = [
            (d(2022, 3, 31), None),
            (d(2022, 4, 1), Some(1_100)),
            (d(2024, 12, 31), Some(1_100)),
            (d(2025, 1, 1), Some(1_200)),
            (d(2026, 11, 11), Some(1_200)),
        ];
        for (date, expected) in cases {
            assert_eq!(rate_on(&rates, date), expected, "{date}");
        }
        assert_eq!(rate_on(&[], d(2026, 1, 1)), None);
    }
}
