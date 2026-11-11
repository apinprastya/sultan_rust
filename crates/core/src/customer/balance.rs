//! The customer balance for saved change, "simpan kembalian" (spec 2.5 §11).
//!
//! The balance is an append-only ledger per customer: the balance is the sum of its entries
//! ([`ledger_balance`]). Entries are only ever added: a void or refund adds reversal entries
//! ([`reverse_sale`]), a merge moves B's balance with a `merge_out`/`merge_in` pair
//! ([`merge_balance`]). This module holds the kinds and amounts; ids, branch, register, user,
//! session, document and time of an entry are stored by the database.
//!
//! In P1 the balance only grows from saved change ([`save_change`]) and shrinks when it pays
//! ([`pay_with_balance`]) or is paid out before an erase ([`payout`], [`check_erase`]). It never
//! expires. Offline branches may drive a balance negative; that is allowed and filled by the
//! next saved change.

use std::fmt;
use std::str::FromStr;

use super::CustomerError;
use crate::money::Money;

/// The kind of a balance ledger entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BalanceEntryKind {
    /// `change_saved` (+): change of a cash sale saved to the balance.
    ChangeSaved,
    /// `payment` (−): the balance used as a tender.
    Payment,
    /// `change_saved_reversed` (−): the sale that saved change was voided or refunded.
    ChangeSavedReversed,
    /// `payment_reversed` (+): the sale paid with the balance was voided or refunded.
    PaymentReversed,
    /// `payout` (−): the balance paid out in cash; + when writing off a negative balance.
    Payout,
    /// `merge_out` (−): B's balance leaving B in a merge; + when that balance is negative.
    MergeOut,
    /// `merge_in` (+): B's balance arriving at A in a merge; − when that balance is negative.
    MergeIn,
}

impl BalanceEntryKind {
    /// Every kind, in the order of the spec's table.
    pub const ALL: [Self; 7] = [
        Self::ChangeSaved,
        Self::Payment,
        Self::ChangeSavedReversed,
        Self::PaymentReversed,
        Self::Payout,
        Self::MergeOut,
        Self::MergeIn,
    ];

    /// The stored name, e.g. `change_saved`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChangeSaved => "change_saved",
            Self::Payment => "payment",
            Self::ChangeSavedReversed => "change_saved_reversed",
            Self::PaymentReversed => "payment_reversed",
            Self::Payout => "payout",
            Self::MergeOut => "merge_out",
            Self::MergeIn => "merge_in",
        }
    }

    /// The sign of the amount in the spec's table: `1` or `-1`.
    #[must_use]
    pub const fn sign(self) -> i64 {
        match self {
            Self::ChangeSaved | Self::PaymentReversed | Self::MergeIn => 1,
            Self::Payment | Self::ChangeSavedReversed | Self::Payout | Self::MergeOut => -1,
        }
    }

    /// Whether the sign is fixed. `payout`, `merge_out` and `merge_in` flip it for a negative
    /// balance (a write-off, or moving a negative balance).
    #[must_use]
    pub const fn strict_sign(self) -> bool {
        !matches!(self, Self::Payout | Self::MergeOut | Self::MergeIn)
    }

    /// The kind that reverses this one when its sale is voided or refunded: `change_saved` →
    /// `change_saved_reversed`, `payment` → `payment_reversed`. Other kinds are not reversed.
    #[must_use]
    pub const fn reversal(self) -> Option<Self> {
        match self {
            Self::ChangeSaved => Some(Self::ChangeSavedReversed),
            Self::Payment => Some(Self::PaymentReversed),
            _ => None,
        }
    }
}

impl fmt::Display for BalanceEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BalanceEntryKind {
    type Err = CustomerError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or(CustomerError::InvalidField {
                field: "kind",
                reason: "not a balance entry kind",
            })
    }
}

/// One ledger entry: its kind and signed amount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceEntry {
    kind: BalanceEntryKind,
    amount: Money,
}

impl BalanceEntry {
    /// An entry with a signed amount. The amount is never 0, and for kinds with a
    /// [strict sign](BalanceEntryKind::strict_sign) it has the sign of the spec's table.
    ///
    /// # Errors
    /// [`CustomerError::InvalidField`] with field `amount`, or
    /// [`CustomerError::AmountOverflow`] for `i64::MIN` (whose reversal would not fit).
    pub fn new(kind: BalanceEntryKind, amount: Money) -> Result<Self, CustomerError> {
        if amount.minor() == i64::MIN {
            return Err(CustomerError::AmountOverflow);
        }
        let signum = amount.minor().signum();
        if signum == 0 {
            return Err(CustomerError::InvalidField {
                field: "amount",
                reason: "must not be zero",
            });
        }
        if kind.strict_sign() && signum != kind.sign() {
            return Err(CustomerError::InvalidField {
                field: "amount",
                reason: "has the wrong sign for the entry kind",
            });
        }
        Ok(Self { kind, amount })
    }

    /// The kind.
    #[must_use]
    pub const fn kind(self) -> BalanceEntryKind {
        self.kind
    }

    /// The signed amount.
    #[must_use]
    pub const fn amount(self) -> Money {
        self.amount
    }

    /// The entry that reverses this one exactly, or `None` for kinds that are not reversed.
    #[must_use]
    pub fn reversal(self) -> Option<Self> {
        // `new` never builds an `i64::MIN` amount, so the negation cannot overflow.
        Some(Self {
            kind: self.kind.reversal()?,
            amount: Money::from_minor(-self.amount.minor()),
        })
    }
}

/// The balance: the sum of the entries.
///
/// # Errors
/// [`CustomerError::AmountOverflow`] when the sum does not fit in 64 bits.
pub fn ledger_balance<'a>(
    entries: impl IntoIterator<Item = &'a BalanceEntry>,
) -> Result<Money, CustomerError> {
    entries.into_iter().try_fold(Money::ZERO, |sum, e| {
        sum.checked_add(e.amount)
            .ok_or(CustomerError::AmountOverflow)
    })
}

/// The company's balance limits (spec 2.5 §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceSettings {
    /// The most change saved on one sale. 0 turns saving change off.
    pub max_saved_per_sale: Money,
    /// The most a customer balance may reach by saving change (a merge may exceed it).
    pub max_balance: Money,
}

impl BalanceSettings {
    /// The default per-sale limit, Rp 5 000.
    pub const DEFAULT_MAX_SAVED_PER_SALE: Money = Money::from_minor(5_000);
    /// The default balance limit, Rp 100 000.
    pub const DEFAULT_MAX_BALANCE: Money = Money::from_minor(100_000);

    /// Checks that neither limit is negative.
    ///
    /// # Errors
    /// [`CustomerError::InvalidField`] naming the negative limit.
    pub fn validate(self) -> Result<Self, CustomerError> {
        for (field, value) in [
            ("max_saved_per_sale", self.max_saved_per_sale),
            ("max_balance", self.max_balance),
        ] {
            if value.is_negative() {
                return Err(CustomerError::InvalidField {
                    field,
                    reason: "must not be negative",
                });
            }
        }
        Ok(self)
    }

    /// Whether saving change is on (`max_saved_per_sale > 0`).
    #[must_use]
    pub const fn saving_enabled(self) -> bool {
        self.max_saved_per_sale.minor() > 0
    }
}

impl Default for BalanceSettings {
    fn default() -> Self {
        Self {
            max_saved_per_sale: Self::DEFAULT_MAX_SAVED_PER_SALE,
            max_balance: Self::DEFAULT_MAX_BALANCE,
        }
    }
}

/// The most change that can be saved on this sale: the smallest of the change due, the per-sale
/// limit and the room left under the balance limit (`max_balance − balance`), never below 0.
/// A negative balance leaves more room, so saved change fills it.
#[must_use]
pub fn max_savable(settings: BalanceSettings, balance: Money, change_due: Money) -> Money {
    let room = settings.max_balance.saturating_sub(balance);
    change_due
        .min(settings.max_saved_per_sale)
        .min(room)
        .max(Money::ZERO)
}

/// Checks saving `amount` of the change due to the balance and returns the `change_saved` entry.
///
/// # Errors
/// - [`CustomerError::InvalidField`] (field `amount`) when `amount` is 0 or less;
/// - [`CustomerError::BalanceLimit`] with [`max_savable`] when `amount` is more than the change
///   due, the per-sale limit or the room under the balance limit (always, with `max` 0, when
///   saving change is off).
pub fn save_change(
    settings: BalanceSettings,
    balance: Money,
    change_due: Money,
    amount: Money,
) -> Result<BalanceEntry, CustomerError> {
    if amount.minor() <= 0 {
        return Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must be greater than zero",
        });
    }
    let max = max_savable(settings, balance, change_due);
    if amount > max {
        return Err(CustomerError::BalanceLimit { max });
    }
    BalanceEntry::new(BalanceEntryKind::ChangeSaved, amount)
}

/// Checks paying `amount` with the balance and returns the `payment` entry (−`amount`).
///
/// # Errors
/// - [`CustomerError::InvalidField`] (field `amount`) when `amount` is 0 or less;
/// - [`CustomerError::InsufficientBalance`] when `amount` is more than the balance.
pub fn pay_with_balance(balance: Money, amount: Money) -> Result<BalanceEntry, CustomerError> {
    if amount.minor() <= 0 {
        return Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must be greater than zero",
        });
    }
    if amount > balance {
        return Err(CustomerError::InsufficientBalance { balance });
    }
    BalanceEntry::new(
        BalanceEntryKind::Payment,
        Money::from_minor(-amount.minor()),
    )
}

/// The entries that reverse a voided or refunded sale's balance entries exactly
/// (`change_saved` → `change_saved_reversed`, `payment` → `payment_reversed`).
///
/// # Errors
/// [`CustomerError::InvalidField`] (field `kind`) when an entry is not a sale entry (e.g. it
/// was reversed already).
pub fn reverse_sale(entries: &[BalanceEntry]) -> Result<Vec<BalanceEntry>, CustomerError> {
    entries
        .iter()
        .map(|e| {
            e.reversal().ok_or(CustomerError::InvalidField {
                field: "kind",
                reason: "only change_saved and payment entries are reversed",
            })
        })
        .collect()
}

/// The two entries of a merge that move B's whole balance to A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BalanceMove {
    /// `merge_out` on B: −balance.
    pub out_of_b: BalanceEntry,
    /// `merge_in` on A: +balance.
    pub into_a: BalanceEntry,
}

/// The `merge_out`/`merge_in` pair that moves B's balance to A, or `None` when B's balance is 0.
/// A's resulting balance may exceed `max_balance`; that is allowed.
///
/// # Errors
/// [`CustomerError::AmountOverflow`] when the balance is `i64::MIN`.
pub fn merge_balance(b_balance: Money) -> Result<Option<BalanceMove>, CustomerError> {
    if b_balance == Money::ZERO {
        return Ok(None);
    }
    let out = Money::ZERO
        .checked_sub(b_balance)
        .ok_or(CustomerError::AmountOverflow)?;
    Ok(Some(BalanceMove {
        out_of_b: BalanceEntry::new(BalanceEntryKind::MergeOut, out)?,
        into_a: BalanceEntry::new(BalanceEntryKind::MergeIn, b_balance)?,
    }))
}

/// Checks paying `amount` of the balance out in cash and returns the `payout` entry
/// (−`amount`). A positive balance is paid out with `0 < amount ≤ balance`; a negative balance
/// is written off with `balance ≤ amount < 0` (Owner only, checked by the server). The payout
/// never moves the balance past 0.
///
/// # Errors
/// - [`CustomerError::InvalidField`] (field `amount`) when `amount` is 0;
/// - [`CustomerError::InsufficientBalance`] when `amount` has the other sign than the balance
///   or is larger than it.
pub fn payout(balance: Money, amount: Money) -> Result<BalanceEntry, CustomerError> {
    let (a, b) = (amount.minor(), balance.minor());
    if a == 0 {
        return Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must not be zero",
        });
    }
    let within = if a > 0 { a <= b } else { b <= a };
    if !within {
        return Err(CustomerError::InsufficientBalance { balance });
    }
    // `a == b == i64::MIN` passes the check above but cannot be negated.
    let entry = a.checked_neg().ok_or(CustomerError::AmountOverflow)?;
    BalanceEntry::new(BalanceEntryKind::Payout, Money::from_minor(entry))
}

/// The erase precondition: the balance must be 0.
///
/// # Errors
/// [`CustomerError::BalanceNotZero`] otherwise.
pub fn check_erase(balance: Money) -> Result<(), CustomerError> {
    if balance == Money::ZERO {
        Ok(())
    } else {
        Err(CustomerError::BalanceNotZero { balance })
    }
}

/// The cash effect of a cash sale whose change is partly saved to the balance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SavedChangeCash {
    /// Cash received − amount due.
    pub change_due: Money,
    /// Change handed back: `change_due − saved`.
    pub change_given: Money,
    /// The change saved to the balance.
    pub saved: Money,
    /// The change of the drawer's expected cash: received − change given.
    pub drawer: Money,
}

/// The expected drawer effect of a cash sale with saved change (spec 2.5 §11): the saved amount
/// stays in the drawer, so expected cash grows by `received − change_given`. E.g. cash 50 000
/// for a 46 300 sale, give 3 000 and save 700 → drawer +47 000, balance +700.
///
/// The limits are checked by [`save_change`]; this only needs `0 ≤ saved ≤ change due`.
///
/// # Errors
/// - [`CustomerError::InvalidField`] (field `received`) when the cash received is less than the
///   amount due, or (field `amount`) when `saved` is negative;
/// - [`CustomerError::BalanceLimit`] when `saved` is more than the change due;
/// - [`CustomerError::AmountOverflow`] on 64-bit overflow.
pub fn saved_change_cash(
    received: Money,
    due: Money,
    saved: Money,
) -> Result<SavedChangeCash, CustomerError> {
    let change_due = received
        .checked_sub(due)
        .ok_or(CustomerError::AmountOverflow)?;
    if change_due.is_negative() {
        return Err(CustomerError::InvalidField {
            field: "received",
            reason: "less than the amount due",
        });
    }
    if saved.is_negative() {
        return Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must not be negative",
        });
    }
    if saved > change_due {
        return Err(CustomerError::BalanceLimit { max: change_due });
    }
    // 0 ≤ saved ≤ change_due ≤ received − due: the subtractions below cannot overflow.
    let change_given = change_due.saturating_sub(saved);
    Ok(SavedChangeCash {
        change_due,
        change_given,
        saved,
        drawer: received.saturating_sub(change_given),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn entry(kind: BalanceEntryKind, v: i64) -> BalanceEntry {
        BalanceEntry::new(kind, m(v)).unwrap()
    }

    use BalanceEntryKind as K;

    #[test]
    fn kinds() {
        let cases = [
            (
                K::ChangeSaved,
                "change_saved",
                1,
                true,
                Some(K::ChangeSavedReversed),
            ),
            (K::Payment, "payment", -1, true, Some(K::PaymentReversed)),
            (
                K::ChangeSavedReversed,
                "change_saved_reversed",
                -1,
                true,
                None,
            ),
            (K::PaymentReversed, "payment_reversed", 1, true, None),
            (K::Payout, "payout", -1, false, None),
            (K::MergeOut, "merge_out", -1, false, None),
            (K::MergeIn, "merge_in", 1, false, None),
        ];
        assert_eq!(cases.len(), K::ALL.len());
        for (kind, name, sign, strict, reversal) in cases {
            assert_eq!(kind.as_str(), name);
            assert_eq!(kind.to_string(), name);
            assert_eq!(name.parse::<K>(), Ok(kind));
            assert_eq!(kind.sign(), sign);
            assert_eq!(kind.strict_sign(), strict);
            assert_eq!(kind.reversal(), reversal);
        }
        assert_eq!("refund".parse::<K>().unwrap_err().field(), Some("kind"));
    }

    #[test]
    fn entry_signs() {
        let zero = Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must not be zero",
        });
        let wrong = Err(CustomerError::InvalidField {
            field: "amount",
            reason: "has the wrong sign for the entry kind",
        });
        let cases = [
            (K::ChangeSaved, 700, Ok(())),
            (K::ChangeSaved, -700, wrong.clone()),
            (K::ChangeSaved, 0, zero.clone()),
            (K::Payment, -500, Ok(())),
            (K::Payment, 500, wrong.clone()),
            (K::ChangeSavedReversed, -700, Ok(())),
            (K::ChangeSavedReversed, 700, wrong.clone()),
            (K::PaymentReversed, 500, Ok(())),
            (K::PaymentReversed, -500, wrong),
            (K::Payout, -2_200, Ok(())),
            (K::Payout, 300, Ok(())),
            (K::Payout, 0, zero.clone()),
            (K::MergeOut, -1, Ok(())),
            (K::MergeOut, 1, Ok(())),
            (K::MergeIn, 1, Ok(())),
            (K::MergeIn, 0, zero),
            (K::Payment, i64::MIN, Err(CustomerError::AmountOverflow)),
        ];
        for (kind, v, want) in cases {
            let got = BalanceEntry::new(kind, m(v));
            assert_eq!(got.clone().map(|_| ()), want, "{kind} {v}");
            if let Ok(e) = got {
                assert_eq!((e.kind(), e.amount()), (kind, m(v)));
            }
        }
    }

    /// Spec 2.5 §15 case 7.
    #[test]
    fn spec_case_7() {
        // Entries +700, +2 000, −500 → 2 200.
        let entries = [
            entry(K::ChangeSaved, 700),
            entry(K::ChangeSaved, 2_000),
            entry(K::Payment, -500),
        ];
        let balance = ledger_balance(&entries).unwrap();
        assert_eq!(balance, m(2_200));

        let settings = BalanceSettings::default();
        // Save 6 000 with a 5 000 per-sale limit → balance_limit (max 5 000).
        assert_eq!(
            save_change(settings, balance, m(10_000), m(6_000)),
            Err(CustomerError::BalanceLimit { max: m(5_000) })
        );
        // Save 3 000 with balance 98 000 and max 100 000 → balance_limit (max 2 000).
        assert_eq!(
            save_change(settings, m(98_000), m(10_000), m(3_000)),
            Err(CustomerError::BalanceLimit { max: m(2_000) })
        );
        // Pay 2 300 with 2 200 → insufficient_balance.
        assert_eq!(
            pay_with_balance(balance, m(2_300)),
            Err(CustomerError::InsufficientBalance { balance })
        );
        // Void the paying sale → +500 back.
        let reversed = reverse_sale(&entries[2..]).unwrap();
        assert_eq!(reversed, vec![entry(K::PaymentReversed, 500)]);
        let all: Vec<_> = entries.iter().chain(&reversed).copied().collect();
        assert_eq!(ledger_balance(&all), Ok(m(2_700)));
    }

    #[test]
    fn ledger_overflow_and_empty() {
        assert_eq!(ledger_balance(&[]), Ok(Money::ZERO));
        let big = [entry(K::MergeIn, i64::MAX), entry(K::ChangeSaved, 1)];
        assert_eq!(ledger_balance(&big), Err(CustomerError::AmountOverflow));
    }

    #[test]
    fn settings() {
        let d = BalanceSettings::default();
        assert_eq!(
            (d.max_saved_per_sale, d.max_balance),
            (m(5_000), m(100_000))
        );
        assert!(d.saving_enabled());
        assert_eq!(d.validate(), Ok(d));
        let off = BalanceSettings {
            max_saved_per_sale: Money::ZERO,
            ..d
        };
        assert!(!off.saving_enabled());
        assert_eq!(off.validate(), Ok(off));
        let cases = [
            (m(-1), m(100), "max_saved_per_sale"),
            (m(1), m(-100), "max_balance"),
        ];
        for (per_sale, max, field) in cases {
            let s = BalanceSettings {
                max_saved_per_sale: per_sale,
                max_balance: max,
            };
            assert_eq!(
                s.validate(),
                Err(CustomerError::InvalidField {
                    field,
                    reason: "must not be negative"
                })
            );
        }
    }

    #[test]
    fn saving_change() {
        let d = BalanceSettings::default();
        let off = BalanceSettings {
            max_saved_per_sale: Money::ZERO,
            ..d
        };
        let not_positive = Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must be greater than zero",
        });
        let limit = |max| Err(CustomerError::BalanceLimit { max: m(max) });
        // (settings, balance, change due, amount, result)
        let cases = [
            (d, 0, 3_700, 700, Ok(())),
            (d, 0, 3_700, 3_700, Ok(())),
            (d, 0, 3_700, 3_701, limit(3_700)),
            (d, 0, 10_000, 5_000, Ok(())),
            (d, 0, 10_000, 5_001, limit(5_000)),
            (d, 99_000, 3_700, 1_000, Ok(())),
            (d, 99_000, 3_700, 1_001, limit(1_000)),
            (d, 100_000, 3_700, 1, limit(0)),
            (d, 150_000, 3_700, 1, limit(0)),
            (d, -2_000, 10_000, 5_000, Ok(())),
            (d, 0, 0, 1, limit(0)),
            (d, 0, -5, 1, limit(0)),
            (off, 0, 3_700, 1, limit(0)),
            (d, 0, 3_700, 0, not_positive.clone()),
            (d, 0, 3_700, -1, not_positive),
        ];
        for (s, balance, due, amount, want) in cases {
            let got = save_change(s, m(balance), m(due), m(amount));
            assert_eq!(got.clone().map(|_| ()), want, "{balance} {due} {amount}");
            if let Ok(e) = got {
                assert_eq!(e, entry(K::ChangeSaved, amount));
            }
        }
    }

    #[test]
    fn paying() {
        let not_positive = Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must be greater than zero",
        });
        let short = |b| Err(CustomerError::InsufficientBalance { balance: m(b) });
        let cases = [
            (2_200, 2_200, Ok(entry(K::Payment, -2_200))),
            (2_200, 500, Ok(entry(K::Payment, -500))),
            (2_200, 2_300, short(2_200)),
            (-100, 1, short(-100)),
            (2_200, 0, not_positive.clone()),
            (2_200, -1, not_positive),
        ];
        for (balance, amount, want) in cases {
            assert_eq!(
                pay_with_balance(m(balance), m(amount)),
                want,
                "{balance} {amount}"
            );
        }
    }

    #[test]
    fn reversals() {
        let entries = [entry(K::ChangeSaved, 700), entry(K::Payment, -500)];
        assert_eq!(
            reverse_sale(&entries),
            Ok(vec![
                entry(K::ChangeSavedReversed, -700),
                entry(K::PaymentReversed, 500)
            ])
        );
        assert_eq!(reverse_sale(&[]), Ok(vec![]));
        for kind in [
            K::ChangeSavedReversed,
            K::PaymentReversed,
            K::MergeIn,
            K::Payout,
        ] {
            let e = entry(kind, kind.sign());
            assert_eq!(e.reversal(), None);
            assert_eq!(
                reverse_sale(&[e]).unwrap_err().field(),
                Some("kind"),
                "{kind}"
            );
        }
    }

    #[test]
    fn merge_moves() {
        assert_eq!(merge_balance(Money::ZERO), Ok(None));
        assert_eq!(
            merge_balance(m(2_200)),
            Ok(Some(BalanceMove {
                out_of_b: entry(K::MergeOut, -2_200),
                into_a: entry(K::MergeIn, 2_200),
            }))
        );
        assert_eq!(
            merge_balance(m(-300)),
            Ok(Some(BalanceMove {
                out_of_b: entry(K::MergeOut, 300),
                into_a: entry(K::MergeIn, -300),
            }))
        );
        assert_eq!(
            merge_balance(m(i64::MIN)),
            Err(CustomerError::AmountOverflow)
        );
    }

    #[test]
    fn payouts() {
        let zero = Err(CustomerError::InvalidField {
            field: "amount",
            reason: "must not be zero",
        });
        let short = |b| Err(CustomerError::InsufficientBalance { balance: m(b) });
        let cases = [
            (2_200, 2_200, Ok(entry(K::Payout, -2_200))),
            (2_200, 1_000, Ok(entry(K::Payout, -1_000))),
            (2_200, 2_201, short(2_200)),
            (2_200, -1, short(2_200)),
            (-300, -300, Ok(entry(K::Payout, 300))),
            (-300, -100, Ok(entry(K::Payout, 100))),
            (-300, -301, short(-300)),
            (-300, 1, short(-300)),
            (0, 1, short(0)),
            (0, -1, short(0)),
            (2_200, 0, zero),
            (i64::MIN, i64::MIN, Err(CustomerError::AmountOverflow)),
        ];
        for (balance, amount, want) in cases {
            assert_eq!(payout(m(balance), m(amount)), want, "{balance} {amount}");
        }
    }

    #[test]
    fn erase_needs_zero_balance() {
        assert_eq!(check_erase(Money::ZERO), Ok(()));
        for b in [2_200, -1] {
            let err = check_erase(m(b)).unwrap_err();
            assert_eq!(err, CustomerError::BalanceNotZero { balance: m(b) });
            assert_eq!(err.code(), "balance_not_zero");
        }
    }

    /// Spec 2.5 §15 case 8.
    #[test]
    fn saved_change_in_the_drawer() {
        let ok = |due, given, saved, drawer| {
            Ok(SavedChangeCash {
                change_due: m(due),
                change_given: m(given),
                saved: m(saved),
                drawer: m(drawer),
            })
        };
        // (received, due, saved, result)
        let cases = [
            (50_000, 46_300, 700, ok(3_700, 3_000, 700, 47_000)),
            (50_000, 46_300, 0, ok(3_700, 3_700, 0, 46_300)),
            (50_000, 46_300, 3_700, ok(3_700, 0, 3_700, 50_000)),
            (46_300, 46_300, 0, ok(0, 0, 0, 46_300)),
            (
                50_000,
                46_300,
                3_701,
                Err(CustomerError::BalanceLimit { max: m(3_700) }),
            ),
            (
                40_000,
                46_300,
                0,
                Err(CustomerError::InvalidField {
                    field: "received",
                    reason: "less than the amount due",
                }),
            ),
            (
                50_000,
                46_300,
                -1,
                Err(CustomerError::InvalidField {
                    field: "amount",
                    reason: "must not be negative",
                }),
            ),
            (i64::MIN, 1, 0, Err(CustomerError::AmountOverflow)),
        ];
        for (received, due, saved, want) in cases {
            assert_eq!(
                saved_change_cash(m(received), m(due), m(saved)),
                want,
                "{received} {due} {saved}"
            );
        }
        // The balance side of the same sale: +700.
        assert_eq!(
            save_change(BalanceSettings::default(), Money::ZERO, m(3_700), m(700)),
            Ok(entry(K::ChangeSaved, 700))
        );
    }
}
