//! What sales do to a cashier session (spec 2.6 §10): expected cash and the close-report
//! counters.
//!
//! This sits next to [`crate::cashier_session::close_report`] (spec 2.1 §5), which keeps
//! working as it is: the S3 server feeds the session's sales, returns, post-voids and cash
//! movements in here as [`SessionEvent`]s.
//!
//! **Expected cash.** §10 writes it as `opening_cash + cash tenders − cash change given − cash
//! refunds + cash_rounding + cash_in − cash_out − payouts`. A cash tender's stored `amount` is
//! the rounded rest it settles, so the cash rounding is part of it; this module counts the cash
//! that physically stays in the drawer per tender, `cash_received − change_given` (= `amount +
//! change_saved`), which already holds the rounding and the saved change, and lists
//! `cash_rounding` separately without adding it a second time (see spec 2.6 §19).

use std::collections::BTreeMap;

use super::payment::TenderKind;
use super::{SaleError, add, sub};
use crate::ids::TenderTypeId;
use crate::money::Money;

/// Which tender a report line is about: the tender type (none for exchange) and its kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TenderKey {
    /// The kind.
    pub kind: TenderKind,
    /// The tender type; `None` for an exchange tender.
    pub tender_type: Option<TenderTypeId>,
}

/// A count and an amount.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct CountAmount {
    /// How many.
    pub count: u32,
    /// The sum.
    pub amount: Money,
}

impl CountAmount {
    fn push(&mut self, amount: Money) -> Result<(), SaleError> {
        self.count = self.count.saturating_add(1);
        self.amount = add(self.amount, amount)?;
        Ok(())
    }
}

/// Sales and refunds of one tender in the report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TenderReport {
    /// Sale tenders (their `amount`).
    pub sales: CountAmount,
    /// Refunds paid with it (returns and post-voids).
    pub refunds: CountAmount,
}

/// One thing that happened in the session (§10). Amounts are positive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionEvent {
    /// A tender of a sale posted in the session.
    Tender {
        /// Which tender.
        key: TenderKey,
        /// Its amount.
        amount: Money,
        /// Cash: what was handed over (`None` = the amount + saved change).
        cash_received: Option<Money>,
        /// Cash: the change handed back.
        change_given: Money,
        /// Cash: the change saved to the balance.
        change_saved: Money,
    },
    /// A refund paid in the session (a return or a post-void, also of a sale of an earlier
    /// session, D10): cash out of the drawer, or to the customer balance.
    Refund {
        /// Which tender (cash or customer balance).
        key: TenderKey,
        /// The amount (for cash, what left the drawer).
        amount: Money,
    },
    /// The cash rounding of a sale or refund document (+/−).
    CashRounding(Money),
    /// A removed cart line (§3) and its amount.
    LineVoid(Money),
    /// A cleared cart and its amount.
    CartVoid(Money),
    /// A posted sale voided in this session, and its total.
    PostVoid(Money),
    /// A return posted in this session, and its refund.
    Return(Money),
    /// A line discount.
    LineDiscount(Money),
    /// A cart discount.
    CartDiscount(Money),
    /// A price override, and how much it lowered the line (0 when it raised it).
    PriceOverride(Money),
    /// Petty cash in.
    CashIn(Money),
    /// Petty cash out.
    CashOut(Money),
    /// A customer balance payout (2.5 §11).
    Payout(Money),
}

/// The session's sales figures (§10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalesSessionReport {
    /// Cash in the drawer at open.
    pub opening_cash: Money,
    /// Σ cash received on sale tenders.
    pub cash_received: Money,
    /// Σ change handed back.
    pub change_given: Money,
    /// Σ cash paid out for returns and post-voids.
    pub cash_refunds: Money,
    /// Σ cash rounding of the documents (included in the cash above, listed for the report).
    pub cash_rounding: Money,
    /// Petty cash in.
    pub cash_in: Money,
    /// Petty cash out.
    pub cash_out: Money,
    /// Balance payouts.
    pub payouts: Money,
    /// `opening_cash + cash_received − change_given − cash_refunds + cash_in − cash_out −
    /// payouts`.
    pub expected_cash: Money,
    /// Per tender: sales and refunds.
    pub tenders: BTreeMap<TenderKey, TenderReport>,
    /// Removed lines.
    pub line_voids: CountAmount,
    /// Cleared carts.
    pub cart_voids: CountAmount,
    /// Post-voids.
    pub post_voids: CountAmount,
    /// Returns.
    pub returns: CountAmount,
    /// Line discounts.
    pub line_discounts: CountAmount,
    /// Cart discounts.
    pub cart_discounts: CountAmount,
    /// Price overrides.
    pub price_overrides: CountAmount,
    /// Change saved to customer balances.
    pub saved_change: Money,
    /// Paid with customer balances.
    pub balance_paid: Money,
}

/// Computes the session's sales figures and expected cash (§10) from its events.
///
/// # Errors
/// [`SaleError::AmountOverflow`].
pub fn session_report(
    opening_cash: Money,
    events: &[SessionEvent],
) -> Result<SalesSessionReport, SaleError> {
    let mut r = SalesSessionReport {
        opening_cash,
        ..SalesSessionReport::default()
    };
    for event in events {
        match *event {
            SessionEvent::Tender {
                key,
                amount,
                cash_received,
                change_given,
                change_saved,
            } => {
                r.tenders.entry(key).or_default().sales.push(amount)?;
                match key.kind {
                    TenderKind::Cash => {
                        let received = match cash_received {
                            Some(c) => c,
                            None => add(add(amount, change_saved)?, change_given)?,
                        };
                        r.cash_received = add(r.cash_received, received)?;
                        r.change_given = add(r.change_given, change_given)?;
                        r.saved_change = add(r.saved_change, change_saved)?;
                    }
                    TenderKind::CustomerBalance => {
                        r.balance_paid = add(r.balance_paid, amount)?;
                    }
                    _ => {}
                }
            }
            SessionEvent::Refund { key, amount } => {
                r.tenders.entry(key).or_default().refunds.push(amount)?;
                if key.kind == TenderKind::Cash {
                    r.cash_refunds = add(r.cash_refunds, amount)?;
                }
            }
            SessionEvent::CashRounding(a) => r.cash_rounding = add(r.cash_rounding, a)?,
            SessionEvent::LineVoid(a) => r.line_voids.push(a)?,
            SessionEvent::CartVoid(a) => r.cart_voids.push(a)?,
            SessionEvent::PostVoid(a) => r.post_voids.push(a)?,
            SessionEvent::Return(a) => r.returns.push(a)?,
            SessionEvent::LineDiscount(a) => r.line_discounts.push(a)?,
            SessionEvent::CartDiscount(a) => r.cart_discounts.push(a)?,
            SessionEvent::PriceOverride(a) => r.price_overrides.push(a)?,
            SessionEvent::CashIn(a) => r.cash_in = add(r.cash_in, a)?,
            SessionEvent::CashOut(a) => r.cash_out = add(r.cash_out, a)?,
            SessionEvent::Payout(a) => r.payouts = add(r.payouts, a)?,
        }
    }
    let mut expected = add(opening_cash, r.cash_received)?;
    expected = sub(expected, r.change_given)?;
    expected = sub(expected, r.cash_refunds)?;
    expected = add(expected, r.cash_in)?;
    expected = sub(expected, r.cash_out)?;
    r.expected_cash = sub(expected, r.payouts)?;
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn key(kind: TenderKind, id: TenderTypeId) -> TenderKey {
        TenderKey {
            kind,
            tender_type: Some(id),
        }
    }

    /// Spec §15 case 18 (the arithmetic) and case 7 (saved change stays in the drawer).
    #[test]
    fn scripted_session() {
        use SessionEvent as E;
        let cash_id = TenderTypeId::generate();
        let qris_id = TenderTypeId::generate();
        let bal_id = TenderTypeId::generate();
        let cash = key(TenderKind::Cash, cash_id);
        let qris = key(TenderKind::Qris, qris_id);
        let bal = key(TenderKind::CustomerBalance, bal_id);
        let tender = |key, amount, received: Option<i64>, given, saved| E::Tender {
            key,
            amount: m(amount),
            cash_received: received.map(m),
            change_given: m(given),
            change_saved: m(saved),
        };
        let events = [
            // Sale 1: 63 450 rounded to 63 500, paid 70 000 cash, change 6 500.
            tender(cash, 63_500, Some(70_000), 6_500, 0),
            E::CashRounding(m(50)),
            // Sale 2: 46 300, cash 50 000, change 3 700 of which 700 saved (case 7).
            tender(cash, 46_300, None, 3_000, 700),
            E::LineDiscount(m(2_000)),
            // Sale 3: QRIS 50 000 + balance 2 000; a cart discount and an override.
            tender(qris, 50_000, None, 0, 0),
            tender(bal, 2_000, None, 0, 0),
            E::CartDiscount(m(5_000)),
            E::PriceOverride(m(1_000)),
            // Voids at the till.
            E::LineVoid(m(5_000)),
            E::LineVoid(m(7_000)),
            E::CartVoid(m(30_000)),
            // A post-void of an older sale, paid back in cash, and a return to the balance.
            E::PostVoid(m(20_000)),
            E::Refund {
                key: cash,
                amount: m(20_000),
            },
            E::Return(m(9_000)),
            E::Refund {
                key: bal,
                amount: m(9_000),
            },
            E::CashIn(m(100_000)),
            E::CashOut(m(15_000)),
            E::Payout(m(2_450)),
        ];
        let r = session_report(m(200_000), &events).unwrap();
        // 200 000 + 70 000 − 6 500 + 47 000 ... = 200 000 + 120 000 − 9 500 − 20 000 + 100 000
        //   − 15 000 − 2 450.
        assert_eq!(
            (
                r.cash_received,
                r.change_given,
                r.cash_refunds,
                r.saved_change
            ),
            (m(120_000), m(9_500), m(20_000), m(700))
        );
        assert_eq!(r.expected_cash, m(373_050));
        // The same as opening + Σ cash amounts + saved change − refunds + in − out − payouts:
        // the rounding is inside the cash amount.
        assert_eq!(
            r.expected_cash.minor(),
            200_000 + 63_500 + 46_300 + 700 - 20_000 + 100_000 - 15_000 - 2_450
        );
        assert_eq!((r.cash_rounding, r.balance_paid), (m(50), m(2_000)));
        let ca = |count, amount| CountAmount {
            count,
            amount: m(amount),
        };
        assert_eq!(r.tenders[&cash].sales, ca(2, 109_800));
        assert_eq!(r.tenders[&cash].refunds, ca(1, 20_000));
        assert_eq!(r.tenders[&qris].sales, ca(1, 50_000));
        assert_eq!(r.tenders[&bal].sales, ca(1, 2_000));
        assert_eq!(r.tenders[&bal].refunds, ca(1, 9_000));
        assert_eq!(
            (r.line_voids, r.cart_voids, r.post_voids, r.returns),
            (ca(2, 12_000), ca(1, 30_000), ca(1, 20_000), ca(1, 9_000))
        );
        assert_eq!(
            (r.line_discounts, r.cart_discounts, r.price_overrides),
            (ca(1, 2_000), ca(1, 5_000), ca(1, 1_000))
        );
        assert_eq!(
            (r.cash_in, r.cash_out, r.payouts),
            (m(100_000), m(15_000), m(2_450))
        );

        // An empty session expects its opening cash.
        let r = session_report(m(500_000), &[]).unwrap();
        assert_eq!(r.expected_cash, m(500_000));
    }

    #[test]
    fn overflow() {
        let huge = m(i64::MAX);
        let cash = TenderKey {
            kind: TenderKind::Cash,
            tender_type: None,
        };
        let cases: [&[SessionEvent]; 4] = [
            &[SessionEvent::CashIn(huge), SessionEvent::CashIn(huge)],
            &[SessionEvent::LineVoid(huge), SessionEvent::LineVoid(huge)],
            &[SessionEvent::Tender {
                key: cash,
                amount: huge,
                cash_received: None,
                change_given: m(1),
                change_saved: m(0),
            }],
            &[SessionEvent::Payout(huge)],
        ];
        for events in cases {
            let opening = if events.len() == 1 { m(-2) } else { m(0) };
            assert_eq!(
                session_report(opening, events),
                Err(SaleError::AmountOverflow),
                "{events:?}"
            );
        }
    }
}
