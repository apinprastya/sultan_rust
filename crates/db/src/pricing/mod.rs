//! The pricing repositories (spec `docs/spec/2.3-pricing.md`, phase R2): price lists, their
//! entries and quantity tiers, tax rates per tax class, the company pricing settings, and the
//! loader that hands a sale's lists to `core::pricing` ([`load_for_resolution`]).
//!
//! Every rule lives in `sultan_core::pricing` (list and entry validation, overlap, resolution)
//! and `sultan_core::tax`; this module loads the inputs, runs the `core` checks and stores the
//! result. Master-data writes take a [`Tx`] and append **one audit row and one outbox row in the
//! same transaction**, like the catalog and the customers. A write that changes nothing (an
//! idempotent replay) appends neither and answers [`Written::Unchanged`].
//!
//! Storage rules on top of `core`:
//! - At most one active, non-archived list per (kind, target): `price_list_conflict`
//!   ([`PricingDbError::ListConflict`]). A unique index is the safety net against races.
//! - Exactly one `default` list, seeded with [`DEFAULT_PRICE_LIST`]. No other list can be of kind
//!   `default`, and a list's kind and target never change (`invalid_field`).
//! - Writing an entry marks its variant **used** (spec 2.2 §2, [`crate::catalog::mark_used`]).
//!
//! Known limitations (sales arrive with 2.6):
//! - Deleting an entry ([`delete_price_entry`]) and changing a tax rate ([`put_tax_rate`]) are
//!   always allowed. Once posted sales exist, both must refuse what a posted sale used.

mod entries;
mod lists;
mod tax;

use chrono::{Datelike, NaiveDate};
use serde_json::{Value, json};
use sultan_core::money::Money;
use sultan_core::pricing::{PriceTier, PricingError, Rule, TierPrice};
use sultan_core::time::Timestamp;

use crate::audit::{self, AuditEntry};
use crate::conn::Tx;
use crate::error::{DbError, Result};
use crate::outbox;
use crate::sql::Row;

pub use crate::catalog::{Actor, Written};
pub use entries::{
    EntryFilter, MAX_ENTRY_PAGE, PriceEntryRecord, PriceTargets, ResolutionLists,
    delete_price_entry, get_price_entry, list_price_entries, load_for_resolution, put_price_entry,
};
pub use lists::{
    DEFAULT_PRICE_LIST, ListFilter, PriceListRecord, archive_price_list, get_price_list,
    list_price_lists, put_price_list,
};
pub use tax::{
    PricingSettings, get_pricing_settings, list_tax_rates, put_pricing_settings, put_tax_rate,
    tax_rates_by_class,
};

/// Errors of the pricing repositories.
#[derive(Debug, thiserror::Error)]
pub enum PricingDbError {
    /// The list, entry or tax class does not exist.
    #[error("not found: {0}")]
    NotFound(&'static str),
    /// A pricing rule from `core`: `invalid_field`, `invalid_rule`, `price_overlap`.
    #[error(transparent)]
    Rule(#[from] PricingError),
    /// Another active, non-archived list has the same kind and target (spec 2.3 §2, `409`).
    #[error("price list {existing} is already active for this target")]
    ListConflict {
        /// The live list holding the target.
        existing: sultan_core::pricing::PriceListId,
    },
    /// The list's target, or the entry's variant or unit, does not exist.
    #[error("unknown {0}")]
    InvalidReference(&'static str),
    /// A field breaks a storage rule (name length, a changed kind or target, a second default
    /// list, an entry moved to another list).
    #[error("invalid {field}: {message}")]
    Invalid {
        /// The field.
        field: &'static str,
        /// What is wrong, in English.
        message: &'static str,
    },
    /// A database error. A lost race on the live-list index is [`DbError::Conflict`]: see
    /// [`PricingDbError::is_retryable`].
    #[error(transparent)]
    Db(#[from] DbError),
}

impl PricingDbError {
    /// The stable problem+json code (spec 2.3 §10), `None` for a database error.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::NotFound(_) => "not_found",
            Self::Rule(e) => e.code(),
            Self::ListConflict { .. } => "price_list_conflict",
            Self::InvalidReference(_) => "invalid_reference",
            Self::Invalid { .. } => "invalid_field",
            Self::Db(_) => return None,
        })
    }

    /// Whether running the whole transaction again may succeed (a lost race on a unique key, a
    /// deadlock, a busy `SQLite` database). After a lost race on the live-list index, the rerun
    /// reports [`PricingDbError::ListConflict`].
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Db(e) if e.is_retryable())
    }

    const fn invalid(field: &'static str, message: &'static str) -> Self {
        Self::Invalid { field, message }
    }
}

/// Result alias of the pricing repositories.
pub type PricingResult<T> = std::result::Result<T, PricingDbError>;

/// One change as recorded: an audit row and an outbox row.
struct Change<'a> {
    action: &'a str,
    target: String,
    before: Option<Value>,
    after: Option<Value>,
    payload: Value,
}

/// Appends the audit row and the outbox row (kind = the audit action) of `change` in `tx`.
async fn record(tx: &mut Tx, actor: &Actor, now: Timestamp, change: Change<'_>) -> Result<()> {
    let mut entry = AuditEntry::new(now, change.action);
    entry.user = actor.user;
    entry.branch = actor.branch;
    entry.register = actor.register;
    entry.target = Some(change.target);
    entry.before = change.before;
    entry.after = change.after;
    audit::append(&mut *tx, &entry).await?;
    outbox::append(&mut *tx, change.action, &change.payload, now).await?;
    Ok(())
}

fn written(saved: crate::Saved) -> Written {
    match saved {
        crate::Saved::Created => Written::Created,
        crate::Saved::Updated => Written::Updated,
    }
}

/// A local date as stored: `YYYYMMDD`.
fn date_value(date: NaiveDate) -> i64 {
    i64::from(date.year()) * 10_000 + i64::from(date.month()) * 100 + i64::from(date.day())
}

/// The inverse of [`date_value`].
fn date_from(value: i64, col: &str) -> Result<NaiveDate> {
    i32::try_from(value / 10_000)
        .ok()
        .zip(u32::try_from(value % 10_000 / 100).ok())
        .zip(u32::try_from(value % 100).ok())
        .and_then(|((y, m), d)| NaiveDate::from_ymd_opt(y, m, d))
        .ok_or_else(|| DbError::Corrupt(format!("column `{col}` is not a YYYYMMDD date: {value}")))
}

fn opt_date(row: &dyn Row, col: &str) -> Result<Option<NaiveDate>> {
    row.opt_i64(col)?.map(|v| date_from(v, col)).transpose()
}

/// A rule as stored: `(rule_kind, rule_value)`.
fn rule_parts(rule: Option<Rule>) -> (Option<&'static str>, Option<i64>) {
    match rule {
        None => (None, None),
        Some(Rule::Pct(bp)) => (Some("pct"), Some(i64::from(bp))),
        Some(Rule::Amount(amount)) => (Some("amount"), Some(amount.minor())),
    }
}

/// Reads the rule columns `rule_kind`, `rule_value`.
fn read_rule(row: &dyn Row) -> Result<Option<Rule>> {
    let kind = row.opt_text("rule_kind")?;
    let value = row.opt_i64("rule_value")?;
    match (kind.as_deref(), value) {
        (None, None) => Ok(None),
        (Some("pct"), Some(bp)) => i32::try_from(bp)
            .map(|bp| Some(Rule::Pct(bp)))
            .map_err(|_| DbError::Corrupt(format!("rule of {bp} basis points"))),
        (Some("amount"), Some(amount)) => Ok(Some(Rule::Amount(Money::from_minor(amount)))),
        (kind, value) => Err(DbError::Corrupt(format!(
            "rule `{kind:?}` with value `{value:?}`"
        ))),
    }
}

fn rule_json(rule: Option<Rule>) -> Value {
    match rule {
        None => Value::Null,
        Some(Rule::Pct(bp)) => json!({ "pct_bp": bp }),
        Some(Rule::Amount(amount)) => json!({ "amount": amount.minor() }),
    }
}

fn tier_json(tier: &PriceTier) -> Value {
    match tier.price {
        TierPrice::Fixed(price) => {
            json!({ "min_qty": tier.min_qty.thousandths(), "price": price.minor() })
        }
        TierPrice::Rule(rule) => {
            json!({ "min_qty": tier.min_qty.thousandths(), "rule": rule_json(Some(rule)) })
        }
    }
}

/// Trims a list name: 1–100 characters.
fn clean_name(name: &str) -> PricingResult<String> {
    let name = name.trim();
    let len = name.chars().count();
    if len == 0 || len > 100 {
        return Err(PricingDbError::invalid(
            "name",
            "must be 1-100 characters after trimming",
        ));
    }
    Ok(name.to_owned())
}

/// `?, ?, …` for `n` values.
fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use sultan_core::catalog::Qty;
    use sultan_core::pricing::PriceListId;

    #[test]
    fn codes_and_retry() {
        let cases: [(PricingDbError, Option<&str>); 9] = [
            (PricingDbError::NotFound("price_list"), Some("not_found")),
            (
                PricingDbError::Rule(PricingError::InvalidRule { reason: "x" }),
                Some("invalid_rule"),
            ),
            (
                PricingDbError::ListConflict {
                    existing: PriceListId::generate(),
                },
                Some("price_list_conflict"),
            ),
            (
                PricingDbError::InvalidReference("target"),
                Some("invalid_reference"),
            ),
            (PricingDbError::invalid("name", "x"), Some("invalid_field")),
            (
                PricingDbError::Rule(PricingError::InvalidField {
                    field: "tiers",
                    reason: "x",
                }),
                Some("invalid_field"),
            ),
            (
                PricingDbError::Rule(PricingError::NoPrice { suggestion: None }),
                Some("no_price"),
            ),
            (
                PricingDbError::Rule(PricingError::Overflow),
                Some("amount_overflow"),
            ),
            (PricingDbError::Db(DbError::NotFound("x")), None),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert!(!error.is_retryable(), "{error}");
            assert!(!error.to_string().is_empty());
        }
        assert!(PricingDbError::Db(DbError::Conflict("x".into())).is_retryable());
    }

    #[test]
    fn dates_round_trip() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 30).unwrap();
        assert_eq!(date_value(d), 20_261_130);
        assert_eq!(date_from(20_261_130, "c").unwrap(), d);
        for bad in [20_261_131, 20_261_300, 0, -1, i64::MAX] {
            assert!(date_from(bad, "c").is_err(), "{bad}");
        }
    }

    #[test]
    fn rules_and_json() {
        assert_eq!(rule_parts(None), (None, None));
        assert_eq!(rule_parts(Some(Rule::Pct(-500))), (Some("pct"), Some(-500)));
        assert_eq!(
            rule_parts(Some(Rule::Amount(Money::from_minor(-300)))),
            (Some("amount"), Some(-300))
        );
        assert_eq!(rule_json(None), Value::Null);
        assert_eq!(rule_json(Some(Rule::Pct(-500))), json!({ "pct_bp": -500 }));
        let fixed = PriceTier {
            min_qty: Qty::ONE,
            price: TierPrice::Fixed(Money::from_minor(9_000)),
        };
        assert_eq!(tier_json(&fixed), json!({ "min_qty": 1000, "price": 9000 }));
        let rule = PriceTier {
            min_qty: Qty::ONE,
            price: TierPrice::Rule(Rule::Amount(Money::from_minor(-500))),
        };
        assert_eq!(
            tier_json(&rule),
            json!({ "min_qty": 1000, "rule": { "amount": -500 } })
        );
    }

    #[test]
    fn helpers() {
        assert_eq!(clean_name(" Gold ").unwrap(), "Gold");
        assert!(clean_name(" ").is_err());
        assert!(clean_name(&"x".repeat(101)).is_err());
        assert_eq!(placeholders(3), "?, ?, ?");
    }
}
