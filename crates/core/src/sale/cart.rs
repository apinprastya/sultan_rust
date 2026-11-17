//! The cart in progress (spec 2.6 §2–§3): lines, the rescan rule (D8), line actions, the
//! customer, the cart discount and the tax-invoice switch.
//!
//! The cart lives in the register's memory (D1); the server rebuilds it from the posted body and
//! checks it again. Prices come from the quote ([`Cart::quote_lines`], [`Cart::apply_quote`]);
//! the cart never resolves prices itself.

use uuid::Uuid;

use super::totals::{CartTotals, TotalsLine, compute_totals, pct_of};
use super::{MAX_LINES, MAX_QTY, SaleError};
use crate::access::{FULL_PCT_BASIS_POINTS, Permission, Requested};
use crate::catalog::{ComponentSnapshot, ProductType, Qty, UnitKind};
use crate::ids::{UnitId, VariantId};
use crate::money::Money;
use crate::pricing::{self, LineMode, PriceSource, PricedLine, SaleItem};
use crate::tax::TaxSettings;

/// A manual discount: a percentage in basis points or an amount in Rupiah (§3, §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Discount {
    /// A percentage in basis points (`1_000` = 10%), at most 100%.
    Pct(u16),
    /// An amount in Rupiah, not negative.
    Amount(Money),
}

impl Discount {
    /// Checks the discount: at most 100%, or an amount that is not negative.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`discount`).
    pub const fn validate(self) -> Result<Self, SaleError> {
        match self {
            Self::Pct(bp) if bp > FULL_PCT_BASIS_POINTS => Err(SaleError::InvalidField {
                field: "discount",
                reason: "must be at most 100%",
            }),
            Self::Amount(a) if a.is_negative() => Err(SaleError::InvalidField {
                field: "discount",
                reason: "must not be negative",
            }),
            _ => Ok(self),
        }
    }

    /// The discount in Rupiah on `base`: a % rounded half up, an amount capped at the base. Never
    /// more than the base, never below 0.
    #[must_use]
    pub fn amount_of(self, base: Money) -> Money {
        let base = base.max(Money::ZERO);
        match self {
            Self::Pct(bp) => pct_of(base, bp.min(FULL_PCT_BASIS_POINTS)),
            Self::Amount(a) => a.min(base),
        }
        .max(Money::ZERO)
    }
}

/// `discount ÷ base` in basis points, rounded up (so 10.01% never passes a 10% limit), at most
/// 100%: the value a `MaxPct` limit is checked against. No discount → 0; a discount on a base of
/// 0 → 100%.
#[must_use]
pub fn discount_bp(base: Money, discount: Money) -> u16 {
    if discount.minor() <= 0 {
        return 0;
    }
    if base.minor() <= 0 {
        return FULL_PCT_BASIS_POINTS;
    }
    let full = i128::from(FULL_PCT_BASIS_POINTS);
    let d = i128::from(discount.minor());
    let b = i128::from(base.minor());
    let bp = (d * full + b - 1) / b;
    u16::try_from(bp.min(full)).unwrap_or(FULL_PCT_BASIS_POINTS)
}

/// What the catalog lookup says about the item on a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CartItem {
    /// The variant.
    pub variant: VariantId,
    /// Its product type: weighed lines never merge (D8), services and bundles move no stock of
    /// their own.
    pub product_type: ProductType,
    /// The unit it is sold in.
    pub unit: UnitId,
    /// The product's base unit.
    pub base_unit: UnitId,
    /// The unit's factor to the base unit.
    pub factor: Qty,
    /// Whole numbers or up to 3 decimals.
    pub unit_kind: UnitKind,
    /// An `open_price` product: the cashier types the price (2.3 §5).
    pub open_price: bool,
    /// The tax rate on the sale date, in basis points.
    pub tax_rate_bp: u16,
    /// A bundle's components as of the sale (2.2 §5); empty otherwise.
    pub components: Vec<ComponentSnapshot>,
}

/// A scale label on a line (2.2 §6, 2.3 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScaleLabel {
    /// Weight-embedded: qty = weight, priced normally.
    Weight,
    /// Price-embedded: the printed amount is final and never takes the cart discount (§6).
    Price {
        /// The printed amount.
        amount: Money,
    },
}

/// How a line's unit price was set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PriceMode {
    /// From the quote.
    Listed,
    /// Typed by the cashier for an open-price item (no permission needed).
    OpenPrice,
    /// A manual override (`price.override`, 2.3 §6), kept when the cart is re-priced.
    Overridden {
        /// The resolved price it replaced, if there was one.
        resolved: Option<Money>,
    },
}

/// A line of the cart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CartLine {
    /// The item.
    pub item: CartItem,
    /// The quantity in the line's unit. A price-embedded label without a store price has no
    /// derived quantity: then it is 0 (2.3 §5).
    pub qty: Qty,
    /// The unit price charged.
    pub unit_price: Money,
    /// Where a listed price came from.
    pub price_source: Option<PriceSource>,
    /// How the unit price was set.
    pub mode: PriceMode,
    /// The scale label the line was scanned from, if any.
    pub scale_label: Option<ScaleLabel>,
    /// The manual line discount.
    pub discount: Option<Discount>,
}

impl CartLine {
    /// A listed line with no price yet (the quote fills it in).
    #[must_use]
    pub fn new(item: CartItem, qty: Qty) -> Self {
        let mode = if item.open_price {
            PriceMode::OpenPrice
        } else {
            PriceMode::Listed
        };
        Self {
            item,
            qty,
            unit_price: Money::ZERO,
            price_source: None,
            mode,
            scale_label: None,
            discount: None,
        }
    }

    /// Whether this is a price-embedded scale label.
    #[must_use]
    pub const fn is_price_label(&self) -> bool {
        matches!(self.scale_label, Some(ScaleLabel::Price { .. }))
    }

    /// Whether a rescan of the same item may add to this line (D8): not weighed, not a scale
    /// label, not open-price, not overridden.
    #[must_use]
    pub fn mergeable(&self) -> bool {
        self.item.product_type != ProductType::Weighed
            && self.scale_label.is_none()
            && self.mode == PriceMode::Listed
            && !self.item.open_price
    }

    /// `unit_price × qty` rounded half up, or the printed amount of a price-embedded label.
    ///
    /// # Errors
    /// [`SaleError::Pricing`] on overflow.
    pub fn gross(&self) -> Result<Money, SaleError> {
        match self.scale_label {
            Some(ScaleLabel::Price { amount }) => Ok(amount),
            _ => Ok(pricing::line_amount(self.unit_price, self.qty)?),
        }
    }

    /// The line as the totals need it.
    ///
    /// # Errors
    /// See [`CartLine::gross`].
    pub fn totals_line(&self) -> Result<TotalsLine, SaleError> {
        Ok(TotalsLine {
            gross: self.gross()?,
            line_discount: self.discount,
            cart_discountable: !self.is_price_label(),
            tax_rate_bp: self.item.tax_rate_bp,
        })
    }

    fn sale_item(&self) -> SaleItem {
        SaleItem {
            variant: self.item.variant,
            unit: self.item.unit,
            base_unit: self.item.base_unit,
            factor: self.item.factor,
        }
    }
}

/// Checks a line quantity (§2): above 0, at most 99 999.999, whole for an integer unit.
///
/// # Errors
/// [`SaleError::InvalidQty`].
pub fn check_qty(qty: Qty, kind: UnitKind) -> Result<(), SaleError> {
    if !qty.is_positive() {
        return Err(SaleError::InvalidQty {
            reason: "must be greater than 0",
        });
    }
    if qty > MAX_QTY {
        return Err(SaleError::InvalidQty {
            reason: "must be at most 99 999.999",
        });
    }
    if kind == UnitKind::Integer && !qty.is_whole() {
        return Err(SaleError::InvalidQty {
            reason: "must be a whole number for this unit",
        });
    }
    Ok(())
}

/// The customer attached to the cart, as far as the cart needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CartCustomer {
    /// The customer.
    pub id: Uuid,
    /// The customer has a `tax_id`, so the sale may be a tax invoice (D13).
    pub has_tax_id: bool,
}

/// Why a line or the whole cart was voided (§3). Fixed in code for now.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VoidReason {
    /// `customer_changed_mind`.
    CustomerChangedMind,
    /// `wrong_item`.
    WrongItem,
    /// `price_dispute`.
    PriceDispute,
    /// `other`, with the cashier's text.
    Other(String),
}

impl VoidReason {
    /// The longest `other` text, in characters.
    pub const MAX_TEXT_LEN: usize = 200;

    /// The stored key.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::CustomerChangedMind => "customer_changed_mind",
            Self::WrongItem => "wrong_item",
            Self::PriceDispute => "price_dispute",
            Self::Other(_) => "other",
        }
    }

    /// The reason from its key and, for `other`, the text (trimmed).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`reason`) for an unknown key, or an `other` text that is
    /// blank or longer than [`VoidReason::MAX_TEXT_LEN`].
    pub fn parse(key: &str, text: Option<&str>) -> Result<Self, SaleError> {
        match key {
            "customer_changed_mind" => Ok(Self::CustomerChangedMind),
            "wrong_item" => Ok(Self::WrongItem),
            "price_dispute" => Ok(Self::PriceDispute),
            "other" => Self::Other(text.unwrap_or_default().to_owned()).validate(),
            _ => Err(SaleError::InvalidField {
                field: "reason",
                reason: "unknown void reason",
            }),
        }
    }

    /// Checks (and trims) an `other` text.
    ///
    /// # Errors
    /// See [`VoidReason::parse`].
    pub fn validate(self) -> Result<Self, SaleError> {
        match self {
            Self::Other(text) => {
                let text = text.trim();
                if text.is_empty() {
                    return Err(SaleError::InvalidField {
                        field: "reason",
                        reason: "`other` needs a text",
                    });
                }
                if text.chars().count() > Self::MAX_TEXT_LEN {
                    return Err(SaleError::InvalidField {
                        field: "reason",
                        reason: "is too long",
                    });
                }
                Ok(Self::Other(text.to_owned()))
            }
            other => Ok(other),
        }
    }
}

/// A removed line (§3): always a void, counted in the close report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoidedLine {
    /// The line as it was.
    pub line: CartLine,
    /// Its gross amount, for the close report.
    pub amount: Money,
    /// Why.
    pub reason: VoidReason,
}

/// What [`Cart::add`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AddOutcome {
    /// The line that holds the item now.
    pub index: usize,
    /// The item was added to an existing line (D8).
    pub merged: bool,
}

/// The till actions of §3–§8 and the permission each needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CartAction {
    /// Add an item, change a qty (also lowering it) or a unit, hold the cart.
    Sell,
    /// Remove a line: every removed line is a void (§3).
    VoidLine,
    /// Clear the whole cart, or drop a held cart without resuming it (§3, §4).
    VoidCart,
    /// Set a manual unit price.
    PriceOverride,
    /// A line discount.
    LineDiscount,
    /// A whole-cart discount.
    CartDiscount,
    /// Void a posted sale (§7).
    PostVoid,
    /// Return by receipt (§8).
    ReturnWithReceipt,
    /// Return without a receipt (§8).
    ReturnNoReceipt,
}

impl CartAction {
    /// The permission the action needs (§3, §11).
    #[must_use]
    pub const fn permission(self) -> Permission {
        match self {
            Self::Sell => Permission::SaleCreate,
            Self::VoidLine => Permission::SaleVoidLine,
            Self::VoidCart => Permission::SaleVoid,
            Self::PriceOverride => Permission::PriceOverride,
            Self::LineDiscount => Permission::SaleDiscount,
            Self::CartDiscount => Permission::SaleCartDiscount,
            Self::PostVoid => Permission::SalePostVoid,
            Self::ReturnWithReceipt => Permission::RefundWithReceipt,
            Self::ReturnNoReceipt => Permission::RefundNoReceipt,
        }
    }
}

/// A permission check a posted cart needs, with the value its limit is checked against
/// (pass both to [`crate::access::PermissionSet::check`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PermissionRequest {
    /// The permission.
    pub permission: Permission,
    /// The value for its limit; `None` when no limit applies (an override without a resolved
    /// price).
    pub requested: Option<Requested>,
    /// The line it is about, or `None` for the cart discount.
    pub line: Option<usize>,
}

/// The cart in progress (§2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cart {
    lines: Vec<CartLine>,
    customer: Option<CartCustomer>,
    discount: Option<Discount>,
    tax_invoice: bool,
}

fn no_line() -> SaleError {
    SaleError::InvalidField {
        field: "line",
        reason: "no such line",
    }
}

impl Cart {
    /// An empty cart.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The lines, in order.
    #[must_use]
    pub fn lines(&self) -> &[CartLine] {
        &self.lines
    }

    /// Whether the cart has no lines.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The attached customer.
    #[must_use]
    pub const fn customer(&self) -> Option<CartCustomer> {
        self.customer
    }

    /// The cart discount.
    #[must_use]
    pub const fn cart_discount(&self) -> Option<Discount> {
        self.discount
    }

    /// Whether the sale is marked as a tax invoice (D13).
    #[must_use]
    pub const fn tax_invoice(&self) -> bool {
        self.tax_invoice
    }

    fn line_mut(&mut self, index: usize) -> Result<&mut CartLine, SaleError> {
        self.lines.get_mut(index).ok_or_else(no_line)
    }

    /// Adds a line (§2). A rescan of the same variant + unit adds its qty to the last line that
    /// [may take it](CartLine::mergeable) when the new line may be merged too (D8); otherwise
    /// a new line is appended.
    ///
    /// # Errors
    /// [`SaleError::InvalidQty`] (see [`check_qty`]; a price-embedded label may have qty 0),
    /// also when the merged qty would be too large; [`SaleError::CartTooLarge`].
    pub fn add(&mut self, line: CartLine) -> Result<AddOutcome, SaleError> {
        if !(line.is_price_label() && line.qty == Qty::ZERO) {
            check_qty(line.qty, line.item.unit_kind)?;
        }
        if line.mergeable()
            && let Some(index) = self.lines.iter().rposition(|l| {
                l.mergeable()
                    && l.item.variant == line.item.variant
                    && l.item.unit == line.item.unit
            })
        {
            let existing = &mut self.lines[index];
            let qty = existing
                .qty
                .checked_add(line.qty)
                .ok_or(SaleError::InvalidQty {
                    reason: "must be at most 99 999.999",
                })?;
            check_qty(qty, existing.item.unit_kind)?;
            existing.qty = qty;
            return Ok(AddOutcome {
                index,
                merged: true,
            });
        }
        if self.lines.len() >= MAX_LINES {
            return Err(SaleError::CartTooLarge { max: MAX_LINES });
        }
        self.lines.push(line);
        Ok(AddOutcome {
            index: self.lines.len() - 1,
            merged: false,
        })
    }

    /// Changes a line's quantity (§3: `sale.create`; lowering is not a void).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`) for an unknown line or a price-embedded label (its
    /// quantity comes from the label); [`SaleError::InvalidQty`].
    pub fn set_qty(&mut self, index: usize, qty: Qty) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.is_price_label() {
            return Err(SaleError::InvalidField {
                field: "line",
                reason: "a price label's quantity cannot change",
            });
        }
        check_qty(qty, line.item.unit_kind)?;
        line.qty = qty;
        Ok(())
    }

    /// Changes a line's unit to another unit of the product (§2). An overridden line goes back
    /// to the listed price (the override was for the old unit); the quote prices it again.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`) for an unknown line, a scale label or another
    /// variant; [`SaleError::InvalidQty`] when the qty does not fit the new unit.
    pub fn set_unit(
        &mut self,
        index: usize,
        unit: UnitId,
        factor: Qty,
        unit_kind: UnitKind,
    ) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.scale_label.is_some() {
            return Err(SaleError::InvalidField {
                field: "line",
                reason: "a scale label's unit cannot change",
            });
        }
        check_qty(line.qty, unit_kind)?;
        line.item.unit = unit;
        line.item.factor = factor;
        line.item.unit_kind = unit_kind;
        if matches!(line.mode, PriceMode::Overridden { .. }) {
            line.mode = PriceMode::Listed;
        }
        line.price_source = None;
        Ok(())
    }

    /// Sets the typed unit price of an open-price line (2.3 §5, no permission).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`) when the line is not open-price, or (`price`) for a
    /// negative price.
    pub fn set_open_price(&mut self, index: usize, price: Money) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.mode != PriceMode::OpenPrice {
            return Err(SaleError::InvalidField {
                field: "line",
                reason: "is not an open-price line",
            });
        }
        if price.is_negative() {
            return Err(SaleError::InvalidField {
                field: "price",
                reason: "must not be negative",
            });
        }
        line.unit_price = price;
        Ok(())
    }

    /// The `price.override` request for a new unit price on a line (2.3 §6), without changing
    /// the line: the price it replaces is the resolved price (an overridden line keeps the one
    /// it had first). Check it with [`pricing::check_price_override`] or the permission set.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`) for an unknown, open-price or scale-label line, or
    /// (`price`) for a negative price.
    pub fn override_request(
        &self,
        index: usize,
        price: Money,
    ) -> Result<PermissionRequest, SaleError> {
        let line = self.lines.get(index).ok_or_else(no_line)?;
        let resolved = Self::override_base(line)?;
        if price.is_negative() {
            return Err(SaleError::InvalidField {
                field: "price",
                reason: "must not be negative",
            });
        }
        Ok(PermissionRequest {
            permission: Permission::PriceOverride,
            requested: resolved.map(|r| Requested::Pct(drop_bp(r, price))),
            line: Some(index),
        })
    }

    fn override_base(line: &CartLine) -> Result<Option<Money>, SaleError> {
        if line.scale_label.is_some() || line.mode == PriceMode::OpenPrice {
            return Err(SaleError::InvalidField {
                field: "line",
                reason: "open-price and scale-label lines cannot be overridden",
            });
        }
        Ok(match line.mode {
            PriceMode::Overridden { resolved } => resolved,
            _ => line.price_source.map(|_| line.unit_price),
        })
    }

    /// Sets a manual unit price (2.3 §6) after the permission check. The line keeps it when the
    /// cart is re-priced, and it is never merged with a rescan again.
    ///
    /// # Errors
    /// As [`Cart::override_request`].
    pub fn override_price(&mut self, index: usize, price: Money) -> Result<(), SaleError> {
        self.override_request(index, price)?;
        let line = self.line_mut(index)?;
        let resolved = Self::override_base(line)?;
        line.mode = PriceMode::Overridden { resolved };
        line.unit_price = price;
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
        self.line_mut(index)?.discount = discount;
        Ok(())
    }

    /// Sets or clears the whole-cart discount (§6).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`discount`).
    pub fn set_cart_discount(&mut self, discount: Option<Discount>) -> Result<(), SaleError> {
        self.discount = discount.map(Discount::validate).transpose()?;
        Ok(())
    }

    /// Attaches or detaches a customer (§2); the caller re-quotes the cart. Detaching clears the
    /// tax-invoice switch.
    pub fn set_customer(&mut self, customer: Option<CartCustomer>) {
        if !customer.is_some_and(|c| c.has_tax_id) {
            self.tax_invoice = false;
        }
        self.customer = customer;
    }

    /// Marks the sale as a tax invoice, or not (D13).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`tax_invoice`) without a customer that has a tax id.
    pub fn set_tax_invoice(&mut self, on: bool) -> Result<(), SaleError> {
        if on && !self.customer.is_some_and(|c| c.has_tax_id) {
            return Err(SaleError::InvalidField {
                field: "tax_invoice",
                reason: "needs a customer with a tax id",
            });
        }
        self.tax_invoice = on;
        Ok(())
    }

    /// Removes a line: always a void (§3), needing [`CartAction::VoidLine`] and a reason.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line` or `reason`).
    pub fn void_line(&mut self, index: usize, reason: VoidReason) -> Result<VoidedLine, SaleError> {
        let reason = reason.validate()?;
        let amount = self.lines.get(index).ok_or_else(no_line)?.gross()?;
        let line = self.lines.remove(index);
        Ok(VoidedLine {
            line,
            amount,
            reason,
        })
    }

    /// Clears the whole cart: a cart void (§3), needing [`CartAction::VoidCart`] and a reason.
    /// Returns every line (for the audit entry) with the cart's gross amount. The customer and
    /// discounts are cleared too.
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`reason`, or `line` for an empty cart);
    /// [`SaleError::AmountOverflow`].
    pub fn void_cart(&mut self, reason: VoidReason) -> Result<(Vec<CartLine>, Money), SaleError> {
        reason.validate()?;
        if self.lines.is_empty() {
            return Err(SaleError::InvalidField {
                field: "line",
                reason: "the cart is empty",
            });
        }
        let mut amount = Money::ZERO;
        for line in &self.lines {
            amount = super::add(amount, line.gross()?)?;
        }
        let lines = std::mem::take(&mut self.lines);
        *self = Self::default();
        Ok((lines, amount))
    }

    /// The lines to quote (2.3 `resolve_cart` or `POST /pricing/quote`), with their index in
    /// the cart. Price-embedded labels are left out: their amount is final (2.3 §15 item 3).
    #[must_use]
    pub fn quote_lines(&self) -> Vec<(usize, pricing::CartLine)> {
        self.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| !l.is_price_label())
            .map(|(i, l)| {
                let mode = match l.mode {
                    PriceMode::Listed => LineMode::Listed,
                    PriceMode::OpenPrice => LineMode::OpenPrice(Some(l.unit_price)),
                    PriceMode::Overridden { .. } => LineMode::Overridden(l.unit_price),
                };
                (
                    i,
                    pricing::CartLine {
                        item: l.sale_item(),
                        qty: l.qty,
                        mode,
                    },
                )
            })
            .collect()
    }

    /// Applies the quoted price of line `index`: a listed line takes the price and its source;
    /// open-price, overridden and price-label lines keep their price (2.3 §6).
    ///
    /// # Errors
    /// [`SaleError::InvalidField`] (`line`).
    pub fn apply_quote(&mut self, index: usize, priced: &PricedLine) -> Result<(), SaleError> {
        let line = self.line_mut(index)?;
        if line.mode == PriceMode::Listed && !line.is_price_label() {
            line.unit_price = priced.unit_price;
            line.price_source = priced.resolved.map(|r| r.source);
        }
        Ok(())
    }

    /// The totals (§6).
    ///
    /// # Errors
    /// See [`compute_totals`].
    pub fn totals(&self, tax: TaxSettings) -> Result<CartTotals, SaleError> {
        let lines = self
            .lines
            .iter()
            .map(CartLine::totals_line)
            .collect::<Result<Vec<_>, _>>()?;
        compute_totals(&lines, self.discount, tax)
    }

    /// Every limited permission the cart needs at posting (§3, §6), in line order then the cart
    /// discount: `sale.discount` per discounted line (% of the line before discount),
    /// `price.override` per overridden line (% below the resolved price; no value without one),
    /// and `sale.cart_discount` (% of the cart total before the cart discount).
    ///
    /// # Errors
    /// See [`compute_totals`].
    pub fn permission_requests(
        &self,
        tax: TaxSettings,
    ) -> Result<Vec<PermissionRequest>, SaleError> {
        let totals = self.totals(tax)?;
        let mut out = Vec::new();
        for (i, (line, t)) in self.lines.iter().zip(&totals.lines).enumerate() {
            if let PriceMode::Overridden { resolved } = line.mode {
                out.push(PermissionRequest {
                    permission: Permission::PriceOverride,
                    requested: resolved.map(|r| Requested::Pct(drop_bp(r, line.unit_price))),
                    line: Some(i),
                });
            }
            if line.discount.is_some() && t.line_discount > Money::ZERO {
                out.push(PermissionRequest {
                    permission: Permission::SaleDiscount,
                    requested: Some(Requested::Pct(t.line_discount_bp())),
                    line: Some(i),
                });
            }
        }
        if self.discount.is_some() && totals.cart_discount > Money::ZERO {
            out.push(PermissionRequest {
                permission: Permission::SaleCartDiscount,
                requested: Some(Requested::Pct(totals.cart_discount_bp())),
                line: None,
            });
        }
        Ok(out)
    }
}

/// How far `new` is below `resolved`, in basis points rounded up; 0 when not below.
fn drop_bp(resolved: Money, new: Money) -> u16 {
    discount_bp(resolved, resolved.saturating_sub(new))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Limit;
    use crate::pricing::{OverrideDecision, PriceListId, PriceListKind, ResolvedPrice};

    fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    fn q(t: i64) -> Qty {
        Qty::from_thousandths(t)
    }

    fn item(variant: VariantId, unit: UnitId, product_type: ProductType) -> CartItem {
        let unit_kind = if product_type == ProductType::Weighed {
            UnitKind::Decimal
        } else {
            UnitKind::Integer
        };
        CartItem {
            variant,
            product_type,
            unit,
            base_unit: unit,
            factor: Qty::ONE,
            unit_kind,
            open_price: false,
            tax_rate_bp: 1_100,
            components: Vec::new(),
        }
    }

    fn priced(line: CartLine, price: i64) -> CartLine {
        CartLine {
            unit_price: m(price),
            price_source: Some(source()),
            ..line
        }
    }

    fn source() -> PriceSource {
        PriceSource {
            list: PriceListId::from_uuid(Uuid::nil()),
            kind: PriceListKind::Default,
            tier_min_qty: None,
            unit_fallback: false,
        }
    }

    #[test]
    fn discounts() {
        #[rustfmt::skip]
        let cases = [
            (Discount::Pct(1_000), 20_000, 2_000),
            (Discount::Pct(333), 1_000, 33),   // 33.3 → 33
            (Discount::Pct(350), 100, 4),      // 3.5 → 4
            (Discount::Pct(10_000), 5_000, 5_000),
            (Discount::Amount(m(500)), 2_000, 500),
            (Discount::Amount(m(5_000)), 2_000, 2_000),
            (Discount::Amount(m(500)), -10, 0),
            (Discount::Pct(20_000), 100, 100),
        ];
        for (d, base, expected) in cases {
            assert_eq!(d.amount_of(m(base)), m(expected), "{d:?} of {base}");
        }
        assert!(Discount::Pct(10_001).validate().is_err());
        assert!(Discount::Amount(m(-1)).validate().is_err());
        assert_eq!(Discount::Pct(10_000).validate(), Ok(Discount::Pct(10_000)));

        #[rustfmt::skip]
        let bps = [
            (20_000, 2_000, 1_000),
            (20_000, 2_001, 1_001), // 10.005% → rounded up
            (3, 1, 3_334),
            (0, 1, 10_000),
            (100, 0, 0),
            (100, 500, 10_000),
            (i64::MAX, i64::MAX, 10_000),
        ];
        for (base, discount, bp) in bps {
            assert_eq!(discount_bp(m(base), m(discount)), bp, "{discount}/{base}");
        }
    }

    #[test]
    fn quantities() {
        use UnitKind::{Decimal, Integer};
        #[rustfmt::skip]
        let cases = [
            (1_000, Integer, None),
            (99_999_000, Integer, None),
            (99_999_999, Decimal, None),
            (1, Decimal, None),
            (0, Decimal, Some("must be greater than 0")),
            (-1_000, Integer, Some("must be greater than 0")),
            (100_000_000, Decimal, Some("must be at most 99 999.999")),
            (1_500, Integer, Some("must be a whole number for this unit")),
        ];
        for (t, kind, err) in cases {
            assert_eq!(
                check_qty(q(t), kind),
                err.map_or(Ok(()), |reason| Err(SaleError::InvalidQty { reason })),
                "{t} {kind:?}"
            );
        }
    }

    /// Spec §15 case 2 and the other D8 exceptions.
    #[test]
    fn rescan_merge_rule() {
        let v = VariantId::generate();
        let pcs = UnitId::generate();
        let box_ = UnitId::generate();
        let kg = UnitId::generate();
        let w = VariantId::generate();
        let stock = |unit| CartLine::new(item(v, unit, ProductType::Stock), Qty::ONE);
        let weighed = || CartLine::new(item(w, kg, ProductType::Weighed), q(1_250));

        let mut cart = Cart::new();
        assert_eq!(
            cart.add(stock(pcs)),
            Ok(AddOutcome {
                index: 0,
                merged: false
            })
        );
        // Same variant + unit → one line with qty 2.
        assert_eq!(
            cart.add(stock(pcs)),
            Ok(AddOutcome {
                index: 0,
                merged: true
            })
        );
        assert_eq!(cart.lines()[0].qty, q(2_000));
        // Another unit → a new line.
        assert!(!cart.add(stock(box_)).unwrap().merged);
        // A weighed item twice → two lines.
        assert!(!cart.add(weighed()).unwrap().merged);
        assert!(!cart.add(weighed()).unwrap().merged);
        // Scale labels never merge.
        let label = |label| CartLine {
            scale_label: Some(label),
            ..stock(pcs)
        };
        assert!(!cart.add(label(ScaleLabel::Weight)).unwrap().merged);
        let price_label = CartLine {
            qty: Qty::ZERO,
            ..label(ScaleLabel::Price { amount: m(15_000) })
        };
        assert!(!cart.add(price_label).unwrap().merged);
        // Open-price items never merge, in either direction.
        let open = CartLine::new(
            CartItem {
                open_price: true,
                ..item(v, pcs, ProductType::Stock)
            },
            Qty::ONE,
        );
        assert_eq!(open.mode, PriceMode::OpenPrice);
        assert!(!cart.add(open).unwrap().merged);
        assert_eq!(cart.lines().len(), 7);
        // An overridden line takes no rescan; the rescan merges into the box line only when
        // that is the same unit.
        cart.override_price(0, m(4_000)).unwrap();
        assert!(!cart.add(stock(pcs)).unwrap().merged);
        assert_eq!(cart.lines().len(), 8);
        // The newest mergeable line takes the next scan.
        assert_eq!(
            cart.add(stock(pcs)).unwrap(),
            AddOutcome {
                index: 7,
                merged: true
            }
        );

        // A merge past the quantity limit is refused, and the line stays.
        let mut cart = Cart::new();
        cart.add(CartLine::new(
            item(v, pcs, ProductType::Stock),
            q(99_999_000),
        ))
        .unwrap();
        assert_eq!(cart.add(stock(pcs)).unwrap_err().code(), "invalid_qty");
        let huge = CartLine {
            qty: q(i64::MAX),
            ..stock(pcs)
        };
        let mut overflow = Cart::new();
        overflow.lines.push(huge);
        assert_eq!(overflow.add(stock(pcs)).unwrap_err().code(), "invalid_qty");
        assert_eq!(cart.lines()[0].qty, q(99_999_000));
        // A bad qty is refused before anything else.
        assert_eq!(
            cart.add(CartLine::new(item(v, pcs, ProductType::Stock), q(500)))
                .unwrap_err(),
            SaleError::InvalidQty {
                reason: "must be a whole number for this unit"
            }
        );
    }

    #[test]
    fn at_most_300_lines() {
        let mut cart = Cart::new();
        let unit = UnitId::generate();
        for _ in 0..MAX_LINES {
            cart.add(CartLine::new(
                item(VariantId::generate(), unit, ProductType::Stock),
                Qty::ONE,
            ))
            .unwrap();
        }
        let err = cart
            .add(CartLine::new(
                item(VariantId::generate(), unit, ProductType::Stock),
                Qty::ONE,
            ))
            .unwrap_err();
        assert_eq!(err, SaleError::CartTooLarge { max: 300 });
        assert_eq!(err.code(), "cart_too_large");
        // A rescan of an existing line still works on a full cart.
        let first = cart.lines()[0].clone();
        assert!(
            cart.add(CartLine {
                qty: Qty::ONE,
                ..first
            })
            .unwrap()
            .merged
        );
    }

    #[test]
    fn line_actions() {
        let v = VariantId::generate();
        let pcs = UnitId::generate();
        let box_ = UnitId::generate();
        let mut cart = Cart::new();
        cart.add(priced(
            CartLine::new(item(v, pcs, ProductType::Stock), q(3_000)),
            5_000,
        ))
        .unwrap();
        let label = CartLine {
            scale_label: Some(ScaleLabel::Price { amount: m(15_000) }),
            ..CartLine::new(
                item(VariantId::generate(), pcs, ProductType::Weighed),
                q(1_155),
            )
        };
        cart.add(label).unwrap();

        // Qty: lowering is fine; 0 is not (that is a void); a label's qty is fixed.
        cart.set_qty(0, q(2_000)).unwrap();
        assert_eq!(
            cart.set_qty(0, Qty::ZERO).unwrap_err().code(),
            "invalid_qty"
        );
        assert_eq!(cart.set_qty(1, Qty::ONE).unwrap_err().field(), Some("line"));
        assert_eq!(cart.set_qty(9, Qty::ONE).unwrap_err().field(), Some("line"));

        // Unit change: an override goes back to listed.
        cart.override_price(0, m(4_500)).unwrap();
        cart.set_unit(0, box_, q(24_000), UnitKind::Integer)
            .unwrap();
        let l = &cart.lines()[0];
        assert_eq!(
            (l.item.unit, l.item.factor, l.mode),
            (box_, q(24_000), PriceMode::Listed)
        );
        assert_eq!(
            cart.set_unit(1, box_, Qty::ONE, UnitKind::Integer)
                .unwrap_err()
                .field(),
            Some("line")
        );
        // A decimal qty does not fit an integer unit.
        let mut kg = Cart::new();
        let w = item(VariantId::generate(), pcs, ProductType::Stock);
        kg.add(CartLine::new(
            CartItem {
                unit_kind: UnitKind::Decimal,
                ..w
            },
            q(1_500),
        ))
        .unwrap();
        assert_eq!(
            kg.set_unit(0, box_, Qty::ONE, UnitKind::Integer)
                .unwrap_err()
                .code(),
            "invalid_qty"
        );

        // Open price only on open-price lines.
        assert_eq!(
            cart.set_open_price(0, m(1)).unwrap_err().field(),
            Some("line")
        );
        let mut open_cart = Cart::new();
        open_cart
            .add(CartLine::new(
                CartItem {
                    open_price: true,
                    ..item(v, pcs, ProductType::Stock)
                },
                Qty::ONE,
            ))
            .unwrap();
        open_cart.set_open_price(0, m(80_000)).unwrap();
        assert_eq!(open_cart.lines()[0].unit_price, m(80_000));
        assert_eq!(
            open_cart.set_open_price(0, m(-1)).unwrap_err().field(),
            Some("price")
        );
        // An open-price line cannot be overridden.
        assert_eq!(
            open_cart.override_request(0, m(1)).unwrap_err().field(),
            Some("line")
        );

        // Discounts.
        cart.set_line_discount(0, Some(Discount::Pct(500))).unwrap();
        assert!(
            cart.set_line_discount(0, Some(Discount::Pct(10_001)))
                .is_err()
        );
        assert!(cart.set_line_discount(5, None).is_err());
        cart.set_cart_discount(Some(Discount::Amount(m(1_000))))
            .unwrap();
        assert!(
            cart.set_cart_discount(Some(Discount::Amount(m(-1))))
                .is_err()
        );
        assert_eq!(cart.cart_discount(), Some(Discount::Amount(m(1_000))));
    }

    #[test]
    fn overrides_and_permission_requests() {
        let v = VariantId::generate();
        let pcs = UnitId::generate();
        let mut cart = Cart::new();
        cart.add(priced(
            CartLine::new(item(v, pcs, ProductType::Stock), Qty::ONE),
            10_000,
        ))
        .unwrap();
        cart.add(priced(
            CartLine::new(
                item(VariantId::generate(), pcs, ProductType::Stock),
                q(2_000),
            ),
            5_000,
        ))
        .unwrap();
        // A line without a resolved price (no_price): no limit applies.
        cart.add(CartLine::new(
            item(VariantId::generate(), pcs, ProductType::Stock),
            Qty::ONE,
        ))
        .unwrap();

        let req = cart.override_request(0, m(7_999)).unwrap();
        assert_eq!(req.requested, Some(Requested::Pct(2_001)));
        // The same value as the pricing check.
        assert_eq!(
            pricing::check_price_override(Some(m(10_000)), m(7_999), Some(Limit::MaxPct(2_000))),
            Ok(OverrideDecision::NeedsManager {
                requested: Requested::Pct(2_001)
            })
        );
        assert_eq!(
            cart.override_request(0, m(12_000)).unwrap().requested,
            Some(Requested::Pct(0))
        );
        assert_eq!(cart.override_request(2, m(1)).unwrap().requested, None);
        assert_eq!(
            cart.override_request(0, m(-1)).unwrap_err().field(),
            Some("price")
        );
        assert!(cart.override_request(7, m(1)).is_err());

        cart.override_price(0, m(8_000)).unwrap();
        // A second override keeps the first resolved price.
        cart.override_price(0, m(9_000)).unwrap();
        assert_eq!(
            cart.lines()[0].mode,
            PriceMode::Overridden {
                resolved: Some(m(10_000))
            }
        );
        cart.override_price(2, m(3_000)).unwrap();
        cart.set_line_discount(1, Some(Discount::Amount(m(1_500))))
            .unwrap();
        cart.set_cart_discount(Some(Discount::Pct(1_000))).unwrap();

        let tax = TaxSettings::default();
        let reqs = cart.permission_requests(tax).unwrap();
        let expected = [
            (
                Permission::PriceOverride,
                Some(Requested::Pct(1_000)),
                Some(0),
            ),
            // 1 500 of 10 000.
            (
                Permission::SaleDiscount,
                Some(Requested::Pct(1_500)),
                Some(1),
            ),
            (Permission::PriceOverride, None, Some(2)),
            // 10% of 9 000 + 8 500 + 3 000 = 2 050 of 20 500.
            (
                Permission::SaleCartDiscount,
                Some(Requested::Pct(1_000)),
                None,
            ),
        ];
        let got: Vec<_> = reqs
            .iter()
            .map(|r| (r.permission, r.requested, r.line))
            .collect();
        assert_eq!(got, expected);

        // A zero discount needs nothing.
        cart.set_line_discount(1, Some(Discount::Pct(0))).unwrap();
        cart.set_cart_discount(Some(Discount::Amount(Money::ZERO)))
            .unwrap();
        assert_eq!(cart.permission_requests(tax).unwrap().len(), 2);
    }

    #[test]
    fn voids_and_customer() {
        let pcs = UnitId::generate();
        let mut cart = Cart::new();
        for price in [5_000, 7_000] {
            cart.add(priced(
                CartLine::new(
                    item(VariantId::generate(), pcs, ProductType::Stock),
                    q(2_000),
                ),
                price,
            ))
            .unwrap();
        }
        // A void needs a valid reason.
        assert_eq!(
            cart.void_line(0, VoidReason::Other("  ".into()))
                .unwrap_err()
                .field(),
            Some("reason")
        );
        assert!(cart.void_line(5, VoidReason::WrongItem).is_err());
        let voided = cart.void_line(0, VoidReason::WrongItem).unwrap();
        assert_eq!(
            (voided.amount, voided.reason),
            (m(10_000), VoidReason::WrongItem)
        );
        assert_eq!(cart.lines().len(), 1);

        let customer = CartCustomer {
            id: Uuid::nil(),
            has_tax_id: true,
        };
        assert_eq!(
            cart.set_tax_invoice(true).unwrap_err().field(),
            Some("tax_invoice")
        );
        cart.set_customer(Some(customer));
        cart.set_tax_invoice(true).unwrap();
        assert!(cart.tax_invoice());
        // Switching to a customer without a tax id or detaching clears it.
        cart.set_customer(Some(CartCustomer {
            has_tax_id: false,
            ..customer
        }));
        assert!(!cart.tax_invoice());
        cart.set_customer(Some(customer));
        cart.set_tax_invoice(true).unwrap();
        cart.set_customer(None);
        assert!(!cart.tax_invoice());
        assert!(cart.customer().is_none());
        cart.set_tax_invoice(false).unwrap();

        cart.set_customer(Some(customer));
        let (lines, amount) = cart.void_cart(VoidReason::CustomerChangedMind).unwrap();
        assert_eq!((lines.len(), amount), (1, m(14_000)));
        assert!(cart.is_empty() && cart.customer().is_none());
        assert_eq!(
            cart.void_cart(VoidReason::PriceDispute)
                .unwrap_err()
                .field(),
            Some("line")
        );
        assert!(cart.void_cart(VoidReason::Other(String::new())).is_err());
    }

    #[test]
    fn void_reasons() {
        #[rustfmt::skip]
        let cases = [
            ("customer_changed_mind", None, Ok(VoidReason::CustomerChangedMind)),
            ("wrong_item", None, Ok(VoidReason::WrongItem)),
            ("price_dispute", Some("ignored"), Ok(VoidReason::PriceDispute)),
            ("other", Some("  dropped it "), Ok(VoidReason::Other("dropped it".into()))),
            ("other", None, Err("`other` needs a text")),
            ("other", Some(&"x".repeat(201)), Err("is too long")),
            ("nope", None, Err("unknown void reason")),
        ];
        for (key, text, expected) in cases {
            let got = VoidReason::parse(key, text);
            match expected {
                Ok(reason) => {
                    assert_eq!(got.as_ref(), Ok(&reason), "{key}");
                    assert_eq!(reason.as_str(), key);
                }
                Err(reason) => assert_eq!(
                    got,
                    Err(SaleError::InvalidField {
                        field: "reason",
                        reason
                    }),
                    "{key}"
                ),
            }
        }
    }

    #[test]
    fn actions_need_permissions() {
        #[rustfmt::skip]
        let cases = [
            (CartAction::Sell, "sale.create"),
            (CartAction::VoidLine, "sale.void_line"),
            (CartAction::VoidCart, "sale.void"),
            (CartAction::PriceOverride, "price.override"),
            (CartAction::LineDiscount, "sale.discount"),
            (CartAction::CartDiscount, "sale.cart_discount"),
            (CartAction::PostVoid, "sale.post_void"),
            (CartAction::ReturnWithReceipt, "refund.with_receipt"),
            (CartAction::ReturnNoReceipt, "refund.no_receipt"),
        ];
        for (action, key) in cases {
            assert_eq!(action.permission().as_str(), key);
        }
    }

    /// Spec §15 case 1: two lines of the same variant in different units reach a tier together;
    /// the cart hands both to the quote and applies the result.
    #[test]
    fn quote_round_trip() {
        let v = VariantId::generate();
        let pcs = UnitId::generate();
        let box_ = UnitId::generate();
        let mut cart = Cart::new();
        cart.add(CartLine::new(item(v, pcs, ProductType::Stock), q(6_000)))
            .unwrap();
        let in_box = CartItem {
            unit: box_,
            factor: q(6_000),
            ..item(v, pcs, ProductType::Stock)
        };
        cart.add(CartLine::new(in_box, Qty::ONE)).unwrap();
        cart.add(CartLine {
            scale_label: Some(ScaleLabel::Price { amount: m(15_000) }),
            ..CartLine::new(
                item(VariantId::generate(), pcs, ProductType::Weighed),
                q(1_000),
            )
        })
        .unwrap();
        let open = CartItem {
            open_price: true,
            ..item(VariantId::generate(), pcs, ProductType::Stock)
        };
        cart.add(CartLine::new(open, Qty::ONE)).unwrap();
        cart.set_open_price(3, m(70_000)).unwrap();

        let quote = cart.quote_lines();
        let indexes: Vec<usize> = quote.iter().map(|(i, _)| *i).collect();
        assert_eq!(indexes, [0, 1, 3], "the price label is not quoted");
        let counted =
            pricing::counted_quantities(&quote.iter().map(|(_, l)| *l).collect::<Vec<_>>());
        assert_eq!(counted.get(&v), Some(&Some(q(12_000))));
        assert_eq!(quote[2].1.mode, LineMode::OpenPrice(Some(m(70_000))));

        let resolved = |price| PricedLine {
            resolved: Some(ResolvedPrice {
                unit_price: m(price),
                source: source(),
            }),
            unit_price: m(price),
            amount: m(price),
        };
        cart.apply_quote(0, &resolved(9_000)).unwrap();
        cart.apply_quote(1, &resolved(54_000)).unwrap();
        cart.apply_quote(2, &resolved(1)).unwrap();
        cart.apply_quote(3, &resolved(1)).unwrap();
        assert!(cart.apply_quote(9, &resolved(1)).is_err());
        let prices: Vec<i64> = cart.lines().iter().map(|l| l.unit_price.minor()).collect();
        assert_eq!(prices, [9_000, 54_000, 0, 70_000]);
        assert_eq!(cart.lines()[0].price_source, Some(source()));
        // An overridden line keeps its price on a re-quote.
        cart.override_price(0, m(8_000)).unwrap();
        assert_eq!(quote_mode(&cart, 0), LineMode::Overridden(m(8_000)));
        cart.apply_quote(0, &resolved(9_000)).unwrap();
        assert_eq!(cart.lines()[0].unit_price, m(8_000));

        let t = cart.totals(TaxSettings::default()).unwrap();
        // 6 × 8 000 + 54 000 + 15 000 + 70 000.
        assert_eq!(t.total, m(187_000));
        // Overflowing gross.
        cart.lines[0].unit_price = m(i64::MAX);
        assert!(cart.totals(TaxSettings::default()).is_err());
        assert!(cart.permission_requests(TaxSettings::default()).is_err());
        assert!(cart.void_cart(VoidReason::WrongItem).is_err());
    }

    fn quote_mode(cart: &Cart, index: usize) -> LineMode {
        cart.quote_lines()
            .into_iter()
            .find(|(i, _)| *i == index)
            .unwrap()
            .1
            .mode
    }
}
