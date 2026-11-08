//! The blind-close count form (spec 2.1 §5): the counted total, typed directly, or the number of
//! notes and coins per Rupiah denomination, which add up automatically.
//!
//! Rows: `0` is the total; `1..=10` are [`DENOMINATIONS`] (largest first). As soon as any
//! denomination has a quantity, the total is their sum and the total row is read-only; the
//! request then carries the counts, which the server checks add up to `counted_cash`. Rp 1.000
//! exists as a note and a coin; it is one row, because the server takes each denomination once.

use sultan_client::{CloseRequest, Count};

use crate::amount::Digits;

/// Rupiah denominations in circulation, largest first (notes, then coins from Rp 500).
pub const DENOMINATIONS: [i64; 10] = [
    100_000, 50_000, 20_000, 10_000, 5_000, 2_000, 1_000, 500, 200, 100,
];

/// Most digits of one quantity (up to 99.999 pieces).
const QUANTITY_DIGITS: usize = 5;

/// Whether `denomination` is (also) a coin.
#[must_use]
pub const fn is_coin(denomination: i64) -> bool {
    denomination <= 1_000
}

/// The count form; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CountForm {
    total: Digits,
    quantities: [Digits; DENOMINATIONS.len()],
    /// The selected row: `0` = total, `n` = `DENOMINATIONS[n - 1]`.
    pub cursor: usize,
}

impl Default for CountForm {
    fn default() -> Self {
        Self {
            total: Digits::amount(),
            quantities: std::array::from_fn(|_| Digits::with_max(QUANTITY_DIGITS)),
            cursor: 0,
        }
    }
}

impl CountForm {
    /// Number of rows (total + denominations).
    pub const ROWS: usize = DENOMINATIONS.len() + 1;

    /// Whether any denomination has a quantity.
    #[must_use]
    pub fn uses_denominations(&self) -> bool {
        self.quantities.iter().any(|q| q.quantity() > 0)
    }

    /// The quantity typed for row `row` (1-based denominations).
    #[must_use]
    pub fn quantity(&self, row: usize) -> u32 {
        row.checked_sub(1)
            .and_then(|i| self.quantities.get(i))
            .map_or(0, Digits::quantity)
    }

    /// Subtotal of one denomination row.
    #[must_use]
    pub fn subtotal(&self, row: usize) -> i64 {
        row.checked_sub(1)
            .and_then(|i| DENOMINATIONS.get(i))
            .map_or(0, |d| d * i64::from(self.quantity(row)))
    }

    /// The counted cash: the sum of the denominations if any, else the typed total. Cannot
    /// overflow: at most 10 × 99.999 × Rp 100.000, or 13 digits.
    #[must_use]
    pub fn counted(&self) -> i64 {
        if self.uses_denominations() {
            (1..Self::ROWS).map(|row| self.subtotal(row)).sum()
        } else {
            self.total.value()
        }
    }

    /// The close request.
    #[must_use]
    pub fn request(&self) -> CloseRequest {
        let counts: Vec<Count> = DENOMINATIONS
            .iter()
            .zip(&self.quantities)
            .filter(|(_, q)| q.quantity() > 0)
            .map(|(d, q)| Count {
                denomination: *d,
                quantity: q.quantity(),
            })
            .collect();
        CloseRequest {
            counted_cash: self.counted(),
            counts: (!counts.is_empty()).then_some(counts),
        }
    }

    fn selected(&mut self) -> Option<&mut Digits> {
        match self.cursor {
            0 if self.uses_denominations() => None,
            0 => Some(&mut self.total),
            row => self.quantities.get_mut(row - 1),
        }
    }

    /// Types a digit into the selected row. Returns whether anything changed.
    pub fn push(&mut self, digit: u8) -> bool {
        self.selected().is_some_and(|d| d.push(digit))
    }

    /// Deletes the last digit of the selected row. Returns whether anything changed.
    pub fn pop(&mut self) -> bool {
        self.selected().is_some_and(Digits::pop)
    }

    /// Moves the selection by `delta` rows, wrapping around.
    pub fn step(&mut self, delta: isize) {
        self.cursor = wrap(self.cursor, delta, Self::ROWS);
    }

    /// Selects `row` if it exists.
    pub fn select(&mut self, row: usize) {
        if row < Self::ROWS {
            self.cursor = row;
        }
    }
}

/// `at + delta`, wrapped into `0..len` (`len > 0`).
#[must_use]
pub fn wrap(at: usize, delta: isize, len: usize) -> usize {
    let len = isize::try_from(len).unwrap_or(isize::MAX).max(1);
    let at = isize::try_from(at).unwrap_or(0);
    usize::try_from((at + delta).rem_euclid(len)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(form: &mut CountForm, row: usize, digits: &[u8]) {
        form.select(row);
        for d in digits {
            form.push(*d);
        }
    }

    #[test]
    fn total_or_denominations() {
        let mut form = CountForm::default();
        typed(&mut form, 0, &[7, 5, 0, 0, 0, 0]);
        assert_eq!(form.counted(), 750_000);
        assert_eq!(
            form.request(),
            CloseRequest {
                counted_cash: 750_000,
                counts: None
            }
        );

        // 5 × 100.000 + 2 × 50.000 + 3 × 500 = 601.500; the typed total is ignored now.
        typed(&mut form, 1, &[5]);
        typed(&mut form, 2, &[2]);
        typed(&mut form, 8, &[3]);
        assert!(form.uses_denominations());
        assert_eq!(form.counted(), 601_500);
        assert_eq!(form.subtotal(2), 100_000);
        assert_eq!(form.subtotal(0), 0);
        let request = form.request();
        assert_eq!(request.counted_cash, 601_500);
        let counts: Vec<(i64, u32)> = request
            .counts
            .unwrap()
            .iter()
            .map(|c| (c.denomination, c.quantity))
            .collect();
        assert_eq!(counts, [(100_000, 5), (50_000, 2), (500, 3)]);

        form.select(0);
        assert!(!form.push(1), "the total is read-only while counting notes");
        assert!(!form.pop());
        form.select(8);
        assert!(form.pop());
        typed(&mut form, 1, &[]);
        assert!(form.pop());
        typed(&mut form, 2, &[]);
        assert!(form.pop());
        assert!(!form.uses_denominations());
        assert_eq!(form.counted(), 750_000, "back to the typed total");
    }

    #[test]
    fn navigation_wraps() {
        let mut form = CountForm::default();
        let cases = [(-1, 10), (1, 0), (1, 1), (3, 4), (-5, 10), (11, 10)];
        for (delta, expected) in cases {
            form.step(delta);
            assert_eq!(form.cursor, expected, "step {delta}");
        }
        form.select(99);
        assert_eq!(form.cursor, 10);
        assert_eq!(form.quantity(99), 0);
        assert!(is_coin(1_000) && is_coin(100) && !is_coin(2_000));
    }

    #[test]
    fn largest_count_does_not_overflow() {
        let mut form = CountForm::default();
        for row in 1..CountForm::ROWS {
            typed(&mut form, row, &[9, 9, 9, 9, 9, 9]);
        }
        let per_piece: i64 = DENOMINATIONS.iter().sum();
        assert_eq!(form.counted(), per_piece * 99_999);
    }
}
