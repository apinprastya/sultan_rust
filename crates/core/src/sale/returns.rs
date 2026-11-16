//! Returns and exchanges (spec 2.6 §8, D11, D12): returnable quantity, the refund per line, the
//! return window, returns without a receipt, restock or write-off, and exchange netting.

use super::{SaleError, SaleStatus, money, sub};
use crate::catalog::Qty;
use crate::doc_number::BranchTimeZone;
use crate::money::Money;
use crate::pricing::{self, div_round_half_up, sale_date};
use crate::time::Timestamp;

/// A sold line as the return needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SoldLine {
    /// The quantity sold, in the line's unit.
    pub qty: Qty,
    /// What the customer paid for it: the line `amount` (after the cart discount share; for a
    /// tax-exclusive sale, the line total with tax).
    pub amount: Money,
}

/// What earlier (posted) returns took from the line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct ReturnedSoFar {
    /// Quantity returned.
    pub qty: Qty,
    /// Amount refunded.
    pub refunded: Money,
}

/// `sold − already returned`, never below 0.
#[must_use]
pub fn returnable_qty(sold: Qty, returned: Qty) -> Qty {
    sold.checked_sub(returned)
        .unwrap_or(Qty::ZERO)
        .max(Qty::ZERO)
}

/// The refund for returning `qty` of a sold line (§8): `amount × qty ÷ sold` rounded half up;
/// the return that brings the line to its sold quantity takes what is left, so the refunds of a
/// line sum exactly to its amount. A partial refund never takes more than what is left (with
/// many small returns the rounding could otherwise overshoot).
///
/// # Errors
/// [`SaleError::InvalidQty`] for `qty` ≤ 0; [`SaleError::ReturnQtyExceeded`] above
/// [`returnable_qty`].
pub fn line_refund(sold: SoldLine, so_far: ReturnedSoFar, qty: Qty) -> Result<Money, SaleError> {
    if !qty.is_positive() {
        return Err(SaleError::InvalidQty {
            reason: "must be greater than 0",
        });
    }
    let max = returnable_qty(sold.qty, so_far.qty);
    if qty > max {
        return Err(SaleError::ReturnQtyExceeded { max });
    }
    let left = sub(sold.amount, so_far.refunded)?.max(Money::ZERO);
    if qty == max {
        return Ok(left);
    }
    let exact = div_round_half_up(
        i128::from(sold.amount.minor()) * i128::from(qty.thousandths()),
        i128::from(sold.qty.thousandths()),
    );
    Ok(money(exact)?.min(left))
}

/// Checks the return window (§8): the sale's branch-local date must be at most `return_days`
/// days before today's; `0` = no limit. Older → a manager override is needed.
///
/// # Errors
/// [`SaleError::ReturnWindow`].
pub fn check_return_window(
    sold_at: Timestamp,
    now: Timestamp,
    tz: BranchTimeZone,
    return_days: u32,
) -> Result<(), SaleError> {
    if return_days == 0 {
        return Ok(());
    }
    let days = (sale_date(now, tz) - sale_date(sold_at, tz)).num_days();
    if days > i64::from(return_days) {
        Err(SaleError::ReturnWindow { days: return_days })
    } else {
        Ok(())
    }
}

/// Checks that a sale can be returned by receipt: posted, and not imported from old Sultan
/// (read-only, §14).
///
/// # Errors
/// [`SaleError::AlreadyVoided`]; [`SaleError::InvalidField`] (`sale`) for a legacy sale.
pub const fn ensure_returnable(status: SaleStatus, legacy: bool) -> Result<(), SaleError> {
    if matches!(status, SaleStatus::Voided) {
        return Err(SaleError::AlreadyVoided);
    }
    if legacy {
        return Err(SaleError::InvalidField {
            field: "sale",
            reason: "imported sales are read-only",
        });
    }
    Ok(())
}

/// The refund of a line returned without a receipt (§8): the current resolved price for the
/// attached customer (or walk-in) × qty, rounded half up. The `refund.no_receipt` `MaxAmount`
/// limit is checked against the return's total refund
/// (`Requested::Amount`).
///
/// # Errors
/// [`SaleError::InvalidQty`]; [`SaleError::Pricing`] on overflow.
pub fn no_receipt_refund(unit_price: Money, qty: Qty) -> Result<Money, SaleError> {
    if !qty.is_positive() {
        return Err(SaleError::InvalidQty {
            reason: "must be greater than 0",
        });
    }
    Ok(pricing::line_amount(unit_price, qty)?)
}

/// What happens to a returned item (D9).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ReturnDisposition {
    /// Back on the shelf: a `return` stock movement.
    Restock,
    /// Damaged or unsellable: `return` + `write_off` movements, with a reason.
    WriteOff {
        /// Why.
        reason: String,
    },
}

impl ReturnDisposition {
    /// The longest write-off reason, in characters.
    pub const MAX_REASON_LEN: usize = 200;

    /// Checks (and trims) a write-off reason: not blank, at most 200 characters.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`reason`).
    pub fn validate(self) -> Result<Self, SaleError> {
        match self {
            Self::Restock => Ok(Self::Restock),
            Self::WriteOff { reason } => {
                let reason = reason.trim();
                if reason.is_empty() || reason.chars().count() > Self::MAX_REASON_LEN {
                    return Err(SaleError::InvalidField {
                        field: "reason",
                        reason: "a write-off needs a reason of 1 to 200 characters",
                    });
                }
                Ok(Self::WriteOff {
                    reason: reason.to_owned(),
                })
            }
        }
    }
}

/// An exchange settled in one flow (D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Exchange {
    /// The `exchange` tender on the new sale: the part of the refund that pays it.
    pub exchange_tender: Money,
    /// What the customer still pays on the new sale (with normal tenders).
    pub customer_pays: Money,
    /// What is refunded (cash or balance) because the new sale costs less.
    pub refund: Money,
}

/// Nets a return's `refund` against the new sale's `new_total` (§8, D12): return 50 000 and new
/// sale 65 000 → the customer pays 15 000; new sale 40 000 → 10 000 is refunded.
///
/// # Errors
/// [`SaleError::InvalidField`] (`amount`) for a negative amount.
pub fn net_exchange(refund: Money, new_total: Money) -> Result<Exchange, SaleError> {
    if refund.is_negative() || new_total.is_negative() {
        return Err(SaleError::InvalidField {
            field: "amount",
            reason: "must not be negative",
        });
    }
    let exchange_tender = refund.min(new_total);
    Ok(Exchange {
        exchange_tender,
        customer_pays: sub(new_total, exchange_tender)?,
        refund: sub(refund, exchange_tender)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn q(t: i64) -> Qty {
        Qty::from_thousandths(t)
    }

    /// Spec §15 cases 13 and 14.
    #[test]
    fn refunds_sum_to_the_line_amount() {
        // 3 units with a discount: amount 26 999 (not divisible by 3).
        let sold = SoldLine {
            qty: q(3_000),
            amount: m(26_999),
        };
        let first = line_refund(sold, ReturnedSoFar::default(), q(1_000)).unwrap();
        assert_eq!(first, m(9_000)); // 8 999.67 → 9 000
        let so_far = ReturnedSoFar {
            qty: q(1_000),
            refunded: first,
        };
        let rest = line_refund(sold, so_far, q(2_000)).unwrap();
        assert_eq!(rest, m(17_999));
        assert_eq!(first.minor() + rest.minor(), 26_999);
        // Case 14: more than sold − returned.
        assert_eq!(
            line_refund(sold, so_far, q(2_001)),
            Err(SaleError::ReturnQtyExceeded { max: q(2_000) })
        );
        assert_eq!(
            line_refund(sold, so_far, Qty::ZERO).unwrap_err().code(),
            "invalid_qty"
        );
        let done = ReturnedSoFar {
            qty: q(3_000),
            refunded: m(26_999),
        };
        assert_eq!(
            line_refund(sold, done, q(1)).unwrap_err().code(),
            "return_qty_exceeded"
        );
        assert_eq!(returnable_qty(q(3_000), q(5_000)), Qty::ZERO);
        assert_eq!(returnable_qty(q(i64::MIN), q(1)), Qty::ZERO);
    }

    #[test]
    fn many_small_returns_never_overshoot() {
        // 3 over 5 units: each 0.6 → 1, but the line only has 3.
        let sold = SoldLine {
            qty: q(5_000),
            amount: m(3),
        };
        let mut so_far = ReturnedSoFar::default();
        let mut refunds = Vec::new();
        for _ in 0..5 {
            let r = line_refund(sold, so_far, Qty::ONE).unwrap();
            refunds.push(r.minor());
            so_far.qty = so_far.qty.checked_add(Qty::ONE).unwrap();
            so_far.refunded = so_far.refunded.checked_add(r).unwrap();
        }
        assert_eq!(refunds, [1, 1, 1, 0, 0]);
        // Weighed: 0.5 of 1.255 kg at 16 302.
        let kg = SoldLine {
            qty: q(1_255),
            amount: m(16_302),
        };
        assert_eq!(
            line_refund(kg, ReturnedSoFar::default(), q(500)),
            Ok(m(6_495))
        );
        let overflow = SoldLine {
            qty: q(1_000),
            amount: m(i64::MAX),
        };
        assert_eq!(
            line_refund(
                overflow,
                ReturnedSoFar {
                    qty: Qty::ZERO,
                    refunded: m(-1)
                },
                Qty::ONE
            ),
            Err(SaleError::AmountOverflow)
        );
    }

    #[test]
    fn return_window() {
        const DAY: i64 = 86_400_000;
        // 2026-11-16T10:00:00Z = 17:00 WIB.
        const NOV16_UTC: i64 = 1_794_787_200_000;
        let now = Timestamp::from_unix_millis(NOV16_UTC + 10 * 3_600_000);
        let tz = BranchTimeZone::default();
        let ago = |days: i64| Timestamp::from_unix_millis(now.unix_millis() - days * DAY);
        #[rustfmt::skip]
        let cases = [
            (ago(30), 30, Ok(())),
            (ago(31), 30, Err(SaleError::ReturnWindow { days: 30 })),
            (ago(31), 0, Ok(())),
            (ago(0), 1, Ok(())),
            (ago(2), 1, Err(SaleError::ReturnWindow { days: 1 })),
            (ago(3_650), 0, Ok(())),
        ];
        for (sold_at, days, expected) in cases {
            assert_eq!(
                check_return_window(sold_at, now, tz, days),
                expected,
                "{sold_at} {days}"
            );
        }
        // Counted in local days: 23:30 WIB yesterday is 1 day ago.
        let late = Timestamp::from_unix_millis(NOV16_UTC - 7 * 3_600_000 - 30 * 60_000);
        assert_eq!(check_return_window(late, now, tz, 1), Ok(()));
    }

    /// Spec §15 case 15 (the amount the limit is checked against).
    #[test]
    fn no_receipt() {
        assert_eq!(no_receipt_refund(m(12_990), q(1_255)), Ok(m(16_302)));
        assert_eq!(no_receipt_refund(m(5_000), q(3_000)), Ok(m(15_000)));
        assert_eq!(
            no_receipt_refund(m(5_000), Qty::ZERO).unwrap_err().code(),
            "invalid_qty"
        );
        assert_eq!(
            no_receipt_refund(m(i64::MAX), q(2_000)).unwrap_err().code(),
            "amount_overflow"
        );
        assert_eq!(ensure_returnable(SaleStatus::Posted, false), Ok(()));
        assert_eq!(
            ensure_returnable(SaleStatus::Voided, false),
            Err(SaleError::AlreadyVoided)
        );
        assert_eq!(
            ensure_returnable(SaleStatus::Posted, true)
                .unwrap_err()
                .field(),
            Some("sale")
        );
    }

    #[test]
    fn dispositions() {
        assert_eq!(
            ReturnDisposition::Restock.validate(),
            Ok(ReturnDisposition::Restock)
        );
        assert_eq!(
            ReturnDisposition::WriteOff {
                reason: " broken ".into()
            }
            .validate(),
            Ok(ReturnDisposition::WriteOff {
                reason: "broken".into()
            })
        );
        for reason in ["", "  ", &"x".repeat(201)] {
            assert!(
                ReturnDisposition::WriteOff {
                    reason: reason.into()
                }
                .validate()
                .is_err()
            );
        }
    }

    /// Spec §15 case 16.
    #[test]
    fn exchanges() {
        #[rustfmt::skip]
        let cases = [
            (50_000, 65_000, (50_000, 15_000, 0)),
            (50_000, 40_000, (40_000, 0, 10_000)),
            (50_000, 50_000, (50_000, 0, 0)),
            (0, 10_000, (0, 10_000, 0)),
        ];
        for (refund, new_total, (tender, pays, back)) in cases {
            assert_eq!(
                net_exchange(m(refund), m(new_total)),
                Ok(Exchange {
                    exchange_tender: m(tender),
                    customer_pays: m(pays),
                    refund: m(back)
                })
            );
        }
        assert!(net_exchange(m(-1), m(1)).is_err());
        assert!(net_exchange(m(1), m(-1)).is_err());
    }
}
