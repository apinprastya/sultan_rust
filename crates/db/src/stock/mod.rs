//! The inventory repositories (spec `docs/spec/2.8-inventory.md`, phase N2): the stock ledger
//! with its cached balances, costs and lots, suppliers and stock reasons (HQ data), the
//! inventory settings, and the branch documents: purchase orders, goods receipts with their
//! payables, supplier returns, transfers between branches and inside a branch, adjustments,
//! opening stock, opnames and reorder rules.
//!
//! Every rule that needs no database lives in `sultan_core::stock` (the average cost, receipt
//! totals and landed cost, FEFO, variance, suggestions, field rules); this module applies it to
//! the stored state and stores the result. On top of that it enforces what only the database
//! can:
//!
//! - **One transaction per write.** Every function that writes takes a [`Tx`] supplied by the
//!   caller, which also appends the audit row and the outbox event (spec 2.8 §15) in the same
//!   transaction (the N3 server, like the routes of 2.1–2.3).
//! - **The ledger** ([`post_moves`], [`Move`]): a posting appends its `stock_movement` rows and
//!   updates `stock_balance` (on hand per location), `stock_cost` (the moving average per branch,
//!   [`sultan_core::stock::CostState`]) and `stock_lot` in the same transaction, so the balance
//!   is always the ledger's sum (§4). An outgoing movement of a tracked product takes its lots
//!   first-expiry-first-out, one row per lot. Sales and returns go through it too (§2): a sale
//!   at the average, a post-void puts back exactly what the sale took, a restock return comes
//!   back at the value the sale line took (into the lots it came from), a write-off at the
//!   average.
//! - **Numbers** (I19): a document gets the next sequence of its branch, prefix and
//!   branch-local day when it is numbered ([`sultan_core::doc_number::stock_number`]); a unique
//!   key turns a lost race into a retryable [`DbError::Conflict`].
//! - **States**: a posted document is never edited. Posting or acting on a document twice
//!   returns it unchanged ([`Written::Unchanged`]), so a replayed request is harmless; an action
//!   the state does not allow is [`StockDbError::State`] (`409 conflict`).
//! - **Checks only the database can make**: a duplicate invoice number per supplier among
//!   posted receipts (a unique key, `409 conflict`), a receipt whose cost was used since
//!   ([`receipt_in_use`], `409 receipt_in_use`), one open opname per location and variant (a
//!   unique lock row, `409 opname_open`), a branch this node does not hold (`409 other_node`).
//!
//! Readings of the spec made while building N2 are listed in spec 2.8 §20.

mod adjust;
mod ledger;
mod numbers;
mod opname;
mod purchase;
mod reasons;
mod receipts;
mod reorder;
mod settings;
mod suppliers;
mod transfers;

use sultan_core::catalog::{ProductType, Qty};
use sultan_core::ids::{LocationId, UnitId, VariantId};
use sultan_core::stock::StockError;

use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

pub use crate::catalog::Written;
pub use adjust::{
    Adjustment, AdjustmentLine, NewAdjustment, NewAdjustmentLine, NewOpeningLine, NewOpeningStock,
    OpeningLine, OpeningStock, get_adjustment, get_opening_stock, list_adjustments,
    list_opening_stock, post_adjustment, post_opening_stock,
};
pub use ledger::{
    CardRow, DocRef, LotRecord, LotRule, Move, MovementRecord, NO_LOCATION, OnHandFilter,
    OnHandRow, Posted, Valuation, branch_on_hand, cost_state, document_movements, list_lots,
    list_on_hand, location_on_hand, post_moves, stock_card,
};
pub use opname::{
    CountInput, NewOpname, Opname, OpnameLineView, OpnameScope, add_counts, cancel_opname,
    get_opname, list_opnames, post_opname, start_opname,
};
pub use purchase::{
    NewPurchaseOrder, NewPurchaseOrderLine, PurchaseOrder, PurchaseOrderLine, PurchaseOrderStatus,
    cancel_purchase_order, close_purchase_order, get_purchase_order, list_purchase_orders,
    on_order, order_purchase_order, put_purchase_order,
};
pub use reasons::{
    EXPIRED_REASON, SEED_REASON_IDS, StockReasonRecord, archive_stock_reason, get_stock_reason,
    list_stock_reasons, put_stock_reason,
};
pub use receipts::{
    GoodsReceipt, GoodsReceiptLine, NewGoodsReceipt, NewGoodsReceiptLine, NewSupplierReturn,
    NewSupplierReturnLine, Payable, PaymentKind, ReceiptPosted, ReceiptStatus, SupplierReturn,
    SupplierReturnLine, get_goods_receipt, get_supplier_return, list_goods_receipts, list_payables,
    list_supplier_returns, post_goods_receipt, post_supplier_return, put_goods_receipt,
    put_supplier_return, receipt_in_use, void_goods_receipt,
};
pub use reorder::{
    ReorderRule, SuggestedLine, delete_reorder_rule, get_reorder_rule, list_reorder_rules,
    low_stock, put_reorder_rule, suggest_purchase_order,
};
pub use settings::{
    ProductExpiry, get_inventory_settings, get_product_expiry, put_inventory_settings,
    set_product_expiry,
};
pub use suppliers::{
    Supplier, SupplierItem, archive_supplier, get_supplier, get_supplier_item, list_supplier_items,
    list_suppliers, put_supplier, put_supplier_item,
};
pub use transfers::{
    InternalTransfer, InternalTransferLine, NewInternalTransfer, NewTransfer, NewTransferLine,
    Transfer, TransferLine, TransferStatus, cancel_transfer, create_transfer,
    get_internal_transfer, get_transfer, in_transit, list_internal_transfers, list_transfers,
    post_internal_transfer, receive_transfer, ship_transfer,
};

/// Errors of the inventory repositories.
#[derive(Debug, thiserror::Error)]
pub enum StockDbError {
    /// The document, supplier, reason, location, variant or unit does not exist.
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A field breaks a storage rule.
    #[error("invalid {field}: {message}")]
    Invalid {
        /// The field.
        field: &'static str,
        /// What is wrong, in English.
        message: String,
    },
    /// An inventory rule from `core` (`invalid_field`, `amount_overflow`).
    #[error(transparent)]
    Rule(#[from] StockError),
    /// The document is not in a state that allows this (`409 conflict`), e.g. editing a posted
    /// receipt or cancelling a shipped transfer.
    #[error("the {doc} is {status}")]
    State {
        /// The document kind.
        doc: &'static str,
        /// Its status.
        status: String,
    },
    /// Another posted receipt of the supplier has this invoice number (`409 conflict`).
    #[error("invoice {0} of this supplier is posted already")]
    DuplicateInvoice(String),
    /// The supplier or reason is archived (`409 archived`).
    #[error("{0} is archived")]
    Archived(&'static str),
    /// A later movement used the receipt's cost, or its payable is paid
    /// (`409 receipt_in_use`).
    #[error("the receipt's stock or payable was used since")]
    ReceiptInUse,
    /// Another open opname counts this variant at this location (`409 opname_open`).
    #[error("opname {0} is still open for this location and variant")]
    OpnameOpen(String),
    /// The other branch is not held by this node (`409 other_node`).
    #[error("the other branch is on another server")]
    OtherNode,
    /// A database error; a lost race on a number or a key is [`DbError::Conflict`].
    #[error(transparent)]
    Db(#[from] DbError),
}

impl StockDbError {
    /// The stable problem+json code (spec 2.8 §16), `None` for a database error.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::NotFound(_) => "not_found",
            Self::Invalid { .. } => "invalid_field",
            Self::Rule(e) => e.code(),
            Self::State { .. } | Self::DuplicateInvoice(_) => "conflict",
            Self::Archived(_) => "archived",
            Self::ReceiptInUse => "receipt_in_use",
            Self::OpnameOpen(_) => "opname_open",
            Self::OtherNode => "other_node",
            Self::Db(_) => return None,
        })
    }

    /// Whether running the whole transaction again may succeed (a lost number race, a deadlock,
    /// a busy `SQLite` database).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Db(e) if e.is_retryable())
    }

    pub(crate) fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            message: message.into(),
        }
    }

    pub(crate) fn state(doc: &'static str, status: &str) -> Self {
        Self::State {
            doc,
            status: status.to_owned(),
        }
    }
}

/// Result alias of the inventory repositories.
pub type StockResult<T> = std::result::Result<T, StockDbError>;

/// The most lines a document takes.
pub const MAX_LINES: usize = 1_000;

/// A variant on an inventory line, as the catalog stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct VariantInfo {
    pub product_type: ProductType,
    pub factor: Qty,
    pub track_expiry: bool,
}

/// Loads `variant` with `unit` (which must be a unit of its product) and checks it can be on an
/// inventory line (§1: no services or bundles).
pub(crate) async fn variant_info(
    tx: &mut Tx,
    variant: VariantId,
    unit: UnitId,
) -> StockResult<VariantInfo> {
    let found = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT p.type, p.track_expiry, u.factor FROM variant v \
         JOIN product p ON p.id = v.product_id \
         JOIN product_unit u ON u.product_id = p.id AND u.id = ? \
         WHERE v.id = ?",
        args![unit, variant],
        |row| {
            let t = row.text("type")?;
            Ok(VariantInfo {
                product_type: t
                    .parse::<ProductType>()
                    .map_err(|_| DbError::Corrupt(format!("unknown product type `{t}`")))?,
                factor: sql::qty(row, "factor")?,
                track_expiry: row.bool("track_expiry")?,
            })
        },
    )
    .await?;
    let Some(info) = found else {
        let known = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT 1 AS x FROM variant WHERE id = ?",
            args![variant],
            |_| Ok(()),
        )
        .await?;
        return Err(if known.is_some() {
            StockDbError::invalid("unit", "is not a unit of the variant's product")
        } else {
            StockDbError::NotFound("variant")
        });
    };
    sultan_core::stock::check_stock_type(info.product_type)?;
    Ok(info)
}

/// Checks that `location` is a live location of `branch`.
pub(crate) async fn check_location(
    tx: &mut Tx,
    branch: sultan_core::ids::BranchId,
    location: LocationId,
) -> StockResult<()> {
    let found = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT archived FROM stock_location WHERE id = ? AND branch_id = ?",
        args![location, branch],
        |row| row.bool("archived"),
    )
    .await?;
    match found {
        None => Err(StockDbError::NotFound("location")),
        Some(true) => Err(StockDbError::invalid("location", "is archived")),
        Some(false) => Ok(()),
    }
}

/// The receiving default of `branch`, else any live location; `None` without one.
pub(crate) async fn receiving_location(
    tx: &mut Tx,
    branch: sultan_core::ids::BranchId,
) -> Result<Option<LocationId>> {
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT id FROM stock_location WHERE branch_id = ? AND archived = 0 \
         ORDER BY receiving_default DESC, id LIMIT 1",
        args![branch],
        |row| sql::id(row, "id"),
    )
    .await
}

/// The location of a document: the given one (checked), else the receiving default.
pub(crate) async fn doc_location(
    tx: &mut Tx,
    branch: sultan_core::ids::BranchId,
    location: Option<LocationId>,
) -> StockResult<LocationId> {
    match location {
        Some(l) => {
            check_location(tx, branch, l).await?;
            Ok(l)
        }
        None => receiving_location(tx, branch)
            .await?
            .ok_or(StockDbError::NotFound("location")),
    }
}

/// Checks the number of lines of a document: 1–[`MAX_LINES`].
pub(crate) fn check_lines(n: usize) -> StockResult<()> {
    if n == 0 || n > MAX_LINES {
        return Err(StockDbError::invalid(
            "lines",
            "a document has 1–1000 lines",
        ));
    }
    Ok(())
}

/// A stored line number.
pub(crate) fn line_no(row: &dyn Row, col: &str) -> Result<u32> {
    let v = row.i64(col)?;
    u32::try_from(v).map_err(|_| DbError::Corrupt(format!("`{col}` {v} out of range")))
}

/// A stored day key.
pub(crate) fn day(row: &dyn Row, col: &str) -> Result<sultan_core::doc_number::LocalDay> {
    let key = row.i64(col)?;
    sultan_core::doc_number::LocalDay::from_key(key)
        .ok_or_else(|| DbError::Corrupt(format!("`{col}` {key} is not a day")))
}

/// A stored nullable day key.
pub(crate) fn opt_day(
    row: &dyn Row,
    col: &str,
) -> Result<Option<sultan_core::doc_number::LocalDay>> {
    row.opt_i64(col)?
        .map(|key| {
            sultan_core::doc_number::LocalDay::from_key(key)
                .ok_or_else(|| DbError::Corrupt(format!("`{col}` {key} is not a day")))
        })
        .transpose()
}

/// `limit` within 1..=`max`; `0` means `default`.
pub(crate) fn clamp_limit(limit: u32, default: u32, max: u32) -> i64 {
    i64::from(if limit == 0 { default } else { limit.min(max) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_retry() {
        let cases: Vec<(StockDbError, Option<&str>)> = vec![
            (StockDbError::NotFound("supplier"), Some("not_found")),
            (StockDbError::invalid("qty", "x"), Some("invalid_field")),
            (
                StockDbError::Rule(StockError::Overflow),
                Some("amount_overflow"),
            ),
            (StockDbError::state("receipt", "posted"), Some("conflict")),
            (
                StockDbError::DuplicateInvoice("INV-1".into()),
                Some("conflict"),
            ),
            (StockDbError::Archived("supplier"), Some("archived")),
            (StockDbError::ReceiptInUse, Some("receipt_in_use")),
            (
                StockDbError::OpnameOpen("JKT01-OP-261126-01".into()),
                Some("opname_open"),
            ),
            (StockDbError::OtherNode, Some("other_node")),
            (StockDbError::Db(DbError::NotFound("x")), None),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert!(!error.is_retryable(), "{error}");
            assert!(!error.to_string().is_empty());
        }
        assert!(StockDbError::Db(DbError::Conflict("x".into())).is_retryable());
    }

    #[test]
    fn limits_and_lines() {
        assert_eq!(clamp_limit(0, 50, 200), 50);
        assert_eq!(clamp_limit(999, 50, 200), 200);
        assert!(check_lines(0).is_err());
        assert!(check_lines(1).is_ok());
        assert!(check_lines(MAX_LINES + 1).is_err());
    }
}
