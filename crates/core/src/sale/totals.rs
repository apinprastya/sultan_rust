//! Cart totals, discounts and tax (spec 2.6 §6).

use super::cart::Discount;
use super::{SaleError, add, money, sub};
use crate::money::Money;
use crate::tax::{TaxSettings, line_tax};

/// One line as the totals need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TotalsLine {
    /// `unit_price × qty` rounded half up to Rp 1, or a price-embedded label's printed amount.
    pub gross: Money,
    /// The manual line discount, if any.
    pub line_discount: Option<Discount>,
    /// Whether the cart discount applies; `false` for a price-embedded scale label (§6).
    pub cart_discountable: bool,
    /// The tax rate on the sale date, in basis points.
    pub tax_rate_bp: u16,
}

/// The computed values of one line (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LineTotals {
    /// `unit_price × qty`, rounded half up.
    pub gross: Money,
    /// The line discount in Rupiah (at most the gross).
    pub line_discount: Money,
    /// `gross − line_discount`.
    pub after_line: Money,
    /// This line's share of the cart discount.
    pub cart_discount_share: Money,
    /// `after_line − cart_discount_share`, never below 0: what the customer pays for the line
    /// (before tax when tax-exclusive).
    pub amount: Money,
    /// The rate used.
    pub tax_rate_bp: u16,
    /// The line's tax (2.3 §7).
    pub tax: Money,
    /// The amount without tax.
    pub net: Money,
    /// The amount (inclusive) or amount + tax (exclusive).
    pub total: Money,
}

impl LineTotals {
    /// The line discount as basis points of the gross, rounded up: what `sale.discount` `MaxPct`
    /// is checked against (§3). See [`super::discount_bp`].
    #[must_use]
    pub fn line_discount_bp(&self) -> u16 {
        super::discount_bp(self.gross, self.line_discount)
    }
}

/// The totals of a cart (§6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CartTotals {
    /// One entry per line, in order.
    pub lines: Vec<LineTotals>,
    /// Σ gross.
    pub subtotal: Money,
    /// Σ line discount.
    pub line_discount: Money,
    /// Σ `after_line`: the cart total before the cart discount.
    pub before_cart_discount: Money,
    /// The cart discount in Rupiah (= Σ shares).
    pub cart_discount: Money,
    /// Σ amount.
    pub amount: Money,
    /// Σ tax.
    pub tax: Money,
    /// What the customer pays before cash rounding: Σ amount (+ Σ tax when tax-exclusive).
    pub total: Money,
}

impl CartTotals {
    /// The cart discount as basis points of the cart total before the cart discount, rounded up:
    /// what `sale.cart_discount` `MaxPct` is checked against (§6).
    #[must_use]
    pub fn cart_discount_bp(&self) -> u16 {
        super::discount_bp(self.before_cart_discount, self.cart_discount)
    }
}

/// Splits `discount` over lines in proportion to `bases` (§6 step 3): each share is first
/// `discount × base ÷ Σ bases` rounded down, then the Rupiah left over go one each to the lines
/// with the largest remainders, ties to the earlier line, so the shares sum exactly to
/// `discount`. A discount above Σ bases is capped at Σ bases; a zero or negative discount or
/// sum gives all zeros. Negative bases count as 0.
///
/// E.g. 10 000 over 30 000 / 20 000 / 1 → 6 000 / 4 000 / 0 (spec §15 case 3).
#[must_use]
pub fn split_cart_discount(discount: Money, bases: &[Money]) -> Vec<Money> {
    let bases: Vec<i128> = bases.iter().map(|b| i128::from(b.minor().max(0))).collect();
    let sum: i128 = bases.iter().sum();
    let discount = i128::from(discount.minor()).min(sum);
    if discount <= 0 || sum <= 0 {
        return vec![Money::ZERO; bases.len()];
    }
    let mut shares: Vec<i128> = Vec::with_capacity(bases.len());
    let mut remainders: Vec<(i128, usize)> = Vec::with_capacity(bases.len());
    for (i, base) in bases.iter().enumerate() {
        let product = discount * base;
        shares.push(product / sum);
        remainders.push((product % sum, i));
    }
    let left = discount - shares.iter().sum::<i128>();
    // Largest remainder first; equal remainders keep the earlier line first.
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for &(_, i) in remainders.iter().take(usize::try_from(left).unwrap_or(0)) {
        shares[i] += 1;
    }
    // Each share is at most its base, which is an `i64`.
    shares
        .into_iter()
        .map(|s| Money::from_minor(i64::try_from(s).unwrap_or(i64::MAX)))
        .collect()
}

/// Computes the cart totals (§6): per line gross → line discount → share of the cart discount
/// → amount → tax, then the sums.
///
/// The cart discount (% or Rp) is taken from the lines that take it (`cart_discountable`): a %
/// is that sum × % rounded half up, an Rp amount is capped at that sum.
///
/// # Errors
/// [`SaleError::InvalidField`] (`discount`) for a discount above 100% or a negative amount;
/// [`SaleError::AmountOverflow`] when a sum does not fit.
pub fn compute_totals(
    lines: &[TotalsLine],
    cart_discount: Option<Discount>,
    tax: TaxSettings,
) -> Result<CartTotals, SaleError> {
    let mut after: Vec<(Money, Money)> = Vec::with_capacity(lines.len());
    for line in lines {
        let discount = match line.line_discount {
            Some(d) => d.validate()?.amount_of(line.gross),
            None => Money::ZERO,
        };
        after.push((discount, sub(line.gross, discount)?));
    }
    let bases: Vec<Money> = lines
        .iter()
        .zip(&after)
        .map(|(l, (_, a))| if l.cart_discountable { *a } else { Money::ZERO })
        .collect();
    let discountable = bases
        .iter()
        .try_fold(Money::ZERO, |s, b| add(s, (*b).max(Money::ZERO)))?;
    let cart_amount = match cart_discount {
        Some(d) => d.validate()?.amount_of(discountable),
        None => Money::ZERO,
    };
    let shares = split_cart_discount(cart_amount, &bases);

    let mut totals = CartTotals {
        lines: Vec::with_capacity(lines.len()),
        subtotal: Money::ZERO,
        line_discount: Money::ZERO,
        before_cart_discount: Money::ZERO,
        cart_discount: Money::ZERO,
        amount: Money::ZERO,
        tax: Money::ZERO,
        total: Money::ZERO,
    };
    for ((line, (discount, after_line)), share) in lines.iter().zip(after).zip(shares) {
        let amount = sub(after_line, share)?.max(Money::ZERO);
        let t = line_tax(amount, line.tax_rate_bp, tax);
        totals.subtotal = add(totals.subtotal, line.gross)?;
        totals.line_discount = add(totals.line_discount, discount)?;
        totals.before_cart_discount = add(totals.before_cart_discount, after_line)?;
        totals.cart_discount = add(totals.cart_discount, share)?;
        totals.amount = add(totals.amount, amount)?;
        totals.tax = add(totals.tax, t.tax)?;
        totals.total = add(totals.total, t.total)?;
        totals.lines.push(LineTotals {
            gross: line.gross,
            line_discount: discount,
            after_line,
            cart_discount_share: share,
            amount,
            tax_rate_bp: line.tax_rate_bp,
            tax: t.tax,
            net: t.net,
            total: t.total,
        });
    }
    Ok(totals)
}

/// `base × bp ÷ 10 000` rounded half up, for a non-negative base.
pub(super) fn pct_of(base: Money, bp: u16) -> Money {
    let product = i128::from(base.minor()) * i128::from(bp);
    // bp ≤ 10 000 after validation, so the result is at most the base.
    money(crate::pricing::div_round_half_up(product, 10_000)).unwrap_or(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn line(gross: i64) -> TotalsLine {
        TotalsLine {
            gross: m(gross),
            line_discount: None,
            cart_discountable: true,
            tax_rate_bp: 1_100,
        }
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
        inclusive: false,
        registered: false,
    };

    #[test]
    fn largest_remainder_split() {
        #[rustfmt::skip]
        let cases: [(i64, &[i64], &[i64]); 10] = [
            // §15 case 3: 5 999.88 / 3 999.92 / 0.20 → floors 5 999 / 3 999 / 0, 2 left over.
            (10_000, &[30_000, 20_000, 1], &[6_000, 4_000, 0]),
            // Equal remainders go to the earlier line.
            (100, &[100, 100, 100], &[34, 33, 33]),
            (2, &[1, 1, 1], &[1, 1, 0]),
            (10_000, &[10_000], &[10_000]),
            // Capped at the sum.
            (50_000, &[10_000, 5_000], &[10_000, 5_000]),
            (0, &[10_000, 5_000], &[0, 0]),
            (-5, &[10_000], &[0]),
            (5, &[0, 0], &[0, 0]),
            (5, &[], &[]),
            // A negative base takes nothing.
            (3, &[-10, 3], &[0, 3]),
        ];
        for (discount, bases, expected) in cases {
            let bases: Vec<Money> = bases.iter().copied().map(m).collect();
            let got = split_cart_discount(m(discount), &bases);
            let expected: Vec<Money> = expected.iter().copied().map(m).collect();
            assert_eq!(got, expected, "{discount} over {bases:?}");
        }
    }

    /// Spec §15 case 3 with tax per line after the share.
    #[test]
    fn cart_discount_then_tax_per_line() {
        let lines = [line(30_000), line(20_000), line(1)];
        let t = compute_totals(&lines, Some(Discount::Amount(m(10_000))), INCL).unwrap();
        let shares: Vec<i64> = t
            .lines
            .iter()
            .map(|l| l.cart_discount_share.minor())
            .collect();
        assert_eq!(shares, [6_000, 4_000, 0]);
        let amounts: Vec<i64> = t.lines.iter().map(|l| l.amount.minor()).collect();
        assert_eq!(amounts, [24_000, 16_000, 1]);
        // 24 000 × 11/111 = 2 378.38 → 2 378; 16 000 → 1 585.59 → 1 586; 1 → 0.099 → 0.
        let taxes: Vec<i64> = t.lines.iter().map(|l| l.tax.minor()).collect();
        assert_eq!(taxes, [2_378, 1_586, 0]);
        assert_eq!(
            (
                t.subtotal,
                t.before_cart_discount,
                t.cart_discount,
                t.amount,
                t.tax,
                t.total
            ),
            (
                m(50_001),
                m(50_001),
                m(10_000),
                m(40_001),
                m(3_964),
                m(40_001)
            )
        );
        // 10 000 / 50 001 = 19.9996% → 2 000 bp rounded up.
        assert_eq!(t.cart_discount_bp(), 2_000);
    }

    #[test]
    fn line_discounts_labels_and_tax_modes() {
        let label = TotalsLine {
            cart_discountable: false,
            line_discount: Some(Discount::Amount(m(1_000))),
            ..line(15_000)
        };
        let pct = TotalsLine {
            line_discount: Some(Discount::Pct(1_000)),
            ..line(20_000)
        };
        let lines = [pct, label];
        // 10% cart discount on the discountable 18 000 only → 1 800, all on the first line.
        let t = compute_totals(&lines, Some(Discount::Pct(1_000)), EXCL).unwrap();
        let l0 = t.lines[0];
        let l1 = t.lines[1];
        assert_eq!(
            (
                l0.line_discount,
                l0.after_line,
                l0.cart_discount_share,
                l0.amount
            ),
            (m(2_000), m(18_000), m(1_800), m(16_200))
        );
        assert_eq!(l0.line_discount_bp(), 1_000);
        assert_eq!(
            (
                l1.line_discount,
                l1.after_line,
                l1.cart_discount_share,
                l1.amount
            ),
            (m(1_000), m(14_000), m(0), m(14_000))
        );
        // Exclusive: 16 200 × 11% = 1 782; 14 000 × 11% = 1 540.
        assert_eq!((l0.tax, l0.net, l0.total), (m(1_782), m(16_200), m(17_982)));
        assert_eq!((t.amount, t.tax, t.total), (m(30_200), m(3_322), m(33_522)));
        // The cart discount % is checked against all lines before it: 1 800 / 32 000.
        assert_eq!(t.cart_discount_bp(), 563);

        // Not registered: no tax.
        let t = compute_totals(&lines, None, UNREG).unwrap();
        assert_eq!((t.tax, t.total), (m(0), m(32_000)));

        // A line discount above the gross is capped; everything discounted → total 0.
        let free = TotalsLine {
            line_discount: Some(Discount::Amount(m(99_999))),
            ..line(5_000)
        };
        let t = compute_totals(&[free], Some(Discount::Amount(m(10))), INCL).unwrap();
        assert_eq!(
            (t.line_discount, t.cart_discount, t.total),
            (m(5_000), m(0), m(0))
        );

        // Empty cart.
        let t = compute_totals(&[], Some(Discount::Pct(10_000)), INCL).unwrap();
        assert_eq!((t.total, t.lines.len(), t.cart_discount_bp()), (m(0), 0, 0));
    }

    #[test]
    fn invalid_discounts_and_overflow() {
        let bad = Some(Discount::Pct(10_001));
        let err = compute_totals(&[line(1)], bad, INCL).unwrap_err();
        assert_eq!(err.field(), Some("discount"));
        let bad_line = TotalsLine {
            line_discount: Some(Discount::Amount(m(-1))),
            ..line(1)
        };
        assert_eq!(
            compute_totals(&[bad_line], None, INCL).unwrap_err().code(),
            "invalid_field"
        );
        let huge = [line(i64::MAX), line(1)];
        assert_eq!(
            compute_totals(&huge, None, INCL),
            Err(SaleError::AmountOverflow)
        );
        assert_eq!(pct_of(m(i64::MAX), 10_000), m(i64::MAX));
    }
}
