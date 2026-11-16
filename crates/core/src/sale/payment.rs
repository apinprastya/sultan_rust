//! Payment (spec 2.6 §5): tender types, split tenders, cash rounding, change and saved change.

use std::fmt;
use std::str::FromStr;

use super::{SaleError, add, sub};
use crate::customer::{self, BalanceEntry, BalanceEntryKind, BalanceSettings};
use crate::ids::TenderTypeId;
use crate::money::Money;
use crate::pricing::CashRounding;

/// The fixed kind of a tender type (D4). [`TenderKind::Exchange`] is not a tender type kind: it
/// is the tender an exchange's refund becomes on the new sale (§8, D12).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TenderKind {
    /// Cash in the drawer: the only tender that may exceed the rest.
    Cash,
    /// Debit or credit card (EDC, recorded by hand, D5).
    Card,
    /// QRIS.
    Qris,
    /// Bank transfer.
    Transfer,
    /// E-wallet.
    Ewallet,
    /// The customer balance (2.5 §11).
    CustomerBalance,
    /// Anything else (vouchers, …).
    Other,
    /// The refund of an exchange's return paying the new sale (system tender, never HQ data).
    Exchange,
}

impl TenderKind {
    /// The kinds a tender type can have (D4), in display order.
    pub const TYPE_KINDS: [Self; 7] = [
        Self::Cash,
        Self::Card,
        Self::Qris,
        Self::Transfer,
        Self::Ewallet,
        Self::CustomerBalance,
        Self::Other,
    ];

    /// The stored key, e.g. `customer_balance`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::Card => "card",
            Self::Qris => "qris",
            Self::Transfer => "transfer",
            Self::Ewallet => "ewallet",
            Self::CustomerBalance => "customer_balance",
            Self::Other => "other",
            Self::Exchange => "exchange",
        }
    }

    /// Whether a refund may be paid with this kind (D11): cash or the customer balance.
    #[must_use]
    pub const fn refundable(self) -> bool {
        matches!(self, Self::Cash | Self::CustomerBalance)
    }
}

impl fmt::Display for TenderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TenderKind {
    type Err = SaleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::TYPE_KINDS
            .into_iter()
            .chain([Self::Exchange])
            .find(|k| k.as_str() == s)
            .ok_or(SaleError::InvalidField {
                field: "kind",
                reason: "unknown tender kind",
            })
    }
}

/// A tender type (HQ master data, §1, D4), e.g. "Debit BCA" of kind card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TenderType {
    /// The id.
    pub id: TenderTypeId,
    /// The name shown and printed, 1–40 characters.
    pub name: String,
    /// The fixed kind.
    pub kind: TenderKind,
    /// A reference (approval code, last 4 digits) must be typed (§5).
    pub needs_reference: bool,
    /// Paying with it opens the drawer.
    pub opens_drawer: bool,
    /// Offered at the register.
    pub active: bool,
}

impl TenderType {
    /// The longest name, in characters.
    pub const MAX_NAME_LEN: usize = 40;

    /// Checks the type: a name of 1–40 characters (trimmed) and a tender type kind.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`name` or `kind`).
    pub fn validate(mut self) -> Result<Self, SaleError> {
        let name = self.name.trim();
        if name.is_empty() || name.chars().count() > Self::MAX_NAME_LEN {
            return Err(SaleError::InvalidField {
                field: "name",
                reason: "must be 1 to 40 characters",
            });
        }
        if self.kind == TenderKind::Exchange {
            return Err(SaleError::InvalidField {
                field: "kind",
                reason: "exchange is not a tender type kind",
            });
        }
        self.name = name.to_owned();
        Ok(self)
    }
}

/// One tender of a sale (§1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SaleTender {
    /// The tender type; `None` for an exchange tender.
    pub tender_type: Option<TenderTypeId>,
    /// Its kind.
    pub kind: TenderKind,
    /// The amount it pays of the sale. For the cash that settles the rest this is the rounded
    /// rest (2.3 §8), so Σ amounts = total + `cash_rounding`.
    pub amount: Money,
    /// Cash only: what the customer handed over.
    pub cash_received: Option<Money>,
    /// Cash only: the change handed back.
    pub change_given: Money,
    /// Cash only: the change saved to the customer balance (2.5 §11); it stays in the drawer.
    pub change_saved: Money,
    /// The reference, as typed (trimmed).
    pub reference: Option<String>,
}

impl SaleTender {
    /// The change due: given + saved.
    #[must_use]
    pub fn change(&self) -> Money {
        self.change_given.saturating_add(self.change_saved)
    }
}

/// The attached customer's balance, for the balance tender and saved change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BalanceContext {
    /// The balance before this sale.
    pub balance: Money,
}

/// What a completed payment posts with the sale.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaidSale {
    /// The tenders, in order.
    pub tenders: Vec<SaleTender>,
    /// The cash rounding (+/−), stored on the sale.
    pub cash_rounding: Money,
    /// The balance ledger entries: one `payment` per balance tender and one `change_saved`.
    pub balance_entries: Vec<BalanceEntry>,
    /// A tender type that opens the drawer was used.
    pub opens_drawer: bool,
}

/// The longest reference, in characters.
const MAX_REFERENCE_LEN: usize = 64;

/// The payment panel of one sale (§5). Tenders are added until the rest is 0. To take a tender
/// back, the register starts a new `Payment` with the remaining ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Payment {
    total: Money,
    rounding: CashRounding,
    cash_rounding: Money,
    tenders: Vec<SaleTender>,
    opens_drawer: bool,
}

impl Payment {
    /// A payment of `total` (the cart total before cash rounding) with the company's cash
    /// rounding setting.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`total`) for a negative total.
    pub fn new(total: Money, rounding: CashRounding) -> Result<Self, SaleError> {
        if total.is_negative() {
            return Err(SaleError::InvalidField {
                field: "total",
                reason: "must not be negative",
            });
        }
        Ok(Self {
            total,
            rounding,
            cash_rounding: Money::ZERO,
            tenders: Vec::new(),
            opens_drawer: false,
        })
    }

    /// The total before cash rounding.
    #[must_use]
    pub const fn total(&self) -> Money {
        self.total
    }

    /// The cash rounding so far (set when cash settles the rest).
    #[must_use]
    pub const fn cash_rounding(&self) -> Money {
        self.cash_rounding
    }

    /// The tenders so far.
    #[must_use]
    pub fn tenders(&self) -> &[SaleTender] {
        &self.tenders
    }

    /// What is still to pay: total + cash rounding − Σ amounts.
    #[must_use]
    pub fn rest(&self) -> Money {
        let paid = self
            .tenders
            .iter()
            .fold(Money::ZERO, |s, t| s.saturating_add(t.amount));
        self.total
            .saturating_add(self.cash_rounding)
            .saturating_sub(paid)
    }

    /// What settles the rest in cash, after cash rounding (shown when the cashier presses F4).
    #[must_use]
    pub fn cash_due(&self) -> Money {
        self.rounding.apply(self.rest(), true).amount
    }

    /// Whether the rest is 0. A zero total needs no tender.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.rest() == Money::ZERO
    }

    /// The change due on the sale (the settling cash tender's).
    #[must_use]
    pub fn change(&self) -> Money {
        self.tenders
            .iter()
            .fold(Money::ZERO, |s, t| s.saturating_add(t.change()))
    }

    /// Adds a tender (§5). For cash, `amount` is the cash received: when it covers the rounded
    /// rest ([`Payment::cash_due`]) it settles the sale, the rounding difference becomes the
    /// sale's `cash_rounding` and the excess is change; less than the exact rest is a partial
    /// cash payment without rounding. Every other kind pays at most the rest. `customer` is the
    /// attached customer's balance, needed for the balance tender.
    ///
    /// # Errors
    /// - [`SaleError::InvalidField`]: `tender_type` inactive or of kind exchange, `amount` not
    ///   above 0 (cash 0 only when the rounded rest is 0) or cash between the exact and the
    ///   rounded rest, `reference` too long, `customer` missing for the balance tender;
    /// - [`SaleError::TenderOverRest`] when nothing is left to pay or a non-cash amount is above
    ///   the rest;
    /// - [`SaleError::TenderReferenceRequired`];
    /// - [`SaleError::Customer`] `insufficient_balance` when the balance (less what earlier
    ///   balance tenders of this sale use) is too low.
    pub fn add(
        &mut self,
        tender: &TenderType,
        amount: Money,
        reference: Option<&str>,
        customer: Option<BalanceContext>,
    ) -> Result<&SaleTender, SaleError> {
        if !tender.active || tender.kind == TenderKind::Exchange {
            return Err(SaleError::InvalidField {
                field: "tender_type",
                reason: "is not an active tender type",
            });
        }
        let rest = self.rest();
        if rest.minor() <= 0 {
            return Err(SaleError::TenderOverRest { rest });
        }
        let reference = reference.map(str::trim).filter(|r| !r.is_empty());
        if tender.needs_reference && reference.is_none() {
            return Err(SaleError::TenderReferenceRequired);
        }
        if reference.is_some_and(|r| r.chars().count() > MAX_REFERENCE_LEN) {
            return Err(SaleError::InvalidField {
                field: "reference",
                reason: "is too long",
            });
        }
        let mut new = SaleTender {
            tender_type: Some(tender.id),
            kind: tender.kind,
            amount,
            cash_received: None,
            change_given: Money::ZERO,
            change_saved: Money::ZERO,
            reference: reference.map(str::to_owned),
        };
        let mut rounding = Money::ZERO;
        if tender.kind == TenderKind::Cash {
            let due = self.rounding.apply(rest, true);
            new.cash_received = Some(amount);
            if amount >= due.amount && !amount.is_negative() {
                new.amount = due.amount;
                new.change_given = sub(amount, due.amount)?;
                rounding = due.difference;
            } else if amount >= rest {
                return Err(SaleError::InvalidField {
                    field: "amount",
                    reason: "is less than the rounded cash total",
                });
            } else {
                positive(amount)?;
            }
        } else {
            positive(amount)?;
            if amount > rest {
                return Err(SaleError::TenderOverRest { rest });
            }
            if tender.kind == TenderKind::CustomerBalance {
                let ctx = customer.ok_or(SaleError::InvalidField {
                    field: "customer",
                    reason: "the balance tender needs an attached customer",
                })?;
                customer::pay_with_balance(self.available_balance(ctx)?, amount)?;
            }
        }
        self.cash_rounding = add(self.cash_rounding, rounding)?;
        self.opens_drawer |= tender.opens_drawer;
        self.tenders.push(new);
        Ok(&self.tenders[self.tenders.len() - 1])
    }

    /// Adds an exchange tender: the refund of the return in the same flow pays (part of) the new
    /// sale (§8, D12). See [`super::net_exchange`] for how much.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`amount`) when not above 0; [`SaleError::TenderOverRest`].
    pub fn add_exchange(&mut self, amount: Money) -> Result<&SaleTender, SaleError> {
        positive(amount)?;
        let rest = self.rest();
        if amount > rest {
            return Err(SaleError::TenderOverRest { rest });
        }
        self.tenders.push(SaleTender {
            tender_type: None,
            kind: TenderKind::Exchange,
            amount,
            cash_received: None,
            change_given: Money::ZERO,
            change_saved: Money::ZERO,
            reference: None,
        });
        Ok(&self.tenders[self.tenders.len() - 1])
    }

    /// The balance left for a balance tender: the balance less what this sale already pays with
    /// it.
    fn available_balance(&self, ctx: BalanceContext) -> Result<Money, SaleError> {
        self.tenders
            .iter()
            .filter(|t| t.kind == TenderKind::CustomerBalance)
            .try_fold(ctx.balance, |b, t| sub(b, t.amount))
    }

    /// Saves `amount` of the change to the attached customer's balance (§5, 2.5 §11), replacing
    /// an earlier choice; `0` gives all the change back again. The rest of the change is handed
    /// over. The limits are those of [`customer::save_change`], with the balance less what this
    /// sale pays with it.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`amount` when there is no cash change or the amount is
    /// negative, `customer` without one); [`SaleError::Customer`] `balance_limit`.
    pub fn save_change(
        &mut self,
        settings: BalanceSettings,
        customer: Option<BalanceContext>,
        amount: Money,
    ) -> Result<Option<BalanceEntry>, SaleError> {
        let ctx = customer.ok_or(SaleError::InvalidField {
            field: "customer",
            reason: "saving change needs an attached customer",
        })?;
        let available = self.available_balance(ctx)?;
        let index = self
            .tenders
            .iter()
            .rposition(|t| t.kind == TenderKind::Cash && t.change() > Money::ZERO)
            .ok_or(SaleError::InvalidField {
                field: "amount",
                reason: "there is no cash change to save",
            })?;
        let tender = &mut self.tenders[index];
        let change = tender.change();
        if amount.is_negative() {
            return Err(SaleError::InvalidField {
                field: "amount",
                reason: "must not be negative",
            });
        }
        let entry = if amount == Money::ZERO {
            None
        } else {
            Some(customer::save_change(settings, available, change, amount)?)
        };
        tender.change_saved = amount;
        tender.change_given = sub(change, amount)?;
        Ok(entry)
    }

    /// The balance ledger entries of the payment: `payment` (−) per balance tender and
    /// `change_saved` (+) for saved change.
    ///
    /// # Errors
    /// [`SaleError::Customer`] for an amount that cannot be an entry (cannot happen for tenders
    /// added through [`Payment::add`]).
    pub fn balance_entries(&self) -> Result<Vec<BalanceEntry>, SaleError> {
        let mut out = Vec::new();
        for t in &self.tenders {
            if t.kind == TenderKind::CustomerBalance {
                out.push(BalanceEntry::new(
                    BalanceEntryKind::Payment,
                    sub(Money::ZERO, t.amount)?,
                )?);
            }
            if t.change_saved > Money::ZERO {
                out.push(BalanceEntry::new(
                    BalanceEntryKind::ChangeSaved,
                    t.change_saved,
                )?);
            }
        }
        Ok(out)
    }

    /// The completed payment, to post with the sale.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`tenders`) while the rest is not 0.
    pub fn finish(&self) -> Result<PaidSale, SaleError> {
        if !self.is_complete() {
            return Err(SaleError::InvalidField {
                field: "tenders",
                reason: "do not settle the total",
            });
        }
        Ok(PaidSale {
            tenders: self.tenders.clone(),
            cash_rounding: self.cash_rounding,
            balance_entries: self.balance_entries()?,
            opens_drawer: self.opens_drawer,
        })
    }
}

fn positive(amount: Money) -> Result<(), SaleError> {
    if amount.minor() > 0 {
        Ok(())
    } else {
        Err(SaleError::InvalidField {
            field: "amount",
            reason: "must be greater than 0",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn tt(kind: TenderKind, needs_reference: bool) -> TenderType {
        TenderType {
            id: TenderTypeId::generate(),
            name: kind.as_str().to_owned(),
            kind,
            needs_reference,
            opens_drawer: kind == TenderKind::Cash,
            active: true,
        }
    }

    fn cash() -> TenderType {
        tt(TenderKind::Cash, false)
    }

    fn qris() -> TenderType {
        tt(TenderKind::Qris, true)
    }

    fn balance() -> TenderType {
        tt(TenderKind::CustomerBalance, false)
    }

    #[test]
    fn kinds_round_trip() {
        for kind in TenderKind::TYPE_KINDS
            .into_iter()
            .chain([TenderKind::Exchange])
        {
            assert_eq!(kind.to_string().parse::<TenderKind>(), Ok(kind));
        }
        assert_eq!(
            "gold".parse::<TenderKind>().unwrap_err().field(),
            Some("kind")
        );
        let refundable: Vec<_> = TenderKind::TYPE_KINDS
            .into_iter()
            .filter(|k| k.refundable())
            .collect();
        assert_eq!(refundable, [TenderKind::Cash, TenderKind::CustomerBalance]);
    }

    #[test]
    fn tender_type_validation() {
        let ok = TenderType {
            name: "  Debit BCA ".into(),
            ..tt(TenderKind::Card, true)
        };
        assert_eq!(ok.validate().unwrap().name, "Debit BCA");
        for (name, kind, field) in [
            ("", TenderKind::Card, "name"),
            ("   ", TenderKind::Card, "name"),
            (&"x".repeat(41) as &str, TenderKind::Card, "name"),
            ("Exchange", TenderKind::Exchange, "kind"),
        ] {
            let t = TenderType {
                name: name.into(),
                ..tt(kind, false)
            };
            assert_eq!(t.validate().unwrap_err().field(), Some(field), "{name}");
        }
    }

    /// Spec §15 case 5.
    #[test]
    fn split_payment_with_change() {
        let mut p = Payment::new(m(63_500), CashRounding::None).unwrap();
        p.add(&qris(), m(50_000), Some("123456"), None).unwrap();
        assert_eq!(p.rest(), m(13_500));
        let t = p.add(&cash(), m(20_000), None, None).unwrap().clone();
        assert_eq!(
            (t.amount, t.cash_received, t.change_given),
            (m(13_500), Some(m(20_000)), m(6_500))
        );
        assert!(p.is_complete());
        assert_eq!(p.change(), m(6_500));
        let paid = p.finish().unwrap();
        assert_eq!((paid.cash_rounding, paid.opens_drawer), (m(0), true));
        assert!(paid.balance_entries.is_empty());
        // Nothing more can be added.
        assert_eq!(
            p.add(&cash(), m(1), None, None),
            Err(SaleError::TenderOverRest { rest: m(0) })
        );

        // QRIS above the rest.
        let mut p = Payment::new(m(63_500), CashRounding::None).unwrap();
        assert_eq!(
            p.add(&qris(), m(63_501), Some("1"), None)
                .unwrap_err()
                .code(),
            "tender_over_rest"
        );
        // A reference is required when the type says so.
        assert_eq!(
            p.add(&qris(), m(1_000), Some("  "), None),
            Err(SaleError::TenderReferenceRequired)
        );
        assert_eq!(
            p.add(&qris(), m(1_000), Some(&"9".repeat(65)), None)
                .unwrap_err()
                .field(),
            Some("reference")
        );
        // Partial cash first, then card for the rest.
        p.add(&cash(), m(3_500), None, None).unwrap();
        let card = tt(TenderKind::Card, false);
        let t = p.add(&card, m(60_000), Some(" 4321 "), None).unwrap();
        assert_eq!(t.reference.as_deref(), Some("4321"));
        assert!(p.is_complete());
        assert!(p.finish().unwrap().opens_drawer);
    }

    /// Spec §15 case 6.
    #[test]
    fn cash_rounding_only_when_cash_settles() {
        // (setting, total, tenders before cash, cash received, amount, rounding, change)
        #[rustfmt::skip]
        let cases = [
            (CashRounding::Nearest100, 63_450, 0, 70_000, 63_500, 50, 6_500),
            (CashRounding::Nearest100, 63_450, 0, 63_500, 63_500, 50, 0),
            (CashRounding::Down100, 63_450, 0, 63_400, 63_400, -50, 0),
            (CashRounding::Down100, 63_450, 0, 63_450, 63_400, -50, 50),
            (CashRounding::Nearest500, 63_450, 50_000, 15_000, 13_500, 50, 1_500),
            (CashRounding::None, 63_450, 0, 63_450, 63_450, 0, 0),
            // Down rounding of a small rest to 0: cash 0 settles it.
            (CashRounding::Down100, 40, 0, 0, 0, -40, 0),
        ];
        for (setting, total, card_first, received, amount, rounding, change) in cases {
            let mut p = Payment::new(m(total), setting).unwrap();
            if card_first > 0 {
                p.add(&tt(TenderKind::Card, false), m(card_first), None, None)
                    .unwrap();
            }
            let t = p.add(&cash(), m(received), None, None).unwrap().clone();
            assert_eq!(
                (t.amount, t.change_given, p.cash_rounding()),
                (m(amount), m(change), m(rounding)),
                "{setting} {total} {received}"
            );
            assert!(p.is_complete(), "{setting} {total} {received}");
            let paid: i64 = p.tenders().iter().map(|t| t.amount.minor()).sum();
            assert_eq!(paid, total + rounding);
        }

        // Paid by card: the exact amount.
        let mut p = Payment::new(m(63_450), CashRounding::Nearest100).unwrap();
        assert_eq!(p.cash_due(), m(63_500));
        p.add(&tt(TenderKind::Card, false), m(63_450), None, None)
            .unwrap();
        assert_eq!((p.cash_rounding(), p.is_complete()), (m(0), true));

        // Between the exact and the rounded rest: refused; below it: a partial payment.
        let mut p = Payment::new(m(63_450), CashRounding::Nearest100).unwrap();
        assert_eq!(
            p.add(&cash(), m(63_480), None, None).unwrap_err().field(),
            Some("amount")
        );
        p.add(&cash(), m(60_000), None, None).unwrap();
        assert_eq!(
            (p.rest(), p.cash_due(), p.cash_rounding()),
            (m(3_450), m(3_500), m(0))
        );
        assert_eq!(
            p.add(&cash(), m(0), None, None).unwrap_err().field(),
            Some("amount")
        );
        assert_eq!(
            p.add(&cash(), m(-5), None, None).unwrap_err().field(),
            Some("amount")
        );
        assert_eq!(p.finish().unwrap_err().field(), Some("tenders"));
    }

    /// Spec §15 cases 7 and 8.
    #[test]
    fn balance_tender_and_saved_change() {
        let settings = BalanceSettings::default();
        let ctx = Some(BalanceContext { balance: m(1_750) });

        // Case 7: save 700 of 3 700.
        let mut p = Payment::new(m(46_300), CashRounding::None).unwrap();
        assert_eq!(
            p.save_change(settings, ctx, m(700)).unwrap_err().field(),
            Some("amount"),
            "no change yet"
        );
        p.add(&cash(), m(50_000), None, None).unwrap();
        assert_eq!(
            p.save_change(settings, None, m(700)).unwrap_err().field(),
            Some("customer")
        );
        let entry = p.save_change(settings, ctx, m(700)).unwrap().unwrap();
        assert_eq!(
            (entry.kind(), entry.amount()),
            (BalanceEntryKind::ChangeSaved, m(700))
        );
        let t = &p.tenders()[0];
        assert_eq!(
            (t.change_given, t.change_saved, t.change()),
            (m(3_000), m(700), m(3_700))
        );
        // Above the per-sale limit (5 000) or the change: balance_limit with the max.
        assert_eq!(
            p.save_change(settings, ctx, m(3_701)).unwrap_err().code(),
            "balance_limit"
        );
        assert_eq!(
            p.save_change(settings, ctx, m(-1)).unwrap_err().field(),
            Some("amount")
        );
        // 0 gives it back; then save again.
        assert_eq!(p.save_change(settings, ctx, Money::ZERO), Ok(None));
        assert_eq!(p.tenders()[0].change_given, m(3_700));
        p.save_change(settings, ctx, m(700)).unwrap();
        let paid = p.finish().unwrap();
        assert_eq!(paid.balance_entries, [entry]);

        // Case 8: a balance tender above the balance.
        let mut p = Payment::new(m(10_000), CashRounding::None).unwrap();
        assert_eq!(
            p.add(&balance(), m(2_000), None, ctx),
            Err(SaleError::Customer(
                customer::CustomerError::InsufficientBalance { balance: m(1_750) }
            ))
        );
        assert_eq!(
            p.add(&balance(), m(1_000), None, None).unwrap_err().field(),
            Some("customer")
        );
        p.add(&balance(), m(1_000), None, ctx).unwrap();
        // A second balance tender sees what is left.
        assert_eq!(
            p.add(&balance(), m(1_000), None, ctx).unwrap_err().code(),
            "insufficient_balance"
        );
        p.add(&balance(), m(750), None, ctx).unwrap();
        p.add(&cash(), m(10_000), None, None).unwrap();
        // Room under max_balance counts the balance spent on this sale.
        let full = BalanceSettings {
            max_saved_per_sale: m(5_000),
            max_balance: m(1_000),
        };
        assert_eq!(
            p.save_change(full, ctx, m(1_000))
                .unwrap()
                .unwrap()
                .amount(),
            m(1_000)
        );
        let paid = p.finish().unwrap();
        let entries: Vec<_> = paid
            .balance_entries
            .iter()
            .map(|e| (e.kind(), e.amount()))
            .collect();
        assert_eq!(
            entries,
            [
                (BalanceEntryKind::Payment, m(-1_000)),
                (BalanceEntryKind::Payment, m(-750)),
                (BalanceEntryKind::ChangeSaved, m(1_000)),
            ]
        );
    }

    #[test]
    fn zero_total_exchange_and_bad_input() {
        // Zero total: complete with no tender.
        let p = Payment::new(Money::ZERO, CashRounding::Nearest500).unwrap();
        assert!(p.is_complete());
        let paid = p.finish().unwrap();
        assert!(paid.tenders.is_empty());
        assert_eq!(p.total(), Money::ZERO);

        assert_eq!(
            Payment::new(m(-1), CashRounding::None).unwrap_err().field(),
            Some("total")
        );

        let mut p = Payment::new(m(65_000), CashRounding::None).unwrap();
        assert_eq!(p.add_exchange(m(0)).unwrap_err().field(), Some("amount"));
        assert_eq!(
            p.add_exchange(m(70_000)).unwrap_err().code(),
            "tender_over_rest"
        );
        let t = p.add_exchange(m(50_000)).unwrap();
        assert_eq!((t.kind, t.tender_type), (TenderKind::Exchange, None));
        assert_eq!(p.rest(), m(15_000));

        let inactive = TenderType {
            active: false,
            ..cash()
        };
        assert_eq!(
            p.add(&inactive, m(1), None, None).unwrap_err().field(),
            Some("tender_type")
        );
        let exchange = tt(TenderKind::Exchange, false);
        assert!(p.add(&exchange, m(1), None, None).is_err());
        assert_eq!(
            p.add(&qris(), m(0), Some("1"), None).unwrap_err().field(),
            Some("amount")
        );
    }
}
