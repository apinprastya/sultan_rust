//! Pricing types (spec 2.3 §10, phase R3) as the server sends them: price lists, entries with
//! quantity tiers, tax rates, the pricing settings ([`crate::BackOffice`]) and the quote
//! ([`crate::Service::quote`]).
//!
//! Money is minor units (`i64`), quantities are thousandths, dates are local `YYYY-MM-DD`
//! strings (the selling branch's day). A rule is a [`PriceRule`]: exactly one of `pct_bp`
//! (basis points, −500 = −5%) and `amount` (minor units).

use serde::{Deserialize, Serialize};

/// The permission that allows every pricing write (spec 2.3 §9).
pub const PRICE_MANAGE: &str = "price.manage";
/// The seeded `default` price list.
pub const DEFAULT_PRICE_LIST: &str = "00000000-0000-7000-8000-000000000201";

/// A rule relative to the next applicable price.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceRule {
    /// ± basis points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct_bp: Option<i32>,
    /// ± minor units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
}

impl PriceRule {
    /// ± `bp` basis points.
    #[must_use]
    pub const fn pct(bp: i32) -> Self {
        Self {
            pct_bp: Some(bp),
            amount: None,
        }
    }

    /// ± `amount` minor units.
    #[must_use]
    pub const fn amount(amount: i64) -> Self {
        Self {
            pct_bp: None,
            amount: Some(amount),
        }
    }
}

/// A price list (`GET /price-lists`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceList {
    /// ID.
    pub id: String,
    /// `default`, `branch`, `group`, `tier` or `customer`.
    pub kind: String,
    /// The branch, group, tier or customer; `None` for `default`.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Name.
    pub name: String,
    /// Switched on.
    pub active: bool,
    /// The list rule for items without an entry.
    #[serde(default)]
    pub rule: Option<PriceRule>,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
    /// When it was archived (unix ms).
    #[serde(default)]
    pub archived_at: Option<i64>,
}

/// Body of `PUT /price-lists/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceListBody {
    /// The kind; fixed once created.
    pub kind: String,
    /// The target; fixed once created.
    pub target_id: Option<String>,
    /// Name, 1–100 characters.
    pub name: String,
    /// Switched on.
    pub active: bool,
    /// The list rule.
    pub rule: Option<PriceRule>,
    /// `true` archives, `false` unarchives.
    pub archived: bool,
}

impl PriceListBody {
    /// The body that stores `list` as it is.
    #[must_use]
    pub fn of(list: &PriceList) -> Self {
        Self {
            kind: list.kind.clone(),
            target_id: list.target_id.clone(),
            name: list.name.clone(),
            active: list.active,
            rule: list.rule,
            archived: list.archived,
        }
    }
}

/// One quantity tier: from `min_qty_thousandths` on, a fixed `price` or a `rule`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceTier {
    /// The least quantity in the entry's unit, thousandths.
    pub min_qty_thousandths: i64,
    /// Fixed unit price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<i64>,
    /// Rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<PriceRule>,
}

impl PriceTier {
    /// A fixed price from `min_qty_thousandths`.
    #[must_use]
    pub const fn fixed(min_qty_thousandths: i64, price: i64) -> Self {
        Self {
            min_qty_thousandths,
            price: Some(price),
            rule: None,
        }
    }

    /// A rule from `min_qty_thousandths`.
    #[must_use]
    pub const fn rule(min_qty_thousandths: i64, rule: PriceRule) -> Self {
        Self {
            min_qty_thousandths,
            price: None,
            rule: Some(rule),
        }
    }
}

/// A price-list entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceEntry {
    /// ID.
    pub id: String,
    /// The list.
    pub list_id: String,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// First local day, or open.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Last local day, or open.
    #[serde(default)]
    pub valid_to: Option<String>,
    /// 1–10 tiers, by `min_qty`.
    pub tiers: Vec<PriceTier>,
}

/// Body of `PUT /price-lists/{id}/entries/{entry_id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceEntryBody {
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// First local day, or open.
    pub valid_from: Option<String>,
    /// Last local day, or open.
    pub valid_to: Option<String>,
    /// The tiers.
    pub tiers: Vec<PriceTier>,
}

/// One page of `GET /price-lists/{id}/entries`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntryPage {
    /// The entries, by ID.
    pub entries: Vec<PriceEntry>,
    /// The next page's `after_id`, if the page was full.
    #[serde(default)]
    pub next_after_id: Option<String>,
}

/// `GET /products/{id}/prices`: every non-archived list and every entry of the product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProductPrices {
    /// The product.
    pub product_id: String,
    /// The non-archived lists, by kind and name.
    pub lists: Vec<PriceList>,
    /// The product's entries in any list.
    pub entries: Vec<PriceEntry>,
}

/// One rate of a tax class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaxRate {
    /// Basis points (1 100 = 11%).
    pub rate_bp: u16,
    /// First local day, `YYYY-MM-DD`.
    pub valid_from: String,
}

/// The company pricing settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingSettings {
    /// Prices include tax.
    pub tax_inclusive: bool,
    /// The company is tax-registered.
    pub tax_registered: bool,
    /// `none`, `nearest_100`, `down_100`, `nearest_500` or `down_500`.
    pub cash_rounding: String,
}

impl Default for PricingSettings {
    fn default() -> Self {
        Self {
            tax_inclusive: true,
            tax_registered: true,
            cash_rounding: "none".to_owned(),
        }
    }
}

/// A line to quote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteLine {
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// Quantity, thousandths (> 0).
    pub qty_thousandths: i64,
    /// The cashier's price (open price) or a manual override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub typed_price: Option<i64>,
}

impl QuoteLine {
    /// A listed line.
    #[must_use]
    pub fn new(variant_id: &str, unit_id: &str, qty_thousandths: i64) -> Self {
        Self {
            variant_id: variant_id.to_owned(),
            unit_id: unit_id.to_owned(),
            qty_thousandths,
            typed_price: None,
        }
    }
}

/// Body of `POST /pricing/quote`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteRequest {
    /// The customer; `None` = walk-in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    /// The lines.
    pub lines: Vec<QuoteLine>,
}

/// Where a price came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceSource {
    /// The list.
    pub list_id: String,
    /// Its kind.
    pub kind: String,
    /// The tier's `min_qty`, thousandths; `None` = the list rule.
    #[serde(default)]
    pub tier_min_qty_thousandths: Option<i64>,
    /// Base-unit price × factor.
    #[serde(default)]
    pub unit_fallback: bool,
}

/// The tax of a quoted line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineTax {
    /// Tax class.
    pub tax_class_id: String,
    /// Rate, basis points.
    pub rate_bp: u16,
    /// Tax.
    pub tax: i64,
    /// Without tax.
    pub net: i64,
    /// What the customer pays for the line.
    pub total: i64,
}

/// Why a quoted line has no price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineError {
    /// `no_price`, `invalid_reference`, `invalid_field` or `amount_overflow`.
    pub code: String,
    /// English message.
    #[serde(default)]
    pub message: String,
    /// The resolved price of an open-price line.
    #[serde(default)]
    pub suggestion: Option<i64>,
}

/// A quoted line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotedLine {
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// `listed`, `open_price` or `overridden`.
    pub mode: String,
    /// Unit price charged.
    #[serde(default)]
    pub unit_price: Option<i64>,
    /// Line amount.
    #[serde(default)]
    pub amount: Option<i64>,
    /// The price from the lists.
    #[serde(default)]
    pub resolved_price: Option<i64>,
    /// Where it came from.
    #[serde(default)]
    pub source: Option<PriceSource>,
    /// Tax.
    #[serde(default)]
    pub tax: Option<LineTax>,
    /// The error.
    #[serde(default)]
    pub error: Option<LineError>,
}

/// Totals of the priced lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuoteTotals {
    /// Sum of amounts.
    pub amount: i64,
    /// Sum of taxes.
    pub tax: i64,
    /// Sum of net amounts.
    pub net: i64,
    /// What the customer pays (before cash rounding).
    pub total: i64,
}

/// The answer of `POST /pricing/quote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quote {
    /// The selling branch.
    pub branch_id: String,
    /// The sale date, `YYYY-MM-DD`.
    pub date: String,
    /// The customer.
    #[serde(default)]
    pub customer_id: Option<String>,
    /// Prices include tax.
    pub tax_inclusive: bool,
    /// Tax-registered.
    pub tax_registered: bool,
    /// The cash-rounding setting (applied at payment, 2.6).
    pub cash_rounding: String,
    /// The lines, in request order.
    pub lines: Vec<QuotedLine>,
    /// Totals.
    pub totals: QuoteTotals,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_shapes() {
        assert_eq!(
            serde_json::to_value(PriceTier::fixed(1000, 9000)).unwrap(),
            json!({ "min_qty_thousandths": 1000, "price": 9000 })
        );
        assert_eq!(
            serde_json::to_value(PriceTier::rule(1000, PriceRule::pct(-500))).unwrap(),
            json!({ "min_qty_thousandths": 1000, "rule": { "pct_bp": -500 } })
        );
        assert_eq!(
            serde_json::to_value(PriceRule::amount(-5)).unwrap(),
            json!({ "amount": -5 })
        );
        let list: PriceList = serde_json::from_value(
            json!({ "id": "l", "kind": "default", "name": "D", "active": true }),
        )
        .unwrap();
        assert_eq!(PriceListBody::of(&list).kind, "default");
        assert_eq!(PricingSettings::default().cash_rounding, "none");
        let q = QuoteRequest {
            customer_id: None,
            lines: vec![QuoteLine::new("v", "u", 1000)],
        };
        assert_eq!(
            serde_json::to_value(q).unwrap(),
            json!({ "lines": [{ "variant_id": "v", "unit_id": "u", "qty_thousandths": 1000 }] })
        );
    }
}
