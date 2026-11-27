//! Opname: counting, variance and what posting moves (spec 2.8 §11, I9, I10).

use super::{StockError, add_qty, invalid, sub_qty};
use crate::catalog::Qty;

/// One count entry: a scan adds to the line, a typed quantity replaces it (§11). Several users
/// may count into one line; their entries apply in the order they were made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CountEntry {
    /// Base units (≥ 0 when it replaces; a scan may also take back with a negative quantity).
    pub base_qty: Qty,
    /// The entry replaces what was counted so far.
    pub replace: bool,
}

/// The counted quantity of a line from its entries in order, `None` when nothing was counted.
///
/// # Errors
/// [`StockError::InvalidField`] (`qty`) for a negative replacing entry or a total below 0;
/// [`StockError::Overflow`].
pub fn counted(entries: &[CountEntry]) -> Result<Option<Qty>, StockError> {
    let mut total: Option<Qty> = None;
    for e in entries {
        if e.replace {
            if e.base_qty.thousandths() < 0 {
                return Err(invalid("qty", "a typed count must not be below 0"));
            }
            total = Some(e.base_qty);
        } else {
            total = Some(add_qty(total.unwrap_or(Qty::ZERO), e.base_qty)?);
        }
    }
    if total.is_some_and(|t| t.thousandths() < 0) {
        return Err(invalid("qty", "the count must not be below 0"));
    }
    Ok(total)
}

/// The variance of a counted line (§11, I9): counted − (snapshot + net movements of the variant
/// at the location since the snapshot, other than this opname's).
///
/// # Errors
/// [`StockError::Overflow`].
pub fn variance(counted: Qty, snapshot: Qty, since: Qty) -> Result<Qty, StockError> {
    sub_qty(counted, add_qty(snapshot, since)?)
}

/// What to do with uncounted variants of a full count (I10).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Uncounted {
    /// Set them to 0.
    SetZero,
    /// Leave them as they are.
    Leave,
}

impl Uncounted {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SetZero => "set_zero",
            Self::Leave => "leave",
        }
    }

    /// The choice of a stored name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        [Self::SetZero, Self::Leave]
            .into_iter()
            .find(|u| u.as_str() == key)
    }
}

/// One line of an opname at posting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpnameLine<V> {
    /// The variant.
    pub variant: V,
    /// On hand at the start.
    pub snapshot: Qty,
    /// Net movements since the start (other than this opname).
    pub since: Qty,
    /// The counted quantity, `None` = not counted.
    pub counted: Option<Qty>,
}

impl<V> OpnameLine<V> {
    /// On hand now: snapshot + movements since.
    ///
    /// # Errors
    /// [`StockError::Overflow`].
    pub fn expected(&self) -> Result<Qty, StockError> {
        add_qty(self.snapshot, self.since)
    }
}

/// The variants of a full count that were not counted and are not 0 now (I10, listed before
/// posting).
///
/// # Errors
/// [`StockError::Overflow`].
pub fn uncounted<V: Copy>(lines: &[OpnameLine<V>]) -> Result<Vec<V>, StockError> {
    let mut out = Vec::new();
    for l in lines {
        if l.counted.is_none() && l.expected()? != Qty::ZERO {
            out.push(l.variant);
        }
    }
    Ok(out)
}

/// The `count` movements of posting (§11): every non-zero variance of a counted line; in a
/// `full` count with [`Uncounted::SetZero`], every uncounted line not at 0 goes to 0. A cycle
/// count never touches what was not counted.
///
/// # Errors
/// [`StockError::Overflow`].
pub fn opname_moves<V: Copy>(
    lines: &[OpnameLine<V>],
    full: bool,
    choice: Uncounted,
) -> Result<Vec<(V, Qty)>, StockError> {
    let mut out = Vec::new();
    for l in lines {
        let delta = match l.counted {
            Some(c) => variance(c, l.snapshot, l.since)?,
            None if full && choice == Uncounted::SetZero => Qty::ZERO
                .checked_sub(l.expected()?)
                .ok_or(StockError::Overflow)?,
            None => continue,
        };
        if delta != Qty::ZERO {
            out.push((l.variant, delta));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{q, qt};
    use super::*;

    fn scan(n: i64) -> CountEntry {
        CountEntry {
            base_qty: qt(n),
            replace: false,
        }
    }

    fn typed(n: i64) -> CountEntry {
        CountEntry {
            base_qty: qt(n),
            replace: true,
        }
    }

    #[test]
    fn counts_from_entries() {
        #[rustfmt::skip]
        type Case = (Vec<CountEntry>, Result<Option<i64>, Option<&'static str>>);
        let cases: Vec<Case> = vec![
            (vec![], Ok(None)),
            // Two counters add up (§17 case 19).
            (vec![scan(3_000), scan(4_000)], Ok(Some(7_000))),
            (
                vec![scan(3_000), typed(10_000), scan(1_000)],
                Ok(Some(11_000)),
            ),
            (vec![typed(0)], Ok(Some(0))),
            (vec![scan(2_000), scan(-1_000)], Ok(Some(1_000))),
            (vec![scan(-1_000)], Err(Some("qty"))),
            (vec![typed(-1)], Err(Some("qty"))),
            (vec![scan(i64::MAX), scan(1)], Err(None)),
        ];
        for (entries, expected) in cases {
            let got = counted(&entries)
                .map(|c| c.map(Qty::thousandths))
                .map_err(|e| e.field());
            assert_eq!(got, expected, "{entries:?}");
        }
    }

    /// Spec §17 case 8: snapshot 10, sold 2 since, counted 7 → variance −1.
    #[test]
    fn variance_with_sales_during_the_count() {
        #[rustfmt::skip]
        let cases = [
            (7, 10, -2, -1),
            (8, 10, -2, 0),
            (12, 10, 0, 2),
            (0, -3, 0, 3),
            (5, 0, 3, 2),
        ];
        for (c, snap, since, v) in cases {
            assert_eq!(
                variance(q(c), q(snap), q(since)),
                Ok(q(v)),
                "{c} {snap} {since}"
            );
        }
        assert_eq!(
            variance(qt(i64::MIN), qt(1), Qty::ZERO),
            Err(StockError::Overflow)
        );
    }

    /// Spec §17 case 9: a full count's uncounted items, set to 0 vs leave; a cycle count.
    #[test]
    fn uncounted_in_a_full_count() {
        let lines = [
            OpnameLine {
                variant: 'a',
                snapshot: q(10),
                since: q(-2),
                counted: Some(q(7)),
            },
            OpnameLine {
                variant: 'b',
                snapshot: q(4),
                since: Qty::ZERO,
                counted: None,
            },
            OpnameLine {
                variant: 'c',
                snapshot: q(1),
                since: q(-1),
                counted: None,
            },
            OpnameLine {
                variant: 'd',
                snapshot: q(-2),
                since: Qty::ZERO,
                counted: None,
            },
            OpnameLine {
                variant: 'e',
                snapshot: q(3),
                since: Qty::ZERO,
                counted: Some(q(3)),
            },
        ];
        assert_eq!(uncounted(&lines), Ok(vec!['b', 'd']));
        #[rustfmt::skip]
        let cases = [
            (true, Uncounted::SetZero, vec![('a', q(-1)), ('b', q(-4)), ('d', q(2))]),
            (true, Uncounted::Leave, vec![('a', q(-1))]),
            (false, Uncounted::SetZero, vec![('a', q(-1))]),
        ];
        for (full, choice, moves) in cases {
            assert_eq!(
                opname_moves(&lines, full, choice),
                Ok(moves),
                "{full} {choice:?}"
            );
        }
        let bad = [OpnameLine {
            variant: 'x',
            snapshot: qt(i64::MAX),
            since: qt(1),
            counted: None,
        }];
        assert_eq!(uncounted(&bad), Err(StockError::Overflow));
        assert_eq!(
            opname_moves(&bad, true, Uncounted::SetZero),
            Err(StockError::Overflow)
        );
        for u in [Uncounted::SetZero, Uncounted::Leave] {
            assert_eq!(Uncounted::from_key(u.as_str()), Some(u));
        }
        assert_eq!(Uncounted::from_key("zero"), None);
    }
}
