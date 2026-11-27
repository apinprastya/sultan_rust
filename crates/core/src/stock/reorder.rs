//! Reorder: min / max rules, the low-stock check and suggested quantities from sales velocity
//! (spec 2.8 §10, I13).

use super::{StockError, add_qty, div_ceil, invalid, to_qty};
use crate::catalog::Qty;

/// Checks a reorder rule: `0 ≤ min ≤ max` (§10).
///
/// # Errors
/// [`StockError::InvalidField`] (`min`, `max`).
pub fn check_rule(min: Qty, max: Qty) -> Result<(), StockError> {
    if min.thousandths() < 0 {
        return Err(invalid("min", "must not be below 0"));
    }
    if max < min {
        return Err(invalid("max", "must not be below min"));
    }
    Ok(())
}

/// Whether a variant is on the low-stock list (§10): on hand + on order ≤ min (with a rule),
/// or on hand below 0.
#[must_use]
pub fn is_low(on_hand: Qty, on_order: Qty, min: Option<Qty>) -> bool {
    if on_hand.thousandths() < 0 {
        return true;
    }
    match (min, on_hand.checked_add(on_order)) {
        (Some(min), Some(available)) => available <= min,
        _ => false,
    }
}

/// The net sold quantity from the ledger sum of `sale` + `sale_void` + `return` over the
/// period (sales are negative): sold − returned.
///
/// # Errors
/// [`StockError::Overflow`].
pub fn net_sold(ledger_sum: Qty) -> Result<Qty, StockError> {
    ledger_sum.checked_neg().ok_or(StockError::Overflow)
}

/// What a suggestion needs (§10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReorderInput {
    /// Branch on hand (base units).
    pub on_hand: Qty,
    /// On order (base units).
    pub on_order: Qty,
    /// The rule `(min, max)` when there is one; it wins over the velocity.
    pub rule: Option<(Qty, Qty)>,
    /// Sold − returned over the last `velocity_days` (base units).
    pub net_sold: Qty,
    /// The velocity period N in days (> 0).
    pub velocity_days: u32,
    /// The supplier's lead days.
    pub lead_days: u32,
    /// The company's cover days.
    pub cover_days: u32,
    /// The purchase unit's factor to the base unit (> 0).
    pub purchase_factor: Qty,
}

/// A suggested line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Suggestion {
    /// Whole purchase units.
    pub units: i64,
    /// The same in base units.
    pub base_qty: Qty,
}

/// The suggested quantity of one variant (§10, §17 case 10), rounded **up** to whole purchase
/// units; `None` when nothing is needed. With a rule: max − (on hand + on order) once that is at
/// or below min. Without: velocity × (lead + cover days) − (on hand + on order), when above 0.
///
/// # Errors
/// [`StockError::InvalidField`] (`velocity_days`, `unit`); [`StockError::Overflow`].
pub fn suggest(input: &ReorderInput) -> Result<Option<Suggestion>, StockError> {
    if input.velocity_days == 0 {
        return Err(invalid("velocity_days", "must be above 0"));
    }
    if !input.purchase_factor.is_positive() {
        return Err(invalid("unit", "has no factor above 0"));
    }
    let available = i128::from(add_qty(input.on_hand, input.on_order)?.thousandths());
    let factor = i128::from(input.purchase_factor.thousandths());
    // The need as a fraction num / den in thousandths.
    let (num, den) = if let Some((min, max)) = input.rule {
        if available > i128::from(min.thousandths()) {
            return Ok(None);
        }
        (i128::from(max.thousandths()) - available, 1)
    } else {
        let days = i128::from(input.velocity_days);
        let cover = i128::from(input.lead_days) + i128::from(input.cover_days);
        (
            i128::from(input.net_sold.thousandths()) * cover - available * days,
            days,
        )
    };
    if num <= 0 {
        return Ok(None);
    }
    let units = div_ceil(num, den * factor);
    Ok(Some(Suggestion {
        units: i64::try_from(units).map_err(|_| StockError::Overflow)?,
        base_qty: to_qty(units * factor)?,
    }))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{q, qt};
    use super::*;

    fn input(on_hand: i64, on_order: i64, rule: Option<(i64, i64)>, factor: i64) -> ReorderInput {
        ReorderInput {
            on_hand: q(on_hand),
            on_order: q(on_order),
            rule: rule.map(|(a, b)| (q(a), q(b))),
            net_sold: q(54),
            velocity_days: 30,
            lead_days: 7,
            cover_days: 14,
            purchase_factor: q(factor),
        }
    }

    /// Spec §17 case 10: 60 sold − 6 returned over 30 days, lead 7 + cover 14, on hand 20, on
    /// order 5 → 12.8 → rounded up to the purchase unit; with a rule min 10 / max 50 and on hand
    /// + on order 9 → 41.
    #[test]
    fn suggestions() {
        assert_eq!(net_sold(q(-54)), Ok(q(54)));
        #[rustfmt::skip]
        let cases = [
            (input(20, 5, None, 1), Some((13, q(13)))),
            (input(20, 5, None, 12), Some((2, q(24)))),
            (input(20, 5, None, 13), Some((1, q(13)))),
            (input(40, 0, None, 1), None),
            (input(37, 0, None, 1), Some((1, q(1)))),   // 37.8 − 37 = 0.8 → 1
            (input(4, 5, Some((10, 50)), 1), Some((41, q(41)))),
            (input(4, 5, Some((10, 50)), 6), Some((7, q(42)))),
            (input(10, 0, Some((10, 50)), 1), Some((40, q(40)))),
            (input(11, 0, Some((10, 50)), 1), None),     // above min: the rule wins, no velocity
            (input(5, 0, Some((5, 5)), 1), None),
            (input(-3, 0, Some((0, 10)), 1), Some((13, q(13)))),
        ];
        for (i, expected) in cases {
            let got = suggest(&i).unwrap().map(|s| (s.units, s.base_qty));
            assert_eq!(got, expected, "{i:?}");
        }
        // A decimal purchase unit (0.5 kg).
        let mut half = input(20, 5, None, 1);
        half.purchase_factor = qt(500);
        assert_eq!(
            suggest(&half).unwrap().map(|s| (s.units, s.base_qty)),
            Some((26, q(13)))
        );
    }

    #[test]
    fn rules_low_stock_and_errors() {
        assert_eq!(check_rule(q(0), q(0)), Ok(()));
        assert_eq!(check_rule(q(10), q(50)), Ok(()));
        assert_eq!(check_rule(q(-1), q(5)).unwrap_err().field(), Some("min"));
        assert_eq!(check_rule(q(6), q(5)).unwrap_err().field(), Some("max"));
        #[rustfmt::skip]
        let cases = [
            (5, 4, Some(10), true),
            (5, 6, Some(10), false),
            (-1, 100, None, true),
            (0, 0, None, false),
            (0, 0, Some(0), true),
        ];
        for (on_hand, on_order, min, low) in cases {
            assert_eq!(is_low(q(on_hand), q(on_order), min.map(q)), low);
        }
        assert!(!is_low(qt(i64::MAX), qt(1), Some(q(1))));
        let mut bad = input(0, 0, None, 1);
        bad.velocity_days = 0;
        assert_eq!(suggest(&bad).unwrap_err().field(), Some("velocity_days"));
        let mut bad = input(0, 0, None, 1);
        bad.purchase_factor = Qty::ZERO;
        assert_eq!(suggest(&bad).unwrap_err().field(), Some("unit"));
        let mut big = input(0, 0, None, 1);
        big.on_hand = qt(i64::MAX);
        big.on_order = qt(1);
        assert_eq!(suggest(&big), Err(StockError::Overflow));
        assert_eq!(net_sold(qt(i64::MIN)), Err(StockError::Overflow));
        let mut huge = input(0, 0, Some((0, 1)), 1);
        huge.rule = Some((Qty::ZERO, qt(i64::MAX)));
        huge.purchase_factor = qt(1);
        assert_eq!(suggest(&huge).unwrap().map(|s| s.units), Some(i64::MAX));
    }
}
