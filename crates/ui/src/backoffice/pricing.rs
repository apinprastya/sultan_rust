//! The back office's pricing screens (spec 2.3, phase R4), part of the [`Controller`] state
//! machine. F7 opens the section; a row of tabs (Ctrl+1 … Ctrl+3) switches between its screens:
//!
//! 1. **Price lists** ([`Screen::PriceLists`]) — every non-archived list (kind, target, name,
//!    on / off, list rule) and an editor like the brand editor: ↑/↓ pick, Ctrl+N new list,
//!    Ctrl+S save, Ctrl+Shift+A archive (asks first). A new list picks its kind and target
//!    (a branch, customer group or membership tier from a picker; a single customer by member
//!    code or phone); both are fixed afterwards (spec 2.3 §15.7). The rule is none, ± % or
//!    ± Rp. The seeded `default` list is shown but cannot be archived, switched off or given a
//!    rule. Errors: `price_list_conflict` names the other active list of the target,
//!    `invalid_rule`, `invalid_reference`, `invalid_field`.
//! 2. **Tax rates** ([`Screen::TaxRates`]) — per tax class (↑/↓ pick) the rate history, and a
//!    new rate with its first day (a future day is allowed; the same day changes that rate,
//!    spec 2.3 §15.8). Ctrl+S adds it.
//! 3. **Pricing settings** ([`Screen::PricingSettings`]) — tax-inclusive prices, tax-registered,
//!    cash rounding. Ctrl+S saves; a change of tax-inclusive asks first, because stored prices
//!    are not converted (spec 2.3 §7).
//!
//! The product form has a **Prices** tab (Ctrl+4, [`ProductPricing`]): `GET
//! /products/{id}/prices`, a list picker (Ctrl+L next list) and the product's entries of that
//! list (variant, unit, dates, tiers), and an entry editor: ↑/↓ pick an entry, Ctrl+N new
//! price, Ctrl+Shift+N schedule a new price (a copy that starts tomorrow; saving ends the picked
//! entry the day before, and puts the old end back if the new entry is refused), Ctrl+Enter
//! save, Ctrl+Shift+D delete (asks first), Ctrl+T add a tier (1–10). Quantities have 3
//! decimals; prices are whole Rupiah.
//!
//! Every write needs `price.manage`; without it the screens are read-only and say why.
//! `price.changed` events reload what is on view.

use std::collections::BTreeMap;

use chrono::{Days, NaiveDate};
use sultan_client::customers::CUSTOMER_MANAGE;
use sultan_client::pricing::{DEFAULT_PRICE_LIST, PRICE_MANAGE};
use sultan_client::{
    ClientError, Customer, PriceEntry, PriceEntryBody, PriceList, PriceListBody, PriceRule,
    PriceTier, PricingSettings, Problem, ProductPrices, TaxRate,
};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::pricing::sale_date;
use sultan_core::time::Timestamp;

use super::customers::Call as CustomerCall;
use super::form::{Tab, new_id, qty_text};
use super::{Confirm, Controller, Effect, Press, PressKey, Screen, Step, key, notice};
use crate::amount;
use crate::controller::{Notice, Tone};

/// The screens of the section, in tab order (Ctrl+1 … Ctrl+3).
pub const TABS: [Screen; 3] = [
    Screen::PriceLists,
    Screen::TaxRates,
    Screen::PricingSettings,
];
/// The kinds a new list may have, in picker order (`default` is seeded only).
pub const KINDS: [&str; 4] = ["branch", "group", "tier", "customer"];
/// Cash rounding settings, in picker order (spec 2.3 §8).
pub const ROUNDINGS: [&str; 5] = ["none", "nearest_100", "down_100", "nearest_500", "down_500"];
/// The most tiers of an entry (spec 2.3 §3).
pub const MAX_TIERS: usize = 10;
/// The `kind`s of [`super::Msg::Edit`] handled here.
pub const EDIT_KINDS: [&str; 4] = ["price-list", "price-entry", "price-tier", "tax-rate"];

/// A list rule picker: none, ± %, ± Rp. A tier's picker is fixed price, ± %, ± Rp (the same
/// indices, with 0 = fixed).
pub const RULE_NONE: usize = 0;
/// ± % (basis points on the wire).
pub const RULE_PCT: usize = 1;
/// ± Rp.
pub const RULE_AMOUNT: usize = 2;

/// Whether `screen` belongs to the pricing section.
#[must_use]
pub fn in_section(screen: Screen) -> bool {
    TABS.contains(&screen)
}

/// A pricing call (inside [`Effect::Pricing`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `price_lists` (without archived ones).
    Lists,
    /// `put_price_list`.
    PutList {
        /// List.
        id: String,
        /// Body.
        body: PriceListBody,
    },
    /// `archive_price_list`.
    ArchiveList {
        /// List.
        id: String,
        /// Idempotency key.
        key: String,
    },
    /// `product_prices`.
    ProductPrices(String),
    /// `put_price_entry`.
    PutEntry {
        /// List.
        list: String,
        /// Entry.
        id: String,
        /// Body.
        body: PriceEntryBody,
    },
    /// `delete_price_entry`.
    DeleteEntry {
        /// List.
        list: String,
        /// Entry.
        id: String,
    },
    /// `tax_rates` of a class.
    TaxRates(String),
    /// `put_tax_rate`.
    PutTaxRate {
        /// Tax class.
        class: String,
        /// Rate.
        rate: TaxRate,
    },
    /// `pricing_settings`.
    Settings,
    /// `put_pricing_settings`.
    PutSettings(PricingSettings),
    /// `customer` (the name of a customer list's target).
    Customer(String),
    /// `customer_by_code` or, for digits, `customer_by_phone` (the target picker).
    FindCustomer(String),
}

/// The answer of a [`Call`] (inside [`super::Msg::Pricing`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// [`Call::Lists`].
    Lists(Result<Vec<PriceList>, ClientError>),
    /// [`Call::PutList`] or [`Call::ArchiveList`] (true).
    ListSaved(bool, Result<PriceList, ClientError>),
    /// [`Call::ProductPrices`] (product).
    ProductPrices(String, Result<ProductPrices, ClientError>),
    /// [`Call::PutEntry`].
    EntrySaved(Result<PriceEntry, ClientError>),
    /// [`Call::DeleteEntry`].
    EntryDeleted(Result<(), ClientError>),
    /// [`Call::TaxRates`] (class).
    TaxRates(String, Result<Vec<TaxRate>, ClientError>),
    /// [`Call::PutTaxRate`] (class).
    TaxRateSaved(String, Result<Vec<TaxRate>, ClientError>),
    /// [`Call::Settings`] or [`Call::PutSettings`] (true).
    Settings(bool, Result<PricingSettings, ClientError>),
    /// [`Call::Customer`] (customer).
    Customer(String, Result<Customer, ClientError>),
    /// [`Call::FindCustomer`] (the text looked up).
    Found(String, Result<Customer, ClientError>),
}

/// A field error: `field` (`name`, `target`, `rule`, `variant`, `unit`, `valid_from`,
/// `valid_to`, `tiers`, `min_qty`, `value`, `rate`), the tier row for tier fields, a message
/// code and its argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Field.
    pub field: &'static str,
    /// Tier row.
    pub row: Option<usize>,
    /// Message code (`bo_widgets.slint`).
    pub code: &'static str,
    /// The `{}` argument.
    pub arg: String,
}

impl Issue {
    fn new(field: &'static str, code: &'static str) -> Self {
        Self {
            field,
            row: None,
            code,
            arg: String::new(),
        }
    }

    fn at(field: &'static str, row: usize, code: &'static str) -> Self {
        Self {
            row: Some(row),
            ..Self::new(field, code)
        }
    }
}

/// `-5` → −500 bp, `2.5` / `2,5` → 250 bp (at most 2 decimals).
#[must_use]
pub fn parse_pct(text: &str) -> Option<i32> {
    let t = text.trim().trim_end_matches('%').trim();
    let (negative, t) = sign(t);
    let t = t.replace(',', ".");
    let (whole, frac) = t.split_once('.').unwrap_or((&t, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
        || frac.len() > 2
        || whole.len() > 6
    {
        return None;
    }
    let whole: i32 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let frac: i32 = format!("{frac:0<2}").parse().ok()?;
    let bp = whole.checked_mul(100)?.checked_add(frac)?;
    Some(if negative { -bp } else { bp })
}

fn sign(t: &str) -> (bool, &str) {
    if let Some(rest) = t.strip_prefix('-').or_else(|| t.strip_prefix('−')) {
        (true, rest.trim())
    } else {
        (false, t.strip_prefix('+').unwrap_or(t).trim())
    }
}

/// `12.500`, `Rp 12.500`, `-500` → minor units (whole Rupiah; `.`, `,` and spaces group).
#[must_use]
pub fn parse_amount(text: &str) -> Option<i64> {
    let (negative, t) = sign(text.trim());
    let t = t.strip_prefix("Rp").unwrap_or(t);
    let digits: String = t
        .chars()
        .filter(|c| !matches!(c, '.' | ',' | ' '))
        .collect();
    if digits.is_empty() || digits.len() > 13 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let value: i64 = digits.parse().ok()?;
    Some(if negative { -value } else { value })
}

/// `12` → 12 000, `0.5` / `0,5` → 500 thousandths (at most 3 decimals).
#[must_use]
pub fn parse_qty(text: &str) -> Option<i64> {
    let t = text.trim().replace(',', ".");
    let (whole, frac) = t.split_once('.').unwrap_or((&t, ""));
    if whole.is_empty() && frac.is_empty()
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
        || frac.len() > 3
        || whole.len() > 12
    {
        return None;
    }
    let whole: i64 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    let frac: i64 = format!("{frac:0<3}").parse().ok()?;
    whole.checked_mul(1000)?.checked_add(frac)
}

/// `2026-12-01` (also `2026-12-1`) → the date; empty → `Ok(None)`.
fn parse_date(text: &str) -> Result<Option<NaiveDate>, ()> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| ())
}

fn date_text(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// −500 bp → `-5%`, +250 → `+2.5%`.
#[must_use]
pub fn pct_text(bp: i32) -> String {
    let text = amount::percent(i64::from(bp));
    if bp > 0 { format!("+{text}") } else { text }
}

/// A rule as shown: `-5%`, `+Rp 500`.
#[must_use]
pub fn rule_text(rule: &PriceRule) -> String {
    match (rule.pct_bp, rule.amount) {
        (Some(bp), _) => pct_text(bp),
        (None, Some(a)) => amount::signed_rupiah(a),
        (None, None) => String::new(),
    }
}

/// A tier as shown: `12 → Rp 4.500`, `1 → -5%`.
#[must_use]
pub fn tier_text(tier: &PriceTier) -> String {
    let price = tier.price.map_or_else(
        || tier.rule.as_ref().map(rule_text).unwrap_or_default(),
        amount::rupiah,
    );
    format!("{} → {price}", qty_text(tier.min_qty_thousandths))
}

/// The editable text of a rule picker: (mode, value).
fn rule_fields(rule: Option<&PriceRule>) -> (usize, String) {
    match rule.map(|r| (r.pct_bp, r.amount)) {
        Some((Some(bp), _)) => (RULE_PCT, percent_value(bp)),
        Some((None, Some(a))) => (RULE_AMOUNT, a.to_string()),
        _ => (RULE_NONE, String::new()),
    }
}

/// −500 → `-5`, 250 → `2.5` (the text a % field holds).
fn percent_value(bp: i32) -> String {
    amount::percent(i64::from(bp))
        .trim_end_matches('%')
        .to_owned()
}

/// The rule of a picker mode and value; `Err` when the value does not parse.
fn rule_of(mode: usize, value: &str) -> Result<Option<PriceRule>, ()> {
    match mode {
        RULE_PCT => parse_pct(value)
            .map(|bp| Some(PriceRule::pct(bp)))
            .ok_or(()),
        RULE_AMOUNT => parse_amount(value)
            .map(|a| Some(PriceRule::amount(a)))
            .ok_or(()),
        _ => Ok(None),
    }
}

/// Today in the branch time zone (spec 2.3 §3: entry dates are local days).
fn local_today() -> NaiveDate {
    sale_date(
        Timestamp::from_unix_millis(chrono::Utc::now().timestamp_millis()),
        BranchTimeZone::default(),
    )
}

/// Whether a dated row is `ended`, `scheduled` or `current` on `today`.
#[must_use]
pub fn status(from: Option<&str>, to: Option<&str>, today: NaiveDate) -> &'static str {
    let day = |d: Option<&str>| d.and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    if day(to).is_some_and(|t| t < today) {
        "ended"
    } else if day(from).is_some_and(|f| f > today) {
        "scheduled"
    } else {
        "current"
    }
}

/// `2026-12-01 – …`: an entry's dates for a message.
#[must_use]
pub fn dates_text(from: Option<&str>, to: Option<&str>) -> String {
    format!("{} – {}", from.unwrap_or("…"), to.unwrap_or("…"))
}

/// The price list editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListEditor {
    /// The highlighted row (into [`PricingState::lists`]).
    pub cursor: usize,
    /// The list being edited, `None` for a new one.
    pub editing: Option<String>,
    /// Kind (`default`, `branch`, `group`, `tier`, `customer`).
    pub kind: String,
    /// Target.
    pub target: Option<String>,
    /// A customer target's member code or phone as typed.
    pub customer: String,
    /// Name as typed.
    pub name: String,
    /// Switched on.
    pub active: bool,
    /// Rule mode ([`RULE_NONE`], [`RULE_PCT`], [`RULE_AMOUNT`]).
    pub rule_mode: usize,
    /// Rule value as typed.
    pub rule_value: String,
    /// Field errors.
    pub issues: Vec<Issue>,
    /// Saved, errors.
    pub notice: Option<Notice>,
    /// A write is running.
    pub saving: bool,
}

impl Default for ListEditor {
    fn default() -> Self {
        Self {
            cursor: 0,
            editing: None,
            kind: KINDS[0].to_owned(),
            target: None,
            customer: String::new(),
            name: String::new(),
            active: true,
            rule_mode: RULE_NONE,
            rule_value: String::new(),
            issues: Vec::new(),
            notice: None,
            saving: false,
        }
    }
}

impl ListEditor {
    fn of(list: &PriceList, cursor: usize) -> Self {
        let (rule_mode, rule_value) = rule_fields(list.rule.as_ref());
        Self {
            cursor,
            editing: Some(list.id.clone()),
            kind: list.kind.clone(),
            target: list.target_id.clone(),
            customer: String::new(),
            name: list.name.clone(),
            active: list.active,
            rule_mode,
            rule_value,
            issues: Vec::new(),
            notice: None,
            saving: false,
        }
    }

    /// The seeded default list.
    #[must_use]
    pub fn is_default(&self) -> bool {
        self.kind == "default"
    }

    /// The first error of `field`.
    #[must_use]
    pub fn issue(&self, field: &str) -> Option<&Issue> {
        self.issues.iter().find(|i| i.field == field)
    }
}

/// One tier row as typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierRow {
    /// `min_qty` in the entry's unit.
    pub min_qty: String,
    /// 0 fixed price, [`RULE_PCT`], [`RULE_AMOUNT`].
    pub mode: usize,
    /// Price or rule value.
    pub value: String,
}

impl TierRow {
    fn of(tier: &PriceTier) -> Self {
        let (mode, value) = match (tier.price, tier.rule.as_ref()) {
            (Some(p), _) => (RULE_NONE, p.to_string()),
            (None, rule) => rule_fields(rule),
        };
        Self {
            min_qty: qty_text(tier.min_qty_thousandths),
            mode,
            value,
        }
    }
}

/// The entry editor of the Prices tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryEditor {
    /// Entry ID (a new UUID for a new entry).
    pub id: String,
    /// The list.
    pub list_id: String,
    /// Not stored yet.
    pub is_new: bool,
    /// Variant.
    pub variant: Option<String>,
    /// Unit.
    pub unit: Option<String>,
    /// First day as typed (`YYYY-MM-DD`, empty = open).
    pub valid_from: String,
    /// Last day as typed.
    pub valid_to: String,
    /// The tiers.
    pub tiers: Vec<TierRow>,
    /// "Schedule new price": the entry that ends the day before this one starts.
    pub ends: Option<String>,
    /// Field errors.
    pub issues: Vec<Issue>,
}

impl EntryEditor {
    fn of(entry: &PriceEntry) -> Self {
        Self {
            id: entry.id.clone(),
            list_id: entry.list_id.clone(),
            is_new: false,
            variant: Some(entry.variant_id.clone()),
            unit: Some(entry.unit_id.clone()),
            valid_from: entry.valid_from.clone().unwrap_or_default(),
            valid_to: entry.valid_to.clone().unwrap_or_default(),
            tiers: entry.tiers.iter().map(TierRow::of).collect(),
            ends: None,
            issues: Vec::new(),
        }
    }

    /// The first error of `field` (of tier `row` for tier fields).
    #[must_use]
    pub fn issue(&self, field: &str, row: Option<usize>) -> Option<&Issue> {
        self.issues
            .iter()
            .find(|i| i.field == field && i.row == row)
    }

    /// The `PUT` body, or the field errors.
    ///
    /// # Errors
    /// Every field that breaks a rule (spec 2.3 §3).
    pub fn body(&self) -> Result<PriceEntryBody, Vec<Issue>> {
        let mut issues = Vec::new();
        if self.variant.is_none() {
            issues.push(Issue::new("variant", "pick_variant"));
        }
        if self.unit.is_none() {
            issues.push(Issue::new("unit", "pick_unit"));
        }
        let from = parse_date(&self.valid_from);
        let to = parse_date(&self.valid_to);
        if from.is_err() {
            issues.push(Issue::new("valid_from", "invalid_date"));
        }
        if to.is_err() {
            issues.push(Issue::new("valid_to", "invalid_date"));
        }
        if let (Ok(Some(f)), Ok(Some(t))) = (from, to)
            && f > t
        {
            issues.push(Issue::new("valid_to", "dates_reversed"));
        }
        if self.tiers.is_empty() || self.tiers.len() > MAX_TIERS {
            issues.push(Issue::new("tiers", "invalid_tiers"));
        }
        let mut tiers = Vec::new();
        for (i, t) in self.tiers.iter().enumerate() {
            let qty = parse_qty(&t.min_qty).filter(|q| *q > 0);
            match qty {
                None => issues.push(Issue::at("min_qty", i, "invalid_qty")),
                Some(q) if tiers.iter().any(|x: &PriceTier| x.min_qty_thousandths == q) => {
                    issues.push(Issue::at("min_qty", i, "duplicate_min_qty"));
                }
                Some(_) => {}
            }
            let tier = match t.mode {
                RULE_PCT | RULE_AMOUNT => rule_of(t.mode, &t.value)
                    .ok()
                    .flatten()
                    .map(|r| PriceTier::rule(qty.unwrap_or(0), r)),
                _ => parse_amount(&t.value)
                    .filter(|p| *p >= 0)
                    .map(|p| PriceTier::fixed(qty.unwrap_or(0), p)),
            };
            match tier {
                Some(tier) => tiers.push(tier),
                None => issues.push(Issue::at(
                    "value",
                    i,
                    if t.mode == RULE_NONE {
                        "invalid_amount"
                    } else {
                        "invalid_rule_value"
                    },
                )),
            }
        }
        if !issues.is_empty() {
            return Err(issues);
        }
        Ok(PriceEntryBody {
            variant_id: self.variant.clone().unwrap_or_default(),
            unit_id: self.unit.clone().unwrap_or_default(),
            valid_from: from.ok().flatten().map(date_text),
            valid_to: to.ok().flatten().map(date_text),
            tiers,
        })
    }
}

/// The Prices tab of the open product.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProductPricing {
    /// The product.
    pub product_id: String,
    /// The lists and entries, once loaded.
    pub prices: Option<ProductPrices>,
    /// The list on view.
    pub list_id: String,
    /// The highlighted entry (into [`ProductPricing::rows`]).
    pub cursor: usize,
    /// The entry editor.
    pub editor: Option<EntryEditor>,
    /// Saved, errors.
    pub notice: Option<Notice>,
    /// A load is running.
    pub loading: bool,
    /// A write is running.
    pub saving: bool,
    /// The new entry to put once the old one has ended (schedule a new price).
    pub pending: Option<(String, String, PriceEntryBody)>,
    /// The old entry as it was, to put back if the new entry is refused.
    pub rollback: Option<(String, String, PriceEntryBody)>,
    /// The old entry is being put back (its answer keeps the error on view).
    pub undoing: bool,
}

impl ProductPricing {
    fn new(product_id: &str) -> Self {
        Self {
            product_id: product_id.to_owned(),
            prices: None,
            list_id: DEFAULT_PRICE_LIST.to_owned(),
            cursor: 0,
            editor: None,
            notice: None,
            loading: false,
            saving: false,
            pending: None,
            rollback: None,
            undoing: false,
        }
    }

    /// The lists in picker order.
    #[must_use]
    pub fn lists(&self) -> &[PriceList] {
        self.prices.as_ref().map_or(&[], |p| p.lists.as_slice())
    }

    /// The entries of `list`, by variant, unit and first day.
    #[must_use]
    pub fn entries_of(&self, list: &str) -> Vec<&PriceEntry> {
        let mut rows: Vec<&PriceEntry> = self
            .prices
            .as_ref()
            .map(|p| p.entries.iter().filter(|e| e.list_id == list).collect())
            .unwrap_or_default();
        rows.sort_by(|a, b| {
            (
                &a.variant_id,
                &a.unit_id,
                a.valid_from.as_deref().unwrap_or(""),
            )
                .cmp(&(
                    &b.variant_id,
                    &b.unit_id,
                    b.valid_from.as_deref().unwrap_or(""),
                ))
        });
        rows
    }

    /// The entries of the list on view.
    #[must_use]
    pub fn rows(&self) -> Vec<&PriceEntry> {
        self.entries_of(&self.list_id)
    }

    fn entry(&self, id: &str) -> Option<&PriceEntry> {
        self.prices
            .as_ref()
            .and_then(|p| p.entries.iter().find(|e| e.id == id))
    }
}

/// The tax rates screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaxState {
    /// The highlighted tax class (into the reference tax classes).
    pub cursor: usize,
    /// Its rates, oldest first, once loaded (with the class they are of).
    pub rates: Option<(String, Vec<TaxRate>)>,
    /// New rate as typed, in %.
    pub rate: String,
    /// Its first day as typed.
    pub valid_from: String,
    /// Field errors.
    pub issues: Vec<Issue>,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// The pricing settings form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsState {
    /// As stored.
    pub stored: Option<PricingSettings>,
    /// As edited.
    pub edit: PricingSettings,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// Everything of the pricing section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PricingState {
    /// The non-archived lists, by kind and name.
    pub lists: Vec<PriceList>,
    /// The names of customer-list targets.
    pub customer_names: BTreeMap<String, String>,
    /// The list editor.
    pub editor: ListEditor,
    /// The lists are loading.
    pub loading: bool,
    /// The Prices tab of the open product.
    pub product: Option<ProductPricing>,
    /// Tax rates.
    pub tax: TaxState,
    /// Pricing settings.
    pub settings: SettingsState,
    /// Bumped when texts change under the user: the view rebuilds the text fields.
    pub serial: u64,
    /// Today for the tests (`None`: the branch's local date).
    pub today: Option<NaiveDate>,
}

impl Controller {
    /// Whether pricing writes are allowed (`price.manage` from any role assignment).
    #[must_use]
    pub fn can_price(&self) -> bool {
        self.may(PRICE_MANAGE)
    }

    /// Today (the branch's local date).
    #[must_use]
    pub fn today(&self) -> NaiveDate {
        self.pricing.today.unwrap_or_else(local_today)
    }

    fn price_call(&self, call: Call) -> Effect {
        Effect::Pricing {
            token: self.token(),
            call,
        }
    }

    fn price_bump(&mut self) {
        self.pricing.serial += 1;
    }

    fn price_denied() -> Notice {
        notice("permission_denied_catalog", PRICE_MANAGE, Tone::Bad)
    }

    /// The name of a list's target: the branch, group, tier or customer.
    #[must_use]
    pub fn target_name(&self, kind: &str, target: Option<&str>) -> String {
        let Some(id) = target else {
            return String::new();
        };
        let found = match kind {
            "branch" => self
                .reference
                .branches
                .iter()
                .find(|b| b.id == id)
                .map(|b| b.name.clone()),
            "group" => self
                .customers
                .groups
                .iter()
                .find(|g| g.id == id)
                .map(|g| g.name.clone()),
            "tier" => self
                .customers
                .tiers
                .iter()
                .find(|t| t.id == id)
                .map(|t| t.name.clone()),
            "customer" => self.pricing.customer_names.get(id).cloned(),
            _ => None,
        };
        found.unwrap_or_else(|| id.to_owned())
    }

    /// The target choices of the editor's kind (`None` first; customers are typed).
    #[must_use]
    pub fn target_choices(&self) -> Vec<(Option<String>, String)> {
        let e = &self.pricing.editor;
        let keep = e.target.as_deref();
        let mut out = vec![(None, String::new())];
        match e.kind.as_str() {
            "branch" => out.extend(
                self.reference
                    .branches
                    .iter()
                    .map(|b| (Some(b.id.clone()), b.name.clone())),
            ),
            "group" => out.extend(
                self.customers
                    .groups
                    .iter()
                    .filter(|g| !g.archived || keep == Some(g.id.as_str()))
                    .map(|g| (Some(g.id.clone()), g.name.clone())),
            ),
            "tier" => out.extend(
                self.customers
                    .tiers
                    .iter()
                    .filter(|t| !t.archived || keep == Some(t.id.as_str()))
                    .map(|t| (Some(t.id.clone()), t.name.clone())),
            ),
            _ => {}
        }
        out
    }

    /// The variant choices of the Prices tab: stored variants that are on (and `keep`).
    #[must_use]
    pub fn price_variants(&self, keep: Option<&str>) -> Vec<(String, String)> {
        let Some(f) = &self.form else {
            return Vec::new();
        };
        f.form
            .data
            .combos
            .iter()
            .filter(|c| c.stored && (c.on || keep == Some(c.variant_id.as_str())))
            .map(|c| (c.variant_id.clone(), f.form.variant_label(&c.variant_id)))
            .collect()
    }

    /// The unit choices of the Prices tab: stored units that are not archived (and `keep`).
    #[must_use]
    pub fn price_units(&self, keep: Option<&str>) -> Vec<(String, String)> {
        let Some(f) = &self.form else {
            return Vec::new();
        };
        f.form
            .data
            .units
            .iter()
            .filter(|u| u.stored && (!u.archived || keep == Some(u.id.as_str())))
            .map(|u| (u.id.clone(), u.name.clone()))
            .collect()
    }

    fn load_lists(&mut self) -> Vec<Effect> {
        self.pricing.loading = true;
        let mut effects = vec![self.price_call(Call::Lists)];
        // Group and tier names (and pickers) come from the customer section.
        let all = self.may(CUSTOMER_MANAGE);
        effects.push(self.customer_call(CustomerCall::Groups(all)));
        effects.push(self.customer_call(CustomerCall::Tiers(all)));
        effects
    }

    fn load_rates(&mut self) -> Vec<Effect> {
        let class = self
            .reference
            .tax_classes
            .get(self.pricing.tax.cursor)
            .map(|c| c.id.clone());
        class.map_or_else(Vec::new, |c| vec![self.price_call(Call::TaxRates(c))])
    }

    /// Entering a screen of the section (from `enter`).
    pub(super) fn enter_pricing(&mut self, target: Screen) -> Vec<Effect> {
        match target {
            Screen::PriceLists => {
                self.focus("list");
                self.load_lists()
            }
            Screen::TaxRates => {
                let today = self.today();
                let t = &mut self.pricing.tax;
                t.issues.clear();
                t.notice = None;
                if t.valid_from.is_empty() {
                    t.valid_from = date_text(today);
                }
                self.price_bump();
                self.focus("tax-rate");
                self.load_rates()
            }
            Screen::PricingSettings => {
                self.pricing.settings.notice = None;
                self.focus("list");
                vec![self.price_call(Call::Settings)]
            }
            _ => Vec::new(),
        }
    }

    /// Keys on the pricing screens (after the F-keys of the header).
    pub(super) fn pricing_capture(&mut self, p: Press, screen: Screen) -> Step {
        for (i, tab) in TABS.iter().enumerate() {
            let digit = char::from_digit(u32::try_from(i + 1).unwrap_or(0), 10).unwrap_or('0');
            if p.ctrl_char(digit) {
                return Step::handled(self.go(*tab));
            }
        }
        if p.f(5) {
            return Step::handled(self.pricing_action("price-refresh").unwrap_or_default());
        }
        let action = if p.ctrl_char('s') {
            match screen {
                Screen::PriceLists => "price-list-save",
                Screen::TaxRates => "tax-rate-add",
                _ => "pricing-settings-save",
            }
        } else if screen == Screen::PriceLists && p.ctrl_char('n') {
            "price-list-new"
        } else if screen == Screen::PriceLists && p.ctrl_shift_char('a') {
            "price-list-archive"
        } else if p.plain(PressKey::Escape) {
            return Step::handled(if screen == Screen::PriceLists {
                self.go(Screen::Products)
            } else {
                self.go(Screen::PriceLists)
            });
        } else if p.plain(PressKey::Up) || p.plain(PressKey::Down) {
            let up = p.plain(PressKey::Up);
            let (cursor, count, kind) = match screen {
                Screen::PriceLists => (
                    self.pricing.editor.cursor,
                    self.pricing.lists.len(),
                    "price-list",
                ),
                Screen::TaxRates => (
                    self.pricing.tax.cursor,
                    self.reference.tax_classes.len(),
                    "tax-class",
                ),
                _ => return Step::default(),
            };
            let row = if up {
                cursor.saturating_sub(1)
            } else {
                (cursor + 1).min(count.saturating_sub(1))
            };
            return Step::handled(self.pricing_pick(kind, row).unwrap_or_default());
        } else {
            return Step::default();
        };
        Step::handled(self.pricing_action(action).unwrap_or_default())
    }

    /// Keys of the product form's Prices tab; `None` when not one of them.
    pub(super) fn prices_tab_capture(&mut self, p: Press) -> Option<Step> {
        if self.form.as_ref().is_none_or(|f| f.form.tab != Tab::Prices) {
            return None;
        }
        let action = if p.ctrl_char('n') {
            "price-new"
        } else if p.ctrl_shift_char('n') {
            "price-schedule"
        } else if p.ctrl && !p.shift && p.key == PressKey::Enter {
            "price-save"
        } else if p.ctrl_shift_char('d') {
            "price-delete"
        } else if p.ctrl_char('t') {
            "price-add-tier"
        } else if p.ctrl_char('l') {
            "price-next-list"
        } else if p.plain(PressKey::Up) || p.plain(PressKey::Down) {
            let pp = self.pricing.product.as_ref()?;
            let count = pp.rows().len();
            let row = if p.plain(PressKey::Up) {
                pp.cursor.saturating_sub(1)
            } else {
                (pp.cursor + 1).min(count.saturating_sub(1))
            };
            return Some(Step::handled(
                self.pricing_pick("price-entry", row).unwrap_or_default(),
            ));
        } else {
            return None;
        };
        Some(Step::handled(
            self.pricing_action(action).unwrap_or_default(),
        ))
    }

    /// Opens the Prices tab of the open product (loads its prices).
    fn open_prices_tab(&mut self) -> Vec<Effect> {
        let Some(f) = &mut self.form else {
            return Vec::new();
        };
        f.form.tab = Tab::Prices;
        let id = f.form.id.clone();
        let is_new = f.form.is_new;
        self.focus("list");
        if is_new {
            self.pricing.product = None;
            return Vec::new();
        }
        if self
            .pricing
            .product
            .as_ref()
            .is_none_or(|p| p.product_id != id)
        {
            self.pricing.product = Some(ProductPricing::new(&id));
        }
        self.reload_prices()
    }

    /// After a product save: a new product saved on the Prices tab gets its prices.
    pub(super) fn prices_after_save(&mut self) -> Vec<Effect> {
        let show = self
            .form
            .as_ref()
            .filter(|f| f.form.tab == Tab::Prices && !f.form.is_new)
            .map(|f| f.form.id.clone());
        match show {
            Some(id)
                if self
                    .pricing
                    .product
                    .as_ref()
                    .is_none_or(|p| p.product_id != id) =>
            {
                self.open_prices_tab()
            }
            _ => Vec::new(),
        }
    }

    fn reload_prices(&mut self) -> Vec<Effect> {
        let Some(pp) = &mut self.pricing.product else {
            return Vec::new();
        };
        pp.loading = true;
        let id = pp.product_id.clone();
        vec![self.price_call(Call::ProductPrices(id))]
    }

    /// The pricing buttons; `None` when `id` is not one of them.
    #[allow(clippy::too_many_lines)] // one arm per button
    pub(super) fn pricing_action(&mut self, id: &str) -> Option<Vec<Effect>> {
        let effects = match id {
            "nav-prices" | "tab-price-lists" => self.go(Screen::PriceLists),
            "tab-tax-rates" => self.go(Screen::TaxRates),
            "tab-pricing-settings" => self.go(Screen::PricingSettings),
            "tab-3" if self.screen() == Screen::Form => self.open_prices_tab(),
            "price-refresh" => match self.screen() {
                Screen::PriceLists => self.load_lists(),
                Screen::TaxRates => self.load_rates(),
                Screen::PricingSettings => vec![self.price_call(Call::Settings)],
                _ => self.reload_prices(),
            },
            "price-list-new" => {
                if !self.can_price() {
                    self.pricing.editor.notice = Some(Self::price_denied());
                    return Some(Vec::new());
                }
                self.pricing.editor = ListEditor {
                    cursor: self.pricing.editor.cursor,
                    ..ListEditor::default()
                };
                self.price_bump();
                self.focus("price-list-name");
                Vec::new()
            }
            "price-list-save" => self.save_list(),
            "price-list-archive" => {
                let e = &mut self.pricing.editor;
                if e.editing.is_none() {
                } else if e.is_default() {
                    e.notice = Some(Notice::bad("default_list_fixed"));
                } else if self.may(PRICE_MANAGE) {
                    self.confirm = Some(Confirm::PriceListArchive);
                } else {
                    self.pricing.editor.notice = Some(Self::price_denied());
                }
                Vec::new()
            }
            "price-find-customer" => self.find_customer(),
            "tax-rate-add" => self.add_rate(),
            "pricing-settings-save" => {
                if !self.can_price() {
                    self.pricing.settings.notice = Some(Self::price_denied());
                    return Some(Vec::new());
                }
                let s = &self.pricing.settings;
                match &s.stored {
                    None => Vec::new(),
                    Some(stored) if stored.tax_inclusive != s.edit.tax_inclusive => {
                        self.confirm = Some(Confirm::TaxMode);
                        Vec::new()
                    }
                    Some(_) => vec![self.price_call(Call::PutSettings(s.edit.clone()))],
                }
            }
            "price-new" | "price-schedule" | "price-save" | "price-delete" | "price-add-tier"
            | "price-next-list" | "price-cancel" => self.price_tab_action(id),
            _ => return None,
        };
        Some(effects)
    }

    /// The Prices tab's buttons.
    fn price_tab_action(&mut self, id: &str) -> Vec<Effect> {
        let can = self.can_price();
        let today = self.today();
        let first_variant = self.price_variants(None).first().map(|v| v.0.clone());
        let first_unit = self.price_units(None).first().map(|u| u.0.clone());
        let Some(pp) = &mut self.pricing.product else {
            return Vec::new();
        };
        let writes = !matches!(id, "price-next-list" | "price-cancel");
        if writes && !can {
            pp.notice = Some(Self::price_denied());
            return Vec::new();
        }
        match id {
            "price-next-list" => {
                let lists = pp.lists();
                if !lists.is_empty() {
                    let at = lists.iter().position(|l| l.id == pp.list_id).unwrap_or(0);
                    let next = lists[(at + 1) % lists.len()].id.clone();
                    self.price_pick_list(next);
                }
            }
            "price-cancel" => {
                pp.editor = None;
                pp.notice = None;
                self.price_bump();
            }
            "price-new" => {
                pp.editor = Some(EntryEditor {
                    id: new_id(),
                    list_id: pp.list_id.clone(),
                    is_new: true,
                    variant: first_variant,
                    unit: first_unit,
                    valid_from: String::new(),
                    valid_to: String::new(),
                    tiers: vec![TierRow {
                        min_qty: "1".to_owned(),
                        ..TierRow::default()
                    }],
                    ends: None,
                    issues: Vec::new(),
                });
                pp.notice = None;
                self.price_bump();
                self.focus("price-value-0");
            }
            "price-schedule" => {
                let Some(old) = pp
                    .editor
                    .as_ref()
                    .filter(|e| !e.is_new)
                    .and_then(|e| pp.entry(&e.id))
                    .cloned()
                else {
                    pp.notice = Some(Notice::bad("pick_entry_first"));
                    return Vec::new();
                };
                // The new price starts tomorrow, or the day after the old one starts.
                let old_from = old
                    .valid_from
                    .as_deref()
                    .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
                let mut start = today + Days::new(1);
                if let Some(f) = old_from
                    && f >= start
                {
                    start = f + Days::new(1);
                }
                let mut editor = EntryEditor::of(&old);
                editor.id = new_id();
                editor.is_new = true;
                editor.valid_from = date_text(start);
                editor.ends = Some(old.id);
                pp.editor = Some(editor);
                pp.notice = Some(notice("schedule_hint", "", Tone::Neutral));
                self.price_bump();
                self.focus("price-value-0");
            }
            "price-add-tier" => {
                if let Some(e) = &mut pp.editor {
                    if e.tiers.len() < MAX_TIERS {
                        e.tiers.push(TierRow::default());
                        self.price_bump();
                    } else {
                        pp.notice = Some(Notice::bad("invalid_tiers"));
                    }
                }
            }
            "price-delete" => {
                if pp.editor.as_ref().is_some_and(|e| !e.is_new) {
                    self.confirm = Some(Confirm::PriceEntryDelete);
                } else {
                    pp.notice = Some(Notice::bad("pick_entry_first"));
                }
            }
            _ => return self.save_entry(),
        }
        Vec::new()
    }

    fn price_pick_list(&mut self, list: String) {
        if let Some(pp) = &mut self.pricing.product {
            pp.list_id = list;
            pp.cursor = 0;
            pp.editor = None;
            pp.notice = None;
            self.price_bump();
        }
        self.price_select_entry(0);
    }

    /// Puts entry `row` of the list on view into the editor.
    fn price_select_entry(&mut self, row: usize) {
        let Some(pp) = &mut self.pricing.product else {
            return;
        };
        let Some(entry) = pp.rows().get(row).map(|e| (*e).clone()) else {
            return;
        };
        pp.cursor = row;
        pp.editor = Some(EntryEditor::of(&entry));
        pp.notice = None;
        self.price_bump();
    }

    fn save_entry(&mut self) -> Vec<Effect> {
        let Some(pp) = &mut self.pricing.product else {
            return Vec::new();
        };
        let Some(e) = &mut pp.editor else {
            return Vec::new();
        };
        if pp.saving {
            return Vec::new();
        }
        let body = match e.body() {
            Ok(body) => body,
            Err(issues) => {
                e.issues = issues;
                pp.notice = Some(Notice::bad("fix_errors"));
                return Vec::new();
            }
        };
        e.issues.clear();
        let list = e.list_id.clone();
        let id = e.id.clone();
        // Schedule a new price: end the old entry the day before the new one starts.
        let ending = e
            .ends
            .as_ref()
            .and_then(|old| pp.prices.as_ref()?.entries.iter().find(|x| x.id == *old))
            .cloned();
        if let Some(old) = ending {
            let start = body
                .valid_from
                .as_deref()
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            let old_from = old
                .valid_from
                .as_deref()
                .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
            let end = start.and_then(|s| s.checked_sub_days(Days::new(1)));
            let Some(end) = end.filter(|end| old_from.is_none_or(|f| f <= *end)) else {
                e.issues = vec![Issue::new("valid_from", "schedule_after")];
                pp.notice = Some(Notice::bad("fix_errors"));
                return Vec::new();
            };
            let old_body = PriceEntryBody {
                variant_id: old.variant_id.clone(),
                unit_id: old.unit_id.clone(),
                valid_from: old.valid_from.clone(),
                valid_to: old.valid_to.clone(),
                tiers: old.tiers.clone(),
            };
            let ended = PriceEntryBody {
                valid_to: Some(date_text(end)),
                ..old_body.clone()
            };
            pp.saving = true;
            pp.notice = None;
            pp.rollback = Some((old.list_id.clone(), old.id.clone(), old_body));
            pp.pending = Some((list, id, body));
            let call = Call::PutEntry {
                list: old.list_id,
                id: old.id,
                body: ended,
            };
            return vec![self.price_call(call)];
        }
        pp.saving = true;
        pp.notice = None;
        vec![self.price_call(Call::PutEntry { list, id, body })]
    }

    fn save_list(&mut self) -> Vec<Effect> {
        if !self.can_price() {
            self.pricing.editor.notice = Some(Self::price_denied());
            return Vec::new();
        }
        let stored = self
            .pricing
            .editor
            .editing
            .as_ref()
            .and_then(|id| self.pricing.lists.iter().find(|l| l.id == *id))
            .cloned();
        let e = &mut self.pricing.editor;
        if e.saving {
            return Vec::new();
        }
        let mut issues = Vec::new();
        let name = e.name.trim().to_owned();
        if name.is_empty() || name.chars().count() > 100 {
            issues.push(Issue::new("name", "invalid_list_name"));
        }
        if !e.is_default() && e.target.is_none() {
            issues.push(Issue::new("target", "pick_target"));
        }
        let rule = if e.is_default() {
            Ok(None)
        } else {
            rule_of(e.rule_mode, &e.rule_value)
        };
        if rule.is_err() {
            issues.push(Issue::new("rule", "invalid_rule_value"));
        }
        if !issues.is_empty() {
            e.issues = issues;
            e.notice = Some(Notice::bad("fix_errors"));
            return Vec::new();
        }
        e.issues.clear();
        e.notice = None;
        e.saving = true;
        let body = PriceListBody {
            kind: e.kind.clone(),
            target_id: e.target.clone(),
            name,
            active: e.active || e.is_default(),
            rule: rule.ok().flatten(),
            archived: stored.is_some_and(|s| s.archived),
        };
        let id = e.editing.clone().unwrap_or_else(new_id);
        vec![self.price_call(Call::PutList { id, body })]
    }

    fn find_customer(&mut self) -> Vec<Effect> {
        let e = &mut self.pricing.editor;
        let text = e.customer.trim().to_owned();
        if e.editing.is_some() || e.kind != "customer" || text.is_empty() {
            return Vec::new();
        }
        e.target = None;
        e.notice = None;
        vec![self.price_call(Call::FindCustomer(text))]
    }

    fn add_rate(&mut self) -> Vec<Effect> {
        if !self.can_price() {
            self.pricing.tax.notice = Some(Self::price_denied());
            return Vec::new();
        }
        let class = self
            .reference
            .tax_classes
            .get(self.pricing.tax.cursor)
            .map(|c| c.id.clone());
        let t = &mut self.pricing.tax;
        let Some(class) = class else {
            return Vec::new();
        };
        t.issues.clear();
        let rate = parse_pct(&t.rate).filter(|bp| (0..=10_000).contains(bp));
        let from = parse_date(&t.valid_from);
        if rate.is_none() {
            t.issues.push(Issue::new("rate", "invalid_rate"));
        }
        if !matches!(from, Ok(Some(_))) {
            t.issues.push(Issue::new("valid_from", "invalid_date"));
        }
        let (Some(rate), Ok(Some(from))) = (rate, from) else {
            t.notice = Some(Notice::bad("fix_errors"));
            return Vec::new();
        };
        t.notice = None;
        let rate = TaxRate {
            rate_bp: u16::try_from(rate).unwrap_or_default(),
            valid_from: date_text(from),
        };
        vec![self.price_call(Call::PutTaxRate { class, rate })]
    }

    /// A confirmed pricing dialog.
    pub(super) fn pricing_confirmed(&mut self, confirm: Confirm) -> Vec<Effect> {
        match confirm {
            Confirm::PriceListArchive => {
                let Some(id) = self.pricing.editor.editing.clone() else {
                    return Vec::new();
                };
                self.pricing.editor.saving = true;
                vec![self.price_call(Call::ArchiveList { id, key: key() })]
            }
            Confirm::PriceEntryDelete => {
                let Some((list, id)) = self.pricing.product.as_mut().and_then(|pp| {
                    let e = pp.editor.as_ref().filter(|e| !e.is_new)?;
                    pp.saving = true;
                    Some((e.list_id.clone(), e.id.clone()))
                }) else {
                    return Vec::new();
                };
                vec![self.price_call(Call::DeleteEntry { list, id })]
            }
            Confirm::TaxMode => {
                let body = self.pricing.settings.edit.clone();
                vec![self.price_call(Call::PutSettings(body))]
            }
            _ => Vec::new(),
        }
    }

    /// Text field edits of the pricing screens.
    pub(super) fn pricing_edit(
        &mut self,
        kind: &str,
        row: usize,
        field: &str,
        value: String,
    ) -> Vec<Effect> {
        match kind {
            "price-list" => {
                let e = &mut self.pricing.editor;
                match field {
                    "name" => e.name = value,
                    "rule" => e.rule_value = value,
                    "customer" => {
                        e.customer = value;
                        e.target = None;
                    }
                    _ => {}
                }
            }
            "tax-rate" => {
                let t = &mut self.pricing.tax;
                match field {
                    "rate" => t.rate = value,
                    "valid_from" => t.valid_from = value,
                    _ => {}
                }
            }
            "price-entry" | "price-tier" => {
                let Some(e) = self
                    .pricing
                    .product
                    .as_mut()
                    .and_then(|p| p.editor.as_mut())
                else {
                    return Vec::new();
                };
                match (kind, field) {
                    ("price-entry", "valid_from") => e.valid_from = value,
                    ("price-entry", "valid_to") => e.valid_to = value,
                    ("price-tier", "min_qty") => {
                        if let Some(t) = e.tiers.get_mut(row) {
                            t.min_qty = value;
                        }
                    }
                    ("price-tier", "value") => {
                        if let Some(t) = e.tiers.get_mut(row) {
                            t.value = value;
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Check boxes of the pricing screens; `None` when not one of them.
    pub(super) fn pricing_toggle(&mut self, kind: &str, field: &str, on: bool) -> Option<()> {
        let can = self.can_price();
        match (kind, field) {
            ("price-list", "active") => {
                let e = &mut self.pricing.editor;
                if can && !e.is_default() {
                    e.active = on;
                }
            }
            ("pricing-settings", "tax_inclusive") => {
                if can {
                    self.pricing.settings.edit.tax_inclusive = on;
                }
            }
            ("pricing-settings", "tax_registered") => {
                if can {
                    self.pricing.settings.edit.tax_registered = on;
                }
            }
            _ => return None,
        }
        Some(())
    }

    /// Pickers of the pricing screens; `None` when not one of them.
    pub(super) fn pricing_choose(
        &mut self,
        kind: &str,
        row: usize,
        field: &str,
        index: usize,
    ) -> Option<Vec<Effect>> {
        let can = self.can_price();
        match (kind, field) {
            ("price-list", "kind") => {
                let e = &mut self.pricing.editor;
                if can
                    && e.editing.is_none()
                    && let Some(k) = KINDS.get(index)
                    && e.kind != *k
                {
                    (*k).clone_into(&mut e.kind);
                    e.target = None;
                    e.customer.clear();
                    self.price_bump();
                }
            }
            ("price-list", "target") => {
                let picked = self.target_choices().get(index).cloned();
                let e = &mut self.pricing.editor;
                if can
                    && e.editing.is_none()
                    && let Some((id, name)) = picked
                {
                    e.target = id;
                    if e.name.trim().is_empty() && e.target.is_some() {
                        e.name = name;
                        self.price_bump();
                    }
                }
            }
            ("price-list", "rule") => {
                let e = &mut self.pricing.editor;
                if can && !e.is_default() && index <= RULE_AMOUNT {
                    e.rule_mode = index;
                }
            }
            ("pricing-settings", "cash_rounding") => {
                if can && let Some(r) = ROUNDINGS.get(index) {
                    (*r).clone_into(&mut self.pricing.settings.edit.cash_rounding);
                }
            }
            ("prices", "list") => {
                let list = self
                    .pricing
                    .product
                    .as_ref()
                    .and_then(|p| p.lists().get(index).map(|l| l.id.clone()));
                if let Some(list) = list {
                    self.price_pick_list(list);
                }
            }
            ("price-entry", "variant" | "unit") => {
                let keep = self
                    .pricing
                    .product
                    .as_ref()
                    .and_then(|p| p.editor.as_ref())
                    .map(|e| (e.variant.clone(), e.unit.clone()));
                let Some((variant, unit)) = keep else {
                    return Some(Vec::new());
                };
                let picked = if field == "variant" {
                    self.price_variants(variant.as_deref()).get(index).cloned()
                } else {
                    self.price_units(unit.as_deref()).get(index).cloned()
                };
                if let (Some((id, _)), Some(e)) = (
                    picked,
                    self.pricing
                        .product
                        .as_mut()
                        .and_then(|p| p.editor.as_mut()),
                ) {
                    if field == "variant" {
                        e.variant = Some(id);
                    } else {
                        e.unit = Some(id);
                    }
                }
            }
            ("price-tier", "mode") => {
                if let Some(t) = self
                    .pricing
                    .product
                    .as_mut()
                    .and_then(|p| p.editor.as_mut())
                    .and_then(|e| e.tiers.get_mut(row))
                    && index <= RULE_AMOUNT
                {
                    t.mode = index;
                }
            }
            _ => return None,
        }
        Some(Vec::new())
    }

    /// Row clicks of the pricing screens; `None` when not one of them.
    pub(super) fn pricing_pick(&mut self, kind: &str, row: usize) -> Option<Vec<Effect>> {
        let effects = match kind {
            "price-list" => {
                self.select_list(row);
                Vec::new()
            }
            "tax-class" => {
                if row >= self.reference.tax_classes.len() {
                    return Some(Vec::new());
                }
                let t = &mut self.pricing.tax;
                if t.cursor != row {
                    t.cursor = row;
                    t.rates = None;
                    t.issues.clear();
                    t.notice = None;
                }
                self.load_rates()
            }
            "price-entry" => {
                self.price_select_entry(row);
                Vec::new()
            }
            "remove-tier" => {
                if self.can_price()
                    && let Some(e) = self
                        .pricing
                        .product
                        .as_mut()
                        .and_then(|p| p.editor.as_mut())
                    && row < e.tiers.len()
                    && e.tiers.len() > 1
                {
                    e.tiers.remove(row);
                    e.issues.clear();
                    self.price_bump();
                }
                Vec::new()
            }
            _ => return None,
        };
        Some(effects)
    }

    fn select_list(&mut self, row: usize) {
        let Some(list) = self.pricing.lists.get(row).cloned() else {
            return;
        };
        self.pricing.editor = ListEditor::of(&list, row);
        self.price_bump();
    }

    /// `price.changed`: what is on view reloads.
    pub(super) fn pricing_event(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::PriceLists if !self.pricing.editor.saving => self.load_lists(),
            Screen::TaxRates => self.load_rates(),
            Screen::Form
                if self
                    .form
                    .as_ref()
                    .is_some_and(|f| f.form.tab == Tab::Prices)
                    && self
                        .pricing
                        .product
                        .as_ref()
                        .is_some_and(|p| !p.saving && !p.loading) =>
            {
                self.reload_prices()
            }
            _ => Vec::new(),
        }
    }

    /// The answer of a pricing call.
    #[allow(clippy::too_many_lines)] // one arm per answer
    pub(super) fn pricing_reply(&mut self, reply: Reply) -> Vec<Effect> {
        let saved_rate = matches!(reply, Reply::TaxRateSaved(..));
        match reply {
            Reply::Lists(result) => {
                self.pricing.loading = false;
                match result {
                    Ok(lists) => {
                        self.pricing.lists = lists;
                        let editing = self.pricing.editor.editing.clone();
                        let row = editing.map_or(Some(self.pricing.editor.cursor), |id| {
                            self.pricing.lists.iter().position(|l| l.id == id)
                        });
                        let is_new = self.pricing.editor.editing.is_none()
                            && !self.pricing.editor.name.is_empty();
                        if !is_new && let Some(row) = row {
                            let n = self.pricing.editor.notice.take();
                            self.select_list(row.min(self.pricing.lists.len().saturating_sub(1)));
                            self.pricing.editor.notice = n;
                        }
                        let unknown: Vec<String> = self
                            .pricing
                            .lists
                            .iter()
                            .filter(|l| l.kind == "customer")
                            .filter_map(|l| l.target_id.clone())
                            .filter(|c| !self.pricing.customer_names.contains_key(c))
                            .collect();
                        return unknown
                            .into_iter()
                            .map(|c| self.price_call(Call::Customer(c)))
                            .collect();
                    }
                    Err(e) => self.pricing.editor.notice = self.failed(&e),
                }
            }
            Reply::ListSaved(archive, result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let lists = self.pricing.lists.clone();
                let e = &mut self.pricing.editor;
                e.saving = false;
                match result {
                    Ok(list) => {
                        if archive {
                            *e = ListEditor {
                                cursor: e.cursor,
                                notice: Some(notice("price_list_archived", list.name, Tone::Good)),
                                ..ListEditor::default()
                            };
                        } else {
                            e.editing = Some(list.id);
                            e.notice = Some(notice("saved", "", Tone::Good));
                        }
                        return self.load_lists();
                    }
                    Err(err) => {
                        let p = err.problem();
                        e.issues.clear();
                        e.notice = failed;
                        match (err.code(), p.and_then(Problem::field)) {
                            (Some("price_list_conflict"), _) => {
                                let other = p
                                    .and_then(|p| p.extra.get("price_list_id"))
                                    .and_then(|v| v.as_str())
                                    .and_then(|id| lists.iter().find(|l| l.id == id))
                                    .map(|l| l.name.clone())
                                    .unwrap_or_default();
                                e.notice = Some(notice("price_list_conflict", other, Tone::Bad));
                            }
                            (Some("invalid_rule"), _) => {
                                e.issues.push(Issue::new("rule", "invalid_rule"));
                                e.notice = Some(Notice::bad("fix_errors"));
                            }
                            (Some("invalid_reference"), _) => {
                                e.notice = Some(Notice::bad("price_target_gone"));
                            }
                            (Some("invalid_field"), Some("name")) => {
                                e.issues.push(Issue::new("name", "invalid_list_name"));
                                e.notice = Some(Notice::bad("fix_errors"));
                            }
                            _ => {}
                        }
                    }
                }
            }
            Reply::ProductPrices(product, result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let Some(pp) = self
                    .pricing
                    .product
                    .as_mut()
                    .filter(|p| p.product_id == product)
                else {
                    return Vec::new();
                };
                pp.loading = false;
                match result {
                    Ok(prices) => {
                        if !prices.lists.iter().any(|l| l.id == pp.list_id) {
                            pp.list_id = prices
                                .lists
                                .first()
                                .map(|l| l.id.clone())
                                .unwrap_or_default();
                        }
                        pp.prices = Some(prices);
                        // The editor keeps its entry; a new one stays as typed.
                        let kept = match &pp.editor {
                            Some(e) if e.is_new => None,
                            Some(e) => Some(pp.rows().iter().position(|x| x.id == e.id)),
                            None => Some(None),
                        };
                        let count = pp.rows().len();
                        pp.cursor = pp.cursor.min(count.saturating_sub(1));
                        match kept {
                            Some(Some(row)) => {
                                let n = pp.notice.take();
                                self.price_select_entry(row);
                                if let Some(pp) = &mut self.pricing.product {
                                    pp.notice = n;
                                }
                            }
                            Some(None) => {
                                let n = pp.notice.take();
                                let cursor = pp.cursor;
                                pp.editor = None;
                                self.price_select_entry(cursor);
                                if let Some(pp) = &mut self.pricing.product {
                                    pp.notice = n;
                                }
                                self.price_bump();
                            }
                            None => {}
                        }
                    }
                    Err(_) => pp.notice = failed,
                }
            }
            Reply::EntrySaved(result) => return self.entry_saved(result),
            Reply::EntryDeleted(result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let Some(pp) = &mut self.pricing.product else {
                    return Vec::new();
                };
                pp.saving = false;
                match result {
                    Ok(()) => {
                        pp.editor = None;
                        pp.notice = Some(notice("deleted_done", "", Tone::Good));
                        return self.reload_prices();
                    }
                    Err(_) => pp.notice = failed,
                }
            }
            Reply::TaxRates(class, result) | Reply::TaxRateSaved(class, result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let t = &mut self.pricing.tax;
                match result {
                    Ok(rates) => {
                        if saved_rate {
                            t.rate.clear();
                            t.notice = Some(notice("saved", "", Tone::Good));
                            self.pricing.serial += 1;
                        }
                        self.pricing.tax.rates = Some((class, rates));
                    }
                    Err(e) => {
                        t.notice = failed;
                        if e.problem().and_then(Problem::field) == Some("valid_from") {
                            t.issues.push(Issue::new("valid_from", "invalid_date"));
                        }
                    }
                }
            }
            Reply::Settings(saved, result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let s = &mut self.pricing.settings;
                match result {
                    Ok(stored) => {
                        s.edit = stored.clone();
                        s.stored = Some(stored);
                        s.notice = saved.then(|| notice("saved", "", Tone::Good));
                    }
                    Err(_) => s.notice = failed,
                }
            }
            Reply::Customer(id, result) => {
                if let Ok(c) = result {
                    self.pricing
                        .customer_names
                        .insert(id, format!("{} ({})", c.name, c.display_code));
                }
            }
            Reply::Found(text, result) => {
                let failed = result.as_ref().err().and_then(|e| {
                    if e.code() == Some("not_found") || e.code() == Some("invalid_code") {
                        Some(Notice::bad("partner_not_found"))
                    } else {
                        self.failed(e)
                    }
                });
                let e = &mut self.pricing.editor;
                if e.customer.trim() != text || e.editing.is_some() {
                    return Vec::new();
                }
                match result {
                    Ok(c) => {
                        let label = format!("{} ({})", c.name, c.display_code);
                        if e.name.trim().is_empty() {
                            e.name.clone_from(&c.name);
                            self.pricing.serial += 1;
                        }
                        let e = &mut self.pricing.editor;
                        e.target = Some(c.id.clone());
                        e.issues.retain(|i| i.field != "target");
                        self.pricing.customer_names.insert(c.id, label);
                    }
                    Err(_) => e.notice = failed,
                }
            }
        }
        Vec::new()
    }

    /// An entry `PUT` answered (a plain save, or a step of "schedule a new price").
    fn entry_saved(&mut self, result: Result<PriceEntry, ClientError>) -> Vec<Effect> {
        let failed = result.as_ref().err().and_then(|e| self.failed(e));
        let Some(pp) = &mut self.pricing.product else {
            return Vec::new();
        };
        if pp.undoing {
            // The old entry's end is back; the error of the new entry stays on view.
            pp.undoing = false;
            pp.saving = false;
            if result.is_err() {
                pp.notice = failed;
            }
            return self.reload_prices();
        }
        match result {
            Ok(entry) => {
                if let Some((list, id, body)) = pp.pending.take() {
                    // The old entry has ended: now the new one.
                    return vec![self.price_call(Call::PutEntry { list, id, body })];
                }
                pp.saving = false;
                pp.rollback = None;
                if let Some(e) = &mut pp.editor
                    && e.id == entry.id
                {
                    e.is_new = false;
                    e.ends = None;
                }
                pp.notice = Some(notice("saved", "", Tone::Good));
                self.reload_prices()
            }
            Err(err) => {
                let others: Vec<PriceEntry> = pp
                    .prices
                    .as_ref()
                    .map(|p| p.entries.clone())
                    .unwrap_or_default();
                let first_step = pp.pending.take().is_some();
                // The new entry was refused after the old one ended: put the old end back.
                let undo = if first_step {
                    pp.rollback = None;
                    None
                } else {
                    pp.rollback.take()
                };
                pp.saving = undo.is_some();
                pp.undoing = undo.is_some();
                pp.notice = failed;
                if let Some(e) = &mut pp.editor {
                    e.issues.clear();
                    let p = err.problem();
                    match (err.code(), p.and_then(Problem::field)) {
                        (Some("price_overlap"), _) => {
                            let other = p
                                .and_then(|p| p.extra.get("entry_id"))
                                .and_then(|v| v.as_str())
                                .and_then(|id| others.iter().find(|x| x.id == id))
                                .map(|x| dates_text(x.valid_from.as_deref(), x.valid_to.as_deref()))
                                .unwrap_or_default();
                            pp.notice = Some(notice("price_overlap", other, Tone::Bad));
                            e.issues.push(Issue::new("valid_from", "dates_overlap"));
                        }
                        (Some("invalid_rule"), _) => {
                            pp.notice = Some(Notice::bad("invalid_rule"));
                        }
                        (Some("invalid_reference"), _) => {
                            pp.notice = Some(Notice::bad("price_reference_gone"));
                        }
                        (Some("invalid_field"), Some(field)) => {
                            let issue = match field {
                                "valid_from" => Some(Issue::new("valid_from", "invalid_date")),
                                "valid_to" => Some(Issue::new("valid_to", "dates_reversed")),
                                f if f.starts_with("tiers.min_qty") => {
                                    Some(Issue::new("tiers", "invalid_tier_qty"))
                                }
                                f if f.starts_with("tiers.price") => {
                                    Some(Issue::new("tiers", "invalid_tier_price"))
                                }
                                f if f.starts_with("tiers") => {
                                    Some(Issue::new("tiers", "invalid_tiers"))
                                }
                                _ => None,
                            };
                            if let Some(issue) = issue {
                                e.issues.push(issue);
                                pp.notice = Some(Notice::bad("fix_errors"));
                            }
                        }
                        _ => {}
                    }
                }
                undo.map(|(list, id, body)| {
                    vec![self.price_call(Call::PutEntry { list, id, body })]
                })
                .unwrap_or_default()
            }
        }
    }
}

#[cfg(test)]
mod tests;
