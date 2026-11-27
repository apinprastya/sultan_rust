//! Transfers between branches: what the receiver gets and what the sender loses in transit
//! (spec 2.8 §8, I8).

use super::{LotStock, StockError, div_round, fefo, invalid, sub_money, sub_qty, to_money};
use crate::catalog::Qty;
use crate::money::Money;

/// Checks a received quantity against the shipped one: `0 ≤ received ≤ shipped` (§8; more is
/// an adjustment).
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`).
pub fn check_received(shipped: Qty, received: Qty) -> Result<(), StockError> {
    if received.thousandths() < 0 {
        return Err(invalid("qty", "must not be below 0"));
    }
    if received > shipped {
        return Err(invalid("qty", "must not exceed the shipped quantity"));
    }
    Ok(())
}

/// Splits the shipped value (≥ 0, `None` without a cost) of `shipped` base units between the
/// `received` part (the receiver's `transfer_in`) and the missing part (the sender's
/// `transit_loss`): `received value = round(value × received / shipped)`, the loss is the rest,
/// so both add up to the shipped value (§8, §17 case 11).
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`) as [`check_received`]; [`StockError::Overflow`].
pub fn split_value(
    shipped: Qty,
    value: Option<Money>,
    received: Qty,
) -> Result<(Option<Money>, Option<Money>), StockError> {
    check_received(shipped, received)?;
    let Some(value) = value else {
        return Ok((None, None));
    };
    if !shipped.is_positive() {
        return Ok((Some(Money::ZERO), Some(value)));
    }
    let got = to_money(div_round(
        i128::from(value.minor()) * i128::from(received.thousandths()),
        i128::from(shipped.thousandths()),
    ))?;
    Ok((Some(got), Some(sub_money(value, got)?)))
}

/// Splits the lots that travel with a line (§8) into the received part, taken
/// first-expiry-first-out, and the missing part. Each part lists `(lot, qty)`; the received
/// part only lists lots (the shortage comes off the latest expiries).
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`); [`StockError::Overflow`].
#[allow(clippy::type_complexity)] // two plain lists of (lot, qty)
pub fn split_lots<L: Copy + PartialEq>(
    lots: &[LotStock<L>],
    received: Qty,
) -> Result<(Vec<(L, Qty)>, Vec<(L, Qty)>), StockError> {
    let got: Vec<(L, Qty)> = fefo(lots, received)?
        .into_iter()
        .filter_map(|t| t.lot.map(|l| (l, t.qty)))
        .collect();
    let mut lost = Vec::new();
    for lot in lots.iter().filter(|l| l.qty.is_positive()) {
        let taken = got
            .iter()
            .find(|(l, _)| *l == lot.lot)
            .map_or(Qty::ZERO, |(_, q)| *q);
        let rest = sub_qty(lot.qty, taken)?;
        if rest.is_positive() {
            lost.push((lot.lot, rest));
        }
    }
    Ok((got, lost))
}

#[cfg(test)]
mod tests {
    use super::super::CostState;
    use super::super::test_util::{q, qt, rp};
    use super::*;
    use crate::doc_number::LocalDay;

    /// Spec §17 case 11: ship 10 at the sender's average, receive 8 → the receiver +8 at the
    /// shipped value, the sender's `transit_loss` 2.
    #[test]
    fn ship_and_receive_with_a_shortage() {
        let mut sender = CostState::default();
        sender.receive(q(30), rp(34_500)).unwrap(); // 1 150 each
        let shipped = sender.issue(q(10)).unwrap().unwrap();
        assert_eq!(shipped, rp(-11_500));
        let value = Money::ZERO.checked_sub(shipped);
        let (got, lost) = split_value(q(10), value, q(8)).unwrap();
        assert_eq!((got, lost), (Some(rp(9_200)), Some(rp(2_300))));
        let mut receiver = CostState::default();
        receiver.receive(q(8), got.unwrap()).unwrap();
        assert_eq!(receiver.unit_cost(), Ok(Some(rp(1_150))));
    }

    #[test]
    fn value_splits() {
        #[rustfmt::skip]
        let cases = [
            (3_000, Some(1_000), 1_000, (Some(333), Some(667))),
            (3_000, Some(1_000), 2_000, (Some(667), Some(333))),
            (3_000, Some(1_000), 3_000, (Some(1_000), Some(0))),
            (3_000, Some(1_000), 0, (Some(0), Some(1_000))),
            (3_000, None, 2_000, (None, None)),
            (0, Some(0), 0, (Some(0), Some(0))),
        ];
        for (shipped, value, received, (a, b)) in cases {
            let got = split_value(qt(shipped), value.map(rp), qt(received)).unwrap();
            assert_eq!(
                got,
                (a.map(rp), b.map(rp)),
                "{shipped} {value:?} {received}"
            );
        }
        assert_eq!(check_received(q(2), q(3)).unwrap_err().field(), Some("qty"));
        assert_eq!(
            check_received(q(2), q(-1)).unwrap_err().field(),
            Some("qty")
        );
        assert_eq!(
            split_value(q(1), Some(Money::from_minor(i64::MAX)), q(1)),
            Ok((Some(Money::from_minor(i64::MAX)), Some(Money::ZERO)))
        );
        assert_eq!(
            split_value(qt(1), Some(Money::from_minor(i64::MIN)), qt(1)),
            Ok((Some(Money::from_minor(i64::MIN)), Some(Money::ZERO)))
        );
    }

    #[test]
    fn lots_travel_and_the_shortage_comes_off_the_latest() {
        let day = |k| LocalDay::from_key(k).unwrap();
        let lots = [
            LotStock {
                lot: 'A',
                expiry: day(20_270_301),
                qty: q(5),
            },
            LotStock {
                lot: 'B',
                expiry: day(20_270_201),
                qty: q(3),
            },
            LotStock {
                lot: 'C',
                expiry: day(20_270_401),
                qty: q(2),
            },
        ];
        let (got, lost) = split_lots(&lots, q(6)).unwrap();
        assert_eq!(got, [('B', q(3)), ('A', q(3))]);
        assert_eq!(lost, [('A', q(2)), ('C', q(2))]);
        let (got, lost) = split_lots(&lots, q(10)).unwrap();
        assert_eq!((got.len(), lost.len()), (3, 0));
        assert_eq!(split_lots(&lots, q(-1)).unwrap_err().field(), Some("qty"));
    }
}
