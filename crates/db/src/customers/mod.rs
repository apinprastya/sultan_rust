//! The customer repositories (spec `docs/spec/2.5-customers.md`, phase M2): customers with
//! lookup, name search, archive, erase and merge; customer groups and membership tiers; the
//! customer balance ledger (saved change) and its company settings.
//!
//! Master-data writes (customers, groups, tiers, settings) take a [`Tx`] and append **one audit
//! row and one outbox row in the same transaction**, like the catalog (`docs/plan.md` §1). A write
//! that changes nothing (an idempotent replay) appends neither and answers
//! [`Written::Unchanged`]. A merge appends one audit row and one outbox row for each of the two
//! customers (spec 2.5 §8).
//!
//! Balance entries are transactional data like sales (spec 2.5 §12): appending one adds an outbox
//! row (`customer.balance_entry`) so it can flow up to HQ, but no audit row.
//!
//! Format rules (phone normalization, tax id, code alphabet) live in `core::customer` and are the
//! caller's job: this module stores already-normalized strings. It only trims text, turns blank
//! optional fields into `None`, checks the name length and the birthday range, and enforces the
//! uniqueness rules (code; phone among non-archived customers; group and tier names; tier ranks).
//!
//! IDs of customers, groups, tiers and balance entries are plain [`Uuid`]s (aliases below) until
//! `core::ids` gets typed IDs for them.

mod balance;
mod customer;
mod groups;

use serde_json::Value;
use sultan_core::money::Money;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::audit::{self, AuditEntry};
use crate::conn::Tx;
use crate::error::DbError;
use crate::outbox;

pub use crate::catalog::{Actor, Written};
pub use balance::{
    BalanceEntry, BalanceEntryKind, CustomerBalance, CustomerBalanceSettings, MAX_ENTRY_PAGE,
    append_balance_entry, get_balance, get_balance_settings, ledger_balance, put_balance_settings,
};
pub use customer::{
    Birthday, Customer, CustomerRecord, ERASED_NAME, MAX_PAGE, MAX_SEARCH_PAGE, archive_customer,
    erase_customer, get_customer, list_duplicates, list_negative_balances, lookup_by_code,
    lookup_by_phone, merge_customers, put_customer, put_synced_customer, search_customers,
    unarchive_customer,
};
pub use groups::{
    CustomerGroup, MembershipTier, archive_group, archive_tier, get_group, get_tier, list_groups,
    list_tiers, put_group, put_tier,
};

/// A customer's ID (`UUIDv7`, created by the client).
pub type CustomerId = Uuid;
/// A customer group's ID.
pub type CustomerGroupId = Uuid;
/// A membership tier's ID.
pub type MembershipTierId = Uuid;
/// A balance entry's ID (`UUIDv7`, created where the entry is made).
pub type BalanceEntryId = Uuid;
/// The sale or payout document a balance entry belongs to.
pub type DocumentId = Uuid;

/// Errors of the customer repositories.
#[derive(Debug, thiserror::Error)]
pub enum CustomerDbError {
    /// The record does not exist (or, for an erase, was already erased).
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A field breaks a storage rule (name length, birthday range, negative limit, entry sign).
    #[error("invalid {field}: {message}")]
    Invalid {
        /// The field.
        field: &'static str,
        /// What is wrong, in English.
        message: String,
    },
    /// The customer's group or tier does not exist.
    #[error("unknown {0}")]
    InvalidReference(&'static str),
    /// Another customer already has this code. The caller draws a new code and tries again
    /// (spec 2.5 §2 "Code clashes").
    #[error("customer code `{code}` is already used")]
    CodeTaken {
        /// The code.
        code: String,
    },
    /// Another non-archived customer has this phone (spec 2.5 §7, `409 phone_taken`).
    #[error("phone `{phone}` is already used by customer {code}")]
    PhoneTaken {
        /// The customer holding the phone.
        customer: CustomerId,
        /// Its code.
        code: String,
        /// The phone.
        phone: String,
    },
    /// A group or tier name, or a tier rank, is used by another group or tier.
    #[error("{field} `{value}` is already used")]
    Duplicate {
        /// `name` or `rank`.
        field: &'static str,
        /// The value.
        value: String,
    },
    /// The customer is archived (or erased): it cannot be edited or merged.
    #[error("customer is archived")]
    Archived,
    /// The customer was merged into another one (spec 2.5 §8, `409 merged`).
    #[error("customer was merged into another customer")]
    Merged,
    /// A customer cannot be merged into itself (spec 2.5 §15 case 17, `422`).
    #[error("a customer cannot be merged into itself")]
    SameCustomer,
    /// An erase while the balance is not zero (spec 2.5 §11, `409 balance_not_zero`).
    #[error("customer balance is {balance}, not 0")]
    BalanceNotZero {
        /// The current balance.
        balance: Money,
    },
    /// A balance entry with this ID exists with different values.
    #[error("balance entry {0} already exists with other values")]
    EntryConflict(BalanceEntryId),
    /// A database error. A lost race on a unique key is [`DbError::Conflict`]: see
    /// [`CustomerDbError::is_retryable`].
    #[error(transparent)]
    Db(#[from] DbError),
}

impl CustomerDbError {
    /// The stable problem+json code (spec 2.5 §13), `None` for a database error.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::NotFound(_) => "not_found",
            Self::Invalid { .. } | Self::SameCustomer => "invalid_field",
            Self::InvalidReference(_) => "invalid_reference",
            Self::CodeTaken { .. } | Self::Duplicate { .. } | Self::EntryConflict(_) => "conflict",
            Self::PhoneTaken { .. } => "phone_taken",
            Self::Archived => "archived",
            Self::Merged => "merged",
            Self::BalanceNotZero { .. } => "balance_not_zero",
            Self::Db(_) => return None,
        })
    }

    /// Whether running the whole transaction again may succeed (a lost race on a unique key, a
    /// deadlock, a busy `SQLite` database): see [`DbError::is_retryable`]. After a lost race on
    /// the phone or code index, the rerun reports [`CustomerDbError::PhoneTaken`] or
    /// [`CustomerDbError::CodeTaken`].
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

/// Result alias of the customer repositories.
pub type CustomerResult<T> = std::result::Result<T, CustomerDbError>;

/// One change as recorded: an audit row and an outbox row.
struct Change<'a> {
    action: &'a str,
    target: String,
    before: Option<Value>,
    after: Option<Value>,
    outbox_kind: &'a str,
    payload: Value,
}

/// Appends the audit row and the outbox row of `change` in `tx`.
async fn record(
    tx: &mut Tx,
    actor: &Actor,
    now: Timestamp,
    change: Change<'_>,
) -> CustomerResult<()> {
    let mut entry = AuditEntry::new(now, change.action);
    entry.user = actor.user;
    entry.branch = actor.branch;
    entry.register = actor.register;
    entry.target = Some(change.target);
    entry.before = change.before;
    entry.after = change.after;
    audit::append(&mut *tx, &entry).await?;
    outbox::append(&mut *tx, change.outbox_kind, &change.payload, now).await?;
    Ok(())
}

fn written(saved: crate::Saved) -> Written {
    match saved {
        crate::Saved::Created => Written::Created,
        crate::Saved::Updated => Written::Updated,
    }
}

/// `limit` within 1..=`max`; `0` means `default`.
fn clamp_limit(limit: u32, default: u32, max: u32) -> i64 {
    i64::from(if limit == 0 { default } else { limit.min(max) })
}

/// Trims `text`; `None` when it is blank.
fn clean(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_owned)
}

/// Checks a required name: 1–100 characters after trimming (spec 2.5 §2, §3, §4).
fn clean_name(name: &str) -> CustomerResult<String> {
    let name = name.trim();
    let len = name.chars().count();
    if len == 0 || len > 100 {
        return Err(CustomerDbError::invalid(
            "name",
            "must be 1-100 characters after trimming",
        ));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_retry() {
        let id = Uuid::from_u128(7);
        let cases: [(CustomerDbError, Option<&str>); 12] = [
            (CustomerDbError::NotFound("customer"), Some("not_found")),
            (CustomerDbError::invalid("name", "x"), Some("invalid_field")),
            (CustomerDbError::SameCustomer, Some("invalid_field")),
            (
                CustomerDbError::InvalidReference("tier"),
                Some("invalid_reference"),
            ),
            (
                CustomerDbError::CodeTaken { code: "A".into() },
                Some("conflict"),
            ),
            (
                CustomerDbError::Duplicate {
                    field: "name",
                    value: "Gold".into(),
                },
                Some("conflict"),
            ),
            (CustomerDbError::EntryConflict(id), Some("conflict")),
            (
                CustomerDbError::PhoneTaken {
                    customer: id,
                    code: "K7QH29XM".into(),
                    phone: "+628".into(),
                },
                Some("phone_taken"),
            ),
            (CustomerDbError::Archived, Some("archived")),
            (CustomerDbError::Merged, Some("merged")),
            (
                CustomerDbError::BalanceNotZero {
                    balance: Money::from_minor(5),
                },
                Some("balance_not_zero"),
            ),
            (CustomerDbError::Db(DbError::NotFound("x")), None),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert!(!error.is_retryable(), "{error}");
            assert!(!error.to_string().is_empty());
        }
        assert!(CustomerDbError::Db(DbError::Conflict("x".into())).is_retryable());
        assert_eq!(
            CustomerDbError::BalanceNotZero {
                balance: Money::from_minor(5)
            }
            .to_string(),
            "customer balance is 5, not 0"
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(clamp_limit(0, 20, 50), 20);
        assert_eq!(clamp_limit(7, 20, 50), 7);
        assert_eq!(clamp_limit(99, 20, 50), 50);
        assert_eq!(clean(Some("  a ")), Some("a".to_owned()));
        assert_eq!(clean(Some("   ")), None);
        assert_eq!(clean(None), None);
        assert_eq!(clean_name(" Budi ").unwrap(), "Budi");
        assert!(clean_name("  ").is_err());
        assert!(clean_name(&"x".repeat(101)).is_err());
        assert!(clean_name(&"é".repeat(100)).is_ok());
    }
}
