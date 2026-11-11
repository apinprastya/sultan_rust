//! The customer balance ledger and its settings (spec 2.5 §11).
//!
//! `customer_balance_entry` is append-only (triggers reject `UPDATE` and `DELETE`). Each append
//! also adds the amount to the cached `customer.balance` in the same transaction, with an atomic
//! `balance = balance + ?`, so the cache always equals the sum of the entries
//! ([`ledger_balance`]). The business limits (`max_saved_per_sale`, `max_balance`, paying more
//! than the balance, archived customers) are checked by the caller with `core`; this module only
//! enforces the sign of each entry kind.

use serde_json::{Value, json};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, UserId};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use super::{
    Actor, BalanceEntryId, Change, CustomerDbError, CustomerId, CustomerResult, DocumentId,
    Written, clamp_limit, record,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::outbox;
use crate::sql::{self, Arg, Row, args};

/// What a balance entry records (spec 2.5 §11 table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BalanceEntryKind {
    /// `change_saved` (+): change saved on a cash sale.
    ChangeSaved,
    /// `payment` (−): the balance used as a tender.
    Payment,
    /// `change_saved_reversed` (−): voiding or refunding a sale that saved change.
    ChangeSavedReversed,
    /// `payment_reversed` (+): voiding or refunding a sale paid with the balance.
    PaymentReversed,
    /// `payout` (− usually; + writes off a negative balance): cash paid out of the drawer.
    Payout,
    /// `merge_out`: a merge moves the merged customer's balance away (spec 2.5 §8).
    MergeOut,
    /// `merge_in`: the surviving customer receives it.
    MergeIn,
}

impl BalanceEntryKind {
    /// Every kind.
    pub const ALL: [Self; 7] = [
        Self::ChangeSaved,
        Self::Payment,
        Self::ChangeSavedReversed,
        Self::PaymentReversed,
        Self::Payout,
        Self::MergeOut,
        Self::MergeIn,
    ];

    /// The stored key, e.g. `change_saved`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ChangeSaved => "change_saved",
            Self::Payment => "payment",
            Self::ChangeSavedReversed => "change_saved_reversed",
            Self::PaymentReversed => "payment_reversed",
            Self::Payout => "payout",
            Self::MergeOut => "merge_out",
            Self::MergeIn => "merge_in",
        }
    }

    fn parse(key: &str) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|k| k.as_str() == key)
            .ok_or_else(|| DbError::Corrupt(format!("unknown balance entry kind `{key}`")))
    }

    /// The sign the amount must have: `Some(true)` positive, `Some(false)` negative, `None`
    /// either (but never zero).
    #[must_use]
    pub const fn positive(self) -> Option<bool> {
        match self {
            Self::ChangeSaved | Self::PaymentReversed => Some(true),
            Self::Payment | Self::ChangeSavedReversed => Some(false),
            Self::Payout | Self::MergeOut | Self::MergeIn => None,
        }
    }
}

/// One balance ledger entry (spec 2.5 §11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceEntry {
    /// `UUIDv7`, created where the entry is made; also the page cursor.
    pub id: BalanceEntryId,
    /// The customer.
    pub customer: CustomerId,
    /// Kind.
    pub kind: BalanceEntryKind,
    /// Signed amount added to the balance.
    pub amount: Money,
    /// Branch where it happened.
    pub branch: Option<BranchId>,
    /// Register (till entries).
    pub register: Option<RegisterId>,
    /// Acting user.
    pub user: Option<UserId>,
    /// Cashier session (till entries).
    pub cashier_session: Option<CashierSessionId>,
    /// The sale or payout document.
    pub document: Option<DocumentId>,
    /// When it happened.
    pub occurred_at: Timestamp,
}

impl BalanceEntry {
    /// A new entry with a fresh ID and no branch, register, user, session or document.
    #[must_use]
    pub fn new(
        customer: CustomerId,
        kind: BalanceEntryKind,
        amount: Money,
        occurred_at: Timestamp,
    ) -> Self {
        Self {
            id: BalanceEntryId::now_v7(),
            customer,
            kind,
            amount,
            branch: None,
            register: None,
            user: None,
            cashier_session: None,
            document: None,
            occurred_at,
        }
    }

    fn json(&self) -> Value {
        let id = |v: Option<uuid::Uuid>| v.map(|u| u.to_string());
        json!({
            "id": self.id.to_string(),
            "customer_id": self.customer.to_string(),
            "kind": self.kind.as_str(),
            "amount": self.amount.minor(),
            "branch_id": self.branch.map(|b| b.to_string()),
            "register_id": self.register.map(|r| r.to_string()),
            "user_id": self.user.map(|u| u.to_string()),
            "cashier_session_id": self.cashier_session.map(|s| s.to_string()),
            "document_id": id(self.document),
            "occurred_at": self.occurred_at.unix_millis(),
        })
    }
}

/// A customer's balance and one page of its entries (`GET /customers/{id}/balance`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerBalance {
    /// The cached balance.
    pub balance: Money,
    /// Entries, newest first.
    pub entries: Vec<BalanceEntry>,
}

/// Largest page of [`get_balance`].
pub const MAX_ENTRY_PAGE: u32 = 100;

const ENTRY_SELECT: &str = "SELECT id, customer_id, kind, amount, branch_id, register_id, \
     user_id, cashier_session_id, document_id, occurred_at FROM customer_balance_entry";

fn map_entry(row: &dyn Row) -> Result<BalanceEntry> {
    Ok(BalanceEntry {
        id: sql::id(row, "id")?,
        customer: sql::id(row, "customer_id")?,
        kind: BalanceEntryKind::parse(&row.text("kind")?)?,
        amount: sql::money(row, "amount")?,
        branch: sql::opt_id(row, "branch_id")?,
        register: sql::opt_id(row, "register_id")?,
        user: sql::opt_id(row, "user_id")?,
        cashier_session: sql::opt_id(row, "cashier_session_id")?,
        document: sql::opt_id(row, "document_id")?,
        occurred_at: sql::ts(row, "occurred_at")?,
    })
}

/// Inserts `entry` and adds its amount to the cached balance. No outbox row.
pub(super) async fn insert_entry(tx: &mut Tx, entry: &BalanceEntry) -> CustomerResult<()> {
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO customer_balance_entry (id, customer_id, kind, amount, branch_id, \
         register_id, user_id, cashier_session_id, document_id, occurred_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            entry.id,
            entry.customer,
            entry.kind.as_str(),
            entry.amount,
            entry.branch,
            entry.register,
            entry.user,
            entry.cashier_session,
            entry.document,
            entry.occurred_at,
        ],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE customer SET balance = balance + ? WHERE id = ?",
        args![entry.amount, entry.customer],
    )
    .await?;
    Ok(())
}

/// Appends a balance entry (a sale's saved change or balance tender, their reversal, a payout,
/// or an entry synced from a branch) and adds it to the customer's cached balance, in `tx`. Adds
/// an outbox row `customer.balance_entry` so the entry flows up to HQ. Replaying the same entry
/// (same ID and values) changes nothing and answers [`Written::Unchanged`]. Returns the new
/// balance.
///
/// The limits of spec 2.5 §11 are the caller's job.
///
/// # Errors
/// [`CustomerDbError::Invalid`] for a zero amount or the wrong sign for the kind;
/// [`CustomerDbError::NotFound`] for an unknown customer; [`CustomerDbError::EntryConflict`]
/// when the ID exists with other values; or the database error.
pub async fn append_balance_entry(
    tx: &mut Tx,
    entry: &BalanceEntry,
    now: Timestamp,
) -> CustomerResult<(Written, Money)> {
    let ok = match entry.kind.positive() {
        _ if entry.amount.minor() == 0 => false,
        Some(positive) => (entry.amount.minor() > 0) == positive,
        None => true,
    };
    if !ok {
        return Err(CustomerDbError::invalid(
            "amount",
            format!("{} for a {} entry", entry.amount, entry.kind.as_str()),
        ));
    }
    let stored_balance = cached_balance(Conn::Tx(&mut *tx), entry.customer)
        .await?
        .ok_or(CustomerDbError::NotFound("customer"))?;
    let sql = format!("{ENTRY_SELECT} WHERE id = ?");
    let existing =
        sql::fetch_optional(Conn::Tx(&mut *tx), &sql, args![entry.id], map_entry).await?;
    if let Some(existing) = existing {
        return if existing == *entry {
            Ok((Written::Unchanged, stored_balance))
        } else {
            Err(CustomerDbError::EntryConflict(entry.id))
        };
    }
    insert_entry(tx, entry).await?;
    outbox::append(&mut *tx, "customer.balance_entry", &entry.json(), now).await?;
    let balance = cached_balance(Conn::Tx(&mut *tx), entry.customer)
        .await?
        .ok_or(CustomerDbError::NotFound("customer"))?;
    Ok((Written::Created, balance))
}

async fn cached_balance(conn: Conn<'_>, customer: CustomerId) -> Result<Option<Money>> {
    sql::fetch_optional(
        conn,
        "SELECT balance FROM customer WHERE id = ?",
        args![customer],
        |row| sql::money(row, "balance"),
    )
    .await
}

/// The customer's cached balance and a page of its entries, newest first (`before` = the last
/// entry ID of the previous page; `limit` 0 = 50, at most [`MAX_ENTRY_PAGE`]). `None` for an
/// unknown customer.
///
/// # Errors
/// The database error.
pub async fn get_balance<'c>(
    conn: impl Into<Conn<'c>>,
    customer: CustomerId,
    before: Option<BalanceEntryId>,
    limit: u32,
) -> Result<Option<CustomerBalance>> {
    let mut conn = conn.into();
    let Some(balance) = cached_balance(conn.reborrow(), customer).await? else {
        return Ok(None);
    };
    let mut values: Vec<Arg> = args![customer];
    let cursor = if let Some(before) = before {
        values.push(before.into());
        " AND id < ?"
    } else {
        ""
    };
    values.push(clamp_limit(limit, 50, MAX_ENTRY_PAGE).into());
    let sql = format!("{ENTRY_SELECT} WHERE customer_id = ?{cursor} ORDER BY id DESC LIMIT ?");
    let entries = sql::fetch_all(conn, &sql, values, map_entry).await?;
    Ok(Some(CustomerBalance { balance, entries }))
}

/// The sum of the customer's ledger entries (what the cached balance must equal).
///
/// # Errors
/// The database error.
pub async fn ledger_balance<'c>(conn: impl Into<Conn<'c>>, customer: CustomerId) -> Result<Money> {
    sql::fetch_optional(
        conn.into(),
        "SELECT CAST(COALESCE(SUM(amount), 0) AS SIGNED) AS total FROM customer_balance_entry \
         WHERE customer_id = ?",
        args![customer],
        |row| sql::money(row, "total"),
    )
    .await
    .map(Option::unwrap_or_default)
}

/// The cash a cashier session paid out of its drawer for balance payouts (spec 2.5 §11, phase
/// M3): minus the sum of its negative `payout` entries, so `0` or more. A positive `payout`
/// entry (a negative balance written off) moves no cash and is not counted. The close report
/// counts this as cash out.
///
/// # Errors
/// The database error.
pub async fn session_payouts<'c>(
    conn: impl Into<Conn<'c>>,
    session: CashierSessionId,
) -> Result<Money> {
    sql::fetch_optional(
        conn.into(),
        "SELECT CAST(COALESCE(SUM(amount), 0) AS SIGNED) AS total FROM customer_balance_entry \
         WHERE cashier_session_id = ? AND kind = ? AND amount < 0",
        args![session, BalanceEntryKind::Payout.as_str()],
        |row| sql::money(row, "total"),
    )
    .await
    .map(|total| Money::from_minor(-total.unwrap_or_default().minor()))
}

/// The company settings for saved change (spec 2.5 §11). `max_saved_per_sale = 0` turns saving
/// change off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CustomerBalanceSettings {
    /// Most change saved on one sale (default Rp 5 000).
    pub max_saved_per_sale: Money,
    /// Most balance a customer may hold through saved change (default Rp 100 000).
    pub max_balance: Money,
}

fn settings_json(s: &CustomerBalanceSettings) -> Value {
    json!({
        "max_saved_per_sale": s.max_saved_per_sale.minor(),
        "max_balance": s.max_balance.minor(),
    })
}

/// The balance settings (`GET /settings/customer-balance`).
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] when the row is missing.
pub async fn get_balance_settings<'c>(
    conn: impl Into<Conn<'c>>,
) -> Result<CustomerBalanceSettings> {
    sql::fetch_optional(
        conn.into(),
        "SELECT max_saved_per_sale, max_balance FROM customer_balance_settings WHERE id = 1",
        args![],
        |row| {
            Ok(CustomerBalanceSettings {
                max_saved_per_sale: sql::money(row, "max_saved_per_sale")?,
                max_balance: sql::money(row, "max_balance")?,
            })
        },
    )
    .await?
    .ok_or_else(|| DbError::Corrupt("customer_balance_settings row is missing".into()))
}

/// Replaces the balance settings (`PUT /settings/customer-balance`). Audit and outbox
/// `customer.balance_settings.put` unless nothing changed.
///
/// # Errors
/// [`CustomerDbError::Invalid`] for a negative limit, or the database error.
pub async fn put_balance_settings(
    tx: &mut Tx,
    settings: &CustomerBalanceSettings,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    for (field, value) in [
        ("max_saved_per_sale", settings.max_saved_per_sale),
        ("max_balance", settings.max_balance),
    ] {
        if value.is_negative() {
            return Err(CustomerDbError::invalid(field, "must not be negative"));
        }
    }
    let old = get_balance_settings(Conn::Tx(&mut *tx)).await?;
    if old == *settings {
        return Ok(Written::Unchanged);
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE customer_balance_settings SET max_saved_per_sale = ?, max_balance = ?, \
         updated_at = ? WHERE id = 1",
        args![settings.max_saved_per_sale, settings.max_balance, now],
    )
    .await?;
    let after = settings_json(settings);
    record(
        tx,
        actor,
        now,
        Change {
            action: "customer.balance_settings.put",
            target: "customer_balance_settings".to_owned(),
            before: Some(settings_json(&old)),
            after: Some(after.clone()),
            outbox_kind: "customer.balance_settings.put",
            payload: after,
        },
    )
    .await?;
    Ok(Written::Updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_signs() {
        for kind in BalanceEntryKind::ALL {
            assert_eq!(BalanceEntryKind::parse(kind.as_str()).unwrap(), kind);
        }
        assert!(BalanceEntryKind::parse("bonus").is_err());
        assert_eq!(BalanceEntryKind::ChangeSaved.positive(), Some(true));
        assert_eq!(BalanceEntryKind::Payment.positive(), Some(false));
        assert_eq!(BalanceEntryKind::MergeOut.positive(), None);
    }
}
