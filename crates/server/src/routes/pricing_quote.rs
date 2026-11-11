//! `POST /pricing/quote` (spec 2.3 §4–§7, §10) and the price in `GET /catalog/lookup`.
//!
//! **Quote.** A user session or a device token ([`CatalogReader`]); no permission. The branch is
//! the register's (device token) or else the session's branch; the sale date is today in that
//! branch's time zone. With a `customer_id` the chain is contract → tier (only when the tier
//! applies on the date, spec 2.5 §4) → group → branch → default; without one, branch → default.
//! Every line is priced together (quantity tiers count a variant over the whole cart, in base
//! units). A line answers its unit price, amount, source (list, kind, tier `min_qty`, unit
//! fallback) and tax (rate on the date, tax, net, total, with the pricing settings), or a
//! per-line `error`: `no_price` (with the resolved price as `suggestion` on an open-price line
//! without a typed price), `invalid_reference` (an unknown variant, or a unit not of its
//! product), `invalid_field` (a negative typed price) or `amount_overflow`. Totals add the
//! priced lines only. A `typed_price` on an `open_price` product is the cashier's price; on any
//! other product it is a manual override (kept; whether the user may do that is checked when
//! the sale is made, 2.6). The quote stores nothing.
//!
//! Cash rounding is not applied here: it belongs to the payment (2.6). The quote only reports
//! the setting (`cash_rounding`).
//!
//! **Lookup price.** [`lookup_price`]: the store chain (branch → default, no customer) for one
//! unit of the looked-up unit (`unit_price`); a price-embedded scale label also gets its
//! quantity from that price (spec 2.3 §5).

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sultan_core::catalog::{ProductUnits, Qty};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::ids::{BranchId, ProductId, TaxClassId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{
    CartLine, LineMode, PriceSource, PricedLine, PricingError, SaleItem, TierMembership,
    counted_quantities, price_label, resolve_cart, resolve_unit_price, sale_date,
};
use sultan_core::tax::{line_tax, rate_on};
use sultan_db::catalog;
use sultan_db::pricing::{self, PriceTargets};
use sultan_db::{customers, org};
use utoipa::ToSchema;
use uuid::Uuid;

use super::parse_id;
use super::pricing::date_text;
use crate::error::ApiError;
use crate::extract::{ApiJson, CatalogReader};
use crate::state::AppState;

/// The most lines a quote takes.
pub const MAX_QUOTE_LINES: usize = 500;

/// The branch a quote or lookup price is for: the register's, else the session's.
#[must_use]
pub fn reader_branch(reader: &CatalogReader) -> Option<BranchId> {
    reader
        .register_branch()
        .or_else(|| reader.user().map(|u| u.session.branch))
}

/// Today in `branch`'s time zone (the default zone for an unknown branch or zone).
async fn today_at(state: &AppState, branch: BranchId) -> Result<NaiveDate, ApiError> {
    let tz = org::get_branch(&state.db, branch)
        .await?
        .and_then(|b| b.timezone.parse::<BranchTimeZone>().ok())
        .unwrap_or_default();
    Ok(sale_date(state.now(), tz))
}

/// A `YYYYMMDD` number as a date.
fn ymd(value: u32) -> Option<NaiveDate> {
    NaiveDate::from_ymd_opt(
        i32::try_from(value / 10_000).ok()?,
        value / 100 % 100,
        value % 100,
    )
}

/// The lists a sale at `branch` for `customer` uses. An unknown, archived, merged or erased
/// customer is `422 invalid_reference` (`customer_id`).
async fn price_targets(
    state: &AppState,
    branch: BranchId,
    customer: Option<Uuid>,
) -> Result<PriceTargets, ApiError> {
    let mut targets = PriceTargets {
        branch: Some(branch),
        ..PriceTargets::default()
    };
    let Some(id) = customer else {
        return Ok(targets);
    };
    let record = customers::get_customer(&state.db, id)
        .await?
        .filter(customers::CustomerRecord::is_active)
        .ok_or_else(|| {
            ApiError::new(
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_reference",
                "unknown or archived customer",
            )
            .with("field", "customer_id")
        })?;
    let c = record.customer;
    targets.customer = Some(c.id);
    targets.group = c.group;
    if let Some(tier) = c.tier {
        let archived = customers::get_tier(&state.db, tier)
            .await?
            .is_none_or(|t| t.archived_at.is_some());
        targets.tier = Some((
            tier,
            TierMembership {
                tier_archived: archived,
                valid_until: c.tier_valid_until.and_then(ymd),
            },
        ));
    }
    Ok(targets)
}

/// What pricing needs of a product.
struct ProductInfo {
    units: ProductUnits,
    open_price: bool,
    tax_class: TaxClassId,
}

/// The products of `variants`, by variant (unknown variants are left out).
async fn products_of(
    state: &AppState,
    variants: &[VariantId],
) -> Result<HashMap<VariantId, Arc<ProductInfo>>, ApiError> {
    let labels = catalog::variant_labels(&state.db, variants).await?;
    let mut products: HashMap<ProductId, Arc<ProductInfo>> = HashMap::new();
    let mut by_variant = HashMap::new();
    for (variant, label) in labels {
        if !products.contains_key(&label.product)
            && let Some(record) = catalog::get_product(&state.db, label.product).await?
        {
            let draft = record.doc.draft;
            products.insert(
                label.product,
                Arc::new(ProductInfo {
                    units: draft.units,
                    open_price: draft.open_price,
                    tax_class: record.doc.tax_class,
                }),
            );
        }
        if let Some(info) = products.get(&label.product) {
            by_variant.insert(variant, info.clone());
        }
    }
    Ok(by_variant)
}

/// The sale item of `variant` in `unit`, or `None` when the unit is not one of its product's.
fn sale_item(info: &ProductInfo, variant: VariantId, unit: UnitId) -> Option<SaleItem> {
    let u = info.units.get(unit)?;
    Some(SaleItem {
        variant,
        unit,
        base_unit: info.units.base,
        factor: u.factor,
    })
}

/// One line of `POST /pricing/quote`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct QuoteLineBody {
    /// The variant.
    pub variant_id: String,
    /// The unit it is sold in.
    pub unit_id: String,
    /// Quantity in thousandths of the unit (> 0).
    pub qty_thousandths: i64,
    /// A typed unit price (minor units): the cashier's price of an `open_price` product, or a
    /// manual override of any other line.
    #[serde(default)]
    pub typed_price: Option<i64>,
}

/// Body of `POST /pricing/quote`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct QuoteBody {
    /// The customer whose contract, tier and group lists apply; `null` = walk-in.
    #[serde(default)]
    pub customer_id: Option<String>,
    /// Up to 500 lines.
    pub lines: Vec<QuoteLineBody>,
}

/// Where a price came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PriceSourceDto {
    /// The list that set the price.
    pub list_id: String,
    /// Its kind: `default`, `branch`, `group`, `tier` or `customer`.
    pub kind: String,
    /// The `min_qty` (thousandths, in the entry's unit) of the tier used; `null` when the list
    /// rule priced it.
    pub tier_min_qty_thousandths: Option<i64>,
    /// The unit had no price of its own: base-unit price × factor (spec 2.3 §4.4).
    pub unit_fallback: bool,
}

impl From<PriceSource> for PriceSourceDto {
    fn from(s: PriceSource) -> Self {
        Self {
            list_id: s.list.to_string(),
            kind: s.kind.as_str().to_owned(),
            tier_min_qty_thousandths: s.tier_min_qty.map(Qty::thousandths),
            unit_fallback: s.unit_fallback,
        }
    }
}

/// The tax of a quoted line (spec 2.3 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LineTaxDto {
    /// The product's tax class.
    pub tax_class_id: String,
    /// The class's rate on the sale date (basis points; 0 without a rate or when not
    /// tax-registered).
    pub rate_bp: u16,
    /// The tax.
    pub tax: i64,
    /// The amount without tax.
    pub net: i64,
    /// What the customer pays for the line: the amount (inclusive), or amount + tax.
    pub total: i64,
}

/// Why a quoted line has no price.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct LineErrorDto {
    /// `no_price`, `invalid_reference`, `invalid_field` or `amount_overflow`.
    pub code: String,
    /// Developer-facing English message.
    pub message: String,
    /// `no_price` on an open-price line: the resolved price as a suggestion.
    pub suggestion: Option<i64>,
}

/// A quoted line, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuoteLineDto {
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The quantity, thousandths.
    pub qty_thousandths: i64,
    /// `listed`, `open_price` or `overridden`.
    pub mode: String,
    /// The unit price charged; `null` with an `error`.
    pub unit_price: Option<i64>,
    /// `unit_price × qty`, rounded half up to Rp 1; `null` with an `error`.
    pub amount: Option<i64>,
    /// The price from the lists: the charged price of a listed line, the suggestion of an
    /// open-price line, the original price of an overridden one.
    pub resolved_price: Option<i64>,
    /// Where `resolved_price` came from.
    pub source: Option<PriceSourceDto>,
    /// The line's tax; `null` with an `error`.
    pub tax: Option<LineTaxDto>,
    /// Why the line has no price.
    pub error: Option<LineErrorDto>,
}

/// Sums of the priced lines.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuoteTotalsDto {
    /// Sum of the line amounts.
    pub amount: i64,
    /// Sum of the line taxes.
    pub tax: i64,
    /// Sum of the net amounts.
    pub net: i64,
    /// What the customer pays (before cash rounding, 2.6).
    pub total: i64,
}

/// The answer of `POST /pricing/quote`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct QuoteDto {
    /// The selling branch.
    pub branch_id: String,
    /// The sale date in the branch's time zone, `YYYY-MM-DD`.
    pub date: String,
    /// The customer, if any.
    pub customer_id: Option<String>,
    /// Prices include tax.
    pub tax_inclusive: bool,
    /// The company is tax-registered.
    pub tax_registered: bool,
    /// The cash-rounding setting, for information: it is applied to cash payments (2.6), not
    /// here.
    pub cash_rounding: String,
    /// The lines.
    pub lines: Vec<QuoteLineDto>,
    /// The totals of the priced lines.
    pub totals: QuoteTotalsDto,
}

fn line_error(error: &PricingError) -> LineErrorDto {
    LineErrorDto {
        code: error.code().to_owned(),
        message: error.to_string(),
        suggestion: match error {
            PricingError::NoPrice { suggestion } => suggestion.map(Money::minor),
            _ => None,
        },
    }
}

fn overflow() -> ApiError {
    PricingError::Overflow.into()
}

fn add(a: i64, b: i64) -> Result<i64, ApiError> {
    a.checked_add(b).ok_or_else(overflow)
}

/// `POST /api/v1/pricing/quote`: prices a cart (see the module docs).
///
/// # Errors
/// `401`, `400 invalid_body`, `422 invalid_field` (`lines`, `lines.qty_thousandths`),
/// `invalid_reference` (`customer_id`), `amount_overflow` (totals).
#[utoipa::path(post, path = "/api/v1/pricing/quote", tag = "pricing",
    security(("bearer" = []), ("register_token" = []), ("bearer" = [], "register_token" = [])),
    request_body = QuoteBody,
    responses((status = 200, body = QuoteDto), (status = 401, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
#[allow(clippy::too_many_lines)] // one pass: load, resolve, tax, totals
pub async fn quote(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiJson(body): ApiJson<QuoteBody>,
) -> Result<Json<QuoteDto>, ApiError> {
    if body.lines.len() > MAX_QUOTE_LINES {
        return Err(ApiError::invalid_field(
            "lines",
            format!("at most {MAX_QUOTE_LINES} lines"),
        ));
    }
    let mut parsed = Vec::with_capacity(body.lines.len());
    for line in &body.lines {
        if line.qty_thousandths <= 0 {
            return Err(ApiError::invalid_field(
                "lines.qty_thousandths",
                "`qty_thousandths` must be greater than zero",
            ));
        }
        parsed.push((
            parse_id::<VariantId>(&line.variant_id, "lines.variant_id")?,
            parse_id::<UnitId>(&line.unit_id, "lines.unit_id")?,
        ));
    }
    let customer: Option<Uuid> = super::parse_opt_id(body.customer_id.as_deref(), "customer_id")?;
    let branch = reader_branch(&reader).ok_or_else(ApiError::invalid_session)?;
    let date = today_at(&state, branch).await?;
    let targets = price_targets(&state, branch, customer).await?;

    let mut variants: Vec<VariantId> = parsed.iter().map(|(v, _)| *v).collect();
    variants.sort();
    variants.dedup();
    let products = products_of(&state, &variants).await?;
    let known: Vec<VariantId> = variants
        .into_iter()
        .filter(|v| products.contains_key(v))
        .collect();
    let lists = pricing::load_for_resolution(&state.db, &targets, &known, date).await?;
    let settings = pricing::get_pricing_settings(&state.db).await?;
    let rates = pricing::tax_rates_by_class(&state.db).await?;

    // The lines that name a known variant + unit form the cart; the others answer an error.
    let mut cart = Vec::new();
    let mut slots: Vec<Option<usize>> = Vec::with_capacity(parsed.len());
    for ((variant, unit), line) in parsed.iter().zip(&body.lines) {
        let item = products
            .get(variant)
            .and_then(|info| Some((info, sale_item(info, *variant, *unit)?)));
        slots.push(item.map(|(info, item)| {
            let typed = line.typed_price.map(Money::from_minor);
            let mode = if info.open_price {
                LineMode::OpenPrice(typed)
            } else {
                typed.map_or(LineMode::Listed, LineMode::Overridden)
            };
            cart.push(CartLine {
                item,
                qty: Qty::from_thousandths(line.qty_thousandths),
                mode,
            });
            cart.len() - 1
        }));
    }
    let chain = lists.chain(date);
    let priced = resolve_cart(&chain, &cart);
    let counted = counted_quantities(&cart);

    let mut totals = QuoteTotalsDto::default();
    let mut lines = Vec::with_capacity(body.lines.len());
    for ((slot, (variant, unit)), line) in slots.iter().zip(&parsed).zip(&body.lines) {
        let mut dto = QuoteLineDto {
            variant_id: variant.to_string(),
            unit_id: unit.to_string(),
            qty_thousandths: line.qty_thousandths,
            mode: "listed".to_owned(),
            unit_price: None,
            amount: None,
            resolved_price: None,
            source: None,
            tax: None,
            error: None,
        };
        let Some(i) = *slot else {
            dto.error = Some(LineErrorDto {
                code: "invalid_reference".to_owned(),
                message: "unknown variant, or a unit not of its product".to_owned(),
                suggestion: None,
            });
            lines.push(dto);
            continue;
        };
        match cart[i].mode {
            LineMode::Listed => "listed",
            LineMode::OpenPrice(_) => "open_price",
            LineMode::Overridden(_) => "overridden",
        }
        .clone_into(&mut dto.mode);
        match &priced[i] {
            Err(error) => dto.error = Some(line_error(error)),
            Ok(PricedLine {
                resolved: _,
                unit_price,
                amount,
            }) => {
                let class = products[variant].tax_class;
                let rate = if settings.tax.registered {
                    rate_on(rates.get(&class).map_or(&[], Vec::as_slice), date).unwrap_or(0)
                } else {
                    0
                };
                let tax = line_tax(*amount, rate, settings.tax);
                totals = QuoteTotalsDto {
                    amount: add(totals.amount, amount.minor())?,
                    tax: add(totals.tax, tax.tax.minor())?,
                    net: add(totals.net, tax.net.minor())?,
                    total: add(totals.total, tax.total.minor())?,
                };
                dto.unit_price = Some(unit_price.minor());
                dto.amount = Some(amount.minor());
                dto.tax = Some(LineTaxDto {
                    tax_class_id: class.to_string(),
                    rate_bp: rate,
                    tax: tax.tax.minor(),
                    net: tax.net.minor(),
                    total: tax.total.minor(),
                });
            }
        }
        let resolved = match &priced[i] {
            Ok(p) => p.resolved,
            // An open-price line without a typed price: its suggestion's source.
            Err(PricingError::NoPrice {
                suggestion: Some(_),
            }) => counted
                .get(&cart[i].item.variant)
                .copied()
                .flatten()
                .and_then(|q| resolve_unit_price(&chain, &cart[i].item, q).ok().flatten()),
            Err(_) => None,
        };
        dto.resolved_price = resolved.map(|r| r.unit_price.minor());
        dto.source = resolved.map(|r| r.source.into());
        lines.push(dto);
    }
    Ok(Json(QuoteDto {
        branch_id: branch.to_string(),
        date: date_text(date),
        customer_id: customer.map(|c| c.to_string()),
        tax_inclusive: settings.tax.inclusive,
        tax_registered: settings.tax.registered,
        cash_rounding: settings.cash_rounding.as_str().to_owned(),
        lines,
        totals,
    }))
}

/// The price shown by a lookup (spec 2.3 §10, §5).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LookupPrice {
    /// One unit of the looked-up unit from the store chain (branch → default), no customer.
    pub unit_price: Option<Money>,
    /// A price-embedded label's quantity: printed amount ÷ that price, 3 dp.
    pub label_qty: Option<Qty>,
}

/// The lookup price of `variant` in `unit` (of `product`) at `branch` today. `label_amount` is a
/// price-embedded label's printed amount (its quantity is derived from the store price).
///
/// # Errors
/// The database error, or `422 amount_overflow`.
pub async fn lookup_price(
    state: &AppState,
    branch: Option<BranchId>,
    product: ProductId,
    (variant, unit): (VariantId, UnitId),
    label_amount: Option<Money>,
) -> Result<LookupPrice, ApiError> {
    let Some(record) = catalog::get_product(&state.db, product).await? else {
        return Ok(LookupPrice::default());
    };
    let info = ProductInfo {
        units: record.doc.draft.units,
        open_price: record.doc.draft.open_price,
        tax_class: record.doc.tax_class,
    };
    let Some(item) = sale_item(&info, variant, unit) else {
        return Ok(LookupPrice::default());
    };
    let date = match branch {
        Some(b) => today_at(state, b).await?,
        None => sale_date(state.now(), BranchTimeZone::default()),
    };
    let targets = PriceTargets {
        branch,
        ..PriceTargets::default()
    };
    let lists = pricing::load_for_resolution(&state.db, &targets, &[variant], date).await?;
    if let Some(amount) = label_amount {
        let label = price_label(lists.branch.as_ref(), &lists.default, date, &item, amount)?;
        return Ok(LookupPrice {
            unit_price: label.unit_price,
            label_qty: label.qty,
        });
    }
    let price = resolve_unit_price(&lists.store_chain(date), &item, item.factor)?;
    Ok(LookupPrice {
        unit_price: price.map(|p| p.unit_price),
        label_qty: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_errors() {
        assert_eq!(ymd(20_261_231), NaiveDate::from_ymd_opt(2026, 12, 31));
        assert_eq!(ymd(20_261_331), None);
        let e = line_error(&PricingError::NoPrice {
            suggestion: Some(Money::from_minor(5)),
        });
        assert_eq!((e.code.as_str(), e.suggestion), ("no_price", Some(5)));
        assert_eq!(line_error(&PricingError::Overflow).suggestion, None);
        assert_eq!(add(i64::MAX, 1).unwrap_err().code(), "amount_overflow");
        assert_eq!(add(1, 2).unwrap(), 3);
    }
}
