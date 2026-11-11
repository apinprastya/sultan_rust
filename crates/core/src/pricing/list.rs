//! Price lists, entries, tiers and rules, and their validation (spec 2.3 §1–§3).

use std::fmt;
use std::str::FromStr;

use chrono::NaiveDate;

use super::rounding::{div_round_half_up, to_money};
use super::{PriceEntryId, PriceListId, PricingError};
use crate::catalog::Qty;
use crate::ids::{UnitId, VariantId};
use crate::money::Money;

/// The most tiers an entry may have (spec 2.3 §3).
pub const MAX_TIERS: usize = 10;

/// Basis points in 100%.
const FULL_BP: i32 = 10_000;

/// What a price list applies to (spec 2.3 §2). The target itself (branch, group, tier, customer)
/// is stored with the list; resolution only needs to know which slot of the chain a list fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PriceListKind {
    /// The one seeded list every chain ends with.
    Default,
    /// A branch's prices.
    Branch,
    /// A customer group's prices.
    Group,
    /// A membership tier's prices.
    Tier,
    /// One customer's contract prices.
    Customer,
}

impl PriceListKind {
    /// All kinds, in chain order from last to first.
    pub const ALL: [Self; 5] = [
        Self::Default,
        Self::Branch,
        Self::Group,
        Self::Tier,
        Self::Customer,
    ];

    /// The stable key (`default`, `branch`, `group`, `tier`, `customer`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Branch => "branch",
            Self::Group => "group",
            Self::Tier => "tier",
            Self::Customer => "customer",
        }
    }
}

impl fmt::Display for PriceListKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for PriceListKind {
    type Err = PricingError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or(PricingError::InvalidField {
                field: "kind",
                reason: "unknown price list kind",
            })
    }
}

/// A price relative to the next applicable price (spec 2.3 §2, §4.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Rule {
    /// ± basis points: −500 = −5%.
    Pct(i32),
    /// ± an amount in minor units: −500 = Rp 500 off.
    Amount(Money),
}

impl Rule {
    /// Applies the rule to `base`: `base × (10 000 + bp) / 10 000` or `base ± amount`, rounded
    /// half up to Rp 1. A result ≤ 0 is no price (`Ok(None)`).
    ///
    /// # Errors
    /// [`PricingError::Overflow`] when the result does not fit.
    pub fn apply(self, base: Money) -> Result<Option<Money>, PricingError> {
        let base = i128::from(base.minor());
        let value = match self {
            Self::Pct(bp) => div_round_half_up(
                base * (i128::from(FULL_BP) + i128::from(bp)),
                i128::from(FULL_BP),
            ),
            Self::Amount(amount) => base + i128::from(amount.minor()),
        };
        if value <= 0 {
            return Ok(None);
        }
        to_money(value).map(Some)
    }

    /// A % rule of −100% or less can never give a price.
    fn validate(self) -> Result<(), PricingError> {
        match self {
            Self::Pct(bp) if bp <= -FULL_BP => Err(PricingError::InvalidRule {
                reason: "a % rule must be above -100%",
            }),
            _ => Ok(()),
        }
    }
}

/// What a tier charges (spec 2.3 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TierPrice {
    /// A fixed unit price (≥ 0; 0 for free items).
    Fixed(Money),
    /// A rule on the price from the lists after this one. Overrides the list rule.
    Rule(Rule),
}

/// One quantity tier of an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PriceTier {
    /// The least quantity, in the entry's unit (> 0, 3 dp), from which this tier applies.
    pub min_qty: Qty,
    /// The price.
    pub price: TierPrice,
}

/// The prices of one variant + unit in one list over an optional date range (spec 2.3 §3).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PriceEntry {
    /// The entry.
    pub id: PriceEntryId,
    /// The priced variant.
    pub variant: VariantId,
    /// The unit the price and the tiers' `min_qty` are in.
    pub unit: UnitId,
    /// First local day the entry applies (inclusive), or open.
    pub valid_from: Option<NaiveDate>,
    /// Last local day the entry applies (inclusive), or open.
    pub valid_to: Option<NaiveDate>,
    /// 1–10 tiers with unique `min_qty`, in any order.
    pub tiers: Vec<PriceTier>,
}

impl PriceEntry {
    /// Whether the entry applies on `date` (both ends inclusive).
    #[must_use]
    pub fn is_valid_on(&self, date: NaiveDate) -> bool {
        self.valid_from.is_none_or(|from| from <= date) && self.valid_to.is_none_or(|to| date <= to)
    }

    /// Whether the two entries are for the same variant + unit and share at least one day.
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        let starts_before_end = |a: &Self, b: &Self| match (a.valid_from, b.valid_to) {
            (Some(from), Some(to)) => from <= to,
            _ => true,
        };
        self.variant == other.variant
            && self.unit == other.unit
            && starts_before_end(self, other)
            && starts_before_end(other, self)
    }
}

/// Validates an entry of a list of `kind` on its own, and against the list's `others` entries
/// (spec 2.3 §3). An entry in `others` with the same id (the stored version of an overwrite) is
/// ignored.
///
/// # Errors
/// - [`PricingError::InvalidField`]: no tiers or more than [`MAX_TIERS`] (`tiers`), a `min_qty`
///   ≤ 0 or repeated (`min_qty`), a negative fixed price (`price`), `valid_from` after `valid_to`
///   (`valid_to`).
/// - [`PricingError::InvalidRule`]: a % rule of −100% or less, or any rule in the `default` list.
/// - [`PricingError::PriceOverlap`]: another entry for the same variant + unit shares a day.
pub fn validate_entry(
    kind: PriceListKind,
    entry: &PriceEntry,
    others: &[PriceEntry],
) -> Result<(), PricingError> {
    let invalid = |field, reason| Err(PricingError::InvalidField { field, reason });
    if entry.tiers.is_empty() {
        return invalid("tiers", "an entry needs at least one tier");
    }
    if entry.tiers.len() > MAX_TIERS {
        return invalid("tiers", "an entry has at most 10 tiers");
    }
    for (i, tier) in entry.tiers.iter().enumerate() {
        if !tier.min_qty.is_positive() {
            return invalid("min_qty", "must be greater than zero");
        }
        if entry.tiers[..i].iter().any(|t| t.min_qty == tier.min_qty) {
            return invalid("min_qty", "must be unique within an entry");
        }
        match tier.price {
            TierPrice::Fixed(price) if price.is_negative() => {
                return invalid("price", "must not be negative");
            }
            TierPrice::Fixed(_) => {}
            TierPrice::Rule(_) if kind == PriceListKind::Default => {
                return Err(PricingError::InvalidRule {
                    reason: "the default list cannot have rules",
                });
            }
            TierPrice::Rule(rule) => rule.validate()?,
        }
    }
    if let (Some(from), Some(to)) = (entry.valid_from, entry.valid_to)
        && from > to
    {
        return invalid("valid_to", "must not be before valid_from");
    }
    if let Some(other) = others
        .iter()
        .find(|o| o.id != entry.id && o.overlaps(entry))
    {
        return Err(PricingError::PriceOverlap {
            variant: entry.variant,
            unit: entry.unit,
            other: other.id,
        });
    }
    Ok(())
}

/// A price list with its entries (spec 2.3 §2).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PriceList {
    /// The list.
    pub id: PriceListId,
    /// Its slot in the chain.
    pub kind: PriceListKind,
    /// Switched on. An inactive list is skipped by resolution.
    pub active: bool,
    /// Archived. An archived list is skipped by resolution.
    pub archived: bool,
    /// The list rule for items without an entry, e.g. Gold −5%.
    pub rule: Option<Rule>,
    /// The entries. Resolution only looks at those valid on the sale date.
    pub entries: Vec<PriceEntry>,
}

impl PriceList {
    /// An empty, active list without a rule.
    #[must_use]
    pub fn new(id: PriceListId, kind: PriceListKind) -> Self {
        Self {
            id,
            kind,
            active: true,
            archived: false,
            rule: None,
            entries: Vec::new(),
        }
    }

    /// Whether resolution uses the list: active and not archived.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.active && !self.archived
    }

    /// Validates the list and all its entries (spec 2.3 §2–§3).
    ///
    /// # Errors
    /// - [`PricingError::InvalidRule`]: a rule on the `default` list, or a % rule of −100% or less.
    /// - [`PricingError::InvalidField`] (`active`): the `default` list switched off or archived.
    /// - Any error of [`validate_entry`].
    pub fn validate(&self) -> Result<(), PricingError> {
        if self.kind == PriceListKind::Default {
            if self.rule.is_some() {
                return Err(PricingError::InvalidRule {
                    reason: "the default list cannot have a rule",
                });
            }
            if !self.is_live() {
                return Err(PricingError::InvalidField {
                    field: "active",
                    reason: "the default list cannot be switched off or archived",
                });
            }
        }
        if let Some(rule) = self.rule {
            rule.validate()?;
        }
        for (i, entry) in self.entries.iter().enumerate() {
            validate_entry(self.kind, entry, &self.entries[..i])?;
        }
        Ok(())
    }
}
