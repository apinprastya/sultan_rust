//! The stock card's running balance (spec 2.8 §14).

use super::{StockError, add_qty};
use crate::catalog::Qty;

/// The running balance after each movement, starting from `opening` (the balance before the
/// first one): the stock card of a variant, oldest movement first.
///
/// # Errors
/// [`StockError::Overflow`].
pub fn running_balance(opening: Qty, movements: &[Qty]) -> Result<Vec<Qty>, StockError> {
    let mut balance = opening;
    movements
        .iter()
        .map(|m| {
            balance = add_qty(balance, *m)?;
            Ok(balance)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{q, qt};
    use super::*;

    #[test]
    fn running_balances() {
        #[rustfmt::skip]
        let cases: Vec<(i64, Vec<i64>, Vec<i64>)> = vec![
            (0, vec![], vec![]),
            (0, vec![10, -3, -9, 4], vec![10, 7, -2, 2]),
            (5, vec![-5], vec![0]),
        ];
        for (opening, moves, balances) in cases {
            let m: Vec<Qty> = moves.iter().map(|n| q(*n)).collect();
            let b: Vec<Qty> = balances.iter().map(|n| q(*n)).collect();
            assert_eq!(running_balance(q(opening), &m), Ok(b));
        }
        assert_eq!(
            running_balance(qt(i64::MAX), &[qt(1)]),
            Err(StockError::Overflow)
        );
    }
}
