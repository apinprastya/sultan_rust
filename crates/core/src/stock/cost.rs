//! The moving weighted average per branch and variant (spec 2.8 §5, I4).

use super::{StockError, add_money, add_qty, div_round, invalid, sub_money, to_money};
use crate::catalog::Qty;
use crate::money::Money;

/// The reference cost: the last known average as the fraction `value / qty` (`qty > 0`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RefCost {
    /// Rupiah.
    pub value: Money,
    /// Base units, above 0.
    pub qty: Qty,
}

impl RefCost {
    /// The value of `qty` base units at this cost, rounded half up: `qty × value / ref qty`.
    ///
    /// # Errors
    /// [`StockError::Overflow`] when it does not fit (or the stored ref qty is not above 0).
    pub fn value_of(self, qty: Qty) -> Result<Money, StockError> {
        if !self.qty.is_positive() {
            return Err(StockError::Overflow);
        }
        let n = i128::from(qty.thousandths()) * i128::from(self.value.minor());
        to_money(div_round(n, i128::from(self.qty.thousandths())))
    }

    /// The unit cost per base unit, rounded to Rp 1 for display (§5).
    ///
    /// # Errors
    /// As [`RefCost::value_of`].
    pub fn unit_cost(self) -> Result<Money, StockError> {
        self.value_of(Qty::ONE)
    }
}

/// The cost state of one branch and variant (§5): quantity (all its locations), value and the
/// reference cost, `None` while there is no cost yet.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CostState {
    /// Base units; may be negative (I7).
    pub qty: Qty,
    /// Rupiah; 0 whenever `qty` is 0.
    pub value: Money,
    /// The reference cost; `None` = no cost yet.
    pub reference: Option<RefCost>,
}

impl CostState {
    /// Incoming at a cost (`receipt`, `opening`, `transfer_in`, an increase with a typed cost,
    /// and a void or restock putting back a recorded value): `q` base units (> 0) worth `v`
    /// (≥ 0). Returns the movement's value, `v`.
    ///
    /// With stock and a cost, `value += v`. Otherwise (none, negative, or stock without a cost)
    /// the stock left after the receipt takes the new unit cost: `value = (qty + q) × v / q`, or
    /// 0 while `qty + q ≤ 0`. Then `ref = (value, qty)` when `qty > 0`, else `(v, q)`.
    ///
    /// # Errors
    /// [`StockError::InvalidField`] (`qty`, `value`) for `q ≤ 0` or `v < 0`;
    /// [`StockError::Overflow`].
    pub fn receive(&mut self, q: Qty, v: Money) -> Result<Money, StockError> {
        if !q.is_positive() {
            return Err(invalid("qty", "must be above 0"));
        }
        if v.is_negative() {
            return Err(invalid("value", "must not be below 0"));
        }
        let after = add_qty(self.qty, q)?;
        if self.qty.is_positive() && self.reference.is_some() {
            self.value = add_money(self.value, v)?;
        } else if after.is_positive() {
            let n = i128::from(after.thousandths()) * i128::from(v.minor());
            self.value = to_money(div_round(n, i128::from(q.thousandths())))?;
        } else {
            self.value = Money::ZERO;
        }
        self.qty = after;
        self.reference = Some(if self.qty.is_positive() {
            RefCost {
                value: self.value,
                qty: self.qty,
            }
        } else {
            RefCost { value: v, qty: q }
        });
        Ok(v)
    }

    /// Outgoing `q` base units (> 0) at the average. Returns the movement's value (≤ 0), or
    /// `None` when there is no cost yet (the quantity still moves). When the quantity reaches
    /// 0 the value becomes 0 and the rounding difference stays on this movement. `ref` stays.
    ///
    /// # Errors
    /// [`StockError::InvalidField`] (`qty`) for `q ≤ 0`; [`StockError::Overflow`].
    pub fn issue(&mut self, q: Qty) -> Result<Option<Money>, StockError> {
        if !q.is_positive() {
            return Err(invalid("qty", "must be above 0"));
        }
        let neg = q.checked_neg().ok_or(StockError::Overflow)?;
        self.at_average(neg)
    }

    /// Incoming `q` base units (> 0) at the average (`internal_in`, an increase of a variant
    /// with a cost, a void or restock without a recorded value). Returns the movement's value
    /// (≥ 0), or `None` without a cost. `ref` stays.
    ///
    /// # Errors
    /// [`StockError::InvalidField`] (`qty`) for `q ≤ 0`; [`StockError::Overflow`].
    pub fn restore(&mut self, q: Qty) -> Result<Option<Money>, StockError> {
        if !q.is_positive() {
            return Err(invalid("qty", "must be above 0"));
        }
        self.at_average(q)
    }

    /// An `adjust` or `count` increase of `q` (> 0): at the average when there is a cost;
    /// else at `typed` (the line's value from its typed unit cost) through
    /// [`receive`](Self::receive); else, when `need_cost`, `invalid_field` (`unit_cost`), and
    /// otherwise without a cost (`None`).
    ///
    /// # Errors
    /// [`StockError::InvalidField`] (`unit_cost`, `qty`, `value`); [`StockError::Overflow`].
    pub fn increase(
        &mut self,
        q: Qty,
        typed: Option<Money>,
        need_cost: bool,
    ) -> Result<Option<Money>, StockError> {
        if self.reference.is_some() {
            return self.restore(q);
        }
        match typed {
            Some(v) => self.receive(q, v).map(Some),
            None if need_cost => Err(invalid(
                "unit_cost",
                "is required while the variant has no cost yet",
            )),
            None => self.restore(q),
        }
    }

    /// Incoming `q` at a recorded value when there is one (a sale void, a restocked return),
    /// else at the average.
    ///
    /// # Errors
    /// As [`receive`](Self::receive) and [`restore`](Self::restore).
    pub fn put_back(&mut self, q: Qty, value: Option<Money>) -> Result<Option<Money>, StockError> {
        match value {
            Some(v) => self.receive(q, v).map(Some),
            None => self.restore(q),
        }
    }

    /// Takes back a receipt of `q` base units (> 0) worth `v` (≥ 0) when it is voided before any
    /// later movement used its cost (§6): `qty −= q`, `value −= v` (0 once `qty` is 0), and the
    /// reference goes back to the remaining average while there is stock with a value. Returns
    /// the movement's value, `−v`.
    ///
    /// # Errors
    /// [`StockError::InvalidField`] (`qty`, `value`); [`StockError::Overflow`].
    pub fn reverse(&mut self, q: Qty, v: Money) -> Result<Money, StockError> {
        if !q.is_positive() {
            return Err(invalid("qty", "must be above 0"));
        }
        if v.is_negative() {
            return Err(invalid("value", "must not be below 0"));
        }
        let neg = q.checked_neg().ok_or(StockError::Overflow)?;
        self.qty = add_qty(self.qty, neg)?;
        self.value = if self.qty == Qty::ZERO {
            Money::ZERO
        } else {
            sub_money(self.value, v)?
        };
        if self.qty.is_positive() && !self.value.is_negative() {
            self.reference = Some(RefCost {
                value: self.value,
                qty: self.qty,
            });
        }
        sub_money(Money::ZERO, v)
    }

    /// Adds a movement whose value is fixed elsewhere (a transit loss taken back in and out at
    /// the shipped value): `qty` and `value` change by it, `ref` stays.
    ///
    /// # Errors
    /// [`StockError::Overflow`].
    pub fn add_fixed(&mut self, signed: Qty, value: Option<Money>) -> Result<(), StockError> {
        self.qty = add_qty(self.qty, signed)?;
        if let Some(v) = value {
            self.value = add_money(self.value, v)?;
        }
        if self.qty == Qty::ZERO {
            self.value = Money::ZERO;
        }
        Ok(())
    }

    /// Moves `signed` base units at the reference cost.
    fn at_average(&mut self, signed: Qty) -> Result<Option<Money>, StockError> {
        let after = add_qty(self.qty, signed)?;
        let Some(reference) = self.reference else {
            self.qty = after;
            return Ok(None);
        };
        let value = if after == Qty::ZERO {
            // The rounding difference stays on this movement (§5, no drift).
            let v = sub_money(Money::ZERO, self.value)?;
            self.value = Money::ZERO;
            v
        } else {
            let v = reference.value_of(signed)?;
            self.value = add_money(self.value, v)?;
            v
        };
        self.qty = after;
        Ok(Some(value))
    }

    /// The unit cost per base unit for display, `None` without a cost (§5).
    ///
    /// # Errors
    /// [`StockError::Overflow`].
    pub fn unit_cost(&self) -> Result<Option<Money>, StockError> {
        self.reference.map(RefCost::unit_cost).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{q, qt, rp};
    use super::*;

    #[derive(Debug, Clone, Copy)]
    enum Op {
        Receive(i64, i64),
        Issue(i64),
        Restore(i64),
        Increase(i64, Option<i64>, bool),
        PutBack(i64, Option<i64>),
    }

    fn run(start: CostState, ops: &[Op]) -> (CostState, Vec<Result<Option<i64>, StockError>>) {
        let mut s = start;
        let out = ops
            .iter()
            .map(|op| {
                let r = match *op {
                    Op::Receive(n, v) => s.receive(qt(n), rp(v)).map(Some),
                    Op::Issue(n) => s.issue(qt(n)),
                    Op::Restore(n) => s.restore(qt(n)),
                    Op::Increase(n, typed, need) => s.increase(qt(n), typed.map(rp), need),
                    Op::PutBack(n, v) => s.put_back(qt(n), v.map(rp)),
                };
                r.map(|v| v.map(Money::minor))
            })
            .collect();
        (s, out)
    }

    fn state(qty: i64, value: i64, reference: Option<(i64, i64)>) -> CostState {
        CostState {
            qty: qt(qty),
            value: rp(value),
            reference: reference.map(|(v, n)| RefCost {
                value: rp(v),
                qty: qt(n),
            }),
        }
    }

    /// Spec §17 cases 1–4 and the edges of §5.
    #[test]
    #[allow(clippy::too_many_lines)] // one table row per case
    fn moving_average_table() {
        use Op::*;
        #[rustfmt::skip]
        type Case = (&'static str, CostState, Vec<Op>, Vec<Option<i64>>, CostState);
        let cases: Vec<Case> = vec![
            // Case 1: 10 @ 1 000 then 10 @ 1 300 → 20 @ 1 150; sell 5 → value 17 250.
            (
                "average",
                CostState::default(),
                vec![
                    Receive(10_000, 10_000),
                    Receive(10_000, 13_000),
                    Issue(5_000),
                ],
                vec![Some(10_000), Some(13_000), Some(-5_750)],
                state(15_000, 17_250, Some((23_000, 20_000))),
            ),
            // Case 2: 3 @ total 1 000, sell 1, 1, 1 → 333, 333, 334, value 0 (no drift).
            (
                "rounding",
                CostState::default(),
                vec![
                    Receive(3_000, 1_000),
                    Issue(1_000),
                    Issue(1_000),
                    Issue(1_000),
                ],
                vec![Some(1_000), Some(-333), Some(-333), Some(-334)],
                state(0, 0, Some((1_000, 3_000))),
            ),
            // Case 3: on hand −2 sold at ref 1 000, receive 10 @ 1 200 → 8 @ 9 600, ref 1 200.
            (
                "negative",
                state(0, 0, Some((1_000, 1_000))),
                vec![Issue(2_000), Receive(10_000, 12_000)],
                vec![Some(-2_000), Some(12_000)],
                state(8_000, 9_600, Some((9_600, 8_000))),
            ),
            // Still negative after the receipt: value 0, ref = the receipt's own cost.
            (
                "still negative",
                state(-5_000, -5_000, Some((1_000, 1_000))),
                vec![Receive(2_000, 2_400)],
                vec![Some(2_400)],
                state(-3_000, 0, Some((2_400, 2_000))),
            ),
            // Exactly 0 after the receipt.
            (
                "to zero",
                state(-2_000, -2_000, Some((1_000, 1_000))),
                vec![Receive(2_000, 2_400)],
                vec![Some(2_400)],
                state(0, 0, Some((2_400, 2_000))),
            ),
            // Case 4: no cost yet → null values; the first receipt sets the ref.
            (
                "no cost",
                CostState::default(),
                vec![
                    Issue(1_000),
                    Restore(500),
                    Receive(4_500, 9_000),
                    Issue(1_000),
                ],
                vec![None, None, Some(9_000), Some(-2_000)],
                state(3_000, 6_000, Some((8_000, 4_000))),
            ),
            // Stock from before 2.8 (qty, no cost) takes the first receipt's unit cost.
            (
                "old stock",
                state(10_000, 0, None),
                vec![Receive(10_000, 12_000)],
                vec![Some(12_000)],
                state(20_000, 24_000, Some((24_000, 20_000))),
            ),
            // Incoming at the average; back to 0 from below keeps no value.
            (
                "restore",
                state(-2_000, -2_001, Some((1_000, 1_000))),
                vec![Restore(1_000), Restore(1_000)],
                vec![Some(1_000), Some(1_001)],
                state(0, 0, Some((1_000, 1_000))),
            ),
            // Increases: average with a cost; typed cost without; none allowed for a count.
            (
                "increase avg",
                state(2_000, 3_000, Some((3_000, 2_000))),
                vec![Increase(1_000, Some(9_999), true)],
                vec![Some(1_500)],
                state(3_000, 4_500, Some((3_000, 2_000))),
            ),
            (
                "increase typed",
                CostState::default(),
                vec![Increase(2_000, Some(5_000), true)],
                vec![Some(5_000)],
                state(2_000, 5_000, Some((5_000, 2_000))),
            ),
            (
                "count no cost",
                CostState::default(),
                vec![Increase(2_000, None, false)],
                vec![None],
                state(2_000, 0, None),
            ),
            // Put back: a recorded value is received, none is at the average.
            (
                "put back",
                state(15_000, 17_250, Some((23_000, 20_000))),
                vec![PutBack(5_000, Some(5_750)), PutBack(1_000, None)],
                vec![Some(5_750), Some(1_150)],
                state(21_000, 24_150, Some((23_000, 20_000))),
            ),
        ];
        for (name, start, ops, values, end) in cases {
            let (got, out) = run(start, &ops);
            let out: Vec<_> = out.into_iter().map(Result::unwrap).collect();
            assert_eq!(out, values, "{name}");
            assert_eq!(got, end, "{name}");
        }
    }

    #[test]
    fn errors() {
        let mut s = CostState::default();
        assert_eq!(
            s.receive(Qty::ZERO, rp(1)).unwrap_err().field(),
            Some("qty")
        );
        assert_eq!(s.receive(q(1), rp(-1)).unwrap_err().field(), Some("value"));
        assert_eq!(s.issue(Qty::ZERO).unwrap_err().field(), Some("qty"));
        assert_eq!(s.restore(q(-1)).unwrap_err().field(), Some("qty"));
        // Case 4: an adjust increase without a cost and without a typed cost.
        assert_eq!(
            s.increase(q(1), None, true).unwrap_err().field(),
            Some("unit_cost")
        );
        assert_eq!(s, CostState::default(), "nothing changed");
        let mut big = state(i64::MAX, 0, None);
        assert_eq!(big.receive(q(1), rp(1)), Err(StockError::Overflow));
        let mut huge = state(1_000, 1, Some((i64::MAX, 1)));
        assert_eq!(huge.issue(qt(2_000)), Err(StockError::Overflow));
        assert_eq!(
            CostState::default()
                .issue(qt(i64::MIN))
                .unwrap_err()
                .field(),
            Some("qty")
        );
        let mut low = state(i64::MIN + 1, 0, None);
        assert_eq!(low.issue(q(1)), Err(StockError::Overflow));
        let bad = RefCost {
            value: rp(1),
            qty: Qty::ZERO,
        };
        assert_eq!(bad.value_of(q(1)), Err(StockError::Overflow));
    }

    /// A receipt voided before its cost was used puts the state back (§6, §17 case 15).
    #[test]
    fn reverse_and_fixed() {
        let mut s = CostState::default();
        s.receive(q(10), rp(10_000)).unwrap();
        let before = s;
        s.receive(q(10), rp(13_000)).unwrap();
        assert_eq!(s.reverse(q(10), rp(13_000)), Ok(rp(-13_000)));
        assert_eq!(s, before);
        assert_eq!(s.reverse(q(10), rp(10_000)), Ok(rp(-10_000)));
        assert_eq!(s, state(0, 0, Some((10_000, 10_000))), "ref kept at 0");
        let mut neg = state(-1_000, -500, Some((500, 1_000)));
        neg.reverse(q(1), rp(700)).unwrap();
        assert_eq!(neg, state(-2_000, -1_200, Some((500, 1_000))));
        assert_eq!(
            s.reverse(Qty::ZERO, rp(1)).unwrap_err().field(),
            Some("qty")
        );
        assert_eq!(s.reverse(q(1), rp(-1)).unwrap_err().field(), Some("value"));
        assert_eq!(
            state(i64::MIN, 0, None).reverse(qt(1), rp(0)),
            Err(StockError::Overflow)
        );
        // A transit loss in and out at the shipped value nets to nothing.
        let mut t = state(5_000, 6_000, Some((6_000, 5_000)));
        t.add_fixed(q(2), Some(rp(2_300))).unwrap();
        t.add_fixed(q(-2), Some(rp(-2_300))).unwrap();
        assert_eq!(t, state(5_000, 6_000, Some((6_000, 5_000))));
        t.add_fixed(q(-5), None).unwrap();
        assert_eq!(t, state(0, 0, Some((6_000, 5_000))));
        assert_eq!(
            state(-1, 0, None).add_fixed(qt(i64::MIN), None),
            Err(StockError::Overflow)
        );
    }

    #[test]
    fn unit_cost_for_display() {
        assert_eq!(
            state(20_000, 23_000, Some((23_000, 20_000))).unit_cost(),
            Ok(Some(rp(1_150)))
        );
        assert_eq!(
            state(3_000, 1_000, Some((1_000, 3_000))).unit_cost(),
            Ok(Some(rp(333)))
        );
        assert_eq!(
            state(3_000, 2_000, Some((2_000, 3_000))).unit_cost(),
            Ok(Some(rp(667)))
        );
        assert_eq!(CostState::default().unit_cost(), Ok(None));
    }
}
