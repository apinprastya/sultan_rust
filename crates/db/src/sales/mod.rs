//! The sales repositories (spec `docs/spec/2.6-sales.md`, phase S2): tender types, posted sales
//! and their post-voids, returns and exchanges, held carts, the stock movement ledger (D9), the
//! customer purchase history and the session figures of a cashier session.
//!
//! Every rule lives in `sultan_core::sale` (totals, payment, refunds, returnable quantities,
//! stock movements); the caller (the S3 server) computes the values with `core` and this module
//! stores them. On top of that it enforces what only the database can:
//!
//! - **One transaction per document.** [`post_sale`], [`void_sale`] and [`post_return`] take a
//!   [`Tx`] and write the document, its stock movements, its customer balance entries (with the
//!   cached balance, [`crate::customers::append_balance_entry`]), one audit row and one outbox row
//!   together (spec 2.6 §11: branch transactional data, pushed to HQ). Tender types are HQ master
//!   data ([`put_tender_type`], [`archive_tender_type`]): audit + outbox as well, nothing on an
//!   unchanged replay. Held carts are branch-local: no outbox, an audit row only when one is
//!   deleted without being resumed.
//! - **Numbers** (spec 2.6 §1): a sale or return gets the next sequence of its register on the
//!   branch-local posting day (`core::doc_number`) inside the posting transaction. A unique key on
//!   `(register_id, local_day, seq)` turns a lost race into a retryable [`DbError::Conflict`]; the
//!   server reruns the transaction (2.1 §5, at most 5 attempts with jittered backoff), exactly like
//!   cashier session numbers ([`crate::cashier_sessions::insert`]).
//! - **Idempotent posting** (spec 2.6 §12): sales and returns store a hash of the posted body
//!   (`request_hash`, computed by the server). Posting the same id with the same hash again
//!   returns the stored document ([`Written::Unchanged`]) and writes nothing; another hash is
//!   [`SaleDbError::Conflict`] (`409 conflict`).
//! - **Races between documents.** A return bumps `sale.return_count` and the returned quantity of
//!   each sale line with guarded `UPDATE`s (`… WHERE status = 'posted'`, `… WHERE returned_qty + ?
//!   <= qty`), and a post-void needs `return_count = 0`. A guard that fails although the
//!   transaction's own reads said it would pass is a lost race: [`DbError::Conflict`], retryable.
//!   On the rerun the reads are fresh and report the real refusal (`has_returns`,
//!   `return_qty_exceeded`, `conflict`).
//! - **Stock** (D9, [`stock_on_hand`]): the movements are posted through the inventory ledger
//!   ([`crate::stock::post_moves`], spec 2.8 §2) at the register's default location, else the
//!   branch's receiving default location, else without a location (spec 2.6 §19, from S2), with
//!   their values: a sale at the average cost (lots FEFO), a post-void puts back exactly what the
//!   sale took, a restocked return comes back at the value the sale line took, a write-off at the
//!   average. Selling marks the variants used ([`crate::catalog::mark_used`]), so they are never
//!   hard-deleted.
//!
//! Known limitations: line voids and cart voids (§3) are audit entries the server writes; they are
//! not part of [`session_events`]. Returns cannot be voided yet (no flow in the spec), so every
//! stored return is `posted`.

mod held;
mod history;
mod returns;
mod sale;
mod settings;
mod stock;
mod tender_types;

use serde_json::{Value, json};
use sultan_core::catalog::{ComponentSnapshot, ProductType, Qty};
use sultan_core::customer::BalanceEntryKind as CoreKind;
use sultan_core::doc_number::{BranchTimeZone, LocalDay, return_number, sale_number};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, TenderTypeId, UserId};
use sultan_core::money::Money;
use sultan_core::sale::{SaleError, TenderKind};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::audit::{self, AuditEntry};
use crate::conn::{Conn, Tx};
use crate::customers::{self, BalanceEntryKind, CustomerDbError};
use crate::error::{DbError, Result};
use crate::outbox;
use crate::sql::{self, Row, args};

pub use crate::catalog::{Actor, Written};
pub use crate::customers::CustomerId;
pub use held::{
    HeldCart, delete_held_cart, get_held_cart, list_held_carts, put_held_cart, take_held_cart,
};
pub use history::{
    HistoryCursor, HistoryDoc, HistoryEntry, MAX_HISTORY_PAGE, purchase_history, recent_receipts,
    session_events,
};
pub use returns::{
    NewReturn, Return, ReturnHeader, ReturnLine, StoredReturnLine, get_return, list_returns,
    post_return, replayed_return,
};
pub use sale::{
    DocFilter, MAX_DOC_PAGE, NewSale, NewSaleVoid, Sale, SaleHeader, SaleLine, SaleTotals,
    SaleVoid, StoredSaleLine, get_sale, get_sale_by_number, list_sales, post_sale, replayed_sale,
    void_sale,
};
pub use settings::{
    DEFAULT_RECEIPT_WIDTH, DEFAULT_RETURN_DAYS, MAX_RETURN_DAYS, RECEIPT_WIDTHS, SalesSettings,
    get_sales_settings, put_sales_settings, receipt_width, return_days,
};
pub use stock::{StockMovementRecord, document_movements, stock_on_hand};
pub use tender_types::{
    CASH_TENDER_TYPE, CUSTOMER_BALANCE_TENDER_TYPE, TenderTypeRecord, archive_tender_type,
    get_tender_type, list_tender_types, put_tender_type,
};

/// Errors of the sales repositories.
#[derive(Debug, thiserror::Error)]
pub enum SaleDbError {
    /// The sale, return, held cart, tender type or customer does not exist.
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A field breaks a storage rule (sums that do not add up, a missing customer, a line that
    /// does not belong to the sale, …).
    #[error("invalid {field}: {message}")]
    Invalid {
        /// The field.
        field: &'static str,
        /// What is wrong, in English.
        message: String,
    },
    /// The tender type is archived: it cannot be edited (`409 archived`).
    #[error("tender type is archived")]
    Archived,
    /// A sales rule from `core`: `has_returns`, `conflict` (already voided), `cart_taken`,
    /// `return_qty_exceeded`, `invalid_field` (an imported sale), ….
    #[error(transparent)]
    Rule(#[from] SaleError),
    /// A document with this ID was posted with another body (`409 conflict`, spec 2.6 §12).
    #[error("{0} {1} already exists with another body")]
    Conflict(&'static str, Uuid),
    /// A database error. A lost race (number, guard, primary key) is [`DbError::Conflict`]: see
    /// [`SaleDbError::is_retryable`].
    #[error(transparent)]
    Db(#[from] DbError),
}

impl SaleDbError {
    /// The stable problem+json code (spec 2.6 §12), `None` for a database error.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::NotFound(_) => "not_found",
            Self::Invalid { .. } => "invalid_field",
            Self::Archived => "archived",
            Self::Rule(e) => e.code(),
            Self::Conflict(..) => "conflict",
            Self::Db(_) => return None,
        })
    }

    /// Whether running the whole transaction again may succeed: a lost race on a number, a
    /// document ID or a guarded update, a deadlock, a busy `SQLite` database
    /// ([`DbError::is_retryable`]).
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Db(e) if e.is_retryable())
    }

    fn invalid(field: &'static str, message: impl Into<String>) -> Self {
        Self::Invalid {
            field,
            message: message.into(),
        }
    }
}

impl From<CustomerDbError> for SaleDbError {
    fn from(e: CustomerDbError) -> Self {
        match e {
            CustomerDbError::NotFound(what) => Self::NotFound(what),
            CustomerDbError::Db(e) => Self::Db(e),
            other => Self::invalid("balance", other.to_string()),
        }
    }
}

/// Result alias of the sales repositories.
pub type SaleResult<T> = std::result::Result<T, SaleDbError>;

/// A refund paid back by a post-void or a return (D11, D12): cash, the customer balance, or (for
/// a return) the exchange tender of the new sale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefundTender {
    /// The tender type; `None` for the exchange tender.
    pub tender_type: Option<TenderTypeId>,
    /// `cash`, `customer_balance` or (returns only) `exchange`.
    pub kind: TenderKind,
    /// The amount, above 0. For cash, what left the drawer (after cash rounding).
    pub amount: Money,
}

/// Where a document was made: the user, register, branch and cashier session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Place {
    pub branch: BranchId,
    pub register: RegisterId,
    pub session: CashierSessionId,
    pub user: UserId,
}

impl Place {
    fn actor(self) -> Actor {
        Actor {
            user: Some(self.user),
            branch: Some(self.branch),
            register: Some(self.register),
        }
    }
}

/// One change as recorded: an audit row and, unless `outbox` is `None`, an outbox row.
struct Change<'a> {
    action: &'a str,
    target: String,
    before: Option<Value>,
    after: Option<Value>,
    outbox: Option<(&'a str, &'a Value)>,
}

/// Appends the audit row and the outbox row of `change` in `tx`.
async fn record(tx: &mut Tx, actor: &Actor, now: Timestamp, change: Change<'_>) -> Result<()> {
    let mut entry = AuditEntry::new(now, change.action);
    entry.user = actor.user;
    entry.branch = actor.branch;
    entry.register = actor.register;
    entry.target = Some(change.target);
    entry.before = change.before;
    entry.after = change.after;
    audit::append(&mut *tx, &entry).await?;
    if let Some((kind, payload)) = change.outbox {
        outbox::append(&mut *tx, kind, payload, now).await?;
    }
    Ok(())
}

/// The kind of a document number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NumberKind {
    Sale,
    Return,
}

/// A document number with the local day and sequence it is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Allocated {
    number: String,
    day: i64,
    seq: i64,
}

/// The next number of `register` on the branch-local day of `at` (spec 2.6 §1). Run in the
/// posting transaction; the unique key `(register_id, local_day, seq)` catches a race.
async fn allocate(
    tx: &mut Tx,
    kind: NumberKind,
    branch: BranchId,
    register: RegisterId,
    at: Timestamp,
) -> SaleResult<Allocated> {
    let (register_branch, branch_code, register_code, timezone) = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT r.branch_id, b.code AS branch_code, r.code AS register_code, b.timezone \
         FROM register r JOIN branch b ON b.id = r.branch_id WHERE r.id = ?",
        args![register],
        |row| {
            Ok((
                sql::id::<BranchId>(row, "branch_id")?,
                row.text("branch_code")?,
                row.text("register_code")?,
                row.text("timezone")?,
            ))
        },
    )
    .await?
    .ok_or(SaleDbError::NotFound("register"))?;
    if register_branch != branch {
        return Err(SaleDbError::invalid(
            "register",
            "the register belongs to another branch",
        ));
    }
    let tz: BranchTimeZone = timezone
        .parse()
        .map_err(|e| DbError::Corrupt(format!("branch time zone: {e}")))?;
    let day = LocalDay::of(at, tz);
    let table = match kind {
        NumberKind::Sale => "sale",
        NumberKind::Return => "sale_return",
    };
    let last = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        &format!("SELECT MAX(seq) AS seq FROM {table} WHERE register_id = ? AND local_day = ?"),
        args![register, day.key()],
        |row| row.opt_i64("seq"),
    )
    .await?
    .flatten()
    .unwrap_or(0);
    let seq = u32::try_from(last + 1)
        .map_err(|_| DbError::Corrupt(format!("document sequence {last} out of range")))?;
    let number = match kind {
        NumberKind::Sale => sale_number(&branch_code, &register_code, day, seq),
        NumberKind::Return => return_number(&branch_code, &register_code, day, seq),
    };
    Ok(Allocated {
        number,
        day: day.key(),
        seq: i64::from(seq),
    })
}

/// Appends `entries` (from `core`) to `customer`'s balance ledger for document `document`.
async fn append_balance(
    tx: &mut Tx,
    customer: Option<CustomerId>,
    entries: &[sultan_core::customer::BalanceEntry],
    place: Place,
    document: Uuid,
    at: Timestamp,
    now: Timestamp,
) -> SaleResult<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let customer = customer.ok_or_else(|| {
        SaleDbError::invalid("customer", "balance entries need the document's customer")
    })?;
    for entry in entries {
        let row = customers::BalanceEntry {
            branch: Some(place.branch),
            register: Some(place.register),
            user: Some(place.user),
            cashier_session: Some(place.session),
            document: Some(document),
            ..customers::BalanceEntry::new(customer, db_kind(entry.kind()), entry.amount(), at)
        };
        customers::append_balance_entry(tx, &row, now).await?;
    }
    Ok(())
}

/// The database's balance entry kind of a `core` kind.
const fn db_kind(kind: CoreKind) -> BalanceEntryKind {
    match kind {
        CoreKind::ChangeSaved => BalanceEntryKind::ChangeSaved,
        CoreKind::Payment => BalanceEntryKind::Payment,
        CoreKind::ChangeSavedReversed => BalanceEntryKind::ChangeSavedReversed,
        CoreKind::PaymentReversed => BalanceEntryKind::PaymentReversed,
        CoreKind::Payout => BalanceEntryKind::Payout,
        CoreKind::MergeOut => BalanceEntryKind::MergeOut,
        CoreKind::MergeIn => BalanceEntryKind::MergeIn,
        CoreKind::Refund => BalanceEntryKind::Refund,
    }
}

/// Parses a stored tender kind.
fn tender_kind(row: &dyn Row, col: &str) -> Result<TenderKind> {
    let key = row.text(col)?;
    key.parse()
        .map_err(|_| DbError::Corrupt(format!("unknown tender kind `{key}`")))
}

/// Parses a stored product type.
fn product_type(row: &dyn Row, col: &str) -> Result<ProductType> {
    let key = row.text(col)?;
    key.parse()
        .map_err(|_| DbError::Corrupt(format!("unknown product type `{key}`")))
}

/// A bundle's component snapshot as stored (JSON), `None` when there are none.
fn components_json(components: &[ComponentSnapshot]) -> Option<String> {
    if components.is_empty() {
        return None;
    }
    let list: Vec<Value> = components
        .iter()
        .map(|c| {
            json!({
                "variant": c.variant.to_string(),
                "product_type": c.product_type.as_str(),
                "unit": c.unit.to_string(),
                "qty": c.qty.thousandths(),
                "factor": c.factor.thousandths(),
            })
        })
        .collect();
    Some(Value::Array(list).to_string())
}

/// Reads a stored component snapshot.
fn components(row: &dyn Row, col: &str) -> Result<Vec<ComponentSnapshot>> {
    let Some(value) = sql::opt_json(row, col)? else {
        return Ok(Vec::new());
    };
    let corrupt = || DbError::Corrupt(format!("column `{col}` is not a component snapshot"));
    let Value::Array(items) = value else {
        return Err(corrupt());
    };
    items
        .iter()
        .map(|item| {
            let text = |key: &str| item.get(key).and_then(Value::as_str).ok_or_else(corrupt);
            let int = |key: &str| item.get(key).and_then(Value::as_i64).ok_or_else(corrupt);
            Ok(ComponentSnapshot {
                variant: text("variant")?.parse().map_err(|_| corrupt())?,
                product_type: text("product_type")?.parse().map_err(|_| corrupt())?,
                unit: text("unit")?.parse().map_err(|_| corrupt())?,
                qty: Qty::from_thousandths(int("qty")?),
                factor: Qty::from_thousandths(int("factor")?),
            })
        })
        .collect()
}

/// `limit` within 1..=`max`; `0` means `default`.
fn clamp_limit(limit: u32, default: u32, max: u32) -> i64 {
    i64::from(if limit == 0 { default } else { limit.min(max) })
}

/// A stored count as `u32`.
fn count(row: &dyn Row, col: &str) -> Result<u32> {
    let value = row.i64(col)?;
    u32::try_from(value).map_err(|_| DbError::Corrupt(format!("`{col}` {value} out of range")))
}

/// Trims an optional text of at most `max` characters; blank → `None`.
fn clean_text(field: &'static str, text: Option<&str>, max: usize) -> SaleResult<Option<String>> {
    let Some(text) = text.map(str::trim).filter(|t| !t.is_empty()) else {
        return Ok(None);
    };
    if text.chars().count() > max {
        return Err(SaleDbError::invalid(
            field,
            format!("must be at most {max} characters"),
        ));
    }
    Ok(Some(text.to_owned()))
}

#[cfg(test)]
mod tests {
    use sultan_core::ids::{UnitId, VariantId};

    use super::*;

    #[test]
    fn codes_and_retry() {
        let id = Uuid::from_u128(1);
        let cases: [(SaleDbError, Option<&str>); 7] = [
            (SaleDbError::NotFound("sale"), Some("not_found")),
            (SaleDbError::invalid("lines", "x"), Some("invalid_field")),
            (SaleDbError::Archived, Some("archived")),
            (
                SaleDbError::Rule(SaleError::HasReturns),
                Some("has_returns"),
            ),
            (SaleDbError::Conflict("sale", id), Some("conflict")),
            (SaleDbError::Rule(SaleError::CartTaken), Some("cart_taken")),
            (SaleDbError::Db(DbError::NotFound("x")), None),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert!(!error.is_retryable(), "{error}");
            assert!(!error.to_string().is_empty());
        }
        assert!(SaleDbError::Db(DbError::Conflict("x".into())).is_retryable());
        assert_eq!(
            SaleDbError::Conflict("sale", id).to_string(),
            format!("sale {id} already exists with another body")
        );
    }

    #[test]
    fn customer_errors_map() {
        assert!(matches!(
            SaleDbError::from(CustomerDbError::NotFound("customer")),
            SaleDbError::NotFound("customer")
        ));
        assert!(matches!(
            SaleDbError::from(CustomerDbError::Db(DbError::Conflict("x".into()))),
            SaleDbError::Db(DbError::Conflict(_))
        ));
        let invalid = SaleDbError::from(CustomerDbError::Archived);
        assert_eq!(invalid.code(), Some("invalid_field"));
    }

    #[test]
    fn kinds_map_one_to_one() {
        for kind in CoreKind::ALL {
            assert_eq!(db_kind(kind).as_str(), kind.as_str());
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(clamp_limit(0, 20, 50), 20);
        assert_eq!(clamp_limit(7, 20, 50), 7);
        assert_eq!(clamp_limit(99, 20, 50), 50);
        assert_eq!(
            clean_text("note", Some("  a "), 5).unwrap(),
            Some("a".into())
        );
        assert_eq!(clean_text("note", Some("  "), 5).unwrap(), None);
        assert_eq!(clean_text("note", None, 5).unwrap(), None);
        assert!(clean_text("note", Some("abcdef"), 5).is_err());
        assert_eq!(components_json(&[]), None);
        let c = ComponentSnapshot {
            variant: VariantId::generate(),
            product_type: ProductType::Stock,
            unit: UnitId::generate(),
            qty: Qty::from_thousandths(2_000),
            factor: Qty::from_thousandths(1_000),
        };
        let text = components_json(&[c]).unwrap();
        assert!(text.contains("\"qty\":2000"), "{text}");
    }
}
