//! Expiry lots taken first-expiry-first-out (spec 2.8 §9, I21, I22).

use super::{StockError, invalid, sub_qty};
use crate::catalog::Qty;
use crate::doc_number::LocalDay;

/// The longest batch number, in characters (§6).
pub const MAX_BATCH_CHARS: usize = 40;

/// A lot with stock, as FEFO sees it. Callers list lots oldest first, so equal expiries keep
/// that order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LotStock<L> {
    /// The lot's identity (an ID, or an index).
    pub lot: L,
    /// Its expiry date.
    pub expiry: LocalDay,
    /// Its quantity in base units; lots at or below 0 are skipped.
    pub qty: Qty,
}

/// One part of an outgoing quantity: from a lot, or from "no lot" (`None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Take<L> {
    /// The lot, `None` = no lot.
    pub lot: Option<L>,
    /// Base units (> 0).
    pub qty: Qty,
}

/// Takes `qty` (≥ 0) first-expiry-first-out (§9): earliest expiry, then the order given
/// (oldest lot first); what the lots do not hold comes from "no lot", which may go negative.
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`) for a negative quantity; [`StockError::Overflow`].
pub fn fefo<L: Copy>(lots: &[LotStock<L>], qty: Qty) -> Result<Vec<Take<L>>, StockError> {
    if qty.thousandths() < 0 {
        return Err(invalid("qty", "must not be below 0"));
    }
    let mut order: Vec<&LotStock<L>> = lots.iter().filter(|l| l.qty.is_positive()).collect();
    order.sort_by_key(|l| l.expiry);
    let mut rest = qty;
    let mut out = Vec::new();
    for lot in order {
        if !rest.is_positive() {
            break;
        }
        let take = lot.qty.min(rest);
        out.push(Take {
            lot: Some(lot.lot),
            qty: take,
        });
        rest = sub_qty(rest, take)?;
    }
    if rest.is_positive() {
        out.push(Take {
            lot: None,
            qty: rest,
        });
    }
    Ok(out)
}

/// Where a lot stands against the warning horizon (§9, I22).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExpiryStatus {
    /// Expired: the expiry date is before today.
    Expired,
    /// Expires within the warn days (today included).
    Expiring,
    /// Later.
    Fine,
}

/// The status of a lot expiring on `expiry`, seen on `today` with `warn_days` of warning.
#[must_use]
pub fn expiry_status(expiry: LocalDay, today: LocalDay, warn_days: u32) -> ExpiryStatus {
    let left = expiry.days_since(today);
    if left < 0 {
        ExpiryStatus::Expired
    } else if left <= i64::from(warn_days) {
        ExpiryStatus::Expiring
    } else {
        ExpiryStatus::Fine
    }
}

/// Checks and cleans a line's batch and expiry (§6, §9): the batch is trimmed and at most
/// [`MAX_BATCH_CHARS`]; a tracked product needs an expiry date. An untracked product keeps
/// neither (they are dropped).
///
/// # Errors
/// [`StockError::InvalidField`] (`batch`, `expiry`).
pub fn check_lot(
    track_expiry: bool,
    batch: Option<&str>,
    expiry: Option<LocalDay>,
) -> Result<Option<(String, LocalDay)>, StockError> {
    let batch = batch.map(str::trim).unwrap_or_default();
    if batch.chars().count() > MAX_BATCH_CHARS {
        return Err(invalid("batch", "is at most 40 characters"));
    }
    if !track_expiry {
        return Ok(None);
    }
    let expiry = expiry.ok_or(invalid("expiry", "is required for a tracked product"))?;
    Ok(Some((batch.to_owned(), expiry)))
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{q, qt};
    use super::*;

    fn day(key: i64) -> LocalDay {
        LocalDay::from_key(key).unwrap()
    }

    /// Spec §17 case 7: lots (A exp 03-01, 5), (B exp 02-01, 3); take 4 → B 3, A 1; take 10 →
    /// A 4, "no lot" 6 (−6 on hand without a lot).
    #[test]
    fn fefo_takes_the_earliest_expiry_first() {
        let mut lots = vec![
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
        ];
        let take = |lots: &mut Vec<LotStock<char>>, n| {
            let out = fefo(lots, n).unwrap();
            for t in &out {
                if let Some(l) = t.lot {
                    let lot = lots.iter_mut().find(|x| x.lot == l).unwrap();
                    lot.qty = lot.qty.checked_sub(t.qty).unwrap();
                }
            }
            out.iter().map(|t| (t.lot, t.qty)).collect::<Vec<_>>()
        };
        assert_eq!(
            take(&mut lots, q(4)),
            [(Some('B'), q(3)), (Some('A'), q(1))]
        );
        assert_eq!(take(&mut lots, q(10)), [(Some('A'), q(4)), (None, q(6))]);
        assert_eq!(take(&mut lots, qt(500)), [(None, qt(500))]);
        assert_eq!(take(&mut lots, Qty::ZERO), []);
    }

    #[test]
    fn fefo_ties_keep_the_given_order_and_skip_empty_lots() {
        let lots = [
            LotStock {
                lot: 1,
                expiry: day(20_270_101),
                qty: q(2),
            },
            LotStock {
                lot: 2,
                expiry: day(20_270_101),
                qty: q(2),
            },
            LotStock {
                lot: 3,
                expiry: day(20_261_201),
                qty: q(-1),
            },
            LotStock {
                lot: 4,
                expiry: day(20_261_101),
                qty: Qty::ZERO,
            },
        ];
        let out: Vec<_> = fefo(&lots, q(3))
            .unwrap()
            .iter()
            .map(|t| (t.lot, t.qty))
            .collect();
        assert_eq!(out, [(Some(1), q(2)), (Some(2), q(1))]);
        assert_eq!(fefo(&lots, q(-1)).unwrap_err().field(), Some("qty"));
    }

    #[test]
    fn expiry_statuses() {
        let today = day(20_261_126);
        #[rustfmt::skip]
        let cases = [
            (20_261_125, 30, ExpiryStatus::Expired),
            (20_261_126, 0, ExpiryStatus::Expiring),
            (20_261_226, 30, ExpiryStatus::Expiring),
            (20_261_227, 30, ExpiryStatus::Fine),
            (20_261_127, 0, ExpiryStatus::Fine),
        ];
        for (expiry, warn, status) in cases {
            assert_eq!(
                expiry_status(day(expiry), today, warn),
                status,
                "{expiry} {warn}"
            );
        }
    }

    #[test]
    fn lot_fields() {
        let exp = day(20_270_101);
        assert_eq!(
            check_lot(true, Some(" B1 "), Some(exp)),
            Ok(Some(("B1".into(), exp)))
        );
        assert_eq!(
            check_lot(true, None, Some(exp)),
            Ok(Some((String::new(), exp)))
        );
        assert_eq!(check_lot(false, Some("B1"), Some(exp)), Ok(None));
        assert_eq!(
            check_lot(true, Some("B1"), None).unwrap_err().field(),
            Some("expiry")
        );
        let long = "x".repeat(41);
        assert_eq!(
            check_lot(false, Some(&long), None).unwrap_err().field(),
            Some("batch")
        );
        assert!(check_lot(true, Some(&"é".repeat(40)), Some(exp)).is_ok());
    }
}
