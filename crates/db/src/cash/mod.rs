//! The cash management repositories (spec `docs/spec/2.7-cash-management.md`, phase K2): cash
//! reasons, cash movements (in, out, safe drop) and their nota photos, the float between sessions,
//! the session figures a close report, an X and a Z need, and the Z report of a branch's business
//! day.
//!
//! Every rule lives in `sultan_core::cash` (reason direction, note, reversal, permissions, the
//! figures, the Z run check); the caller (the K3 server) applies it and this module stores the
//! result. On top of that it enforces what only the database can:
//!
//! - **One transaction per write.** [`put_cash_reason`] and [`archive_cash_reason`] (HQ master
//!   data), [`post_cash_movement`], [`add_photo`] and [`insert_z_report`] take a [`Tx`] and write
//!   the row with one audit row and one outbox row (spec 2.7 §8: `cash_reason.put`,
//!   `cash_reason.archive`, `cash_movement.posted`, `cash_movement.photo_added`,
//!   `z_report.posted`). An unchanged replay writes nothing.
//! - **Numbers** (§1, K9, K17): a cash movement gets the next sequence of its register on the
//!   branch-local posting day ([`sultan_core::doc_number::cash_movement_number`]), a Z the next of
//!   its branch and business day ([`next_z_number`]), inside the posting transaction. A unique key
//!   turns a lost race into a retryable [`DbError::Conflict`], like sales and session numbers.
//! - **Idempotent posting** (§11): a movement stores the hash of its posted body. The same id
//!   with the same hash returns the stored movement ([`Written::Unchanged`]); another hash is
//!   [`CashDbError::Conflict`].
//! - **Reversal once** (K10): `cash_movement.reverses` is unique; [`post_cash_movement`] checks the
//!   target first (same session, not a reversal, not reversed yet) and the key catches a race.
//! - **Append-only:** `cash_movement`, `cash_movement_photo` and `z_report` refuse `UPDATE` and
//!   `DELETE` by trigger, like `audit_log` and the other ledgers. Cash reasons are archived, never
//!   deleted (no function deletes one).
//! - **Photos** (§2a): at most [`sultan_core::cash::MAX_PHOTOS`] per movement, numbered 1–3 with a
//!   unique key, so two racing uploads cannot make a fourth.
//! - **Sessions in a Z** (§6, K16): [`insert_z_report`] links each covered session with a guarded
//!   `UPDATE` (closed, of the day and branch, not in a Z yet); a guard that fails is a lost race.
//!
//! The session inputs of `core::cash::session_figures` come from [`session_cash`] (the movements,
//! photo counts, no-sale drawer opens and X reads) and [`session_cash_events`] (the netted cash
//! in / out / drop events that the close report's two blocks add to the sales events of
//! [`crate::sales::session_events`]).

mod movements;
mod photos;
mod reasons;
mod sessions;
mod z;

use serde_json::Value;
use sultan_core::cash::CashError;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::audit::{self, AuditEntry};
use crate::conn::Tx;
use crate::error::{DbError, Result};
use crate::outbox;

pub use crate::catalog::{Actor, Written};
pub use movements::{
    CashFilter, CashMovementRecord, MAX_CASH_PAGE, NewCashMovement, get_cash_movement,
    get_cash_movement_by_number, list_cash_movements, post_cash_movement, replayed_cash_movement,
};
pub use photos::{NotaPhoto, add_photo, count_photos, get_photo, list_photos};
pub use reasons::{
    CashReasonRecord, MAX_REASON_NAME, SEED_REASON_IDS, archive_cash_reason, get_cash_reason,
    list_cash_reasons, put_cash_reason,
};
pub use sessions::{
    DRAWER_OPEN_ACTION, SessionCash, SessionCashState, X_REPORT_ACTION, nota_flag_above,
    proposed_float, record_close_cash, session_cash, session_cash_events, session_cash_state,
    set_opening_proposed,
};
pub use z::{
    DaySession, NewZReport, PendingDay, ZFilter, ZNumber, ZReportRecord, day_sessions,
    get_z_report, insert_z_report, list_z_reports, next_z_number, open_day_sessions, pending_days,
    z_candidates, z_report_sessions,
};

/// Errors of the cash repositories.
#[derive(Debug, thiserror::Error)]
pub enum CashDbError {
    /// The reason, movement, photo, Z report, register or session does not exist.
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
    /// The cash reason is archived: it cannot be edited (`409 archived`).
    #[error("cash reason is archived")]
    Archived,
    /// The movement's cashier session is not open on its register (`409 no_open_session`).
    #[error("no open cashier session")]
    NoOpenSession,
    /// A cash rule from `core`: `already_reversed`, `photo_limit`, `invalid_field`, ….
    #[error(transparent)]
    Rule(#[from] CashError),
    /// A document with this ID was posted with another body (`409 conflict`).
    #[error("{0} {1} already exists with another body")]
    Conflict(&'static str, Uuid),
    /// A database error. A lost race (number, reversal, photo slot, Z link) is
    /// [`DbError::Conflict`]: see [`CashDbError::is_retryable`].
    #[error(transparent)]
    Db(#[from] DbError),
}

impl CashDbError {
    /// The stable problem+json code (spec 2.7 §11), `None` for a database error.
    #[must_use]
    pub fn code(&self) -> Option<&'static str> {
        Some(match self {
            Self::NotFound(_) => "not_found",
            Self::Invalid { .. } => "invalid_field",
            Self::Archived => "archived",
            Self::NoOpenSession => "no_open_session",
            Self::Rule(e) => e.code(),
            Self::Conflict(..) => "conflict",
            Self::Db(_) => return None,
        })
    }

    /// Whether running the whole transaction again may succeed: a lost race on a number, a
    /// reversal, a photo slot or a Z link, a deadlock, a busy `SQLite` database
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

/// Result alias of the cash repositories.
pub type CashResult<T> = std::result::Result<T, CashDbError>;

/// One change as recorded: an audit row and an outbox row.
struct Change<'a> {
    action: &'a str,
    target: String,
    before: Option<Value>,
    after: Value,
    outbox: (&'a str, &'a Value),
}

/// Appends the audit row and the outbox row of `change` in `tx`.
async fn record(tx: &mut Tx, actor: &Actor, now: Timestamp, change: Change<'_>) -> Result<()> {
    let mut entry = AuditEntry::new(now, change.action);
    entry.user = actor.user;
    entry.branch = actor.branch;
    entry.register = actor.register;
    entry.target = Some(change.target);
    entry.before = change.before;
    entry.after = Some(change.after);
    audit::append(&mut *tx, &entry).await?;
    let (kind, payload) = change.outbox;
    outbox::append(&mut *tx, kind, payload, now).await?;
    Ok(())
}

/// A stored count as `u32`.
fn count(row: &dyn crate::sql::Row, col: &str) -> Result<u32> {
    let value = row.i64(col)?;
    u32::try_from(value).map_err(|_| DbError::Corrupt(format!("`{col}` {value} out of range")))
}

/// `limit` within 1..=`max`; `0` means `default`.
fn clamp_limit(limit: u32, default: u32, max: u32) -> i64 {
    i64::from(if limit == 0 { default } else { limit.min(max) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_and_retry() {
        let id = Uuid::from_u128(1);
        let cases: [(CashDbError, Option<&str>); 8] = [
            (CashDbError::NotFound("cash movement"), Some("not_found")),
            (CashDbError::invalid("name", "x"), Some("invalid_field")),
            (CashDbError::Archived, Some("archived")),
            (CashDbError::NoOpenSession, Some("no_open_session")),
            (
                CashDbError::Rule(CashError::AlreadyReversed),
                Some("already_reversed"),
            ),
            (
                CashDbError::Rule(CashError::PhotoLimit { max: 3 }),
                Some("photo_limit"),
            ),
            (CashDbError::Conflict("cash movement", id), Some("conflict")),
            (CashDbError::Db(DbError::NotFound("x")), None),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{error}");
            assert!(!error.is_retryable(), "{error}");
            assert!(!error.to_string().is_empty());
        }
        assert!(CashDbError::Db(DbError::Conflict("x".into())).is_retryable());
        assert_eq!(
            CashDbError::Conflict("cash movement", id).to_string(),
            format!("cash movement {id} already exists with another body")
        );
    }

    #[test]
    fn limits() {
        assert_eq!(clamp_limit(0, 50, 200), 50);
        assert_eq!(clamp_limit(7, 50, 200), 7);
        assert_eq!(clamp_limit(999, 50, 200), 200);
    }
}
