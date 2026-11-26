//! Inventory (spec `docs/spec/2.8-inventory.md`, phase N1): the stock ledger's movement kinds,
//! the moving-average cost per branch and variant, goods receipt totals and landed cost, expiry
//! lots taken first-expiry-first-out, opname variance, reorder suggestions, transfers and the
//! stock card.
//!
//! Pure: no IO, no clock. The database (N2) stores the ledger, the cached balances, costs and
//! lots and the documents; this module decides what is valid and computes the values.
//!
//! - **Movements** ([`MovementKind`]): §2. Every kind has a fixed sign ([`Sign`]); `adjust` and
//!   `count` go either way. [`MovementKind::ReceiptVoid`] reverses a voided receipt (§20
//!   reading 1). Document kinds and their number prefixes are [`DocKind`] (I19); the numbers are
//!   [`crate::doc_number::stock_number`].
//! - **Cost** ([`CostState`], [`RefCost`]): §5. Value and quantity per branch and variant, the
//!   reference cost as a fraction, so nothing drifts: incoming at a cost
//!   ([`CostState::receive`]), outgoing ([`CostState::issue`]), incoming at the average
//!   ([`CostState::restore`]) and adjustment increases ([`CostState::increase`]). "No cost yet"
//!   is `None`, never 0.
//! - **Receipts** ([`receipt_totals`], [`TaxMode`], [`spread`]): §6. Line nets, the header
//!   discount and other costs spread by the largest-remainder method, PPN included / added /
//!   none, and the landed cost with or without PPN (PKP, I5); [`supplier_return_credit`].
//! - **Lots** ([`fefo`], [`expiry_status`], [`check_lot`]): §9.
//! - **Opname** ([`counted`], [`variance`], [`opname_moves`], [`uncounted`]): §11.
//! - **Reorder** ([`suggest`], [`is_low`], [`check_rule`]): §10.
//! - **Transfers** ([`check_received`], [`split_value`], [`split_lots`]): §8.
//! - **Stock card** ([`running_balance`]): §14.
//! - **Rules** ([`check_stock_type`], [`check_adjust_line`], [`check_supplier`],
//!   [`InventorySettings`], [`SEED_REASONS`]): §1, §3, §7, §13.
//!
//! Readings of the spec made while building N1 are listed in spec 2.8 §20.

mod card;
mod cost;
mod count;
mod lots;
mod receipt;
mod reorder;
mod rules;
mod transfer;

pub use card::running_balance;
pub use cost::{CostState, RefCost};
pub use count::{CountEntry, OpnameLine, Uncounted, counted, opname_moves, uncounted, variance};
pub use lots::{ExpiryStatus, LotStock, MAX_BATCH_CHARS, Take, check_lot, expiry_status, fefo};
pub use receipt::{
    ReceiptInput, ReceiptLineInput, ReceiptLineTotals, ReceiptTotals, TaxMode, receipt_totals,
    spread, supplier_return_credit,
};
pub use reorder::{ReorderInput, Suggestion, check_rule, is_low, net_sold, suggest};
pub use rules::{
    DEFAULT_COVER_DAYS, DEFAULT_EXPIRY_WARN_DAYS, DEFAULT_PPN_RATE_BP, DEFAULT_VELOCITY_DAYS,
    InventorySettings, MAX_NOTE_CHARS, MAX_REASON_NAME_CHARS, MAX_SUPPLIER_CODE_CHARS,
    MAX_SUPPLIER_NAME_CHARS, ReasonDirection, SEED_REASONS, SeedReason, StockReason, SupplierInput,
    check_adjust_line, check_note, check_reason_name, check_stock_type, check_supplier,
};
pub use transfer::{check_received, split_lots, split_value};

use crate::catalog::Qty;
use crate::money::Money;

/// Errors of the inventory rules. Each maps to a stable problem+json code through
/// [`StockError::code`] (spec 2.8 §16).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StockError {
    /// A field breaks a simple rule.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// The field: `qty`, `price`, `discount`, `unit_cost`, `variant`, `reason`, ….
        field: &'static str,
        /// Why it is invalid.
        reason: &'static str,
    },
    /// A quantity or an amount does not fit in 64 bits.
    #[error("amount overflow")]
    Overflow,
}

impl StockError {
    /// The stable problem+json code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::InvalidField { .. } => "invalid_field",
            Self::Overflow => "amount_overflow",
        }
    }

    /// The field the error is about, when there is one.
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        match self {
            Self::InvalidField { field, .. } => Some(field),
            Self::Overflow => None,
        }
    }
}

/// Shorthand for [`StockError::InvalidField`].
pub(crate) const fn invalid(field: &'static str, reason: &'static str) -> StockError {
    StockError::InvalidField { field, reason }
}

/// The sign a movement kind must have (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Sign {
    /// Stock out: the quantity is below 0.
    Out,
    /// Stock in: the quantity is above 0.
    In,
    /// Either way, never 0 (`adjust`, `count`).
    Either,
}

/// The kind of a stock ledger row (§2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MovementKind {
    /// `sale` (−), 2.6.
    Sale,
    /// `sale_void` (+), a post-void, 2.6.
    SaleVoid,
    /// `return` (+), 2.6.
    Return,
    /// `write_off` (−), a returned item not restocked, 2.6.
    WriteOff,
    /// `receipt` (+), a goods receipt line at its landed cost.
    Receipt,
    /// `receipt_void` (−), a voided goods receipt taken back at the value it came in with.
    ReceiptVoid,
    /// `supplier_return` (−), at the average.
    SupplierReturn,
    /// `transfer_out` (−), shipped to another branch at the sender's average.
    TransferOut,
    /// `transfer_in` (+), received from another branch at the shipped value.
    TransferIn,
    /// `transit_loss` (−), the shortage of a transfer, at the sender.
    TransitLoss,
    /// `internal_out` (−), to another location of the branch.
    InternalOut,
    /// `internal_in` (+), from another location of the branch.
    InternalIn,
    /// `adjust` (±), an adjustment line.
    Adjust,
    /// `count` (±), an opname variance.
    Count,
    /// `opening` (+), opening stock at the typed cost.
    Opening,
}

impl MovementKind {
    /// Every kind.
    pub const ALL: [Self; 15] = [
        Self::Sale,
        Self::SaleVoid,
        Self::Return,
        Self::WriteOff,
        Self::Receipt,
        Self::ReceiptVoid,
        Self::SupplierReturn,
        Self::TransferOut,
        Self::TransferIn,
        Self::TransitLoss,
        Self::InternalOut,
        Self::InternalIn,
        Self::Adjust,
        Self::Count,
        Self::Opening,
    ];

    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sale => "sale",
            Self::SaleVoid => "sale_void",
            Self::Return => "return",
            Self::WriteOff => "write_off",
            Self::Receipt => "receipt",
            Self::ReceiptVoid => "receipt_void",
            Self::SupplierReturn => "supplier_return",
            Self::TransferOut => "transfer_out",
            Self::TransferIn => "transfer_in",
            Self::TransitLoss => "transit_loss",
            Self::InternalOut => "internal_out",
            Self::InternalIn => "internal_in",
            Self::Adjust => "adjust",
            Self::Count => "count",
            Self::Opening => "opening",
        }
    }

    /// The kind of a stored name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == key)
    }

    /// The sign its quantity must have.
    #[must_use]
    pub const fn sign(self) -> Sign {
        match self {
            Self::Sale
            | Self::WriteOff
            | Self::ReceiptVoid
            | Self::SupplierReturn
            | Self::TransferOut
            | Self::TransitLoss
            | Self::InternalOut => Sign::Out,
            Self::SaleVoid
            | Self::Return
            | Self::Receipt
            | Self::TransferIn
            | Self::InternalIn
            | Self::Opening => Sign::In,
            Self::Adjust | Self::Count => Sign::Either,
        }
    }

    /// Checks that `qty` has this kind's sign (§2).
    ///
    /// # Errors
    /// [`StockError::InvalidField`] (`qty`) for a wrong sign or 0.
    pub fn check_qty(self, qty: Qty) -> Result<(), StockError> {
        let t = qty.thousandths();
        let ok = match self.sign() {
            Sign::Out => t < 0,
            Sign::In => t > 0,
            Sign::Either => t != 0,
        };
        if ok {
            Ok(())
        } else {
            Err(invalid("qty", "has the wrong sign for the movement kind"))
        }
    }
}

impl From<crate::sale::StockMovementKind> for MovementKind {
    fn from(kind: crate::sale::StockMovementKind) -> Self {
        use crate::sale::StockMovementKind as S;
        match kind {
            S::Sale => Self::Sale,
            S::SaleVoid => Self::SaleVoid,
            S::Return => Self::Return,
            S::WriteOff => Self::WriteOff,
        }
    }
}

/// The inventory documents (§1) with their number prefixes (I19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocKind {
    /// Purchase order, `-PO-`.
    PurchaseOrder,
    /// Goods receipt, `-GR-`.
    GoodsReceipt,
    /// Supplier return, `-RS-`.
    SupplierReturn,
    /// Transfer between branches, `-TR-`.
    Transfer,
    /// Internal transfer, `-IT-`.
    InternalTransfer,
    /// Adjustment, `-ADJ-`.
    Adjustment,
    /// Opname, `-OP-`.
    Opname,
    /// Opening stock, `-OS-`.
    OpeningStock,
}

impl DocKind {
    /// Every document kind.
    pub const ALL: [Self; 8] = [
        Self::PurchaseOrder,
        Self::GoodsReceipt,
        Self::SupplierReturn,
        Self::Transfer,
        Self::InternalTransfer,
        Self::Adjustment,
        Self::Opname,
        Self::OpeningStock,
    ];

    /// The prefix in its number, e.g. `PO`.
    #[must_use]
    pub const fn prefix(self) -> &'static str {
        match self {
            Self::PurchaseOrder => "PO",
            Self::GoodsReceipt => "GR",
            Self::SupplierReturn => "RS",
            Self::Transfer => "TR",
            Self::InternalTransfer => "IT",
            Self::Adjustment => "ADJ",
            Self::Opname => "OP",
            Self::OpeningStock => "OS",
        }
    }

    /// The name stored as a movement's `doc_kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PurchaseOrder => "purchase_order",
            Self::GoodsReceipt => "goods_receipt",
            Self::SupplierReturn => "supplier_return",
            Self::Transfer => "transfer",
            Self::InternalTransfer => "internal_transfer",
            Self::Adjustment => "adjustment",
            Self::Opname => "opname",
            Self::OpeningStock => "opening_stock",
        }
    }

    /// The kind of a stored name.
    #[must_use]
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == key)
    }
}

/// The value of `qty` at `price` per unit, rounded half up to Rp 1.
///
/// # Errors
/// [`StockError::Overflow`] when it does not fit.
pub fn line_value(qty: Qty, price: Money) -> Result<Money, StockError> {
    let product = i128::from(qty.thousandths()) * i128::from(price.minor());
    to_money(div_round(product, 1_000))
}

/// The price per base unit of `price` per unit of `factor` base units, rounded half up to Rp 1
/// (a supplier item's last price, §3).
///
/// # Errors
/// [`StockError::InvalidField`] (`unit`) for a factor that is not above 0.
pub fn per_base_price(price: Money, factor: Qty) -> Result<Money, StockError> {
    if !factor.is_positive() {
        return Err(invalid("unit", "has no factor above 0"));
    }
    to_money(div_round(
        i128::from(price.minor()) * 1_000,
        i128::from(factor.thousandths()),
    ))
}

/// `qty` in base units: `qty × factor` (§1).
///
/// # Errors
/// [`StockError::InvalidField`] (`unit`) for a factor that is not above 0;
/// [`StockError::Overflow`] when it does not fit.
pub fn to_base(qty: Qty, factor: Qty) -> Result<Qty, StockError> {
    if !factor.is_positive() {
        return Err(invalid("unit", "has no factor above 0"));
    }
    qty.times_factor(factor).ok_or(StockError::Overflow)
}

/// `n / d` rounded half away from zero; `d` must be positive.
pub(crate) const fn div_round(n: i128, d: i128) -> i128 {
    let q = n / d;
    let r = n % d;
    if r.abs() * 2 >= d {
        if n < 0 { q - 1 } else { q + 1 }
    } else {
        q
    }
}

/// `⌈n / d⌉` for `n ≥ 0`, `d > 0`.
pub(crate) const fn div_ceil(n: i128, d: i128) -> i128 {
    (n + d - 1) / d
}

pub(crate) fn to_money(v: i128) -> Result<Money, StockError> {
    i64::try_from(v)
        .map(Money::from_minor)
        .map_err(|_| StockError::Overflow)
}

pub(crate) fn to_qty(v: i128) -> Result<Qty, StockError> {
    i64::try_from(v)
        .map(Qty::from_thousandths)
        .map_err(|_| StockError::Overflow)
}

pub(crate) fn add_money(a: Money, b: Money) -> Result<Money, StockError> {
    a.checked_add(b).ok_or(StockError::Overflow)
}

pub(crate) fn sub_money(a: Money, b: Money) -> Result<Money, StockError> {
    a.checked_sub(b).ok_or(StockError::Overflow)
}

pub(crate) fn add_qty(a: Qty, b: Qty) -> Result<Qty, StockError> {
    a.checked_add(b).ok_or(StockError::Overflow)
}

pub(crate) fn sub_qty(a: Qty, b: Qty) -> Result<Qty, StockError> {
    a.checked_sub(b).ok_or(StockError::Overflow)
}

#[cfg(test)]
pub(crate) mod test_util {
    use super::*;

    pub fn q(whole: i64) -> Qty {
        Qty::from_thousandths(whole * 1_000)
    }

    pub fn qt(thousandths: i64) -> Qty {
        Qty::from_thousandths(thousandths)
    }

    pub fn rp(n: i64) -> Money {
        Money::from_minor(n)
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::{q, qt, rp};
    use super::*;

    #[test]
    fn codes_fields_and_messages() {
        let e = invalid("qty", "must be above 0");
        assert_eq!(
            (e.code(), e.field(), e.to_string().as_str()),
            ("invalid_field", Some("qty"), "invalid qty: must be above 0")
        );
        let o = StockError::Overflow;
        assert_eq!(
            (o.code(), o.field(), o.to_string().as_str()),
            ("amount_overflow", None, "amount overflow")
        );
    }

    #[test]
    fn kinds_round_trip_with_their_signs() {
        use crate::sale::StockMovementKind as S;
        #[rustfmt::skip]
        let cases = [
            (MovementKind::Sale, "sale", Sign::Out),
            (MovementKind::SaleVoid, "sale_void", Sign::In),
            (MovementKind::Return, "return", Sign::In),
            (MovementKind::WriteOff, "write_off", Sign::Out),
            (MovementKind::Receipt, "receipt", Sign::In),
            (MovementKind::ReceiptVoid, "receipt_void", Sign::Out),
            (MovementKind::SupplierReturn, "supplier_return", Sign::Out),
            (MovementKind::TransferOut, "transfer_out", Sign::Out),
            (MovementKind::TransferIn, "transfer_in", Sign::In),
            (MovementKind::TransitLoss, "transit_loss", Sign::Out),
            (MovementKind::InternalOut, "internal_out", Sign::Out),
            (MovementKind::InternalIn, "internal_in", Sign::In),
            (MovementKind::Adjust, "adjust", Sign::Either),
            (MovementKind::Count, "count", Sign::Either),
            (MovementKind::Opening, "opening", Sign::In),
        ];
        assert_eq!(cases.len(), MovementKind::ALL.len());
        for (kind, key, sign) in cases {
            assert_eq!(kind.as_str(), key);
            assert_eq!(MovementKind::from_key(key), Some(kind));
            assert_eq!(kind.sign(), sign, "{key}");
            let (good, bad): (&[i64], &[i64]) = match sign {
                Sign::Out => (&[-1], &[0, 1]),
                Sign::In => (&[1], &[0, -1]),
                Sign::Either => (&[-1, 1], &[0]),
            };
            for t in good {
                assert_eq!(kind.check_qty(qt(*t)), Ok(()), "{key} {t}");
            }
            for t in bad {
                assert_eq!(
                    kind.check_qty(qt(*t)).unwrap_err().field(),
                    Some("qty"),
                    "{key} {t}"
                );
            }
        }
        assert_eq!(MovementKind::from_key("transfer"), None);
        for (s, k) in [
            (S::Sale, MovementKind::Sale),
            (S::SaleVoid, MovementKind::SaleVoid),
            (S::Return, MovementKind::Return),
            (S::WriteOff, MovementKind::WriteOff),
        ] {
            assert_eq!(MovementKind::from(s), k);
            assert_eq!(s.as_str(), k.as_str());
        }
    }

    #[test]
    fn doc_kinds() {
        #[rustfmt::skip]
        let cases = [
            (DocKind::PurchaseOrder, "PO", "purchase_order"),
            (DocKind::GoodsReceipt, "GR", "goods_receipt"),
            (DocKind::SupplierReturn, "RS", "supplier_return"),
            (DocKind::Transfer, "TR", "transfer"),
            (DocKind::InternalTransfer, "IT", "internal_transfer"),
            (DocKind::Adjustment, "ADJ", "adjustment"),
            (DocKind::Opname, "OP", "opname"),
            (DocKind::OpeningStock, "OS", "opening_stock"),
        ];
        assert_eq!(cases.len(), DocKind::ALL.len());
        for (kind, prefix, key) in cases {
            assert_eq!(kind.prefix(), prefix);
            assert_eq!(kind.as_str(), key);
            assert_eq!(DocKind::from_key(key), Some(kind));
        }
        assert_eq!(DocKind::from_key("sale"), None);
    }

    #[test]
    fn values_and_base_units() {
        #[rustfmt::skip]
        let cases = [
            (q(2), rp(48_000), Ok(rp(96_000))),
            (qt(1_250), rp(1_000), Ok(rp(1_250))),
            (qt(333), rp(1_500), Ok(rp(500))),   // 499.5 → 500
            (qt(-333), rp(1_500), Ok(rp(-500))),
            (qt(i64::MAX), rp(i64::MAX), Err(StockError::Overflow)),
        ];
        for (qty, price, expected) in cases {
            assert_eq!(line_value(qty, price), expected, "{qty} × {price}");
        }
        assert_eq!(per_base_price(rp(48_000), q(24)), Ok(rp(2_000)));
        assert_eq!(per_base_price(rp(1_000), q(3)), Ok(rp(333)));
        assert_eq!(per_base_price(rp(1_000), qt(500)), Ok(rp(2_000)));
        assert_eq!(
            per_base_price(rp(1), Qty::ZERO).unwrap_err().field(),
            Some("unit")
        );
        // Spec §17 case 6: 2 box(24) → 48 base units.
        assert_eq!(to_base(q(2), q(24)), Ok(q(48)));
        assert_eq!(to_base(qt(500), qt(2_000)), Ok(q(1)));
        assert_eq!(to_base(q(1), Qty::ZERO).unwrap_err().field(), Some("unit"));
        assert_eq!(to_base(qt(i64::MAX), q(2)), Err(StockError::Overflow));
    }

    #[test]
    fn helpers() {
        assert_eq!(div_round(5, 2), 3);
        assert_eq!(div_round(-5, 2), -3);
        assert_eq!(div_round(4, 3), 1);
        assert_eq!(div_ceil(0, 3), 0);
        assert_eq!(div_ceil(7, 3), 3);
        assert_eq!(div_ceil(9, 3), 3);
        assert_eq!(to_money(i128::MAX), Err(StockError::Overflow));
        assert_eq!(to_qty(i128::MIN), Err(StockError::Overflow));
        let max = Money::from_minor(i64::MAX);
        assert_eq!(add_money(max, rp(1)), Err(StockError::Overflow));
        assert_eq!(
            sub_money(Money::from_minor(i64::MIN), rp(1)),
            Err(StockError::Overflow)
        );
        assert_eq!(add_qty(qt(i64::MAX), qt(1)), Err(StockError::Overflow));
        assert_eq!(sub_qty(qt(i64::MIN), qt(1)), Err(StockError::Overflow));
        assert_eq!(sub_qty(q(3), q(1)), Ok(q(2)));
    }
}
