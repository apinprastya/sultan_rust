//! The sell screen's cart and payment (spec 2.6 §2, §3, §5, §6, phase S4). Pure state: the
//! controller ([`crate::till`]) decides when to look up, quote, void and post.
//!
//! - **Lines** ([`Line`]) keep the lookup answer (name, variant, units, SKU) next to the core
//!   line ([`sultan_core::sale::CartLine`]: qty, unit price, price mode, scale label, discount,
//!   tax rate). A rescan of the same variant + unit adds to the newest line that may merge (D8,
//!   [`CartLine::mergeable`]); weighed items, scale labels, open-price and overridden lines
//!   always get a new line. At most [`MAX_LINES`] lines, quantities through [`check_qty`].
//! - **Prices** come from the quote of the whole cart ([`Cart::quote_request`],
//!   [`Cart::apply_quote`]): listed lines take the quoted price, open-price and overridden lines
//!   keep theirs (2.3 §6), every quoted line takes its tax rate. A price-embedded label keeps its
//!   printed amount; it is quoted only for its tax rate. A new line that the quote cannot price
//!   (`no_price`) is taken out again ([`Cart::drop_unpriced`]): it was never in the cart, so this
//!   is not a void.
//! - **Totals** ([`Cart::totals`]) are `core::sale::compute_totals` with the quote's tax
//!   settings, so they match what the server computes at posting (§19 item 37).
//! - **Approvals** ([`Cart::approvals`]): the limited checks of the cart in the server's order
//!   (per line `price.override`, then `sale.discount`; then `sale.cart_discount`), each with the
//!   value its limit is checked against.
//! - **Payment** ([`Pay`]) wraps `core::sale::Payment`: cash with rounding and change, non-cash
//!   tenders up to the rest with a reference when the type needs one, the customer balance, and
//!   saved change.
//! - **Crash copy** ([`Saved`]): the cart as JSON with the cashier session it belongs to (D2).

use serde::{Deserialize, Serialize};
use sultan_client::{
    Customer, Discount as WireDiscount, Lookup, LookupUnit, Quote, QuoteLine, QuoteRequest,
    SaleLineRequest, SaleRequest, SaleTotals, TenderRequest, TenderType,
};
use sultan_core::catalog::{ProductType, Qty, UnitKind};
use sultan_core::customer::BalanceSettings;
use sultan_core::ids::{TenderTypeId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::CashRounding;
use sultan_core::sale::{
    BalanceContext, CartItem, CartLine, CartTotals, Discount, MAX_LINES, Payment, PriceMode,
    SaleError, ScaleLabel, TenderKind, TenderType as CoreTender, check_qty, compute_totals,
    discount_bp,
};
use sultan_core::tax::TaxSettings;

use crate::controller::new_key;

/// A cart line: the lookup answer and the core line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The item as looked up; `unit_id` and `unit_name` follow a unit change.
    pub item: Lookup,
    /// The core line.
    pub core: CartLine,
    /// Not quoted yet since it was added: a `no_price` answer takes it out again.
    pub fresh: bool,
    /// The quote's error code for the line (`no_price`, …).
    pub error: Option<String>,
}

fn unit_kind(kind: &str) -> UnitKind {
    if kind == "decimal" {
        UnitKind::Decimal
    } else {
        UnitKind::Integer
    }
}

fn product_type(item: &Lookup) -> ProductType {
    item.product_type.parse().unwrap_or(ProductType::Stock)
}

/// The kind of the item's unit: from its unit list, else decimal for weighed items.
fn kind_of(item: &Lookup) -> UnitKind {
    item.units
        .iter()
        .find(|u| u.id == item.unit_id)
        .map_or_else(
            || {
                if product_type(item) == ProductType::Weighed {
                    UnitKind::Decimal
                } else {
                    UnitKind::Integer
                }
            },
            |u| unit_kind(&u.kind),
        )
}

fn factor_of(item: &Lookup) -> Qty {
    item.units
        .iter()
        .find(|u| u.id == item.unit_id)
        .map_or(Qty::ONE, |u| Qty::from_thousandths(u.factor_thousandths))
}

/// The scale label a lookup answer came from, if any.
fn scale_label(item: &Lookup) -> Option<ScaleLabel> {
    match (item.source.as_str(), item.amount) {
        (_, Some(amount)) => Some(ScaleLabel::Price {
            amount: Money::from_minor(amount),
        }),
        ("scale_barcode", None) => Some(ScaleLabel::Weight),
        _ => None,
    }
}

impl Line {
    /// A new line of `qty` of `item`; `price` is the typed price of an open-price item.
    #[must_use]
    pub fn new(item: Lookup, qty: Qty, price: Option<i64>) -> Self {
        let core_item = CartItem {
            variant: item
                .variant_id
                .parse()
                .unwrap_or_else(|_| VariantId::generate()),
            product_type: product_type(&item),
            unit: item.unit_id.parse().unwrap_or_else(|_| UnitId::generate()),
            base_unit: item.unit_id.parse().unwrap_or_else(|_| UnitId::generate()),
            factor: factor_of(&item),
            unit_kind: kind_of(&item),
            open_price: item.open_price,
            tax_rate_bp: 0,
            components: Vec::new(),
        };
        let mut core = CartLine::new(core_item, qty);
        core.scale_label = scale_label(&item);
        if let Some(p) = price {
            core.unit_price = Money::from_minor(p);
        }
        Self {
            item,
            core,
            fresh: true,
            error: None,
        }
    }

    /// Whether the unit price was set by hand (an override).
    #[must_use]
    pub const fn overridden(&self) -> bool {
        matches!(self.core.mode, PriceMode::Overridden { .. })
    }

    /// The price an override replaced, if any.
    #[must_use]
    pub const fn resolved(&self) -> Option<i64> {
        match self.core.mode {
            PriceMode::Overridden { resolved: Some(r) } => Some(r.minor()),
            _ => None,
        }
    }

    fn same_item(&self, other: &Self) -> bool {
        self.item.variant_id == other.item.variant_id && self.item.unit_id == other.item.unit_id
    }

    /// One unit step for `+` / `−`: 1 of the line's unit.
    const fn step() -> i64 {
        1_000
    }
}

/// The quote's settings the cart needs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    /// Tax inclusive / registered.
    pub tax: TaxSettings,
    /// Cash rounding.
    pub rounding: CashRounding,
}

/// The cart in progress (D1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cart {
    /// The cart's ID: the `context_ref` of its voids' overrides, and the sale ID it posts as.
    pub id: String,
    /// The lines, in order.
    pub lines: Vec<Line>,
    /// The whole-cart discount.
    pub discount: Option<Discount>,
    /// A tax invoice (D13).
    pub tax_invoice: bool,
    /// From the last quote.
    pub settings: Settings,
    /// The quote waiting for its answer: its number and the line of each quoted line.
    pub quoting: Option<(u64, Vec<usize>)>,
}

impl Default for Cart {
    fn default() -> Self {
        Self {
            id: new_key(),
            lines: Vec::new(),
            discount: None,
            tax_invoice: false,
            settings: Settings::default(),
            quoting: None,
        }
    }
}

fn invalid(field: &'static str, reason: &'static str) -> SaleError {
    SaleError::InvalidField { field, reason }
}

impl Cart {
    /// Whether the cart has no lines.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    fn line_mut(&mut self, index: usize) -> Result<&mut Line, SaleError> {
        self.lines
            .get_mut(index)
            .ok_or_else(|| invalid("line", "no such line"))
    }

    /// Adds `line` (§2, D8). Returns the index of the line that holds it now.
    ///
    /// # Errors
    /// [`SaleError::InvalidQty`], [`SaleError::CartTooLarge`].
    pub fn add(&mut self, line: Line) -> Result<usize, SaleError> {
        if !(line.core.is_price_label() && line.core.qty == Qty::ZERO) {
            check_qty(line.core.qty, line.core.item.unit_kind)?;
        }
        if line.core.mergeable()
            && let Some(index) = self
                .lines
                .iter()
                .rposition(|l| l.core.mergeable() && l.same_item(&line))
        {
            let existing = &mut self.lines[index];
            let qty =
                existing
                    .core
                    .qty
                    .checked_add(line.core.qty)
                    .ok_or(SaleError::InvalidQty {
                        reason: "must be at most 99 999.999",
                    })?;
            check_qty(qty, existing.core.item.unit_kind)?;
            existing.core.qty = qty;
            return Ok(index);
        }
        if self.lines.len() >= MAX_LINES {
            return Err(SaleError::CartTooLarge { max: MAX_LINES });
        }
        self.lines.push(line);
        Ok(self.lines.len() - 1)
    }

    /// Sets a line's quantity (lowering it is not a void, §3).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`: unknown, or a price label), [`SaleError::InvalidQty`].
    pub fn set_qty(&mut self, index: usize, qty: Qty) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.core.is_price_label() {
            return Err(invalid("line", "a price label's quantity cannot change"));
        }
        check_qty(qty, line.core.item.unit_kind)?;
        line.core.qty = qty;
        Ok(())
    }

    /// `+` / `−`: one unit more or less. Below one unit is refused: removing is a void.
    ///
    /// # Errors
    /// As [`Cart::set_qty`].
    pub fn bump(&mut self, index: usize, up: bool) -> Result<(), SaleError> {
        let qty = self
            .lines
            .get(index)
            .ok_or_else(|| invalid("line", "no such line"))?
            .core
            .qty
            .thousandths();
        let next = if up {
            qty.saturating_add(Line::step())
        } else {
            qty - Line::step()
        };
        self.set_qty(index, Qty::from_thousandths(next))
    }

    /// Changes a line's unit (§2). An overridden line goes back to the listed price (§19 item 3).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`: unknown or a scale label), [`SaleError::InvalidQty`].
    pub fn set_unit(&mut self, index: usize, unit: &LookupUnit) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.core.scale_label.is_some() {
            return Err(invalid("line", "a scale label's unit cannot change"));
        }
        let kind = unit_kind(&unit.kind);
        check_qty(line.core.qty, kind)?;
        line.item.unit_id.clone_from(&unit.id);
        line.item.unit_name.clone_from(&unit.name);
        line.core.item.unit = unit.id.parse().unwrap_or_else(|_| UnitId::generate());
        line.core.item.factor = Qty::from_thousandths(unit.factor_thousandths);
        line.core.item.unit_kind = kind;
        if line.overridden() {
            line.core.mode = PriceMode::Listed;
        }
        line.core.price_source = None;
        Ok(())
    }

    /// A manual unit price (2.3 §6): the price it replaces is the quoted one (an overridden line
    /// keeps the first). The approval is asked at posting ([`Cart::approvals`]).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`: unknown, open-price or scale label; `price`).
    pub fn override_price(&mut self, index: usize, price: i64) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.core.scale_label.is_some() || line.core.mode == PriceMode::OpenPrice {
            return Err(invalid(
                "line",
                "open-price and scale-label lines cannot be overridden",
            ));
        }
        if price < 0 {
            return Err(invalid("price", "must not be negative"));
        }
        let resolved = match line.core.mode {
            PriceMode::Overridden { resolved } => resolved,
            _ => (line.error.is_none() && !line.fresh).then_some(line.core.unit_price),
        };
        line.core.mode = PriceMode::Overridden { resolved };
        line.core.unit_price = Money::from_minor(price);
        line.error = None;
        Ok(())
    }

    /// Sets or clears a line discount (§3).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line` or `discount`).
    pub fn set_line_discount(
        &mut self,
        index: usize,
        discount: Option<Discount>,
    ) -> Result<(), SaleError> {
        let discount = discount.map(Discount::validate).transpose()?;
        self.line_mut(index)?.core.discount = discount;
        Ok(())
    }

    /// Sets or clears the cart discount (§6).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`discount`).
    pub fn set_cart_discount(&mut self, discount: Option<Discount>) -> Result<(), SaleError> {
        self.discount = discount.map(Discount::validate).transpose()?;
        Ok(())
    }

    /// The quote request of the cart (spec 2.3 §10) and the line of each quoted line.
    #[must_use]
    pub fn quote_request(&self, customer: Option<&str>) -> (QuoteRequest, Vec<usize>) {
        let mut lines = Vec::new();
        let mut map = Vec::new();
        for (i, l) in self.lines.iter().enumerate() {
            let typed = match l.core.mode {
                PriceMode::Listed => None,
                PriceMode::OpenPrice | PriceMode::Overridden { .. } => {
                    Some(l.core.unit_price.minor())
                }
            };
            let qty = if l.core.is_price_label() {
                // Quoted for its tax rate only: the printed amount is final.
                l.core.qty.thousandths().max(1_000)
            } else {
                l.core.qty.thousandths()
            };
            lines.push(QuoteLine {
                typed_price: typed,
                ..QuoteLine::new(&l.item.variant_id, &l.item.unit_id, qty)
            });
            map.push(i);
        }
        (
            QuoteRequest {
                customer_id: customer.map(str::to_owned),
                lines,
            },
            map,
        )
    }

    /// Applies a quote of the lines `map` names (see the module docs).
    pub fn apply_quote(&mut self, quote: &Quote, map: &[usize]) {
        self.settings = Settings {
            tax: TaxSettings {
                inclusive: quote.tax_inclusive,
                registered: quote.tax_registered,
            },
            rounding: quote.cash_rounding.parse().unwrap_or_default(),
        };
        for (q, &i) in quote.lines.iter().zip(map) {
            let Some(line) = self.lines.get_mut(i) else {
                continue;
            };
            if let Some(tax) = &q.tax {
                line.core.item.tax_rate_bp = tax.rate_bp;
            }
            if line.core.is_price_label() {
                line.fresh = false;
                continue;
            }
            line.error = q.error.as_ref().map(|e| e.code.clone());
            if line.core.mode == PriceMode::Listed {
                match q.unit_price {
                    Some(p) if line.error.is_none() => line.core.unit_price = Money::from_minor(p),
                    _ => {
                        line.error.get_or_insert_with(|| "no_price".to_owned());
                    }
                }
            }
            if line.error.is_none() {
                line.fresh = false;
            }
        }
    }

    /// Takes out the new lines the quote could not price: they never were in the cart (§2: a
    /// `no_price` item is not added). Returns their names.
    pub fn drop_unpriced(&mut self) -> Vec<String> {
        let mut dropped = Vec::new();
        self.lines.retain(|l| {
            let keep = !(l.fresh && l.error.is_some());
            if !keep {
                dropped.push(l.item.name.clone());
            }
            keep
        });
        dropped
    }

    /// Whether every line has a price (no quote error) and no quote is waiting.
    #[must_use]
    pub fn priced(&self) -> bool {
        self.quoting.is_none() && self.lines.iter().all(|l| l.error.is_none())
    }

    /// The totals (§6).
    ///
    /// # Errors
    /// See [`compute_totals`].
    pub fn totals(&self) -> Result<CartTotals, SaleError> {
        let lines = self
            .lines
            .iter()
            .map(|l| l.core.totals_line())
            .collect::<Result<Vec<_>, _>>()?;
        compute_totals(&lines, self.discount, self.settings.tax)
    }

    /// The item count: whole units of integer lines, one per decimal (weighed) line.
    #[must_use]
    pub fn item_count(&self) -> i64 {
        self.lines
            .iter()
            .map(|l| match l.core.item.unit_kind {
                UnitKind::Integer => l.core.qty.thousandths() / 1_000,
                UnitKind::Decimal => 1,
            })
            .sum()
    }

    /// The limited checks the posting needs, in the server's order (see the module docs):
    /// `(permission, value)`, values in basis points.
    ///
    /// # Errors
    /// See [`compute_totals`].
    pub fn approvals(&self) -> Result<Vec<(&'static str, Option<i64>)>, SaleError> {
        let totals = self.totals()?;
        let mut out = Vec::new();
        for (line, t) in self.lines.iter().zip(&totals.lines) {
            if let PriceMode::Overridden { resolved } = line.core.mode {
                out.push((
                    "price.override",
                    resolved
                        .map(|r| i64::from(discount_bp(r, r.saturating_sub(line.core.unit_price)))),
                ));
            }
            if line.core.discount.is_some() && t.line_discount > Money::ZERO {
                out.push(("sale.discount", Some(i64::from(t.line_discount_bp()))));
            }
        }
        if self.discount.is_some() && totals.cart_discount > Money::ZERO {
            out.push((
                "sale.cart_discount",
                Some(i64::from(totals.cart_discount_bp())),
            ));
        }
        Ok(out)
    }

    /// The posted body (§19 item 37) with these tenders and saved change.
    ///
    /// # Errors
    /// See [`compute_totals`].
    pub fn sale_request(
        &self,
        customer: Option<&str>,
        tenders: Vec<TenderRequest>,
        save_change: Option<i64>,
    ) -> Result<SaleRequest, SaleError> {
        let t = self.totals()?;
        Ok(SaleRequest {
            customer_id: customer.map(str::to_owned),
            tax_invoice: self.tax_invoice,
            lines: self.lines.iter().map(line_request).collect(),
            cart_discount: self.discount.map(wire_discount),
            tenders,
            save_change,
            totals: SaleTotals {
                subtotal: t.subtotal.minor(),
                line_discount: t.line_discount.minor(),
                cart_discount: t.cart_discount.minor(),
                tax: t.tax.minor(),
                total: t.total.minor(),
            },
        })
    }
}

/// A core discount as the API sends it.
#[must_use]
pub fn wire_discount(d: Discount) -> WireDiscount {
    match d {
        Discount::Pct(bp) => WireDiscount {
            pct_bp: Some(bp),
            amount: None,
        },
        Discount::Amount(a) => WireDiscount {
            pct_bp: None,
            amount: Some(a.minor()),
        },
    }
}

fn core_discount(d: &WireDiscount) -> Option<Discount> {
    match (d.pct_bp, d.amount) {
        (Some(bp), _) => Some(Discount::Pct(bp)),
        (None, Some(a)) => Some(Discount::Amount(Money::from_minor(a))),
        (None, None) => None,
    }
}

fn line_request(l: &Line) -> SaleLineRequest {
    let (scale_label, label_amount) = match l.core.scale_label {
        Some(ScaleLabel::Weight) => (Some("weight".to_owned()), None),
        Some(ScaleLabel::Price { amount }) => (Some("price".to_owned()), Some(amount.minor())),
        None => (None, None),
    };
    SaleLineRequest {
        variant_id: l.item.variant_id.clone(),
        unit_id: l.item.unit_id.clone(),
        qty_thousandths: l.core.qty.thousandths(),
        typed_price: match l.core.mode {
            PriceMode::Listed => None,
            _ => Some(l.core.unit_price.minor()),
        },
        discount: l.core.discount.map(wire_discount),
        scale_label,
        label_amount,
    }
}

/// One line of the crash copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SavedLine {
    /// The lookup answer (with the current unit).
    pub item: Lookup,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// `listed`, `open_price` or `overridden`.
    pub mode: String,
    /// The unit price.
    pub price: i64,
    /// The price an override replaced.
    #[serde(default)]
    pub resolved: Option<i64>,
    /// The line discount.
    #[serde(default)]
    pub discount: Option<WireDiscount>,
}

/// The crash copy of the cart (D2): written on every change, restored for the same cashier
/// session only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    /// The cashier session the cart belongs to.
    pub session_id: String,
    /// The cart ID.
    pub cart_id: String,
    /// The attached customer.
    #[serde(default)]
    pub customer: Option<Customer>,
    /// The lines.
    pub lines: Vec<SavedLine>,
    /// The cart discount.
    #[serde(default)]
    pub discount: Option<WireDiscount>,
    /// A tax invoice.
    #[serde(default)]
    pub tax_invoice: bool,
}

impl Saved {
    /// The crash copy of `cart` in `session`.
    #[must_use]
    pub fn of(cart: &Cart, session_id: &str, customer: Option<&Customer>) -> Self {
        Self {
            session_id: session_id.to_owned(),
            cart_id: cart.id.clone(),
            customer: customer.cloned(),
            lines: cart
                .lines
                .iter()
                .map(|l| SavedLine {
                    item: l.item.clone(),
                    qty_thousandths: l.core.qty.thousandths(),
                    mode: match l.core.mode {
                        PriceMode::Listed => "listed",
                        PriceMode::OpenPrice => "open_price",
                        PriceMode::Overridden { .. } => "overridden",
                    }
                    .to_owned(),
                    price: l.core.unit_price.minor(),
                    resolved: l.resolved(),
                    discount: l.core.discount.map(wire_discount),
                })
                .collect(),
            discount: cart.discount.map(wire_discount),
            tax_invoice: cart.tax_invoice,
        }
    }

    /// The cart again (prices are quoted again by the caller).
    #[must_use]
    pub fn cart(&self) -> Cart {
        let lines = self
            .lines
            .iter()
            .map(|s| {
                let mut line = Line::new(
                    s.item.clone(),
                    Qty::from_thousandths(s.qty_thousandths),
                    None,
                );
                line.core.unit_price = Money::from_minor(s.price);
                line.core.mode = match s.mode.as_str() {
                    "open_price" => PriceMode::OpenPrice,
                    "overridden" => PriceMode::Overridden {
                        resolved: s.resolved.map(Money::from_minor),
                    },
                    _ => PriceMode::Listed,
                };
                line.core.discount = s.discount.as_ref().and_then(core_discount);
                line.fresh = false;
                line
            })
            .collect();
        Cart {
            id: self.cart_id.clone(),
            lines,
            discount: self.discount.as_ref().and_then(core_discount),
            tax_invoice: self.tax_invoice,
            settings: Settings::default(),
            quoting: None,
        }
    }
}

/// What the payment panel is asking for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayStep {
    /// The cash received (empty = exactly what is due).
    Cash,
    /// Picking a non-cash tender type (index into [`Pay::others`]).
    Pick(usize),
    /// The amount of the picked tender (empty = the rest, or the balance).
    Amount(usize),
    /// The reference of the picked tender and its amount.
    Reference(usize, i64),
    /// Paid: Enter posts, F8 saves change.
    Ready,
    /// The change to save (F8).
    Save,
}

/// The payment panel (§5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pay {
    /// The core payment.
    pub payment: Payment,
    /// The active tender types.
    pub types: Vec<TenderType>,
    ids: Vec<TenderTypeId>,
    /// The step.
    pub step: PayStep,
    /// Typed digits of the current amount.
    pub digits: crate::amount::Digits,
    /// Typed reference text.
    pub text: String,
    /// The attached customer's balance (the balance tender and saved change).
    pub balance: Option<i64>,
    /// Override tokens collected for this sale (comma separated at posting).
    pub tokens: Vec<String>,
    /// The checks approved so far.
    pub approved: Vec<(String, Option<i64>)>,
    /// A post is running.
    pub posting: bool,
    rounding: CashRounding,
    /// The exchange credit applied first (D12).
    pub exchange: i64,
}

/// The longest reference, in characters (`core` refuses more).
pub const MAX_REFERENCE: usize = 64;

/// An exchange `credit` against a new cart of `total` (D12, `core::sale::net_exchange`): what
/// the customer still pays, and what is paid back besides: in cash after cash rounding (as the
/// server's refund plan does), or the exact rest to the balance (`balance`).
#[must_use]
pub fn exchange_net(credit: i64, total: i64, rounding: CashRounding, balance: bool) -> (i64, i64) {
    let Ok(net) =
        sultan_core::sale::net_exchange(Money::from_minor(credit), Money::from_minor(total))
    else {
        return (total.max(0), 0);
    };
    let back = if balance {
        net.refund
    } else {
        rounding.apply(net.refund, true).amount
    };
    (net.customer_pays.minor(), back.minor())
}

impl Pay {
    /// A payment of `total` with these tender types.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] for a negative total.
    pub fn new(
        total: i64,
        rounding: CashRounding,
        types: Vec<TenderType>,
        balance: Option<i64>,
    ) -> Result<Self, SaleError> {
        let ids = types.iter().map(|_| TenderTypeId::generate()).collect();
        let payment = Payment::new(Money::from_minor(total), rounding)?;
        let step = if payment.is_complete() {
            PayStep::Ready
        } else {
            PayStep::Cash
        };
        Ok(Self {
            payment,
            types,
            ids,
            step,
            digits: crate::amount::Digits::amount(),
            text: String::new(),
            balance,
            tokens: Vec::new(),
            approved: Vec::new(),
            posting: false,
            rounding,
            exchange: 0,
        })
    }

    /// Applies an exchange's refund first: at most the total (`core::sale::net_exchange`).
    ///
    /// # Errors
    /// `invalid_field` for a negative credit.
    pub fn with_exchange(mut self, credit: i64) -> Result<Self, SaleError> {
        let net = sultan_core::sale::net_exchange(Money::from_minor(credit), self.payment.total())?;
        self.exchange = net.exchange_tender.minor();
        if self.exchange > 0 {
            self.payment
                .add_exchange(Money::from_minor(self.exchange))?;
        }
        self.after_tender();
        Ok(self)
    }

    /// What an exchange pays back besides (D12): the credit this sale does not use, in cash
    /// after cash rounding, or to the balance (`balance`). See [`exchange_net`].
    #[must_use]
    pub fn pay_back(&self, credit: i64, balance: bool) -> i64 {
        exchange_net(credit, self.payment.total().minor(), self.rounding, balance).1
    }

    fn core(&self, index: usize) -> Option<CoreTender> {
        let t = self.types.get(index)?;
        Some(CoreTender {
            id: self.ids[index],
            name: t.name.clone(),
            kind: t.kind.parse().ok()?,
            needs_reference: t.needs_reference,
            opens_drawer: t.opens_drawer,
            active: t.active && !t.archived,
        })
    }

    /// The non-cash tender types the F9 list offers: the balance only with a customer attached.
    #[must_use]
    pub fn others(&self) -> Vec<usize> {
        self.types
            .iter()
            .enumerate()
            .filter(|(_, t)| t.active && !t.archived && t.kind != "cash")
            .filter(|(_, t)| t.kind != "customer_balance" || self.balance.is_some())
            .map(|(i, _)| i)
            .collect()
    }

    fn cash(&self) -> Option<usize> {
        self.types
            .iter()
            .position(|t| t.kind == "cash" && t.active && !t.archived)
    }

    fn context(&self) -> Option<BalanceContext> {
        self.balance.map(|b| BalanceContext {
            balance: Money::from_minor(b),
        })
    }

    /// The rest to pay.
    #[must_use]
    pub fn rest(&self) -> i64 {
        self.payment.rest().minor()
    }

    /// What settles the rest in cash.
    #[must_use]
    pub fn cash_due(&self) -> i64 {
        self.payment.cash_due().minor()
    }

    /// Whether the sale is paid.
    #[must_use]
    pub fn complete(&self) -> bool {
        self.payment.is_complete()
    }

    fn after_tender(&mut self) {
        self.digits.clear();
        self.text.clear();
        self.step = if self.complete() {
            PayStep::Ready
        } else {
            PayStep::Cash
        };
    }

    /// Adds the cash typed (or exactly what is due when nothing was typed).
    ///
    /// # Errors
    /// The tender rules of `core::sale::Payment::add`; `invalid_field` without a cash type.
    pub fn add_cash(&mut self) -> Result<(), SaleError> {
        let index = self
            .cash()
            .ok_or_else(|| invalid("tender_type", "no cash tender type"))?;
        let amount = if self.digits.is_empty() {
            self.cash_due()
        } else {
            self.digits.value()
        };
        let core = self
            .core(index)
            .ok_or_else(|| invalid("tender_type", "unknown"))?;
        self.payment
            .add(&core, Money::from_minor(amount), None, self.context())?;
        self.after_tender();
        Ok(())
    }

    /// The default amount of tender `index`: the rest, or at most the balance.
    #[must_use]
    pub fn default_amount(&self, index: usize) -> i64 {
        let rest = self.rest();
        match (self.types.get(index), self.balance) {
            (Some(t), Some(b)) if t.kind == "customer_balance" => {
                let used: i64 = self
                    .payment
                    .tenders()
                    .iter()
                    .filter(|t| t.kind == TenderKind::CustomerBalance)
                    .map(|t| t.amount.minor())
                    .sum();
                rest.min(b - used).max(0)
            }
            _ => rest,
        }
    }

    /// The amount step is done: asks for the reference when the type needs one, else adds.
    ///
    /// # Errors
    /// As [`Pay::add_other`].
    pub fn confirm_amount(&mut self, index: usize) -> Result<(), SaleError> {
        let amount = if self.digits.is_empty() {
            self.default_amount(index)
        } else {
            self.digits.value()
        };
        let cash = self.types.get(index).is_some_and(|t| t.kind == "cash");
        if !cash && amount > self.rest() {
            return Err(SaleError::TenderOverRest {
                rest: self.payment.rest(),
            });
        }
        if self.types.get(index).is_some_and(|t| t.needs_reference) {
            self.text.clear();
            self.step = PayStep::Reference(index, amount);
            return Ok(());
        }
        self.add_other(index, amount, None)
    }

    /// Adds a non-cash tender.
    ///
    /// # Errors
    /// `tender_over_rest`, `tender_reference_required`, `insufficient_balance`, `invalid_field`.
    pub fn add_other(
        &mut self,
        index: usize,
        amount: i64,
        reference: Option<&str>,
    ) -> Result<(), SaleError> {
        let core = self
            .core(index)
            .ok_or_else(|| invalid("tender_type", "unknown"))?;
        self.payment
            .add(&core, Money::from_minor(amount), reference, self.context())?;
        self.after_tender();
        Ok(())
    }

    /// Takes the last tender back (Esc): a new payment with the others (`core`'s rule).
    pub fn undo(&mut self) -> bool {
        let tenders = self.payment.tenders().to_vec();
        let Some((_, keep)) = tenders.split_last() else {
            return false;
        };
        let mut payment = self.payment.clone();
        if let Ok(fresh) = Payment::new(payment.total(), self.rounding) {
            payment = fresh;
        }
        if self.exchange > 0 {
            let _ = payment.add_exchange(Money::from_minor(self.exchange));
        }
        if keep.is_empty() && self.exchange > 0 && tenders.len() == 1 {
            // Only the exchange credit is left: it is not taken back.
            return false;
        }
        for t in keep {
            let Some(index) = t
                .tender_type
                .and_then(|id| self.ids.iter().position(|x| *x == id))
            else {
                continue;
            };
            let Some(core) = self.core(index) else {
                continue;
            };
            let amount = t.cash_received.unwrap_or(t.amount);
            let _ = payment.add(&core, amount, t.reference.as_deref(), self.context());
        }
        self.payment = payment;
        self.after_tender();
        true
    }

    /// The change due (given + saved).
    #[must_use]
    pub fn change(&self) -> i64 {
        self.payment.change().minor()
    }

    /// The change saved so far.
    #[must_use]
    pub fn saved(&self) -> i64 {
        self.payment
            .tenders()
            .iter()
            .map(|t| t.change_saved.minor())
            .sum()
    }

    /// Saves the typed amount of the change (F8; `0` gives it all back). The limits of 2.5
    /// §11 that the register does not know are checked by the server at posting.
    ///
    /// # Errors
    /// `invalid_field` (no change, no customer, more than the change).
    pub fn save_change(&mut self) -> Result<(), SaleError> {
        let amount = self.digits.value();
        if amount > self.change() + self.saved() {
            return Err(invalid("amount", "more than the change"));
        }
        let open = BalanceSettings {
            max_saved_per_sale: Money::from_minor(i64::MAX / 4),
            max_balance: Money::from_minor(i64::MAX / 4),
        };
        self.payment
            .save_change(open, self.context(), Money::from_minor(amount))?;
        self.digits.clear();
        self.step = PayStep::Ready;
        Ok(())
    }

    /// The tenders as posted (cash: the cash received).
    #[must_use]
    pub fn requests(&self) -> Vec<TenderRequest> {
        self.payment
            .tenders()
            .iter()
            .filter_map(|t| {
                let index = self.ids.iter().position(|x| Some(*x) == t.tender_type)?;
                Some(TenderRequest {
                    tender_type_id: self.types[index].id.clone(),
                    amount: t.cash_received.unwrap_or(t.amount).minor(),
                    reference: t.reference.clone(),
                })
            })
            .collect()
    }

    /// The tenders for the panel: name, amount, reference (an exchange credit is named
    /// `exchange`, which the window translates).
    #[must_use]
    pub fn rows(&self) -> Vec<(String, i64, String)> {
        self.payment
            .tenders()
            .iter()
            .filter_map(|t| {
                if t.tender_type.is_none() {
                    return Some(("exchange".to_owned(), t.amount.minor(), String::new()));
                }
                let index = self.ids.iter().position(|x| Some(*x) == t.tender_type)?;
                Some((
                    self.types[index].name.clone(),
                    t.cash_received.unwrap_or(t.amount).minor(),
                    t.reference.clone().unwrap_or_default(),
                ))
            })
            .collect()
    }

    /// The saved change to post, if any.
    #[must_use]
    pub fn save_request(&self) -> Option<i64> {
        Some(self.saved()).filter(|s| *s > 0)
    }

    /// Whether a used tender type opens the drawer.
    #[must_use]
    pub fn opens_drawer(&self) -> bool {
        self.payment.tenders().iter().any(|t| {
            t.tender_type
                .and_then(|id| self.ids.iter().position(|x| *x == id))
                .is_some_and(|i| self.types[i].opens_drawer)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exchange_net_pays_back_cash_rounded_or_the_exact_balance() {
        // Credit 75 650 against 5 000: 70 650 back.
        let net = |rounding, balance| exchange_net(75_650, 5_000, rounding, balance);
        assert_eq!(net(CashRounding::None, false), (0, 70_650));
        assert_eq!(net(CashRounding::Nearest100, false), (0, 70_700));
        assert_eq!(net(CashRounding::Down100, false), (0, 70_600));
        assert_eq!(net(CashRounding::Down100, true), (0, 70_650));
        // The cart costs more: the customer pays the rest, nothing comes back.
        assert_eq!(
            exchange_net(5_000, 7_000, CashRounding::Down500, false),
            (2_000, 0)
        );
        assert_eq!(
            exchange_net(-1, 7_000, CashRounding::None, false),
            (7_000, 0)
        );
    }
}
