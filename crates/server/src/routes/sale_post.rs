//! Rebuilding and checking a posted cart (spec 2.6 §5, §6, D1): the server repeats the quote, the
//! totals, every permission check and the payment before it stores a sale. Shared by
//! `PUT /sales/{id}` ([`super::sales`]) and the exchange sale of `PUT /returns/{id}`
//! ([`super::returns`]).
//!
//! - **Lines** name a variant, a unit and a quantity (thousandths), with an optional typed price
//!   (the cashier's price of an open-price item, or a manual override of any other line), a line
//!   discount (`pct_bp` or `amount`) and a scale label (`weight`, or `price` with its printed
//!   `label_amount`). Lines are kept as posted (no merging: the register already applied D8).
//! - **Quote** ([`build_sale`]): every line is priced again with the server's lists for the
//!   branch, the customer and today's branch-local date (2.3 `resolve_cart`), and the totals are
//!   computed with `core::sale::compute_totals`. When they differ from the posted `totals` the
//!   answer is `409 cart_stale` with the fresh quote (`quote.totals`, `quote.lines`).
//! - **Permissions** ([`BuiltSale::actions`]): `price.override` per overridden line (% below the
//!   resolved price), `sale.discount` per discounted line and `sale.cart_discount`, checked with
//!   `X-Override` by [`crate::overrides::authorize_all`] (one token per approval, comma
//!   separated).
//! - **Payment**: tenders are added in order (`core::sale::Payment`): inactive or archived
//!   tender types are refused (`invalid_field`, `tender_type`), references, only cash above the
//!   rest, the customer balance (the balance this node knows: on a branch node HQ's last synced
//!   balance plus the branch's own entries, 2.5 §11), then saved change with the 2.5 limits.

use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::{Permission, Requested};
use sultan_core::catalog::{ComponentSnapshot, ProductType, Qty, UnitKind};
use sultan_core::customer::BalanceSettings;
use sultan_core::ids::{
    BranchId, CashierSessionId, ProductId, RegisterId, SaleId, TaxClassId, UnitId, UserId,
    VariantId,
};
use sultan_core::money::Money;
use sultan_core::pricing::{self as core_pricing, LineMode, PricedLine, PricingError, SaleItem};
use sultan_core::sale::{
    BalanceContext, CartTotals, Discount, MAX_LINES, Payment, PriceMode, SaleError, ScaleLabel,
    StockLine, TotalsLine, check_qty, compute_totals, discount_bp, sale_movements,
};
use sultan_core::tax::rate_on;
use sultan_core::time::Timestamp;
use sultan_db::catalog::{self, ProductRecord};
use sultan_db::customers::CustomerRecord;
use sultan_db::pricing::{self, PricingSettings};
use sultan_db::sales::{self, NewSale, SaleLine, SaleTotals};
use sultan_db::{TokenHash, cashier_sessions, customers};
use utoipa::ToSchema;
use uuid::Uuid;

use super::parse_id;
use super::pricing_quote::{price_targets, today_at};
use crate::error::ApiError;
use crate::extract::RegisterSession;
use crate::overrides::Action;
use crate::state::AppState;

/// A manual discount: exactly one of `pct_bp` (basis points, at most 10 000) or `amount`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct DiscountBody {
    /// A percentage in basis points (`1000` = 10%).
    #[serde(default)]
    pub pct_bp: Option<u16>,
    /// An amount in minor units.
    #[serde(default)]
    pub amount: Option<i64>,
}

/// One cart line as posted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct SaleLineBody {
    /// The variant.
    pub variant_id: String,
    /// The unit it is sold in.
    pub unit_id: String,
    /// Quantity in thousandths of the unit (0 only for a price label without a derived qty).
    pub qty_thousandths: i64,
    /// The cashier's price of an open-price item, or a manual override (`price.override`).
    #[serde(default)]
    pub typed_price: Option<i64>,
    /// A line discount.
    #[serde(default)]
    pub discount: Option<DiscountBody>,
    /// `weight` or `price` when the line was scanned from a scale label.
    #[serde(default)]
    pub scale_label: Option<String>,
    /// The printed amount of a `price` label.
    #[serde(default)]
    pub label_amount: Option<i64>,
}

/// One tender as posted.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct TenderBody {
    /// The tender type.
    pub tender_type_id: String,
    /// The amount; for cash, the cash received.
    pub amount: i64,
    /// A reference (approval code, last 4 digits).
    #[serde(default)]
    pub reference: Option<String>,
}

/// The totals the register showed (spec 2.6 §5: compared with the fresh quote).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct TotalsBody {
    /// Σ gross.
    pub subtotal: i64,
    /// Σ line discounts.
    pub line_discount: i64,
    /// The cart discount.
    pub cart_discount: i64,
    /// Σ tax.
    pub tax: i64,
    /// What the customer pays before cash rounding.
    pub total: i64,
}

impl From<SaleTotals> for TotalsBody {
    fn from(t: SaleTotals) -> Self {
        Self {
            subtotal: t.subtotal.minor(),
            line_discount: t.line_discount.minor(),
            cart_discount: t.cart_discount.minor(),
            tax: t.tax.minor(),
            total: t.total.minor(),
        }
    }
}

/// Body of `PUT /sales/{id}` (and of an exchange sale).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct SaleBody {
    /// The attached customer.
    #[serde(default)]
    pub customer_id: Option<String>,
    /// Print as a tax invoice (D13): needs a customer with a tax id.
    #[serde(default)]
    pub tax_invoice: bool,
    /// The lines, 1–300.
    pub lines: Vec<SaleLineBody>,
    /// A whole-cart discount.
    #[serde(default)]
    pub cart_discount: Option<DiscountBody>,
    /// The tenders, in order (none for a zero total).
    #[serde(default)]
    pub tenders: Vec<TenderBody>,
    /// Change saved to the customer balance (2.5 §11).
    #[serde(default)]
    pub save_change: Option<i64>,
    /// The totals the register showed.
    pub totals: TotalsBody,
}

/// Where a document is made: the till's branch, register, open session and user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Till {
    /// The register's branch.
    pub branch: BranchId,
    /// The register.
    pub register: RegisterId,
    /// The caller's open cashier session on it.
    pub session: CashierSessionId,
    /// The caller.
    pub user: UserId,
}

/// The caller's open cashier session on this register (spec 2.6 §7, §8, D10); otherwise
/// `409 no_open_session`.
///
/// # Errors
/// See above, or the database error.
pub async fn till(state: &AppState, caller: &RegisterSession) -> Result<Till, ApiError> {
    let record = cashier_sessions::open_for_register(&state.db, caller.register.register).await?;
    match record {
        Some(r) if r.session.user() == caller.user.user.id => Ok(Till {
            branch: caller.register.branch,
            register: caller.register.register,
            session: r.session.id(),
            user: caller.user.user.id,
        }),
        _ => Err(sultan_core::cashier_session::SessionError::NoOpenSession.into()),
    }
}

/// The hash of a posted body, stored with the document (spec 2.6 §19 item 23).
#[must_use]
pub fn body_hash<T: Serialize>(body: &T) -> TokenHash {
    crate::token::hash(&serde_json::to_string(body).unwrap_or_default())
}

/// What the sales routes need of a sold item.
#[derive(Debug, Clone)]
pub struct Item {
    /// The variant.
    pub variant: VariantId,
    /// The unit.
    pub unit: UnitId,
    /// The receipt name (product + variant).
    pub name: String,
    /// The product type.
    pub product_type: ProductType,
    /// The product's base unit.
    pub base_unit: UnitId,
    /// The unit's factor.
    pub factor: Qty,
    /// Integer or decimal.
    pub unit_kind: UnitKind,
    /// The unit's short name for the receipt.
    pub unit_name: String,
    /// An open-price product.
    pub open_price: bool,
    /// The product's tax class.
    pub tax_class: TaxClassId,
    /// A bundle's components as of now.
    pub components: Vec<ComponentSnapshot>,
}

impl Item {
    /// As the pricing rules need it.
    #[must_use]
    pub const fn sale_item(&self) -> SaleItem {
        SaleItem {
            variant: self.variant,
            unit: self.unit,
            base_unit: self.base_unit,
            factor: self.factor,
        }
    }
}

fn reference_error(field: &str, detail: &str) -> ApiError {
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "invalid_reference",
        detail,
    )
    .with("field", field)
}

/// Loads the products of `variants` (name, units, product type) by variant.
async fn products(
    state: &AppState,
    variants: &[VariantId],
) -> Result<HashMap<VariantId, (String, ProductRecord)>, ApiError> {
    let labels = catalog::variant_labels(&state.db, variants).await?;
    let mut records: HashMap<ProductId, ProductRecord> = HashMap::new();
    let mut out = HashMap::new();
    for (variant, label) in labels {
        if !records.contains_key(&label.product)
            && let Some(record) = catalog::get_product(&state.db, label.product).await?
        {
            records.insert(label.product, record);
        }
        if let Some(record) = records.get(&label.product) {
            out.insert(variant, (label.name, record.clone()));
        }
    }
    Ok(out)
}

/// Loads every `(variant, unit)` as an [`Item`]. An unknown variant, or a unit not of its
/// product, is `422 invalid_reference` (`field`); an archived product or variant is
/// `422 archived` unless `allow_archived` (a return of something sold earlier).
///
/// # Errors
/// See above, or the database error.
pub async fn load_items(
    state: &AppState,
    wanted: &[(VariantId, UnitId)],
    field: &str,
    allow_archived: bool,
) -> Result<Vec<Item>, ApiError> {
    let mut variants: Vec<VariantId> = wanted.iter().map(|(v, _)| *v).collect();
    variants.sort();
    variants.dedup();
    let found = products(state, &variants).await?;
    let mut component_variants: Vec<VariantId> = found
        .values()
        .flat_map(|(_, r)| r.doc.draft.components.iter().map(|c| c.variant))
        .collect();
    component_variants.sort();
    component_variants.dedup();
    let components = products(state, &component_variants).await?;
    let mut out = Vec::with_capacity(wanted.len());
    for (variant, unit) in wanted {
        let (name, record) = found
            .get(variant)
            .ok_or_else(|| reference_error(field, "unknown variant"))?;
        let draft = &record.doc.draft;
        let archived = record.archived_at.is_some()
            || draft
                .variants
                .iter()
                .any(|v| v.id == *variant && v.archived);
        if archived && !allow_archived {
            return Err(ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "archived",
                "the product is archived",
            )
            .with("field", field));
        }
        let u = draft
            .units
            .units
            .iter()
            .find(|u| u.id == *unit)
            .ok_or_else(|| reference_error(field, "the unit is not one of the product's"))?;
        let mut snapshots = Vec::new();
        if draft.product_type == ProductType::Bundle {
            for c in &draft.components {
                let (_, cr) = components
                    .get(&c.variant)
                    .ok_or_else(|| reference_error(field, "unknown bundle component"))?;
                let cu = cr
                    .doc
                    .draft
                    .units
                    .units
                    .iter()
                    .find(|x| x.id == c.unit)
                    .ok_or_else(|| reference_error(field, "unknown bundle component unit"))?;
                snapshots.push(ComponentSnapshot {
                    variant: c.variant,
                    product_type: cr.doc.draft.product_type,
                    unit: c.unit,
                    qty: c.qty,
                    factor: cu.factor,
                });
            }
        }
        let unit_name = if u.short_name.is_empty() {
            u.name.clone()
        } else {
            u.short_name.clone()
        };
        out.push(Item {
            variant: *variant,
            unit: *unit,
            name: name.clone(),
            product_type: draft.product_type,
            base_unit: draft.units.base,
            factor: u.factor,
            unit_kind: u.kind,
            unit_name,
            open_price: draft.open_price,
            tax_class: record.doc.tax_class,
            components: snapshots,
        });
    }
    Ok(out)
}

/// What pricing needs once per request: the date, settings and tax rates.
#[derive(Debug, Clone)]
pub struct PriceContext {
    /// Today in the branch's time zone.
    pub date: NaiveDate,
    /// The pricing settings (tax mode, cash rounding).
    pub settings: PricingSettings,
    rates: std::collections::BTreeMap<TaxClassId, Vec<sultan_core::tax::TaxRate>>,
}

impl PriceContext {
    /// Loads the context for `branch`.
    ///
    /// # Errors
    /// The database error.
    pub async fn load(state: &AppState, branch: BranchId) -> Result<Self, ApiError> {
        Ok(Self {
            date: today_at(state, branch).await?,
            settings: pricing::get_pricing_settings(&state.db).await?,
            rates: pricing::tax_rates_by_class(&state.db).await?,
        })
    }

    /// The tax rate of `class` today, 0 when not tax-registered.
    #[must_use]
    pub fn rate(&self, class: TaxClassId) -> u16 {
        if !self.settings.tax.registered {
            return 0;
        }
        rate_on(self.rates.get(&class).map_or(&[], Vec::as_slice), self.date).unwrap_or(0)
    }
}

/// Prices `lines` with the lists of `branch` and `customer` (2.3 `resolve_cart`).
///
/// # Errors
/// `422 invalid_reference` (`customer_id`), or the database error.
pub async fn quote(
    state: &AppState,
    branch: BranchId,
    customer: Option<Uuid>,
    ctx: &PriceContext,
    lines: &[core_pricing::CartLine],
) -> Result<Vec<Result<PricedLine, PricingError>>, ApiError> {
    let targets = price_targets(state, branch, customer).await?;
    let mut variants: Vec<VariantId> = lines.iter().map(|l| l.item.variant).collect();
    variants.sort();
    variants.dedup();
    let lists = pricing::load_for_resolution(&state.db, &targets, &variants, ctx.date).await?;
    Ok(core_pricing::resolve_cart(&lists.chain(ctx.date), lines))
}

/// The attached customer, active; `422 invalid_reference` (`customer_id`) otherwise.
///
/// # Errors
/// See above, or the database error.
pub async fn customer(state: &AppState, id: Uuid) -> Result<CustomerRecord, ApiError> {
    customers::get_customer(&state.db, id)
        .await?
        .filter(CustomerRecord::is_active)
        .ok_or_else(|| reference_error("customer_id", "unknown or archived customer"))
}

/// The balance settings as `core` needs them.
///
/// # Errors
/// The database error.
pub async fn balance_settings(state: &AppState) -> Result<BalanceSettings, ApiError> {
    let s = customers::get_balance_settings(&state.db).await?;
    Ok(BalanceSettings {
        max_saved_per_sale: s.max_saved_per_sale,
        max_balance: s.max_balance,
    })
}

fn discount(body: Option<&DiscountBody>, field: &str) -> Result<Option<Discount>, ApiError> {
    let Some(d) = body else {
        return Ok(None);
    };
    let discount = match (d.pct_bp, d.amount) {
        (Some(bp), None) => Discount::Pct(bp),
        (None, Some(a)) => Discount::Amount(Money::from_minor(a)),
        (None, None) => return Ok(None),
        (Some(_), Some(_)) => {
            return Err(ApiError::invalid_field(
                field,
                format!("`{field}` takes `pct_bp` or `amount`, not both"),
            ));
        }
    };
    Ok(Some(discount.validate()?))
}

/// One limited permission a posted sale needs, with the line it is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Check {
    /// The permission.
    pub permission: Permission,
    /// Its value.
    pub value: Option<Requested>,
    /// The line of a `price.override` (its approver is stored on the line).
    pub override_line: Option<usize>,
}

/// A rebuilt sale, before its permission checks.
#[derive(Debug, Clone)]
pub struct BuiltSale {
    /// The sale to post.
    pub sale: NewSale,
    /// The limited permissions it needs.
    pub checks: Vec<Check>,
    /// The part of an exchange's refund that pays it.
    pub exchange_used: Money,
}

impl BuiltSale {
    /// The checks as override actions on `context_ref`.
    #[must_use]
    pub fn actions<'a>(&self, context_ref: &'a str) -> Vec<(Action<'a>, Option<ApiError>)> {
        self.checks
            .iter()
            .map(|c| {
                (
                    Action {
                        permission: c.permission,
                        value: c.value,
                        context_ref,
                    },
                    None,
                )
            })
            .collect()
    }

    /// Stores the approver of each `price.override` on its line.
    pub fn set_approvers(&mut self, approvers: &[Option<UserId>]) {
        for (check, approver) in self.checks.iter().zip(approvers) {
            if let Some(line) = check.override_line {
                self.sale.lines[line].override_by = *approver;
            }
        }
    }
}

fn scale_label(line: &SaleLineBody) -> Result<Option<ScaleLabel>, ApiError> {
    match line.scale_label.as_deref() {
        None => Ok(None),
        Some("weight") => Ok(Some(ScaleLabel::Weight)),
        Some("price") => match line.label_amount {
            Some(a) if a >= 0 => Ok(Some(ScaleLabel::Price {
                amount: Money::from_minor(a),
            })),
            _ => Err(ApiError::invalid_field(
                "lines.label_amount",
                "a price label needs its printed amount",
            )),
        },
        Some(_) => Err(ApiError::invalid_field(
            "lines.scale_label",
            "`scale_label` is `weight` or `price`",
        )),
    }
}

fn line_error(index: usize, error: PricingError) -> ApiError {
    ApiError::from(error).with("line", index)
}

/// The fresh quote of a stale cart, for `409 cart_stale`.
fn stale(totals: &CartTotals, unit_prices: &[Money]) -> ApiError {
    let lines: Vec<_> = totals
        .lines
        .iter()
        .zip(unit_prices)
        .map(|(l, p)| {
            json!({
                "unit_price": p.minor(),
                "gross": l.gross.minor(),
                "line_discount": l.line_discount.minor(),
                "cart_discount_share": l.cart_discount_share.minor(),
                "tax": l.tax.minor(),
                "total": l.total.minor(),
            })
        })
        .collect();
    let fresh = TotalsBody::from(SaleTotals::from(totals));
    ApiError::from(SaleError::CartStale).with(
        "quote",
        json!({ "totals": serde_json::to_value(fresh).unwrap_or_default(), "lines": lines }),
    )
}

/// Rebuilds the sale `id` of `body` at `till` (see the module docs): quote, totals (stale
/// check), the permission checks it needs, the payment, the stock movements. `exchange` is the
/// refund of a return in the same flow (D12): it pays the sale first, as an `exchange` tender.
///
/// # Errors
/// `409 cart_stale`; `422` `invalid_field`, `invalid_qty`, `cart_too_large`, `no_price`,
/// `archived`, `invalid_reference`, `tender_reference_required`, `tender_over_rest`,
/// `insufficient_balance`, `balance_limit`; or the database error.
#[allow(clippy::too_many_lines)] // one pass over the posting rules, in spec order
pub async fn build_sale(
    state: &AppState,
    till: Till,
    id: SaleId,
    body: &SaleBody,
    exchange: Option<Money>,
    request_hash: TokenHash,
    now: Timestamp,
) -> Result<BuiltSale, ApiError> {
    if body.lines.is_empty() {
        return Err(ApiError::invalid_field(
            "lines",
            "a sale has at least one line",
        ));
    }
    if body.lines.len() > MAX_LINES {
        return Err(SaleError::CartTooLarge { max: MAX_LINES }.into());
    }
    let mut wanted = Vec::with_capacity(body.lines.len());
    for line in &body.lines {
        wanted.push((
            parse_id::<VariantId>(&line.variant_id, "lines.variant_id")?,
            parse_id::<UnitId>(&line.unit_id, "lines.unit_id")?,
        ));
    }
    let items = load_items(state, &wanted, "lines", false).await?;
    let customer_id: Option<Uuid> =
        super::parse_opt_id(body.customer_id.as_deref(), "customer_id")?;
    let customer = match customer_id {
        Some(c) => Some(customer(state, c).await?),
        None => None,
    };
    if body.tax_invoice
        && customer
            .as_ref()
            .is_none_or(|c| c.customer.tax_id.is_none())
    {
        return Err(SaleError::InvalidField {
            field: "tax_invoice",
            reason: "needs a customer with a tax id",
        }
        .into());
    }
    let ctx = PriceContext::load(state, till.branch).await?;

    // Lines, then the quote of every line that is not a price label without a quantity.
    let mut labels = Vec::with_capacity(items.len());
    let mut quoted = Vec::new();
    let mut slots = Vec::with_capacity(items.len());
    for (i, (item, line)) in items.iter().zip(&body.lines).enumerate() {
        let qty = Qty::from_thousandths(line.qty_thousandths);
        let label = scale_label(line)?;
        let price_label = matches!(label, Some(ScaleLabel::Price { .. }));
        if !(price_label && qty == Qty::ZERO) {
            check_qty(qty, item.unit_kind).map_err(|e| ApiError::from(e).with("line", i))?;
        }
        if line.typed_price.is_some_and(|p| p < 0) {
            return Err(ApiError::invalid_field(
                "lines.typed_price",
                "`typed_price` must not be negative",
            ));
        }
        let typed = line.typed_price.map(Money::from_minor);
        let mode = if price_label {
            LineMode::Listed
        } else if item.open_price {
            LineMode::OpenPrice(typed)
        } else {
            typed.map_or(LineMode::Listed, LineMode::Overridden)
        };
        labels.push(label);
        if price_label && qty == Qty::ZERO {
            slots.push(None);
        } else {
            quoted.push(core_pricing::CartLine {
                item: item.sale_item(),
                qty,
                mode,
            });
            slots.push(Some(quoted.len() - 1));
        }
    }
    let priced = quote(state, till.branch, customer_id, &ctx, &quoted).await?;

    let mut lines = Vec::with_capacity(items.len());
    let mut totals_lines = Vec::with_capacity(items.len());
    let mut unit_prices = Vec::with_capacity(items.len());
    for (i, ((item, line), label)) in items.iter().zip(&body.lines).zip(&labels).enumerate() {
        let qty = Qty::from_thousandths(line.qty_thousandths);
        let result = slots[i].map(|s| priced[s].clone());
        let (unit_price, price_mode, price_list, gross) =
            if let Some(ScaleLabel::Price { amount }) = label {
                let p = result.and_then(Result::ok);
                (
                    p.map_or(Money::ZERO, |p| p.unit_price),
                    PriceMode::Listed,
                    p.and_then(|p| p.resolved).map(|r| r.source.list),
                    *amount,
                )
            } else {
                let p = result
                    .unwrap_or(Err(PricingError::Overflow))
                    .map_err(|e| line_error(i, e))?;
                let mode = if item.open_price {
                    PriceMode::OpenPrice
                } else if line.typed_price.is_some() {
                    PriceMode::Overridden {
                        resolved: p.resolved.map(|r| r.unit_price),
                    }
                } else {
                    PriceMode::Listed
                };
                let list = match mode {
                    PriceMode::Listed => p.resolved.map(|r| r.source.list),
                    _ => None,
                };
                (p.unit_price, mode, list, p.amount)
            };
        unit_prices.push(unit_price);
        totals_lines.push(TotalsLine {
            gross,
            line_discount: discount(line.discount.as_ref(), "lines.discount")?,
            cart_discountable: !matches!(label, Some(ScaleLabel::Price { .. })),
            tax_rate_bp: ctx.rate(item.tax_class),
        });
        lines.push(SaleLine {
            variant: item.variant,
            unit: item.unit,
            name: item.name.clone(),
            product_type: item.product_type,
            factor: item.factor,
            components: item.components.clone(),
            qty,
            unit_price,
            price_mode,
            price_list,
            override_by: None,
            scale_label: *label,
            totals: sultan_core::sale::LineTotals {
                gross,
                line_discount: Money::ZERO,
                after_line: gross,
                cart_discount_share: Money::ZERO,
                amount: gross,
                tax_rate_bp: 0,
                tax: Money::ZERO,
                net: gross,
                total: gross,
            },
        });
    }
    let cart_discount = discount(body.cart_discount.as_ref(), "cart_discount")?;
    let totals = compute_totals(&totals_lines, cart_discount, ctx.settings.tax)?;
    for (line, t) in lines.iter_mut().zip(&totals.lines) {
        line.totals = *t;
    }
    let sale_totals = SaleTotals::from(&totals);
    if TotalsBody::from(sale_totals) != body.totals {
        return Err(stale(&totals, &unit_prices));
    }

    // The limited permissions (spec 2.6 §3, §6).
    let mut checks = Vec::new();
    for (i, (line, t)) in lines.iter().zip(&totals.lines).enumerate() {
        if let PriceMode::Overridden { resolved } = line.price_mode {
            checks.push(Check {
                permission: Permission::PriceOverride,
                value: resolved
                    .map(|r| Requested::Pct(discount_bp(r, r.saturating_sub(line.unit_price)))),
                override_line: Some(i),
            });
        }
        if t.line_discount > Money::ZERO {
            checks.push(Check {
                permission: Permission::SaleDiscount,
                value: Some(Requested::Pct(t.line_discount_bp())),
                override_line: None,
            });
        }
    }
    if totals.cart_discount > Money::ZERO {
        checks.push(Check {
            permission: Permission::SaleCartDiscount,
            value: Some(Requested::Pct(totals.cart_discount_bp())),
            override_line: None,
        });
    }

    // Payment.
    let balance = customer
        .as_ref()
        .map(|c| BalanceContext { balance: c.balance });
    let mut payment = Payment::new(totals.total, ctx.settings.cash_rounding)?;
    let mut exchange_used = Money::ZERO;
    if let Some(refund) = exchange {
        exchange_used = refund.min(totals.total).max(Money::ZERO);
        if exchange_used > Money::ZERO {
            payment.add_exchange(exchange_used)?;
        }
    }
    for t in &body.tenders {
        let id = parse_id(&t.tender_type_id, "tenders.tender_type_id")?;
        let mut record = sales::get_tender_type(&state.db, id)
            .await?
            .ok_or_else(|| reference_error("tenders.tender_type_id", "unknown tender type"))?;
        if record.archived_at.is_some() {
            record.tender.active = false;
        }
        payment.add(
            &record.tender,
            Money::from_minor(t.amount),
            t.reference.as_deref(),
            balance,
        )?;
    }
    if let Some(saved) = body.save_change.filter(|s| *s != 0) {
        let settings = balance_settings(state).await?;
        payment.save_change(settings, balance, Money::from_minor(saved))?;
    }
    let paid = payment.finish()?;
    let stock: Vec<StockLine<'_>> = lines.iter().map(SaleLine::stock_line).collect();
    let movements = sale_movements(&stock)?;
    Ok(BuiltSale {
        sale: NewSale {
            id,
            request_hash,
            branch: till.branch,
            register: till.register,
            cashier_session: till.session,
            user: till.user,
            customer: customer_id,
            tax_invoice: body.tax_invoice,
            tax_inclusive: ctx.settings.tax.inclusive,
            totals: sale_totals,
            lines,
            payment: paid,
            movements,
            posted_at: now,
        },
        checks,
        exchange_used,
    })
}
