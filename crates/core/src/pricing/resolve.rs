//! Price resolution (spec 2.3 §4) and the special lines (§5).

use std::collections::BTreeMap;

use chrono::NaiveDate;

use super::list::{PriceList, PriceListKind, TierPrice};
use super::rounding::{line_amount, to_money};
use super::{PriceListId, PricingError};
use crate::catalog::{Qty, qty_from_price};
use crate::ids::{UnitId, VariantId};
use crate::money::Money;

/// A customer's membership tier as far as pricing needs it (spec 2.5 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TierMembership {
    /// The tier itself is archived.
    pub tier_archived: bool,
    /// The customer's `tier_valid_until`, or open.
    pub valid_until: Option<NaiveDate>,
}

impl TierMembership {
    /// Whether the tier applies on `date`: not archived, and no end date or an end date on or
    /// after `date` (spec 2.5 §4; the rule lives in [`crate::customer::tier_applies`]).
    #[must_use]
    pub fn applies_on(self, date: NaiveDate) -> bool {
        crate::customer::tier_applies(self.tier_archived, self.valid_until, date)
    }
}

/// The customer's lists for a sale (any may be absent).
#[derive(Debug, Clone, Copy, Default)]
pub struct CustomerLists<'a> {
    /// The customer's contract list.
    pub contract: Option<&'a PriceList>,
    /// The tier's list and the customer's membership of that tier.
    pub tier: Option<(&'a PriceList, TierMembership)>,
    /// The customer group's list.
    pub group: Option<&'a PriceList>,
}

/// The ordered lists a sale is priced from (spec 2.3 §4.1), and the sale date.
#[derive(Debug, Clone)]
pub struct PriceChain<'a> {
    lists: Vec<&'a PriceList>,
    date: NaiveDate,
}

impl<'a> PriceChain<'a> {
    /// Builds the chain contract → tier → group → branch → default for a sale on `date` (the
    /// branch-local day), skipping absent, inactive and archived lists, and the tier list when
    /// the membership does not apply on `date`.
    #[must_use]
    pub fn build(
        customer: Option<&CustomerLists<'a>>,
        branch: Option<&'a PriceList>,
        default: &'a PriceList,
        date: NaiveDate,
    ) -> Self {
        let mut lists = Vec::with_capacity(5);
        if let Some(c) = customer {
            lists.extend(c.contract);
            lists.extend(
                c.tier
                    .filter(|(_, membership)| membership.applies_on(date))
                    .map(|(list, _)| list),
            );
            lists.extend(c.group);
        }
        lists.extend(branch);
        lists.push(default);
        lists.retain(|l| l.is_live());
        Self { lists, date }
    }

    /// The store chain without customer lists (branch → default), used by price-embedded scale
    /// labels and the lookup price.
    #[must_use]
    pub fn store(branch: Option<&'a PriceList>, default: &'a PriceList, date: NaiveDate) -> Self {
        Self::build(None, branch, default, date)
    }

    /// The lists in order, first wins.
    #[must_use]
    pub fn lists(&self) -> &[&'a PriceList] {
        &self.lists
    }

    /// The sale date.
    #[must_use]
    pub const fn date(&self) -> NaiveDate {
        self.date
    }

    /// The price of (variant, unit) from the chain (§4.3), with `counted` the variant's counted
    /// quantity in base units and `factor` the unit's factor (to convert tier `min_qty`).
    /// Evaluated from the last list to the first, so each list sees the price after it.
    /// The flag says whether any list had an entry with a matching tier (for §4.4).
    fn price_of(
        &self,
        variant: VariantId,
        unit: UnitId,
        factor: Qty,
        counted: Qty,
    ) -> Result<(Option<ResolvedPrice>, bool), PricingError> {
        let mut next: Option<ResolvedPrice> = None;
        let mut any_tier = false;
        for list in self.lists.iter().rev() {
            let tier = list
                .entries
                .iter()
                .filter(|e| e.variant == variant && e.unit == unit && e.is_valid_on(self.date))
                .flat_map(|e| &e.tiers)
                .filter(|t| t.min_qty.times_factor(factor).is_some_and(|q| q <= counted))
                .max_by_key(|t| t.min_qty);
            let source = |tier_min_qty| PriceSource {
                list: list.id,
                kind: list.kind,
                tier_min_qty,
                unit_fallback: false,
            };
            let rule = match tier {
                Some(t) => {
                    any_tier = true;
                    match t.price {
                        TierPrice::Fixed(price) => {
                            next = Some(ResolvedPrice {
                                unit_price: price,
                                source: source(Some(t.min_qty)),
                            });
                            continue;
                        }
                        TierPrice::Rule(rule) => Some((rule, Some(t.min_qty))),
                    }
                }
                None => list.rule.map(|r| (r, None)),
            };
            if let Some((rule, tier_min_qty)) = rule
                && let Some(base) = next
                && let Some(price) = rule.apply(base.unit_price)?
            {
                next = Some(ResolvedPrice {
                    unit_price: price,
                    source: source(tier_min_qty),
                });
            }
            // Otherwise: no rule, no base, or a result ≤ 0 → fall through.
        }
        Ok((next, any_tier))
    }
}

/// A variant sold in a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SaleItem {
    /// The variant.
    pub variant: VariantId,
    /// The unit it is sold in.
    pub unit: UnitId,
    /// The product's base unit.
    pub base_unit: UnitId,
    /// The unit's factor to the base unit (1 for the base unit).
    pub factor: Qty,
}

/// Where a resolved price came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PriceSource {
    /// The list that set the price (the first one in the chain that priced it).
    pub list: PriceListId,
    /// That list's kind.
    pub kind: PriceListKind,
    /// The `min_qty` of the entry tier used, or `None` when the list rule priced it.
    pub tier_min_qty: Option<Qty>,
    /// The price is the base-unit price × factor (§4.4); the tier is then the base unit's.
    pub unit_fallback: bool,
}

/// A unit price from the lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedPrice {
    /// The unit price in the line's unit.
    pub unit_price: Money,
    /// Where it came from.
    pub source: PriceSource,
}

/// The unit price of `item` with the variant's counted quantity `counted` (base units), with
/// unit fallback (§4.3–4.4). `Ok(None)` is no price.
///
/// For the lookup price of one unit, pass `counted = item.factor`.
///
/// # Errors
/// [`PricingError::Overflow`] when an amount does not fit.
pub fn resolve_unit_price(
    chain: &PriceChain<'_>,
    item: &SaleItem,
    counted: Qty,
) -> Result<Option<ResolvedPrice>, PricingError> {
    let (price, any_tier) = chain.price_of(item.variant, item.unit, item.factor, counted)?;
    if any_tier || item.unit == item.base_unit {
        return Ok(price);
    }
    let (base, _) = chain.price_of(item.variant, item.base_unit, Qty::ONE, counted)?;
    base.map(|b| {
        let product = i128::from(b.unit_price.minor()) * i128::from(item.factor.thousandths());
        Ok(ResolvedPrice {
            unit_price: to_money(super::div_round_half_up(product, 1_000))?,
            source: PriceSource {
                unit_fallback: true,
                ..b.source
            },
        })
    })
    .transpose()
}

/// How a cart line is priced (spec 2.3 §5–§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineMode {
    /// From the lists.
    Listed,
    /// An `open_price` product: the typed unit price, if typed yet.
    OpenPrice(Option<Money>),
    /// A manual override: the typed unit price, kept when the cart is re-priced.
    Overridden(Money),
}

/// A cart line to price.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CartLine {
    /// The variant and unit.
    pub item: SaleItem,
    /// The quantity in the line's unit (3 dp).
    pub qty: Qty,
    /// How the line is priced.
    pub mode: LineMode,
}

/// A priced cart line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PricedLine {
    /// The price from the lists, if any: the charged price of a listed line, the suggestion on
    /// an open-price line, the original price of an overridden line.
    pub resolved: Option<ResolvedPrice>,
    /// The unit price charged.
    pub unit_price: Money,
    /// `unit_price × qty`, rounded half up to Rp 1.
    pub amount: Money,
}

/// The counted quantity of each variant: the sum of all its lines in base units (§4.2).
/// A variant whose sum overflows maps to `None`.
#[must_use]
pub fn counted_quantities(lines: &[CartLine]) -> BTreeMap<VariantId, Option<Qty>> {
    let mut counted: BTreeMap<VariantId, Option<Qty>> = BTreeMap::new();
    for line in lines {
        let sum = counted.entry(line.item.variant).or_insert(Some(Qty::ZERO));
        *sum = sum.and_then(|s| {
            line.qty
                .times_factor(line.item.factor)
                .and_then(|q| s.checked_add(q))
        });
    }
    counted
}

/// Prices every line of a cart (spec 2.3 §4–§6). Tiers count each variant over all its lines,
/// so the whole cart is re-priced together. One result per line, in order.
///
/// Per-line errors: [`PricingError::NoPrice`] for a listed line without a price or an open-price
/// line without a typed price (with the resolved price as `suggestion`);
/// [`PricingError::InvalidField`] (`price`) for a negative typed price;
/// [`PricingError::Overflow`].
#[must_use]
pub fn resolve_cart(
    chain: &PriceChain<'_>,
    lines: &[CartLine],
) -> Vec<Result<PricedLine, PricingError>> {
    let counted = counted_quantities(lines);
    lines
        .iter()
        .map(|line| {
            let q = counted
                .get(&line.item.variant)
                .copied()
                .flatten()
                .ok_or(PricingError::Overflow)?;
            let resolved = resolve_unit_price(chain, &line.item, q)?;
            let unit_price = match line.mode {
                LineMode::Listed => resolved.map(|r| r.unit_price),
                LineMode::OpenPrice(typed) => typed,
                LineMode::Overridden(typed) => Some(typed),
            }
            .ok_or(PricingError::NoPrice {
                suggestion: match line.mode {
                    LineMode::OpenPrice(_) => resolved.map(|r| r.unit_price),
                    _ => None,
                },
            })?;
            if unit_price.is_negative() {
                return Err(PricingError::InvalidField {
                    field: "price",
                    reason: "must not be negative",
                });
            }
            Ok(PricedLine {
                resolved,
                unit_price,
                amount: line_amount(unit_price, line.qty)?,
            })
        })
        .collect()
}

/// A price-embedded scale label line (spec 2.3 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LabelLine {
    /// The printed amount: the final line amount.
    pub amount: Money,
    /// The store unit price the quantity is derived from, if any.
    pub unit_price: Option<Money>,
    /// `amount ÷ unit_price`, rounded to 3 dp; empty without a (positive) store price.
    pub qty: Option<Qty>,
}

/// Prices a price-embedded scale label: the printed `amount` is final; the quantity comes from
/// the store chain **without** customer lists (branch → default), for one unit, so customer
/// prices and tiers never change it (spec 2.3 §5, 2.2 §6).
///
/// # Errors
/// [`PricingError::Overflow`] when an amount or the quantity does not fit.
pub fn price_label(
    branch: Option<&PriceList>,
    default: &PriceList,
    date: NaiveDate,
    item: &SaleItem,
    amount: Money,
) -> Result<LabelLine, PricingError> {
    let chain = PriceChain::store(branch, default, date);
    let unit_price = resolve_unit_price(&chain, item, item.factor)?.map(|r| r.unit_price);
    let qty = match unit_price {
        Some(price) if price.minor() > 0 => {
            Some(qty_from_price(amount, price).map_err(|_| PricingError::Overflow)?)
        }
        _ => None,
    };
    Ok(LabelLine {
        amount,
        unit_price,
        qty,
    })
}
