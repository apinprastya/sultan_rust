//! Paying money back (spec 2.6 §7, §8, D10, D11): post-void and return refunds go to cash or
//! the customer balance only.

use super::payment::{SaleTender, TenderKind};
use super::{SaleError, SaleStatus, add, sub};
use crate::customer::{BalanceEntry, BalanceEntryKind, BalanceSettings};
use crate::money::Money;
use crate::pricing::CashRounding;

/// Where the refund goes (D11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefundMethod {
    /// Cash from the current session's drawer.
    Cash,
    /// The customer balance, up to `max_balance`; the rest in cash.
    Balance,
}

/// What a refund pays back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefundInput {
    /// The exact refund.
    pub amount: Money,
    /// The part of the original sale paid with the customer balance and not reversed yet (0
    /// without a receipt). It goes back to the balance first (§8).
    pub balance_paid: Money,
    /// Cash or balance for the rest.
    pub method: RefundMethod,
    /// The customer's current balance, when there is a customer (the sale's, or the attached
    /// one for a return without receipt).
    pub balance: Option<Money>,
    /// The balance limits (`max_balance` caps a refund to the balance; `max_saved_per_sale`
    /// does not apply, §8).
    pub settings: BalanceSettings,
    /// The company's cash rounding, applied to the cash part (2.3 §8).
    pub rounding: CashRounding,
}

/// How a refund is paid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefundPlan {
    /// Back to the balance as `payment_reversed`.
    pub payment_reversed: Money,
    /// To the balance as `refund` (method balance).
    pub to_balance: Money,
    /// The cash part before cash rounding.
    pub cash_exact: Money,
    /// The cash paid out of the drawer.
    pub cash: Money,
    /// `cash − cash_exact`, stored on the return or void (+/−).
    pub cash_rounding: Money,
    /// The balance entries: `payment_reversed` (+) and `refund` (+), when not 0.
    pub balance_entries: Vec<BalanceEntry>,
}

fn customer_needed() -> SaleError {
    SaleError::InvalidField {
        field: "customer",
        reason: "a refund to the balance needs the customer",
    }
}

/// Plans a refund (§8, D11): the part paid with the balance goes back to the balance first, then
/// the rest to the balance (method balance, capped so the balance stays at most `max_balance`,
/// never below 0) and whatever is left in cash, rounded with the cash rounding.
///
/// # Errors
/// [`SaleError::InvalidField`] (`amount` negative; `customer` when the balance is needed but
/// unknown); [`SaleError::AmountOverflow`].
pub fn plan_refund(input: RefundInput) -> Result<RefundPlan, SaleError> {
    if input.amount.is_negative() {
        return Err(SaleError::InvalidField {
            field: "amount",
            reason: "must not be negative",
        });
    }
    let payment_reversed = input.amount.min(input.balance_paid.max(Money::ZERO));
    let rest = sub(input.amount, payment_reversed)?;
    let to_balance = match input.method {
        RefundMethod::Cash => Money::ZERO,
        RefundMethod::Balance => {
            let balance = input.balance.ok_or_else(customer_needed)?;
            let after = add(balance, payment_reversed)?;
            let room = input
                .settings
                .max_balance
                .saturating_sub(after)
                .max(Money::ZERO);
            rest.min(room)
        }
    };
    if payment_reversed > Money::ZERO && input.balance.is_none() {
        return Err(customer_needed());
    }
    let cash_exact = sub(rest, to_balance)?;
    let rounded = input.rounding.apply(cash_exact, true);
    let mut balance_entries = Vec::new();
    if payment_reversed > Money::ZERO {
        balance_entries.push(BalanceEntry::new(
            BalanceEntryKind::PaymentReversed,
            payment_reversed,
        )?);
    }
    if to_balance > Money::ZERO {
        balance_entries.push(BalanceEntry::new(BalanceEntryKind::Refund, to_balance)?);
    }
    Ok(RefundPlan {
        payment_reversed,
        to_balance,
        cash_exact,
        cash: rounded.amount,
        cash_rounding: rounded.difference,
        balance_entries,
    })
}

/// A post-void request (§7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoidInput<'a> {
    /// The sale's status.
    pub status: SaleStatus,
    /// Whether the sale has a (posted) return.
    pub has_returns: bool,
    /// The sale's tenders.
    pub tenders: &'a [SaleTender],
    /// Cash or balance for what was not paid with the balance.
    pub method: RefundMethod,
    /// The sale customer's current balance, if the sale has a customer.
    pub balance: Option<Money>,
    /// The balance limits.
    pub settings: BalanceSettings,
    /// The company's cash rounding.
    pub rounding: CashRounding,
}

/// How a post-void pays back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoidPlan {
    /// Saved change taken back from the balance (`change_saved_reversed`).
    pub change_saved_reversed: Money,
    /// The refund of everything the customer paid.
    pub refund: RefundPlan,
    /// Every balance entry, in order: `change_saved_reversed` (−), `payment_reversed` (+),
    /// `refund` (+).
    pub balance_entries: Vec<BalanceEntry>,
}

/// Plans a post-void (§7, D10, D11). The customer gets back what they paid: the balance tenders
/// go back to the balance (`payment_reversed`); saved change is taken back
/// (`change_saved_reversed`) and paid out with the rest, since it was cash the customer handed
/// over; everything else is refunded in cash or to the balance ([`plan_refund`]). The cash comes
/// out of the current session's drawer. Stock is reversed with [`super::void_movements`].
///
/// # Errors
/// [`SaleError::AlreadyVoided`]; [`SaleError::HasReturns`]; see [`plan_refund`].
pub fn plan_post_void(input: VoidInput<'_>) -> Result<VoidPlan, SaleError> {
    if input.status == SaleStatus::Voided {
        return Err(SaleError::AlreadyVoided);
    }
    if input.has_returns {
        return Err(SaleError::HasReturns);
    }
    let mut paid = Money::ZERO;
    let mut saved = Money::ZERO;
    let mut balance_paid = Money::ZERO;
    for t in input.tenders {
        paid = add(paid, t.amount)?;
        saved = add(saved, t.change_saved)?;
        if t.kind == TenderKind::CustomerBalance {
            balance_paid = add(balance_paid, t.amount)?;
        }
    }
    let mut balance_entries = Vec::new();
    let balance = if saved > Money::ZERO {
        let balance = input.balance.ok_or_else(customer_needed)?;
        balance_entries.push(BalanceEntry::new(
            BalanceEntryKind::ChangeSavedReversed,
            sub(Money::ZERO, saved)?,
        )?);
        Some(sub(balance, saved)?)
    } else {
        input.balance
    };
    let refund = plan_refund(RefundInput {
        amount: add(paid, saved)?,
        balance_paid,
        method: input.method,
        balance,
        settings: input.settings,
        rounding: input.rounding,
    })?;
    balance_entries.extend(refund.balance_entries.iter().copied());
    Ok(VoidPlan {
        change_saved_reversed: saved,
        refund,
        balance_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::TenderTypeId;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn tender(kind: TenderKind, amount: i64, given: i64, saved: i64) -> SaleTender {
        SaleTender {
            tender_type: Some(TenderTypeId::generate()),
            kind,
            amount: m(amount),
            cash_received: (kind == TenderKind::Cash).then(|| m(amount + given + saved)),
            change_given: m(given),
            change_saved: m(saved),
            reference: None,
        }
    }

    fn input(
        amount: i64,
        balance_paid: i64,
        method: RefundMethod,
        balance: Option<i64>,
    ) -> RefundInput {
        RefundInput {
            amount: m(amount),
            balance_paid: m(balance_paid),
            method,
            balance: balance.map(m),
            settings: BalanceSettings::default(),
            rounding: CashRounding::None,
        }
    }

    #[test]
    fn refund_plans() {
        use RefundMethod::{Balance, Cash};
        // (amount, balance paid, method, balance) → (reversed, to balance, cash)
        #[rustfmt::skip]
        let cases = [
            ((50_000, 0, Cash, None), (0, 0, 50_000)),
            ((50_000, 6_000, Cash, Some(0)), (6_000, 0, 44_000)),
            ((5_000, 6_000, Cash, Some(0)), (5_000, 0, 0)),
            // Balance up to max_balance (100 000), the rest in cash; max_saved_per_sale ignored.
            ((50_000, 0, Balance, Some(10_000)), (0, 50_000, 0)),
            ((50_000, 0, Balance, Some(80_000)), (0, 20_000, 30_000)),
            ((50_000, 10_000, Balance, Some(80_000)), (10_000, 10_000, 30_000)),
            ((50_000, 0, Balance, Some(120_000)), (0, 0, 50_000)),
            ((50_000, 0, Balance, Some(-5_000)), (0, 50_000, 0)),
            ((0, 0, Balance, Some(0)), (0, 0, 0)),
        ];
        for ((amount, bp, method, balance), (reversed, to_balance, cash)) in cases {
            let plan = plan_refund(input(amount, bp, method, balance)).unwrap();
            assert_eq!(
                (plan.payment_reversed, plan.to_balance, plan.cash),
                (m(reversed), m(to_balance), m(cash)),
                "{amount} {bp} {method:?} {balance:?}"
            );
            let entries: i64 = plan
                .balance_entries
                .iter()
                .map(|e| e.amount().minor())
                .sum();
            assert_eq!(entries, reversed + to_balance);
        }
        // Errors.
        assert_eq!(
            plan_refund(input(-1, 0, Cash, None)).unwrap_err().field(),
            Some("amount")
        );
        assert_eq!(
            plan_refund(input(1, 0, Balance, None)).unwrap_err().field(),
            Some("customer")
        );
        assert_eq!(
            plan_refund(input(1, 1, Cash, None)).unwrap_err().field(),
            Some("customer")
        );
        // Cash rounding on the cash part.
        let plan = plan_refund(RefundInput {
            rounding: CashRounding::Down100,
            ..input(15_333, 0, Cash, None)
        })
        .unwrap();
        assert_eq!(
            (plan.cash_exact, plan.cash, plan.cash_rounding),
            (m(15_333), m(15_300), m(-33))
        );
    }

    /// Spec §15 case 12 (core part).
    #[test]
    fn post_void() {
        let settings = BalanceSettings::default();
        // Paid 6 300 from the balance and 40 000 in cash from 50 000 (change 3 000, 700 saved).
        let tenders = [
            tender(TenderKind::CustomerBalance, 6_300, 0, 0),
            tender(TenderKind::Cash, 40_000, 9_300, 700),
        ];
        let base = VoidInput {
            status: SaleStatus::Posted,
            has_returns: false,
            tenders: &tenders,
            method: RefundMethod::Cash,
            balance: Some(m(2_000)),
            settings,
            rounding: CashRounding::None,
        };
        let plan = plan_post_void(base).unwrap();
        assert_eq!(plan.change_saved_reversed, m(700));
        assert_eq!(
            (
                plan.refund.payment_reversed,
                plan.refund.to_balance,
                plan.refund.cash
            ),
            (m(6_300), m(0), m(40_700))
        );
        let entries: Vec<_> = plan
            .balance_entries
            .iter()
            .map(|e| (e.kind(), e.amount()))
            .collect();
        assert_eq!(
            entries,
            [
                (BalanceEntryKind::ChangeSavedReversed, m(-700)),
                (BalanceEntryKind::PaymentReversed, m(6_300)),
            ]
        );

        // A card sale post-voided to the balance: capped by max_balance, the rest in cash.
        let card = [tender(TenderKind::Card, 63_450, 0, 0)];
        let plan = plan_post_void(VoidInput {
            tenders: &card,
            method: RefundMethod::Balance,
            balance: Some(m(99_000)),
            ..base
        })
        .unwrap();
        assert_eq!(
            (plan.refund.to_balance, plan.refund.cash),
            (m(1_000), m(62_450))
        );
        // … or in cash, without a customer.
        let plan = plan_post_void(VoidInput {
            tenders: &card,
            balance: None,
            ..base
        })
        .unwrap();
        assert_eq!(
            (plan.refund.cash, plan.balance_entries.len()),
            (m(63_450), 0)
        );

        // Refusals.
        assert_eq!(
            plan_post_void(VoidInput {
                has_returns: true,
                ..base
            }),
            Err(SaleError::HasReturns)
        );
        assert_eq!(
            plan_post_void(VoidInput {
                status: SaleStatus::Voided,
                ..base
            }),
            Err(SaleError::AlreadyVoided)
        );
        assert_eq!(
            plan_post_void(VoidInput {
                balance: None,
                ..base
            })
            .unwrap_err()
            .field(),
            Some("customer")
        );
        // A zero-total sale voids to nothing.
        let plan = plan_post_void(VoidInput {
            tenders: &[],
            ..base
        })
        .unwrap();
        assert_eq!((plan.refund.cash, plan.balance_entries.len()), (m(0), 0));
    }
}
