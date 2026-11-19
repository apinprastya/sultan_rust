//! Sales at the register (spec `docs/spec/2.6-sales.md`, phase S1): the cart, its totals,
//! payment, post-void, returns and exchanges, stock movements, the receipt text and the cashier
//! session effects.
//!
//! Pure: no IO, no clock. Prices come from the quote (2.3), customers and balances from 2.5, and
//! the current time is passed in. Everything that is stored (sales, returns, ledgers, numbers) is
//! the job of the database phase (S2); this module computes the values that get stored.
//!
//! - **Cart** ([`Cart`], [`CartLine`], [`CartItem`]): lines with variant, unit + factor, qty,
//!   unit price, price source, open price / override / scale label, line discount; customer,
//!   cart discount and the tax-invoice switch. A rescan of the same variant + unit adds to the
//!   existing line, except weighed items, scale labels, open-price and overridden lines (D8).
//!   At most [`MAX_LINES`] lines, quantities in `(0, 99 999.999]`, whole numbers for integer
//!   units. Every removed line is a void ([`VoidReason`], [`CartAction::permission`]).
//! - **Totals** ([`compute_totals`], [`split_cart_discount`]): spec §6. Gross rounded half up,
//!   line discount, cart discount split in proportion with the largest remainder (ties to the
//!   earlier line), price-embedded scale labels left out of the cart discount, tax per line.
//! - **Permissions** ([`Cart::permission_requests`], [`discount_bp`]): the values that
//!   `sale.discount`, `sale.cart_discount` and `price.override` are checked against.
//! - **Payment** ([`Payment`], [`TenderType`], [`TenderKind`]): spec §5. Only cash may exceed the
//!   rest, references, the customer balance as a tender, cash rounding when cash settles the rest,
//!   change and saved change.
//! - **Refunds** ([`plan_post_void`], [`plan_refund`]): spec §7, §8, D10, D11.
//! - **Returns** ([`line_refund`], [`check_return_window`], [`net_exchange`]): spec §8, D12.
//! - **Stock** ([`sale_movements`], [`return_movements`], [`void_movements`]): D9, base units,
//!   bundle components exploded.
//! - **Receipt** ([`build_receipt`]): spec §9, fixed-width text for 32, 42 or 48 columns.
//! - **Session** ([`session_report`]): spec §10, expected cash and the close-report counters.
//!
//! Document numbers are in [`crate::doc_number`] ([`crate::doc_number::sale_number`],
//! [`crate::doc_number::return_number`]).
//!
//! Known limitations and readings of the spec are listed in spec 2.6 §19 (clarifications from
//! S1); the main ones are repeated on the items they concern.

mod cart;
mod payment;
mod receipt;
mod refund;
mod returns;
mod session;
mod stock;
mod totals;

pub use cart::{
    AddOutcome, Cart, CartAction, CartCustomer, CartItem, CartLine, Discount, PermissionRequest,
    PriceMode, ScaleLabel, VoidReason, VoidedLine, check_qty, discount_bp,
};
pub use payment::{BalanceContext, PaidSale, Payment, SaleTender, TenderKind, TenderType};
pub use receipt::{
    Label, Receipt, ReceiptCustomerBlock, ReceiptHeader, ReceiptInput, ReceiptLine, ReceiptSaved,
    ReceiptTender, ReceiptTotals, ReceiptWidth, ReturnReceiptInput, ReturnReceiptLine,
    ReturnReceiptTender, TaxInvoiceBlock, TaxLine, build_receipt, build_return_receipt, format_qty,
    format_rupiah, mask_reference,
};
pub(crate) use receipt::{Page, format_pct};
pub use refund::{
    RefundInput, RefundMethod, RefundPlan, VoidInput, VoidPlan, plan_post_void, plan_refund,
};
pub use returns::{
    Exchange, ReturnDisposition, ReturnedSoFar, SoldLine, check_return_window, ensure_returnable,
    line_refund, net_exchange, no_receipt_refund, returnable_qty,
};
pub use session::{
    CountAmount, SalesSessionReport, SessionEvent, TenderKey, TenderReport, session_report,
};
pub use stock::{
    StockLine, StockMovement, StockMovementKind, return_movements, sale_movements, void_movements,
};
pub use totals::{CartTotals, LineTotals, TotalsLine, compute_totals, split_cart_discount};

use crate::catalog::Qty;
use crate::customer::CustomerError;
use crate::money::Money;
use crate::pricing::PricingError;

/// The most lines a cart may hold (spec 2.6 §2).
pub const MAX_LINES: usize = 300;

/// The largest line quantity, 99 999.999 (spec 2.6 §2).
pub const MAX_QTY: Qty = Qty::from_thousandths(99_999_999);

/// The status of a posted sale or return (spec 2.6 §1). There is no stored draft (D1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SaleStatus {
    /// Posted.
    Posted,
    /// Voided (post-void, §7); it keeps its number and its reversal entries.
    Voided,
}

impl SaleStatus {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Posted => "posted",
            Self::Voided => "voided",
        }
    }
}

/// Errors of the sales rules. Each variant maps to a stable problem+json code through
/// [`SaleError::code`]: the new codes of spec 2.6 §12, plus existing ones (`invalid_field`,
/// `invalid_qty`, `conflict`, `amount_overflow`, and the customer and pricing codes such as
/// `insufficient_balance` and `balance_limit`).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SaleError {
    /// A field breaks a simple rule.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// The field, e.g. `line`, `discount`, `customer`, `amount`, `reason`.
        field: &'static str,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// A quantity outside `(0, 99 999.999]` or with decimals for an integer unit (§2).
    #[error("invalid quantity: {reason}")]
    InvalidQty {
        /// Why it is invalid.
        reason: &'static str,
    },
    /// The cart already has [`MAX_LINES`] lines (§2).
    #[error("a cart holds at most {max} lines")]
    CartTooLarge {
        /// The limit.
        max: usize,
    },
    /// The totals posted differ from the server's fresh quote (§5).
    #[error("the cart's prices changed; quote again")]
    CartStale,
    /// The held cart was already taken by another register (§4).
    #[error("the held cart was taken already")]
    CartTaken,
    /// A sale with a return cannot be voided (§7).
    #[error("the sale has returns and cannot be voided")]
    HasReturns,
    /// The sale is voided already: no second void, no return.
    #[error("the sale is voided")]
    AlreadyVoided,
    /// Returning more than `sold − already returned` (§8).
    #[error("at most {max} can still be returned")]
    ReturnQtyExceeded {
        /// What can still be returned.
        max: Qty,
    },
    /// The sale is older than `return_days`: a manager override is needed (§8).
    #[error("the sale is older than {days} days")]
    ReturnWindow {
        /// The company's `return_days`.
        days: u32,
    },
    /// The tender type needs a reference and none was typed (§5).
    #[error("the tender needs a reference")]
    TenderReferenceRequired,
    /// A non-cash tender above the rest (§5). Only cash may exceed the rest.
    #[error("only cash may exceed the rest of {rest}")]
    TenderOverRest {
        /// The rest still to pay.
        rest: Money,
    },
    /// A customer-balance rule (`insufficient_balance`, `balance_limit`, …).
    #[error(transparent)]
    Customer(#[from] CustomerError),
    /// A pricing rule (`no_price`, override `invalid_field`, …).
    #[error(transparent)]
    Pricing(#[from] PricingError),
    /// An amount does not fit in 64 bits.
    #[error("amount overflow")]
    AmountOverflow,
}

impl SaleError {
    /// The stable problem+json code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidField { .. } => "invalid_field",
            Self::InvalidQty { .. } => "invalid_qty",
            Self::CartTooLarge { .. } => "cart_too_large",
            Self::CartStale => "cart_stale",
            Self::CartTaken => "cart_taken",
            Self::HasReturns => "has_returns",
            Self::AlreadyVoided => "conflict",
            Self::ReturnQtyExceeded { .. } => "return_qty_exceeded",
            Self::ReturnWindow { .. } => "return_window",
            Self::TenderReferenceRequired => "tender_reference_required",
            Self::TenderOverRest { .. } => "tender_over_rest",
            Self::Customer(e) => e.code(),
            Self::Pricing(e) => e.code(),
            Self::AmountOverflow => "amount_overflow",
        }
    }

    /// The field the error is about, when there is one (for problem+json's `field`).
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidField { field, .. } => Some(field),
            Self::InvalidQty { .. } | Self::ReturnQtyExceeded { .. } => Some("qty"),
            Self::TenderReferenceRequired => Some("reference"),
            Self::TenderOverRest { .. } => Some("amount"),
            Self::Customer(e) => e.field(),
            Self::CartTooLarge { .. }
            | Self::CartStale
            | Self::CartTaken
            | Self::HasReturns
            | Self::AlreadyVoided
            | Self::ReturnWindow { .. }
            | Self::Pricing(_)
            | Self::AmountOverflow => None,
        }
    }
}

/// Checks that the totals the register posted are the ones the server computed again (§5):
/// otherwise `409 cart_stale`, and the register shows the fresh quote.
///
/// # Errors
/// [`SaleError::CartStale`].
pub fn ensure_not_stale(posted: &CartTotals, fresh: &CartTotals) -> Result<(), SaleError> {
    if posted == fresh {
        Ok(())
    } else {
        Err(SaleError::CartStale)
    }
}

/// Takes a held cart (§4): `taken_by` is who took it already, if anyone.
///
/// # Errors
/// [`SaleError::CartTaken`] when it was taken already.
pub const fn take_held_cart<T: Copy>(taken_by: Option<T>) -> Result<(), SaleError> {
    match taken_by {
        Some(_) => Err(SaleError::CartTaken),
        None => Ok(()),
    }
}

/// Adds money with overflow reported as [`SaleError::AmountOverflow`].
pub(crate) fn add(a: Money, b: Money) -> Result<Money, SaleError> {
    a.checked_add(b).ok_or(SaleError::AmountOverflow)
}

/// Subtracts money with overflow reported as [`SaleError::AmountOverflow`].
pub(crate) fn sub(a: Money, b: Money) -> Result<Money, SaleError> {
    a.checked_sub(b).ok_or(SaleError::AmountOverflow)
}

/// Converts an `i128` back to money.
pub(crate) fn money(value: i128) -> Result<Money, SaleError> {
    i64::try_from(value)
        .map(Money::from_minor)
        .map_err(|_| SaleError::AmountOverflow)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_fields_and_messages() {
        let m = Money::from_minor(500);
        let cases: Vec<(SaleError, &str, Option<&str>, &str)> = vec![
            (
                SaleError::InvalidField {
                    field: "line",
                    reason: "no such line",
                },
                "invalid_field",
                Some("line"),
                "invalid line: no such line",
            ),
            (
                SaleError::InvalidQty { reason: "too big" },
                "invalid_qty",
                Some("qty"),
                "invalid quantity: too big",
            ),
            (
                SaleError::CartTooLarge { max: 300 },
                "cart_too_large",
                None,
                "a cart holds at most 300 lines",
            ),
            (
                SaleError::CartStale,
                "cart_stale",
                None,
                "the cart's prices changed; quote again",
            ),
            (
                SaleError::CartTaken,
                "cart_taken",
                None,
                "the held cart was taken already",
            ),
            (
                SaleError::HasReturns,
                "has_returns",
                None,
                "the sale has returns and cannot be voided",
            ),
            (
                SaleError::AlreadyVoided,
                "conflict",
                None,
                "the sale is voided",
            ),
            (
                SaleError::ReturnQtyExceeded {
                    max: Qty::from_thousandths(2_000),
                },
                "return_qty_exceeded",
                Some("qty"),
                "at most 2.000 can still be returned",
            ),
            (
                SaleError::ReturnWindow { days: 30 },
                "return_window",
                None,
                "the sale is older than 30 days",
            ),
            (
                SaleError::TenderReferenceRequired,
                "tender_reference_required",
                Some("reference"),
                "the tender needs a reference",
            ),
            (
                SaleError::TenderOverRest { rest: m },
                "tender_over_rest",
                Some("amount"),
                "only cash may exceed the rest of 500",
            ),
            (
                SaleError::Customer(CustomerError::InsufficientBalance { balance: m }),
                "insufficient_balance",
                Some("amount"),
                "the balance is only 500",
            ),
            (
                SaleError::Pricing(PricingError::Overflow),
                "amount_overflow",
                None,
                "amount does not fit in 64 bits",
            ),
            (
                SaleError::AmountOverflow,
                "amount_overflow",
                None,
                "amount overflow",
            ),
        ];
        for (err, code, field, msg) in cases {
            assert_eq!(err.code(), code, "{err:?}");
            assert_eq!(err.field(), field, "{err:?}");
            assert_eq!(err.to_string(), msg);
        }
        assert_eq!(SaleStatus::Posted.as_str(), "posted");
        assert_eq!(SaleStatus::Voided.as_str(), "voided");
    }

    /// Spec 2.6 §15 case 11 (the core part): the second take is refused.
    #[test]
    fn held_cart_take_and_stale_totals() {
        assert_eq!(take_held_cart::<u8>(None), Ok(()));
        assert_eq!(take_held_cart(Some(1_u8)), Err(SaleError::CartTaken));

        let line = |price| TotalsLine {
            gross: Money::from_minor(price),
            line_discount: None,
            cart_discountable: true,
            tax_rate_bp: 0,
        };
        let tax = crate::tax::TaxSettings::default();
        let a = compute_totals(&[line(10_000)], None, tax).unwrap();
        let b = compute_totals(&[line(11_000)], None, tax).unwrap();
        assert_eq!(ensure_not_stale(&a, &a.clone()), Ok(()));
        assert_eq!(ensure_not_stale(&a, &b), Err(SaleError::CartStale));
    }

    #[test]
    fn money_helpers_report_overflow() {
        let max = Money::from_minor(i64::MAX);
        assert_eq!(
            add(max, Money::from_minor(1)),
            Err(SaleError::AmountOverflow)
        );
        assert_eq!(
            sub(Money::from_minor(i64::MIN), Money::from_minor(1)),
            Err(SaleError::AmountOverflow)
        );
        assert_eq!(money(i128::MAX), Err(SaleError::AmountOverflow));
        assert_eq!(money(5), Ok(Money::from_minor(5)));
    }
}
