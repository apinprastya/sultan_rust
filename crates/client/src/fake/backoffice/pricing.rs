//! The fake's pricing (phase R3, spec 2.3): price lists, entries, tax rates and settings for the
//! back office, the register's quote and the lookup price. Prices are resolved with the same
//! `sultan_core::pricing` rules as the server; the storage rules follow the server's answers
//! (`price_list_conflict`, `price_overlap`, `invalid_rule`, `invalid_reference`,
//! `invalid_field`, `404 not_found`, `403 permission_denied`). Every write that changed
//! something emits `price.changed {scope, variant_ids}` like the server.
//!
//! Simplifications: `price.manage` is held by every user with `catalog.manage` (the seeded Owner
//! and Manager hold both); a branch list's target must be one of the fake's two branches, while
//! group, tier and customer targets are not checked; the register and the back office both price
//! at [`BRANCH_ID`]. Customers for quotes are registered with
//! [`FakeService::seed_price_customer`]; [`FakeService::seed_pricing_demo`] seeds two priced
//! products and a Gold tier.

use std::collections::{BTreeMap, HashMap};
use std::str::FromStr;

use chrono::NaiveDate;
use serde_json::{Value, json};
use sultan_core::catalog::{Qty, gtin_check_digit};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::ids::{UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{
    CartLine, CashRounding, CustomerLists, LineMode, PriceChain, PriceEntry as CoreEntry,
    PriceEntryId, PriceList as CoreList, PriceListId, PriceListKind, PriceTier as CoreTier,
    PricingError, Rule, SaleItem, TierMembership, TierPrice, counted_quantities, price_label,
    resolve_cart, resolve_unit_price, sale_date, validate_entry,
};
use sultan_core::tax::{TaxSettings, line_tax, rate_on};
use sultan_core::time::Timestamp;

use super::{
    BRANCH_ID, EXEMPT_TAX_CLASS, OTHER_BRANCH_ID, STANDARD_TAX_CLASS, State, denied, not_found,
};
use crate::catalog::Product;
use crate::fake::FakeService;
use crate::pricing::{
    DEFAULT_PRICE_LIST, EntryPage, LineError, LineTax, PRICE_MANAGE, PriceEntry, PriceEntryBody,
    PriceList, PriceListBody, PriceRule, PriceSource, PriceTier, PricingSettings, ProductPrices,
    Quote, QuoteRequest, QuoteTotals, QuotedLine, TaxRate,
};
use crate::{ClientError, Problem, ServerEvent};

/// The most entries per page, like the server.
const PAGE: usize = 1_000;

/// A customer as the fake's quote sees it (spec 2.5 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PriceCustomer {
    /// The customer group.
    pub group: Option<String>,
    /// The membership tier.
    pub tier: Option<String>,
    /// Last day the tier applies, `YYYY-MM-DD`; `None` = open.
    pub tier_valid_until: Option<String>,
    /// The tier is archived.
    pub tier_archived: bool,
}

/// What [`FakeService::seed_pricing_demo`] created.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PricingDemo {
    /// Teh Botol: pcs 5 000, from 12 pcs 4 500 (a quantity tier). Barcode `8991000000107`.
    pub teh: Product,
    /// Aqua 600ml: pcs 3 500; box(24) has no price of its own (unit fallback 84 000). Barcodes
    /// `8991000000206` (pcs) and `AQUA-BOX`.
    pub aqua: Product,
    /// The Gold tier.
    pub gold_tier: String,
    /// The Gold tier's list: −5% on everything.
    pub gold_list: String,
    /// A customer with the Gold tier (open-ended).
    pub gold_customer: String,
}

/// The fake's pricing state.
pub(in crate::fake) struct Pricing {
    lists: BTreeMap<String, PriceList>,
    entries: BTreeMap<String, PriceEntry>,
    rates: BTreeMap<String, Vec<TaxRate>>,
    settings: PricingSettings,
    customers: HashMap<String, PriceCustomer>,
}

impl Default for Pricing {
    fn default() -> Self {
        let default = PriceList {
            id: DEFAULT_PRICE_LIST.to_owned(),
            kind: "default".to_owned(),
            target_id: None,
            name: "Default".to_owned(),
            active: true,
            rule: None,
            archived: false,
            archived_at: None,
        };
        let rate = |bp, from: &str| TaxRate {
            rate_bp: bp,
            valid_from: from.to_owned(),
        };
        Self {
            lists: BTreeMap::from([(default.id.clone(), default)]),
            entries: BTreeMap::new(),
            rates: BTreeMap::from([
                (
                    STANDARD_TAX_CLASS.to_owned(),
                    vec![rate(1100, "2022-04-01")],
                ),
                (EXEMPT_TAX_CLASS.to_owned(), vec![rate(0, "2000-01-01")]),
            ]),
            settings: PricingSettings::default(),
            customers: HashMap::new(),
        }
    }
}

fn problem(status: u16, code: &str, detail: &str) -> Problem {
    Problem::new(status, code, detail)
}

fn invalid(field: &str, detail: &str) -> ClientError {
    problem(422, "invalid_field", detail)
        .with("field", field)
        .into()
}

fn invalid_reference(field: &str) -> ClientError {
    problem(422, "invalid_reference", "unknown reference")
        .with("field", field)
        .into()
}

/// A core pricing error shaped like the server's.
fn rule_error(error: &PricingError) -> ClientError {
    let p = problem(0, error.code(), &error.to_string());
    match error {
        PricingError::InvalidField { field, .. } => {
            let field = match *field {
                "min_qty" => "tiers.min_qty_thousandths",
                "price" => "tiers.price",
                f => f,
            };
            Problem { status: 422, ..p }.with("field", field)
        }
        PricingError::InvalidRule { .. } => Problem { status: 422, ..p }.with("field", "rule"),
        PricingError::PriceOverlap { other, .. } => {
            Problem { status: 409, ..p }.with("entry_id", other.to_string())
        }
        PricingError::NoPrice { suggestion } => Problem { status: 422, ..p }.with(
            "suggestion",
            suggestion.map_or(Value::Null, |m| m.minor().into()),
        ),
        PricingError::Overflow => Problem { status: 422, ..p },
    }
    .into()
}

fn changed(scope: &str, variants: Option<Vec<String>>) -> ServerEvent {
    let variant_ids = variants.map_or(Value::Null, |mut v| {
        v.sort();
        v.dedup();
        v.into()
    });
    ServerEvent {
        kind: "price.changed".to_owned(),
        data: json!({ "scope": scope, "variant_ids": variant_ids }),
    }
}

fn parse_rule(rule: PriceRule, field: &str) -> Result<Rule, ClientError> {
    match (rule.pct_bp, rule.amount) {
        (Some(bp), None) => Ok(Rule::Pct(bp)),
        (None, Some(amount)) => Ok(Rule::Amount(Money::from_minor(amount))),
        _ => Err(invalid(field, "needs exactly one of `pct_bp` and `amount`")),
    }
}

fn parse_tier(tier: &PriceTier) -> Result<CoreTier, ClientError> {
    let price = match (tier.price, tier.rule) {
        (Some(price), None) => TierPrice::Fixed(Money::from_minor(price)),
        (None, Some(rule)) => TierPrice::Rule(parse_rule(rule, "tiers.rule")?),
        _ => {
            return Err(invalid(
                "tiers",
                "a tier needs exactly one of `price` and `rule`",
            ));
        }
    };
    Ok(CoreTier {
        min_qty: Qty::from_thousandths(tier.min_qty_thousandths),
        price,
    })
}

fn parse_date(field: &str, value: Option<&str>) -> Result<Option<NaiveDate>, ClientError> {
    value
        .map(|v| {
            NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d")
                .map_err(|_| invalid(field, "must be a date YYYY-MM-DD"))
        })
        .transpose()
}

fn date_text(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn uuid_of<T: FromStr>(value: &str, field: &str) -> Result<T, ClientError> {
    value.parse().map_err(|_| {
        ClientError::Api(problem(
            400,
            "invalid_body",
            &format!("`{field}` is not a UUID"),
        ))
    })
}

fn list_id(value: &str) -> Result<PriceListId, ClientError> {
    uuid_of(value, "id").map(PriceListId::from_uuid)
}

/// An entry in `core` form; the dates and tiers were checked when it was stored.
fn core_entry(e: &PriceEntry) -> Option<CoreEntry> {
    Some(CoreEntry {
        id: PriceEntryId::from_uuid(e.id.parse().ok()?),
        variant: e.variant_id.parse().ok()?,
        unit: e.unit_id.parse().ok()?,
        valid_from: parse_date("valid_from", e.valid_from.as_deref()).ok()?,
        valid_to: parse_date("valid_to", e.valid_to.as_deref()).ok()?,
        tiers: e.tiers.iter().filter_map(|t| parse_tier(t).ok()).collect(),
    })
}

fn kind_of(value: &str) -> Result<PriceListKind, ClientError> {
    value
        .parse()
        .map_err(|_| invalid("kind", "must be default, branch, group, tier or customer"))
}

fn rounding_of(value: &str) -> Option<CashRounding> {
    CashRounding::ALL.into_iter().find(|r| r.as_str() == value)
}

fn is_live(list: &PriceList) -> bool {
    list.active && !list.archived
}

impl Pricing {
    /// The list in `core` form with all its entries.
    fn core_list(&self, list: &PriceList) -> Option<CoreList> {
        Some(CoreList {
            id: list_id(&list.id).ok()?,
            kind: list.kind.parse().ok()?,
            active: list.active,
            archived: list.archived,
            rule: list.rule.and_then(|r| parse_rule(r, "rule").ok()),
            entries: self
                .entries
                .values()
                .filter(|e| e.list_id == list.id)
                .filter_map(core_entry)
                .collect(),
        })
    }

    /// The live list of `kind` for `target`.
    fn live(&self, kind: &str, target: Option<&str>) -> Option<CoreList> {
        let target = target?;
        self.lists
            .values()
            .find(|l| l.kind == kind && l.target_id.as_deref() == Some(target) && is_live(l))
            .and_then(|l| self.core_list(l))
    }

    fn default_list(&self) -> Option<CoreList> {
        self.lists
            .get(DEFAULT_PRICE_LIST)
            .and_then(|l| self.core_list(l))
    }
}

/// The sale item of `variant` in `unit` of `product`.
fn sale_item(product: &Product, variant: &str, unit: &str) -> Option<SaleItem> {
    let u = product.units.iter().find(|u| u.id == unit)?;
    product.variants.iter().find(|v| v.id == variant)?;
    Some(SaleItem {
        variant: variant.parse().ok()?,
        unit: unit.parse().ok()?,
        base_unit: product.base_unit_id.parse().ok()?,
        factor: Qty::from_thousandths(u.factor_thousandths),
    })
}

impl State {
    fn today(&self) -> NaiveDate {
        sale_date(
            Timestamp::from_unix_millis(self.now),
            BranchTimeZone::default(),
        )
    }

    fn price_manager(&self, token: &str) -> Result<(), ClientError> {
        let user = self.bo_user(token)?;
        if user.manage || user.manage_here {
            Ok(())
        } else {
            Err(denied(PRICE_MANAGE))
        }
    }

    /// The product holding `variant`.
    fn product_of(&self, variant: &str) -> Option<&Product> {
        self.catalog
            .products
            .values()
            .map(|s| &s.product)
            .find(|p| p.variants.iter().any(|v| v.id == variant))
    }

    pub(in crate::fake) fn price_lists(
        &self,
        token: &str,
        include_archived: bool,
    ) -> Result<Vec<PriceList>, ClientError> {
        self.bo_user(token)?;
        let mut lists: Vec<PriceList> = self
            .catalog
            .pricing
            .lists
            .values()
            .filter(|l| include_archived || !l.archived)
            .cloned()
            .collect();
        lists.sort_by(|a, b| (&a.kind, &a.name, &a.id).cmp(&(&b.kind, &b.name, &b.id)));
        Ok(lists)
    }

    pub(in crate::fake) fn put_price_list(
        &mut self,
        token: &str,
        id: &str,
        body: &PriceListBody,
    ) -> Result<PriceList, ClientError> {
        self.price_manager(token)?;
        let core_id = list_id(id)?;
        let kind = kind_of(&body.kind)?;
        let rule = body.rule.map(|r| parse_rule(r, "rule")).transpose()?;
        let name = body.name.trim();
        if name.is_empty() || name.chars().count() > 100 {
            return Err(invalid("name", "must be 1-100 characters after trimming"));
        }
        let old = self.catalog.pricing.lists.get(id).cloned();
        match &old {
            Some(o) if o.kind != body.kind => return Err(invalid("kind", "cannot be changed")),
            Some(o) if o.target_id != body.target_id => {
                return Err(invalid("target_id", "cannot be changed"));
            }
            None if kind == PriceListKind::Default => {
                return Err(invalid("kind", "there is only one default list"));
            }
            _ => {}
        }
        let new = PriceList {
            id: id.to_owned(),
            kind: body.kind.clone(),
            target_id: body.target_id.clone(),
            name: name.to_owned(),
            active: body.active,
            rule: body.rule,
            archived: body.archived,
            archived_at: match (body.archived, old.as_ref().and_then(|o| o.archived_at)) {
                (false, _) => None,
                (true, Some(kept)) => Some(kept),
                (true, None) => Some(self.now),
            },
        };
        CoreList {
            id: core_id,
            kind,
            active: new.active,
            archived: new.archived,
            rule,
            entries: Vec::new(),
        }
        .validate()
        .map_err(|e| rule_error(&e))?;
        if old.as_ref() == Some(&new) {
            return Ok(new);
        }
        match (kind, new.target_id.as_deref()) {
            (PriceListKind::Default, None) => {}
            (PriceListKind::Default, Some(_)) => {
                return Err(invalid("target_id", "the default list has no target"));
            }
            (_, None) => return Err(invalid("target_id", "required for this kind of list")),
            (PriceListKind::Branch, Some(b)) if b != BRANCH_ID && b != OTHER_BRANCH_ID => {
                return Err(invalid_reference("target_id"));
            }
            _ => {}
        }
        if is_live(&new) && kind != PriceListKind::Default {
            let existing = self.catalog.pricing.lists.values().find(|l| {
                l.id != new.id && l.kind == new.kind && l.target_id == new.target_id && is_live(l)
            });
            if let Some(existing) = existing {
                return Err(problem(
                    409,
                    "price_list_conflict",
                    "another active price list already has this target",
                )
                .with("price_list_id", existing.id.as_str())
                .into());
            }
        }
        self.catalog
            .pricing
            .lists
            .insert(id.to_owned(), new.clone());
        self.catalog.events.push(changed("price_list", None));
        Ok(new)
    }

    pub(in crate::fake) fn archive_price_list(
        &mut self,
        token: &str,
        id: &str,
    ) -> Result<PriceList, ClientError> {
        self.price_manager(token)?;
        let list = self
            .catalog
            .pricing
            .lists
            .get(id)
            .cloned()
            .ok_or_else(|| not_found("price_list"))?;
        let mut body = PriceListBody::of(&list);
        body.archived = true;
        self.put_price_list(token, id, &body)
    }

    pub(in crate::fake) fn price_entries(
        &self,
        token: &str,
        list: &str,
        variant: Option<&str>,
        after: Option<&str>,
    ) -> Result<EntryPage, ClientError> {
        self.bo_user(token)?;
        if !self.catalog.pricing.lists.contains_key(list) {
            return Err(not_found("price_list"));
        }
        let entries: Vec<PriceEntry> = self
            .catalog
            .pricing
            .entries
            .values()
            .filter(|e| e.list_id == list)
            .filter(|e| variant.is_none_or(|v| e.variant_id == v))
            .filter(|e| after.is_none_or(|a| e.id.as_str() > a))
            .take(PAGE)
            .cloned()
            .collect();
        Ok(EntryPage {
            next_after_id: entries
                .last()
                .filter(|_| entries.len() == PAGE)
                .map(|e| e.id.clone()),
            entries,
        })
    }

    pub(in crate::fake) fn put_price_entry(
        &mut self,
        token: &str,
        list: &str,
        id: &str,
        body: &PriceEntryBody,
    ) -> Result<PriceEntry, ClientError> {
        self.price_manager(token)?;
        let entry_id = PriceEntryId::from_uuid(uuid_of(id, "entry_id")?);
        let variant: VariantId = uuid_of(&body.variant_id, "variant_id")?;
        let unit: UnitId = uuid_of(&body.unit_id, "unit_id")?;
        let valid_from = parse_date("valid_from", body.valid_from.as_deref())?;
        let valid_to = parse_date("valid_to", body.valid_to.as_deref())?;
        let mut tiers: Vec<PriceTier> = body.tiers.clone();
        let core_tiers = tiers.iter().map(parse_tier).collect::<Result<_, _>>()?;
        let stored_list = self
            .catalog
            .pricing
            .lists
            .get(list)
            .ok_or_else(|| not_found("price_list"))?;
        let kind = kind_of(&stored_list.kind)?;
        let old = self.catalog.pricing.entries.get(id).cloned();
        if old.as_ref().is_some_and(|o| o.list_id != list) {
            return Err(invalid("list_id", "the entry belongs to another list"));
        }
        let others: Vec<CoreEntry> = self
            .catalog
            .pricing
            .entries
            .values()
            .filter(|e| e.list_id == list && e.id != id)
            .filter_map(core_entry)
            .collect();
        let core = CoreEntry {
            id: entry_id,
            variant,
            unit,
            valid_from,
            valid_to,
            tiers: core_tiers,
        };
        validate_entry(kind, &core, &others).map_err(|e| rule_error(&e))?;
        tiers.sort_by_key(|t| t.min_qty_thousandths);
        let new = PriceEntry {
            id: id.to_owned(),
            list_id: list.to_owned(),
            variant_id: body.variant_id.clone(),
            unit_id: body.unit_id.clone(),
            valid_from: valid_from.map(date_text),
            valid_to: valid_to.map(date_text),
            tiers,
        };
        if old.as_ref() == Some(&new) {
            return Ok(new);
        }
        let product = self
            .product_of(&new.variant_id)
            .ok_or_else(|| invalid_reference("variant_id"))?;
        if !product.units.iter().any(|u| u.id == new.unit_id) {
            return Err(invalid_reference("unit_id"));
        }
        let product_id = product.id.clone();
        if let Some(stored) = self.catalog.products.get_mut(&product_id) {
            stored.used = true;
        }
        let mut variants = vec![new.variant_id.clone()];
        variants.extend(old.map(|o| o.variant_id));
        self.catalog
            .pricing
            .entries
            .insert(id.to_owned(), new.clone());
        self.catalog
            .events
            .push(changed("price_entry", Some(variants)));
        Ok(new)
    }

    pub(in crate::fake) fn delete_price_entry(
        &mut self,
        token: &str,
        list: &str,
        id: &str,
    ) -> Result<(), ClientError> {
        self.price_manager(token)?;
        let entry = self
            .catalog
            .pricing
            .entries
            .get(id)
            .filter(|e| e.list_id == list)
            .cloned()
            .ok_or_else(|| not_found("price_entry"))?;
        self.catalog.pricing.entries.remove(id);
        self.catalog
            .events
            .push(changed("price_entry", Some(vec![entry.variant_id])));
        Ok(())
    }

    pub(in crate::fake) fn product_prices(
        &self,
        token: &str,
        product: &str,
    ) -> Result<ProductPrices, ClientError> {
        let lists = self.price_lists(token, false)?;
        let p = self
            .catalog
            .products
            .get(product)
            .ok_or_else(|| not_found("product"))?;
        let entries = self
            .catalog
            .pricing
            .entries
            .values()
            .filter(|e| p.product.variants.iter().any(|v| v.id == e.variant_id))
            .cloned()
            .collect();
        Ok(ProductPrices {
            product_id: product.to_owned(),
            lists,
            entries,
        })
    }

    pub(in crate::fake) fn tax_rates(
        &self,
        token: &str,
        class: &str,
    ) -> Result<Vec<TaxRate>, ClientError> {
        self.bo_user(token)?;
        self.catalog
            .pricing
            .rates
            .get(class)
            .cloned()
            .ok_or_else(|| not_found("tax_class"))
    }

    pub(in crate::fake) fn put_tax_rate(
        &mut self,
        token: &str,
        class: &str,
        rate: &TaxRate,
    ) -> Result<Vec<TaxRate>, ClientError> {
        self.price_manager(token)?;
        let from = parse_date("valid_from", Some(&rate.valid_from))?
            .map(date_text)
            .unwrap_or_default();
        let rates = self
            .catalog
            .pricing
            .rates
            .get_mut(class)
            .ok_or_else(|| not_found("tax_class"))?;
        let new = TaxRate {
            rate_bp: rate.rate_bp,
            valid_from: from,
        };
        if !rates.contains(&new) {
            rates.retain(|r| r.valid_from != new.valid_from);
            rates.push(new);
            rates.sort_by(|a, b| a.valid_from.cmp(&b.valid_from));
            self.catalog.events.push(changed("tax_rate", None));
        }
        Ok(self.catalog.pricing.rates[class].clone())
    }

    pub(in crate::fake) fn pricing_settings(
        &self,
        token: &str,
    ) -> Result<PricingSettings, ClientError> {
        self.bo_user(token)?;
        Ok(self.catalog.pricing.settings.clone())
    }

    pub(in crate::fake) fn put_pricing_settings(
        &mut self,
        token: &str,
        body: &PricingSettings,
    ) -> Result<PricingSettings, ClientError> {
        self.price_manager(token)?;
        if rounding_of(&body.cash_rounding).is_none() {
            return Err(invalid(
                "cash_rounding",
                "must be none, nearest_100, down_100, nearest_500 or down_500",
            ));
        }
        if self.catalog.pricing.settings != *body {
            self.catalog.pricing.settings = body.clone();
            self.catalog.events.push(changed("settings", None));
        }
        Ok(body.clone())
    }

    /// The lookup's store price of one unit, and a price label's quantity (spec 2.3 §5, §10).
    pub(in crate::fake) fn lookup_price(
        &self,
        product: &Product,
        variant: &str,
        unit: &str,
        label_amount: Option<i64>,
    ) -> (Option<i64>, Option<i64>) {
        let pricing = &self.catalog.pricing;
        let (Some(item), Some(default)) =
            (sale_item(product, variant, unit), pricing.default_list())
        else {
            return (None, None);
        };
        let branch = pricing.live("branch", Some(BRANCH_ID));
        let date = self.today();
        if let Some(amount) = label_amount {
            return price_label(
                branch.as_ref(),
                &default,
                date,
                &item,
                Money::from_minor(amount),
            )
            .map_or((None, None), |l| {
                (l.unit_price.map(Money::minor), l.qty.map(Qty::thousandths))
            });
        }
        let chain = PriceChain::store(branch.as_ref(), &default, date);
        let price = resolve_unit_price(&chain, &item, item.factor)
            .ok()
            .flatten();
        (price.map(|p| p.unit_price.minor()), None)
    }

    /// `POST /pricing/quote` (see the module docs).
    #[allow(clippy::too_many_lines)] // mirrors the server's one pass
    pub(in crate::fake) fn quote(
        &self,
        token: &str,
        request: &QuoteRequest,
    ) -> Result<Quote, ClientError> {
        if !token.is_empty() && self.caller(token).is_err() {
            self.bo_user(token)?;
        }
        if request.lines.len() > 500 {
            return Err(invalid("lines", "at most 500 lines"));
        }
        if request.lines.iter().any(|l| l.qty_thousandths <= 0) {
            return Err(invalid(
                "lines.qty_thousandths",
                "`qty_thousandths` must be greater than zero",
            ));
        }
        let pricing = &self.catalog.pricing;
        let customer = request
            .customer_id
            .as_deref()
            .map(|id| {
                pricing
                    .customers
                    .get(id)
                    .ok_or_else(|| invalid_reference("customer_id"))
            })
            .transpose()?;
        let date = self.today();
        let default = pricing
            .default_list()
            .ok_or_else(|| ClientError::Unexpected("no default list".into()))?;
        let branch = pricing.live("branch", Some(BRANCH_ID));
        let contract = request
            .customer_id
            .as_deref()
            .and_then(|c| pricing.live("customer", Some(c)));
        let tier = customer.and_then(|c| {
            let list = pricing.live("tier", c.tier.as_deref())?;
            let until = parse_date("tier_valid_until", c.tier_valid_until.as_deref()).ok()?;
            Some((
                list,
                TierMembership {
                    tier_archived: c.tier_archived,
                    valid_until: until,
                },
            ))
        });
        let group = customer.and_then(|c| pricing.live("group", c.group.as_deref()));
        let lists = CustomerLists {
            contract: contract.as_ref(),
            tier: tier.as_ref().map(|(l, m)| (l, *m)),
            group: group.as_ref(),
        };
        let chain = PriceChain::build(Some(&lists), branch.as_ref(), &default, date);

        let mut cart = Vec::new();
        let mut slots = Vec::new();
        for line in &request.lines {
            let found = self
                .product_of(&line.variant_id)
                .and_then(|p| Some((p, sale_item(p, &line.variant_id, &line.unit_id)?)));
            slots.push(found.map(|(p, item)| {
                let typed = line.typed_price.map(Money::from_minor);
                let mode = if p.open_price {
                    LineMode::OpenPrice(typed)
                } else {
                    typed.map_or(LineMode::Listed, LineMode::Overridden)
                };
                cart.push(CartLine {
                    item,
                    qty: Qty::from_thousandths(line.qty_thousandths),
                    mode,
                });
                (cart.len() - 1, p.tax_class_id.clone())
            }));
        }
        let priced = resolve_cart(&chain, &cart);
        let counted = counted_quantities(&cart);
        let settings = TaxSettings {
            inclusive: pricing.settings.tax_inclusive,
            registered: pricing.settings.tax_registered,
        };
        let mut totals = QuoteTotals::default();
        let mut lines = Vec::new();
        for (slot, line) in slots.into_iter().zip(&request.lines) {
            let mut q = QuotedLine {
                variant_id: line.variant_id.clone(),
                unit_id: line.unit_id.clone(),
                qty_thousandths: line.qty_thousandths,
                mode: "listed".to_owned(),
                unit_price: None,
                amount: None,
                resolved_price: None,
                source: None,
                tax: None,
                error: None,
            };
            let Some((i, class)) = slot else {
                q.error = Some(LineError {
                    code: "invalid_reference".to_owned(),
                    message: "unknown variant, or a unit not of its product".to_owned(),
                    suggestion: None,
                });
                lines.push(q);
                continue;
            };
            match cart[i].mode {
                LineMode::Listed => "listed",
                LineMode::OpenPrice(_) => "open_price",
                LineMode::Overridden(_) => "overridden",
            }
            .clone_into(&mut q.mode);
            let resolved = match &priced[i] {
                Ok(p) => {
                    let rates: Vec<sultan_core::tax::TaxRate> = pricing
                        .rates
                        .get(&class)
                        .map(|rs| {
                            rs.iter()
                                .filter_map(|r| {
                                    Some(sultan_core::tax::TaxRate {
                                        rate_bp: r.rate_bp,
                                        valid_from: parse_date("v", Some(&r.valid_from)).ok()??,
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let rate = if settings.registered {
                        rate_on(&rates, date).unwrap_or(0)
                    } else {
                        0
                    };
                    let tax = line_tax(p.amount, rate, settings);
                    totals.amount += p.amount.minor();
                    totals.tax += tax.tax.minor();
                    totals.net += tax.net.minor();
                    totals.total += tax.total.minor();
                    q.unit_price = Some(p.unit_price.minor());
                    q.amount = Some(p.amount.minor());
                    q.tax = Some(LineTax {
                        tax_class_id: class,
                        rate_bp: rate,
                        tax: tax.tax.minor(),
                        net: tax.net.minor(),
                        total: tax.total.minor(),
                    });
                    p.resolved
                }
                Err(e) => {
                    let ClientError::Api(p) = rule_error(e) else {
                        unreachable!("rule errors are problems")
                    };
                    q.error = Some(LineError {
                        code: p.code.clone(),
                        message: p.detail.clone(),
                        suggestion: match e {
                            PricingError::NoPrice { suggestion } => suggestion.map(Money::minor),
                            _ => None,
                        },
                    });
                    match e {
                        PricingError::NoPrice {
                            suggestion: Some(_),
                        } => counted
                            .get(&cart[i].item.variant)
                            .copied()
                            .flatten()
                            .and_then(|n| {
                                resolve_unit_price(&chain, &cart[i].item, n).ok().flatten()
                            }),
                        _ => None,
                    }
                }
            };
            q.resolved_price = resolved.map(|r| r.unit_price.minor());
            q.source = resolved.map(|r| PriceSource {
                list_id: r.source.list.to_string(),
                kind: r.source.kind.as_str().to_owned(),
                tier_min_qty_thousandths: r.source.tier_min_qty.map(Qty::thousandths),
                unit_fallback: r.source.unit_fallback,
            });
            lines.push(q);
        }
        Ok(Quote {
            branch_id: BRANCH_ID.to_owned(),
            date: date_text(date),
            customer_id: request.customer_id.clone(),
            tax_inclusive: pricing.settings.tax_inclusive,
            tax_registered: pricing.settings.tax_registered,
            cash_rounding: pricing.settings.cash_rounding.clone(),
            lines,
            totals,
        })
    }
}

/// A fixed ID for the demo (a valid UUID).
fn demo_id(n: u32) -> String {
    format!("0190a000-0000-7000-8000-{n:012x}")
}

fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap_or_default())
}

impl FakeService {
    /// Registers a customer for quotes (`customer_id` → group and tier).
    pub fn seed_price_customer(&self, id: &str, customer: PriceCustomer) {
        self.state()
            .catalog
            .pricing
            .customers
            .insert(id.to_owned(), customer);
    }

    /// Stores a price list as the owner without recording a call (setup).
    ///
    /// # Panics
    /// If the write is rejected.
    pub fn seed_price_list(&self, id: &str, body: &PriceListBody) -> PriceList {
        self.seed(|s| s.put_price_list("seed", id, body))
    }

    /// Stores an entry as the owner without recording a call (setup).
    ///
    /// # Panics
    /// If the write is rejected.
    pub fn seed_price_entry(&self, list: &str, id: &str, body: &PriceEntryBody) -> PriceEntry {
        self.seed(|s| s.put_price_entry("seed", list, id, body))
    }

    fn seed<T>(&self, write: impl FnOnce(&mut State) -> Result<T, ClientError>) -> T {
        let mut s = self.state();
        s.catalog.tokens.insert("seed".into(), "u-owner".into());
        let result = write(&mut s);
        s.catalog.tokens.remove("seed");
        s.catalog.events.clear();
        result.unwrap_or_else(|e| panic!("seed pricing: {e}"))
    }

    /// Seeds two priced products and a Gold tier (see [`PricingDemo`]) for the pricing UI tests
    /// and screenshots.
    ///
    /// # Panics
    /// If a seed write is rejected.
    #[must_use]
    pub fn seed_pricing_demo(&self) -> PricingDemo {
        let product = |n: u32, code: &str, name: &str, units: Value, barcodes: Value| {
            let body: Product = serde_json::from_value(json!({
                "code": code, "name": name, "type": "stock", "tax_class_id": STANDARD_TAX_CLASS,
                "units": units, "base_unit_id": demo_id(n + 1),
                "variants": [{ "id": demo_id(n + 3) }], "barcodes": barcodes,
            }))
            .unwrap_or_else(|e| panic!("demo product: {e}"));
            self.seed_product(&demo_id(n), &body)
        };
        let pcs = |n: u32| {
            json!({ "id": demo_id(n + 1), "name": "pcs", "kind": "integer",
                    "factor_thousandths": 1000 })
        };
        let teh = product(
            0x100,
            "TEH",
            "Teh Botol",
            json!([pcs(0x100)]),
            json!([{ "id": demo_id(0x104), "variant_id": demo_id(0x103),
                     "unit_id": demo_id(0x101), "code": ean13("899100000010") }]),
        );
        let aqua = product(
            0x200,
            "AQUA",
            "Aqua 600ml",
            json!([pcs(0x200), { "id": demo_id(0x202), "name": "box", "kind": "integer",
                                 "factor_thousandths": 24000 }]),
            json!([
                { "id": demo_id(0x204), "variant_id": demo_id(0x203), "unit_id": demo_id(0x201),
                  "code": ean13("899100000020") },
                { "id": demo_id(0x205), "variant_id": demo_id(0x203), "unit_id": demo_id(0x202),
                  "code": "AQUA-BOX" },
            ]),
        );
        let entry = |n: u32, variant: u32, unit: u32, tiers: Vec<PriceTier>| {
            self.seed_price_entry(
                DEFAULT_PRICE_LIST,
                &demo_id(n),
                &PriceEntryBody {
                    variant_id: demo_id(variant),
                    unit_id: demo_id(unit),
                    valid_from: None,
                    valid_to: None,
                    tiers,
                },
            );
        };
        entry(
            0x110,
            0x103,
            0x101,
            vec![
                PriceTier::fixed(1000, 5_000),
                PriceTier::fixed(12_000, 4_500),
            ],
        );
        entry(0x210, 0x203, 0x201, vec![PriceTier::fixed(1000, 3_500)]);
        let gold_tier = "tier-gold".to_owned();
        let gold_list = demo_id(0x300);
        self.seed_price_list(
            &gold_list,
            &PriceListBody {
                kind: "tier".to_owned(),
                target_id: Some(gold_tier.clone()),
                name: "Gold".to_owned(),
                active: true,
                rule: Some(PriceRule::pct(-500)),
                archived: false,
            },
        );
        let gold_customer = "customer-gold".to_owned();
        self.seed_price_customer(
            &gold_customer,
            PriceCustomer {
                tier: Some(gold_tier.clone()),
                ..PriceCustomer::default()
            },
        );
        PricingDemo {
            teh,
            aqua,
            gold_tier,
            gold_list,
            gold_customer,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_shaped_like_the_server() {
        let code = |e: ClientError| {
            let p = e.problem().cloned().unwrap();
            (p.status, p.code.clone(), p.field().map(str::to_owned))
        };
        assert_eq!(
            code(rule_error(&PricingError::InvalidField {
                field: "price",
                reason: "x"
            })),
            (422, "invalid_field".into(), Some("tiers.price".into()))
        );
        assert_eq!(
            code(rule_error(&PricingError::Overflow)),
            (422, "amount_overflow".into(), None)
        );
        assert_eq!(
            code(rule_error(&PricingError::NoPrice { suggestion: None })).1,
            "no_price"
        );
        assert!(parse_rule(PriceRule::default(), "rule").is_err());
        assert!(
            parse_tier(&PriceTier {
                min_qty_thousandths: 1,
                price: None,
                rule: None
            })
            .is_err()
        );
        assert_eq!(rounding_of("down_100"), Some(CashRounding::Down100));
        assert!(uuid_of::<VariantId>("x", "f").is_err());
        assert_eq!(
            changed("x", Some(vec!["b".into(), "a".into(), "a".into()])).data["variant_ids"],
            json!(["a", "b"])
        );
    }
}
