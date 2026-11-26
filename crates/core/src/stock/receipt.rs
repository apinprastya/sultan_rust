//! Goods receipt totals and landed cost (spec 2.8 §6, I5, I6).

use super::{StockError, add_money, div_round, invalid, line_value, sub_money, to_base, to_money};
use crate::catalog::Qty;
use crate::money::Money;

/// 100% in basis points.
const FULL_BP: i128 = 10_000;

/// How a receipt's prices treat PPN (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TaxMode {
    /// Prices include PPN: base = net × 100 / (100 + rate).
    Included,
    /// PPN is added on top: PPN = base × rate / 100.
    Added,
    /// No tax.
    None,
}

impl TaxMode {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Included => "included",
            Self::Added => "added",
            Self::None => "none",
        }
    }

    /// The mode of a stored name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::Included, Self::Added, Self::None]
            .into_iter()
            .find(|m| m.as_str() == key)
    }
}

/// One receipt line as the totals need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptLineInput {
    /// Quantity in the line's unit (> 0).
    pub qty: Qty,
    /// Price per unit, before discount, in the receipt's tax mode (≥ 0).
    pub price: Money,
    /// Line discount in Rp (0 ≤ discount ≤ qty × price).
    pub discount: Money,
    /// The unit's factor to the base unit.
    pub factor: Qty,
}

/// A receipt's header as the totals need it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptInput<'a> {
    /// The tax mode.
    pub tax_mode: TaxMode,
    /// The PPN rate in basis points (1 100 = 11%).
    pub rate_bp: u16,
    /// Header discount (Rp, ≤ Σ line nets).
    pub header_discount: Money,
    /// Other costs (Rp, ≥ 0), spread over the lines by value.
    pub other_costs: Money,
    /// The company is PKP: PPN stays out of the cost (I5).
    pub pkp: bool,
    /// The lines (at least one).
    pub lines: &'a [ReceiptLineInput],
}

/// The totals of one line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiptLineTotals {
    /// `qty × price`.
    pub gross: Money,
    /// `gross − line discount`.
    pub net: Money,
    /// Its share of the header discount.
    pub header_discount: Money,
    /// `net − header discount share`: the line's value in the tax mode.
    pub after_discount: Money,
    /// Its share of the PPN (0 without tax).
    pub tax: Money,
    /// The value without PPN.
    pub base: Money,
    /// Its share of the other costs.
    pub other_costs: Money,
    /// The landed cost: the `receipt` movement's value (§6).
    pub landed: Money,
    /// The quantity in base units.
    pub base_qty: Qty,
}

impl ReceiptLineTotals {
    /// The unit cost per base unit (landed / base qty), rounded to Rp 1 for display.
    #[must_use]
    pub fn unit_cost(&self) -> Option<Money> {
        let qty = i128::from(self.base_qty.thousandths());
        if qty <= 0 {
            return None;
        }
        to_money(div_round(i128::from(self.landed.minor()) * 1_000, qty)).ok()
    }
}

/// The totals of a receipt (§6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptTotals {
    /// Per line, in order.
    pub lines: Vec<ReceiptLineTotals>,
    /// Σ line nets.
    pub net: Money,
    /// The header discount.
    pub header_discount: Money,
    /// Σ bases (without PPN).
    pub base: Money,
    /// The PPN.
    pub tax: Money,
    /// The invoice total: Σ net − header discount (+ PPN when added). The payable's amount.
    pub total: Money,
    /// The other costs.
    pub other_costs: Money,
    /// Σ landed costs.
    pub landed: Money,
}

/// Spreads `total` (≥ 0) over `weights` (≥ 0) in proportion, with the largest-remainder
/// method, so the parts add up to `total` exactly (§6). Ties go to the earlier part. All-zero
/// weights spread evenly.
///
/// # Errors
/// [`StockError::InvalidField`] (`weights`) for a negative weight, a negative total, or a total
/// without parts; [`StockError::Overflow`].
pub fn spread(total: Money, weights: &[Money]) -> Result<Vec<Money>, StockError> {
    if total.is_negative() || weights.iter().any(|w| w.is_negative()) {
        return Err(invalid("weights", "must not be below 0"));
    }
    if weights.is_empty() {
        return if total == Money::ZERO {
            Ok(Vec::new())
        } else {
            Err(invalid("weights", "nothing to spread over"))
        };
    }
    let even = weights.iter().all(|w| *w == Money::ZERO);
    let ws: Vec<i128> = weights
        .iter()
        .map(|w| if even { 1 } else { i128::from(w.minor()) })
        .collect();
    let sum: i128 = ws.iter().sum();
    let t = i128::from(total.minor());
    let mut parts: Vec<i128> = ws.iter().map(|w| t * w / sum).collect();
    let mut rest = t - parts.iter().sum::<i128>();
    let mut order: Vec<usize> = (0..ws.len()).collect();
    // Largest remainder first; the sort is stable, so ties keep line order.
    order.sort_by_key(|&i| std::cmp::Reverse(t * ws[i] % sum));
    for i in order {
        if rest == 0 {
            break;
        }
        parts[i] += 1;
        rest -= 1;
    }
    parts.into_iter().map(to_money).collect()
}

/// The totals and landed cost of a receipt (§6, §17 case 5).
///
/// # Errors
/// [`StockError::InvalidField`]: `lines` (none), `qty` (≤ 0), `price` (< 0), `discount` (< 0
/// or above the line amount), `header_discount` (< 0 or above Σ nets), `other_costs` (< 0),
/// `tax_rate` (above 100%), `unit`; [`StockError::Overflow`].
pub fn receipt_totals(input: &ReceiptInput<'_>) -> Result<ReceiptTotals, StockError> {
    if input.lines.is_empty() {
        return Err(invalid("lines", "a receipt has at least one line"));
    }
    if i128::from(input.rate_bp) > FULL_BP {
        return Err(invalid("tax_rate", "must be at most 100%"));
    }
    if input.header_discount.is_negative() {
        return Err(invalid("header_discount", "must not be below 0"));
    }
    if input.other_costs.is_negative() {
        return Err(invalid("other_costs", "must not be below 0"));
    }
    let mut gross = Vec::with_capacity(input.lines.len());
    let mut nets = Vec::with_capacity(input.lines.len());
    let mut base_qty = Vec::with_capacity(input.lines.len());
    for line in input.lines {
        if !line.qty.is_positive() {
            return Err(invalid("qty", "must be above 0"));
        }
        if line.price.is_negative() {
            return Err(invalid("price", "must not be below 0"));
        }
        let amount = line_value(line.qty, line.price)?;
        if line.discount.is_negative() || line.discount > amount {
            return Err(invalid("discount", "must be between 0 and the line amount"));
        }
        gross.push(amount);
        nets.push(sub_money(amount, line.discount)?);
        base_qty.push(to_base(line.qty, line.factor)?);
    }
    let net = nets.iter().try_fold(Money::ZERO, |a, b| add_money(a, *b))?;
    if input.header_discount > net {
        return Err(invalid("header_discount", "must not exceed the lines"));
    }
    let header = spread(input.header_discount, &nets)?;
    let after: Vec<Money> = nets
        .iter()
        .zip(&header)
        .map(|(n, h)| sub_money(*n, *h))
        .collect::<Result<_, _>>()?;
    let after_total = sub_money(net, input.header_discount)?;
    let rate = i128::from(input.rate_bp);
    let a = i128::from(after_total.minor());
    let tax_total = match input.tax_mode {
        TaxMode::Included => to_money(a - div_round(a * FULL_BP, FULL_BP + rate))?,
        TaxMode::Added => to_money(div_round(a * rate, FULL_BP))?,
        TaxMode::None => Money::ZERO,
    };
    let taxes = spread(tax_total, &after)?;
    let others = spread(input.other_costs, &after)?;
    let mut lines = Vec::with_capacity(input.lines.len());
    for i in 0..input.lines.len() {
        let base = match input.tax_mode {
            TaxMode::Included => sub_money(after[i], taxes[i])?,
            TaxMode::Added | TaxMode::None => after[i],
        };
        let cost = if input.pkp {
            base
        } else {
            add_money(base, taxes[i])?
        };
        lines.push(ReceiptLineTotals {
            gross: gross[i],
            net: nets[i],
            header_discount: header[i],
            after_discount: after[i],
            tax: taxes[i],
            base,
            other_costs: others[i],
            landed: add_money(cost, others[i])?,
            base_qty: base_qty[i],
        });
    }
    let sum = |f: fn(&ReceiptLineTotals) -> Money| {
        lines
            .iter()
            .try_fold(Money::ZERO, |acc, l| add_money(acc, f(l)))
    };
    let total = match input.tax_mode {
        TaxMode::Added => add_money(after_total, tax_total)?,
        TaxMode::Included | TaxMode::None => after_total,
    };
    Ok(ReceiptTotals {
        net,
        header_discount: input.header_discount,
        base: sum(|l| l.base)?,
        tax: tax_total,
        total,
        other_costs: input.other_costs,
        landed: sum(|l| l.landed)?,
        lines,
    })
}

/// The supplier credit of a supplier return (§6): Σ qty × price, with PPN added on top when the
/// receipt's tax mode is `added` (`included` prices hold it already).
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`, `price`, `tax_rate`); [`StockError::Overflow`].
pub fn supplier_return_credit(
    lines: &[(Qty, Money)],
    tax_mode: TaxMode,
    rate_bp: u16,
) -> Result<Money, StockError> {
    if i128::from(rate_bp) > FULL_BP {
        return Err(invalid("tax_rate", "must be at most 100%"));
    }
    let mut base = Money::ZERO;
    for (qty, price) in lines {
        if !qty.is_positive() {
            return Err(invalid("qty", "must be above 0"));
        }
        if price.is_negative() {
            return Err(invalid("price", "must not be below 0"));
        }
        base = add_money(base, line_value(*qty, *price)?)?;
    }
    match tax_mode {
        TaxMode::Added => {
            let tax = div_round(i128::from(base.minor()) * i128::from(rate_bp), FULL_BP);
            add_money(base, to_money(tax)?)
        }
        TaxMode::Included | TaxMode::None => Ok(base),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{q, qt, rp};
    use super::*;

    fn line(qty: i64, price: i64, discount: i64) -> ReceiptLineInput {
        ReceiptLineInput {
            qty: q(qty),
            price: rp(price),
            discount: rp(discount),
            factor: Qty::ONE,
        }
    }

    #[test]
    fn spread_largest_remainder() {
        #[rustfmt::skip]
        let cases: Vec<(i64, Vec<i64>, Vec<i64>)> = vec![
            (100, vec![1, 1, 1], vec![34, 33, 33]),
            (10, vec![50, 30, 20], vec![5, 3, 2]),
            (7, vec![1, 2], vec![2, 5]),        // 2.33 / 4.67 → the larger remainder gets it
            (1, vec![1, 1], vec![1, 0]),        // a tie goes to the earlier part
            (5, vec![0, 0], vec![3, 2]),        // all zero → evenly
            (0, vec![3, 4], vec![0, 0]),
            (1_000, vec![0, 7], vec![0, 1_000]),
            (0, vec![], vec![]),
        ];
        for (total, weights, parts) in cases {
            let w: Vec<Money> = weights.iter().map(|n| rp(*n)).collect();
            let got = spread(rp(total), &w).unwrap();
            assert_eq!(
                got,
                parts.iter().map(|n| rp(*n)).collect::<Vec<_>>(),
                "{total} {weights:?}"
            );
            let sum: i64 = got.iter().map(|m| m.minor()).sum();
            assert_eq!(sum, total);
        }
        assert_eq!(spread(rp(1), &[]).unwrap_err().field(), Some("weights"));
        assert_eq!(
            spread(rp(-1), &[rp(1)]).unwrap_err().field(),
            Some("weights")
        );
        assert_eq!(
            spread(rp(1), &[rp(-1)]).unwrap_err().field(),
            Some("weights")
        );
    }

    /// Spec §17 case 5: tax modes × header discount × other costs, PKP vs non-PKP.
    #[test]
    fn receipt_totals_table() {
        // Two lines: 3 × 10 000 − 1 000 = 29 000 and 1 × 11 000 = 11 000; Σ 40 000.
        let lines = [line(3, 10_000, 1_000), line(1, 11_000, 0)];
        #[rustfmt::skip]
        let cases = [
            // (mode, header discount, other costs, pkp) → (tax, total, landed per line)
            (TaxMode::None, 0, 0, false, 0, 40_000, [29_000, 11_000]),
            (TaxMode::None, 1_000, 0, false, 0, 39_000, [28_275, 10_725]),
            (TaxMode::None, 0, 1_001, true, 0, 40_000, [29_726, 11_275]),
            // Included: base = 40 000 × 100 / 111 = 36 036, PPN 3 964.
            (TaxMode::Included, 0, 0, false, 3_964, 40_000, [29_000, 11_000]),
            (TaxMode::Included, 0, 0, true, 3_964, 40_000, [26_126, 9_910]),
            // Included with a header discount and other costs, PKP.
            (TaxMode::Included, 1_000, 500, true, 3_865, 39_000, [25_836, 9_799]),
            // Added: PPN = 40 000 × 11% = 4 400 on top.
            (TaxMode::Added, 0, 0, false, 4_400, 44_400, [32_190, 12_210]),
            (TaxMode::Added, 0, 0, true, 4_400, 44_400, [29_000, 11_000]),
            (TaxMode::Added, 1_000, 2_000, false, 4_290, 43_290, [32_835, 12_455]),
        ];
        for (mode, hd, oc, pkp, tax, total, landed) in cases {
            let t = receipt_totals(&ReceiptInput {
                tax_mode: mode,
                rate_bp: 1_100,
                header_discount: rp(hd),
                other_costs: rp(oc),
                pkp,
                lines: &lines,
            })
            .unwrap();
            let name = format!("{mode:?} hd {hd} oc {oc} pkp {pkp}");
            assert_eq!((t.tax, t.total), (rp(tax), rp(total)), "{name}");
            let got: Vec<i64> = t.lines.iter().map(|l| l.landed.minor()).collect();
            assert_eq!(got, landed, "{name}");
            // The spreads add up to the whole.
            let sum = |f: fn(&ReceiptLineTotals) -> Money| -> i64 {
                t.lines.iter().map(|l| f(l).minor()).sum()
            };
            assert_eq!(sum(|l| l.header_discount), hd, "{name}");
            assert_eq!(sum(|l| l.tax), tax, "{name}");
            assert_eq!(sum(|l| l.other_costs), oc, "{name}");
            assert_eq!(sum(|l| l.landed), t.landed.minor(), "{name}");
            assert_eq!(t.net, rp(40_000));
            assert_eq!(t.header_discount, rp(hd));
            assert_eq!(t.other_costs, rp(oc));
            let expected_base = if mode == TaxMode::Included {
                40_000 - hd - tax
            } else {
                40_000 - hd
            };
            assert_eq!(t.base, rp(expected_base), "{name}");
            let cost = if pkp {
                expected_base
            } else {
                expected_base + tax
            };
            assert_eq!(t.landed.minor(), cost + oc, "{name}");
        }
    }

    /// Spec §17 case 6: 2 box(24) @ 48 000 → 48 base units @ 2 000.
    #[test]
    fn unit_conversion() {
        let lines = [ReceiptLineInput {
            qty: q(2),
            price: rp(48_000),
            discount: Money::ZERO,
            factor: q(24),
        }];
        let t = receipt_totals(&ReceiptInput {
            tax_mode: TaxMode::None,
            rate_bp: 1_100,
            header_discount: Money::ZERO,
            other_costs: Money::ZERO,
            pkp: false,
            lines: &lines,
        })
        .unwrap();
        let l = t.lines[0];
        assert_eq!(
            (l.base_qty, l.landed, l.unit_cost()),
            (q(48), rp(96_000), Some(rp(2_000)))
        );
        let zero = ReceiptLineTotals {
            base_qty: Qty::ZERO,
            ..l
        };
        assert_eq!(zero.unit_cost(), None);
        assert_eq!(
            ReceiptLineTotals {
                base_qty: qt(1_500),
                landed: rp(1_000),
                ..l
            }
            .unit_cost(),
            Some(rp(667))
        );
    }

    #[test]
    fn receipt_errors() {
        static NO_QTY: [ReceiptLineInput; 1] = [ReceiptLineInput {
            qty: Qty::ZERO,
            price: Money::from_minor(1),
            discount: Money::ZERO,
            factor: Qty::ONE,
        }];
        static NEG_PRICE: [ReceiptLineInput; 1] = [ReceiptLineInput {
            qty: Qty::ONE,
            price: Money::from_minor(-1),
            discount: Money::ZERO,
            factor: Qty::ONE,
        }];
        static BIG_DISC: [ReceiptLineInput; 1] = [ReceiptLineInput {
            qty: Qty::ONE,
            price: Money::from_minor(10),
            discount: Money::from_minor(11),
            factor: Qty::ONE,
        }];
        static NO_FACTOR: [ReceiptLineInput; 1] = [ReceiptLineInput {
            qty: Qty::ONE,
            price: Money::from_minor(10),
            discount: Money::ZERO,
            factor: Qty::ZERO,
        }];
        let ok = [line(1, 1_000, 0)];
        let input = |lines: &'static [ReceiptLineInput]| ReceiptInput {
            tax_mode: TaxMode::Added,
            rate_bp: 1_100,
            header_discount: Money::ZERO,
            other_costs: Money::ZERO,
            pkp: false,
            lines,
        };
        let field = |i: ReceiptInput<'_>| receipt_totals(&i).unwrap_err().field();
        assert_eq!(field(input(&[])), Some("lines"));
        assert_eq!(field(input(&NO_QTY)), Some("qty"));
        assert_eq!(field(input(&NEG_PRICE)), Some("price"));
        assert_eq!(field(input(&BIG_DISC)), Some("discount"));
        assert_eq!(field(input(&NO_FACTOR)), Some("unit"));
        let with = |f: &dyn Fn(&mut ReceiptInput<'_>)| {
            let mut i = ReceiptInput {
                lines: &ok,
                ..input(&[])
            };
            f(&mut i);
            receipt_totals(&i).unwrap_err().field()
        };
        assert_eq!(with(&|i| i.rate_bp = 10_001), Some("tax_rate"));
        assert_eq!(
            with(&|i| i.header_discount = rp(-1)),
            Some("header_discount")
        );
        assert_eq!(
            with(&|i| i.header_discount = rp(1_001)),
            Some("header_discount")
        );
        assert_eq!(with(&|i| i.other_costs = rp(-1)), Some("other_costs"));
        let huge = [line(1, i64::MAX, 0), line(1, i64::MAX, 0)];
        assert_eq!(
            receipt_totals(&ReceiptInput {
                lines: &huge,
                ..input(&[])
            }),
            Err(StockError::Overflow)
        );
    }

    #[test]
    fn tax_modes_and_return_credit() {
        for mode in [TaxMode::Included, TaxMode::Added, TaxMode::None] {
            assert_eq!(TaxMode::from_key(mode.as_str()), Some(mode));
        }
        assert_eq!(TaxMode::from_key("exclusive"), None);
        let lines = [(q(2), rp(10_000)), (qt(500), rp(3_000))];
        assert_eq!(
            supplier_return_credit(&lines, TaxMode::None, 1_100),
            Ok(rp(21_500))
        );
        assert_eq!(
            supplier_return_credit(&lines, TaxMode::Included, 1_100),
            Ok(rp(21_500))
        );
        assert_eq!(
            supplier_return_credit(&lines, TaxMode::Added, 1_100),
            Ok(rp(23_865))
        );
        let field = |l: &[(Qty, Money)], rate| {
            supplier_return_credit(l, TaxMode::Added, rate)
                .unwrap_err()
                .field()
        };
        assert_eq!(field(&[(Qty::ZERO, rp(1))], 0), Some("qty"));
        assert_eq!(field(&[(q(1), rp(-1))], 0), Some("price"));
        assert_eq!(field(&lines, 10_001), Some("tax_rate"));
    }
}
