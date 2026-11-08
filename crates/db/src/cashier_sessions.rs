//! Cashier sessions (spec 2.1 §5), mapped to and from `core::cashier_session::CashierSession`.
//!
//! The rules (one open session per register and per user, blind close, force close) are in
//! `core`. The schema adds a unique index on open sessions as a safety net: if two requests race
//! past the `core` check, the second [`insert`] fails with
//! [`DbError::Conflict`](crate::DbError::Conflict).
//!
//! **Session numbers** (Phase 9, spec 2.1 §5): [`insert`] allocates
//! `<BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>` inside the caller's transaction. The date is the open
//! time's calendar day in the branch's time zone (`core::doc_number`), `NN` the next sequence of
//! that register on that local day. `UNIQUE (register_id, local_day, seq)` makes a lost race a
//! `Conflict` too; [`DbError::is_retryable`](crate::DbError::is_retryable) tells the caller to
//! run the transaction again.
//!
//! **Close reports** are stored in full ([`close`], [`stored_close`]): the cash figures, the
//! per-tender totals and the optional count per denomination, so a past report can be shown and
//! reprinted exactly as it was at the close (spec 2.1 §5). [`report_context`] adds the names a
//! printed report needs.

use std::collections::BTreeMap;

use sultan_core::cashier_session::{
    CashierSession, CloseKind, CloseReport, SessionStatus, Tender, TenderTotals,
};
use sultan_core::doc_number::{BranchTimeZone, LocalDay, session_number};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, UserId};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// How many notes or coins of one denomination were counted at close (optional, spec 2.1 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DenominationCount {
    /// Face value in minor units, e.g. 50 000.
    pub denomination: Money,
    /// Number counted.
    pub quantity: u32,
}

/// A stored session with what the API shows next to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// The session.
    pub session: CashierSession,
    /// Its number, e.g. `JKT01-R2-S-261107-01`.
    pub number: String,
    /// The register's branch.
    pub branch: BranchId,
    /// The expected cash stored at the close; `None` while open (blind close: the API shows a
    /// live figure only to holders of `session.view_expected`).
    pub expected_at_close: Option<Money>,
}

/// The stored report of a closed session: exactly the report computed at the close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredClose {
    /// The close report (cash figures, per-tender totals, counted, expected, over/short).
    pub report: CloseReport,
    /// The per-denomination count, if one was entered (largest first).
    pub counts: Vec<DenominationCount>,
}

/// Names and settings a printed close report shows (spec 2.1 §5 "printed on the receipt
/// printer"), read at the time of the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportContext {
    /// Branch code, e.g. `JKT01`.
    pub branch_code: String,
    /// Branch name.
    pub branch_name: String,
    /// Branch IANA time zone, for printing local times.
    pub timezone: String,
    /// Register code, e.g. `R2`.
    pub register_code: String,
    /// Register name.
    pub register_name: String,
    /// The cashier's display name.
    pub cashier_name: String,
    /// The display name of the manager who force-closed it, if forced.
    pub forced_by_name: Option<String>,
}

/// Who may see which sessions, for [`list`] (spec 2.1 §3, §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visibility {
    /// Every session.
    All,
    /// The user's own sessions, plus every session at these branches.
    OwnAnd {
        /// The caller.
        user: UserId,
        /// Branches whose sessions the caller may read.
        branches: Vec<BranchId>,
    },
}

/// Filters for [`list`]. All set fields must match. Results are newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionFilter {
    /// What the caller may see.
    pub visibility: Visibility,
    /// Only this branch.
    pub branch: Option<BranchId>,
    /// Only this register.
    pub register: Option<RegisterId>,
    /// Only this cashier.
    pub user: Option<UserId>,
    /// `Some(true)` = open only, `Some(false)` = closed only.
    pub open: Option<bool>,
    /// Opened at or after this time.
    pub from: Option<Timestamp>,
    /// Opened before this time.
    pub until: Option<Timestamp>,
    /// Page cursor: only sessions older than this ID (the last ID of the previous page).
    pub before_id: Option<CashierSessionId>,
    /// Page size; `0` means 100.
    pub limit: u32,
}

impl SessionFilter {
    /// No filter, for a caller who sees `visibility`.
    #[must_use]
    pub const fn new(visibility: Visibility) -> Self {
        Self {
            visibility,
            branch: None,
            register: None,
            user: None,
            open: None,
            from: None,
            until: None,
            before_id: None,
            limit: 0,
        }
    }
}

const SESSION_SELECT: &str = "SELECT s.id, s.user_id, s.register_id, s.opening_cash, \
                              s.opened_at, s.closed_at, s.close_kind, s.forced_by, s.number, \
                              s.expected_cash, r.branch_id FROM cashier_session s \
                              JOIN register r ON r.id = s.register_id";

/// Stores a newly opened session (from `CashierSession::open`) and gives it the next number of
/// its register and local day; returns the number. Run it in the open transaction.
///
/// # Errors
/// [`DbError::Conflict`] if the register or the user already has an open session (the database
/// safety net) or another transaction took the same number (retry, see the module docs);
/// [`DbError::NotFound`] for an unknown register; [`DbError::Corrupt`] if `session` is not open
/// or the branch's stored time zone is invalid.
pub async fn insert(tx: &mut Tx, session: &CashierSession) -> Result<String> {
    if !session.is_open() {
        return Err(DbError::Corrupt(
            "only an open cashier session can be inserted".into(),
        ));
    }
    let (branch_code, register_code, timezone) = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT b.code AS branch_code, r.code AS register_code, b.timezone FROM register r \
         JOIN branch b ON b.id = r.branch_id WHERE r.id = ?",
        args![session.register()],
        |row| {
            Ok((
                row.text("branch_code")?,
                row.text("register_code")?,
                row.text("timezone")?,
            ))
        },
    )
    .await?
    .ok_or(DbError::NotFound("register"))?;
    let tz: BranchTimeZone = timezone
        .parse()
        .map_err(|e| DbError::Corrupt(format!("branch time zone: {e}")))?;
    let day = LocalDay::of(session.opened_at(), tz);
    let last = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT MAX(seq) AS seq FROM cashier_session WHERE register_id = ? AND local_day = ?",
        args![session.register(), day.key()],
        |row| row.opt_i64("seq"),
    )
    .await?
    .flatten()
    .unwrap_or(0);
    let seq = u32::try_from(last + 1)
        .map_err(|_| DbError::Corrupt(format!("session sequence {last} out of range")))?;
    let number = session_number(&branch_code, &register_code, day, seq);
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO cashier_session (id, user_id, register_id, opening_cash, opened_at, \
         local_day, seq, number) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            session.id(),
            session.user(),
            session.register(),
            session.opening_cash(),
            session.opened_at(),
            day.key(),
            i64::from(seq),
            number.as_str(),
        ],
    )
    .await?;
    Ok(number)
}

/// One session.
///
/// # Errors
/// The database error.
pub async fn get<'c>(
    conn: impl Into<Conn<'c>>,
    id: CashierSessionId,
) -> Result<Option<SessionRecord>> {
    let sql = format!("{SESSION_SELECT} WHERE s.id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_record).await
}

/// The open session on a register, if any.
///
/// # Errors
/// The database error.
pub async fn open_for_register<'c>(
    conn: impl Into<Conn<'c>>,
    register: RegisterId,
) -> Result<Option<SessionRecord>> {
    let sql = format!("{SESSION_SELECT} WHERE s.register_id = ? AND s.closed_at IS NULL");
    sql::fetch_optional(conn.into(), &sql, args![register], map_record).await
}

/// The open session of a user (on any register), if any.
///
/// # Errors
/// The database error.
pub async fn open_for_user<'c>(
    conn: impl Into<Conn<'c>>,
    user: UserId,
) -> Result<Option<SessionRecord>> {
    let sql = format!("{SESSION_SELECT} WHERE s.user_id = ? AND s.closed_at IS NULL");
    sql::fetch_optional(conn.into(), &sql, args![user], map_record).await
}

/// The open sessions on `register` or of `user`: the `existing` input of
/// `CashierSession::open` / `ensure_can_open`.
///
/// # Errors
/// The database error.
pub async fn open_conflicts<'c>(
    conn: impl Into<Conn<'c>>,
    user: UserId,
    register: RegisterId,
) -> Result<Vec<CashierSession>> {
    let sql = format!(
        "{SESSION_SELECT} WHERE s.closed_at IS NULL AND (s.register_id = ? OR s.user_id = ?) \
         ORDER BY s.id"
    );
    let records = sql::fetch_all(conn.into(), &sql, args![register, user], map_record).await?;
    Ok(records.into_iter().map(|r| r.session).collect())
}

/// Sessions matching `filter`, newest first (`GET /cashier-sessions`).
///
/// # Errors
/// The database error.
pub async fn list<'c>(
    conn: impl Into<Conn<'c>>,
    filter: &SessionFilter,
) -> Result<Vec<SessionRecord>> {
    let mut conditions: Vec<String> = Vec::new();
    let mut values: Vec<Arg> = Vec::new();
    if let Visibility::OwnAnd { user, branches } = &filter.visibility {
        values.push((*user).into());
        if branches.is_empty() {
            conditions.push("s.user_id = ?".into());
        } else {
            let marks = vec!["?"; branches.len()].join(", ");
            conditions.push(format!("(s.user_id = ? OR r.branch_id IN ({marks}))"));
            values.extend(branches.iter().map(|b| Arg::from(*b)));
        }
    }
    let mut add = |condition: &str, value: Arg| {
        conditions.push(condition.to_owned());
        values.push(value);
    };
    if let Some(b) = filter.branch {
        add("r.branch_id = ?", b.into());
    }
    if let Some(r) = filter.register {
        add("s.register_id = ?", r.into());
    }
    if let Some(u) = filter.user {
        add("s.user_id = ?", u.into());
    }
    if let Some(from) = filter.from {
        add("s.opened_at >= ?", from.into());
    }
    if let Some(until) = filter.until {
        add("s.opened_at < ?", until.into());
    }
    if let Some(id) = filter.before_id {
        add("s.id < ?", id.into());
    }
    match filter.open {
        Some(true) => conditions.push("s.closed_at IS NULL".into()),
        Some(false) => conditions.push("s.closed_at IS NOT NULL".into()),
        None => {}
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let limit = if filter.limit == 0 { 100 } else { filter.limit };
    values.push(i64::from(limit).into());
    let sql = format!("{SESSION_SELECT} {where_clause} ORDER BY s.id DESC LIMIT ?");
    sql::fetch_all(conn.into(), &sql, values, map_record).await
}

/// Stores a close (normal or forced): `session` is the state after `CashierSession::close` or
/// `force_close`, `report` its close report, `counts` the optional per-denomination count.
/// Takes a transaction because it writes several rows.
///
/// # Errors
/// [`DbError::NotFound`] if the session does not exist or is already closed (a close cannot be
/// undone, spec 2.1 §5); [`DbError::Corrupt`] if `session` is still open.
pub async fn close(
    tx: &mut Tx,
    session: &CashierSession,
    report: &CloseReport,
    counts: &[DenominationCount],
) -> Result<()> {
    let SessionStatus::Closed { at, kind } = session.status() else {
        return Err(DbError::Corrupt(
            "close needs a closed cashier session".into(),
        ));
    };
    let (kind, forced_by) = match kind {
        CloseKind::Normal => ("normal", None),
        CloseKind::Forced { by } => ("forced", Some(by)),
    };
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE cashier_session SET closed_at = ?, close_kind = ?, forced_by = ?, \
         counted_cash = ?, expected_cash = ?, over_short = ?, transactions = ?, cash_sales = ?, \
         cash_refunds = ?, cash_in = ?, cash_out = ? WHERE id = ? AND closed_at IS NULL",
        args![
            at,
            kind,
            forced_by,
            report.counted_cash,
            report.expected_cash,
            report.over_short,
            i64::from(report.transactions),
            report.cash_sales,
            report.cash_refunds,
            report.cash_in,
            report.cash_out,
            session.id(),
        ],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::NotFound("open cashier session"));
    }
    for count in counts {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO cashier_session_count (session_id, denomination, quantity) \
             VALUES (?, ?, ?)",
            args![session.id(), count.denomination, i64::from(count.quantity)],
        )
        .await?;
    }
    for (tender, totals) in &report.tenders {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO cashier_session_tender (session_id, tender, sales, refunds, net, \
             tx_count) VALUES (?, ?, ?, ?, ?, ?)",
            args![
                session.id(),
                tender.as_str(),
                totals.sales,
                totals.refunds,
                totals.net,
                i64::from(totals.count),
            ],
        )
        .await?;
    }
    Ok(())
}

/// The stored report of a session; `None` while it is open or if it does not exist.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for an unknown stored tender.
pub async fn stored_close<'c>(
    conn: impl Into<Conn<'c>>,
    id: CashierSessionId,
) -> Result<Option<StoredClose>> {
    let mut conn = conn.into();
    let report = sql::fetch_optional(
        conn.reborrow(),
        "SELECT opening_cash, cash_sales, cash_refunds, cash_in, cash_out, counted_cash, \
         expected_cash, over_short, transactions FROM cashier_session \
         WHERE id = ? AND closed_at IS NOT NULL",
        args![id],
        |row| {
            Ok(CloseReport {
                opening_cash: sql::money(row, "opening_cash")?,
                cash_sales: sql::money(row, "cash_sales")?,
                cash_refunds: sql::money(row, "cash_refunds")?,
                cash_in: sql::money(row, "cash_in")?,
                cash_out: sql::money(row, "cash_out")?,
                expected_cash: sql::money(row, "expected_cash")?,
                counted_cash: sql::money(row, "counted_cash")?,
                over_short: sql::money(row, "over_short")?,
                tenders: BTreeMap::new(),
                transactions: count(row, "transactions")?,
            })
        },
    )
    .await?;
    let Some(mut report) = report else {
        return Ok(None);
    };
    let tenders = sql::fetch_all(
        conn.reborrow(),
        "SELECT tender, sales, refunds, net, tx_count FROM cashier_session_tender \
         WHERE session_id = ?",
        args![id],
        |row| {
            let tender: Tender = row
                .text("tender")?
                .parse()
                .map_err(|e| DbError::Corrupt(format!("{e}")))?;
            Ok((
                tender,
                TenderTotals {
                    sales: sql::money(row, "sales")?,
                    refunds: sql::money(row, "refunds")?,
                    net: sql::money(row, "net")?,
                    count: count(row, "tx_count")?,
                },
            ))
        },
    )
    .await?;
    report.tenders = tenders.into_iter().collect();
    let counts = sql::fetch_all(
        conn,
        "SELECT denomination, quantity FROM cashier_session_count WHERE session_id = ? \
         ORDER BY denomination DESC",
        args![id],
        |row| {
            Ok(DenominationCount {
                denomination: sql::money(row, "denomination")?,
                quantity: count(row, "quantity")?,
            })
        },
    )
    .await?;
    Ok(Some(StoredClose { report, counts }))
}

/// The names and settings a printed report of session `id` shows; `None` for an unknown session.
///
/// # Errors
/// The database error.
pub async fn report_context<'c>(
    conn: impl Into<Conn<'c>>,
    id: CashierSessionId,
) -> Result<Option<ReportContext>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT b.code AS branch_code, b.name AS branch_name, b.timezone, \
         r.code AS register_code, r.name AS register_name, u.display_name AS cashier_name, \
         f.display_name AS forced_by_name FROM cashier_session s \
         JOIN register r ON r.id = s.register_id JOIN branch b ON b.id = r.branch_id \
         JOIN app_user u ON u.id = s.user_id LEFT JOIN app_user f ON f.id = s.forced_by \
         WHERE s.id = ?",
        args![id],
        |row| {
            Ok(ReportContext {
                branch_code: row.text("branch_code")?,
                branch_name: row.text("branch_name")?,
                timezone: row.text("timezone")?,
                register_code: row.text("register_code")?,
                register_name: row.text("register_name")?,
                cashier_name: row.text("cashier_name")?,
                forced_by_name: row.opt_text("forced_by_name")?,
            })
        },
    )
    .await
}

fn count(row: &dyn Row, col: &str) -> Result<u32> {
    let value = row.i64(col)?;
    u32::try_from(value).map_err(|_| DbError::Corrupt(format!("`{col}` {value} out of range")))
}

fn map_record(row: &dyn Row) -> Result<SessionRecord> {
    let status = match (sql::opt_ts(row, "closed_at")?, row.opt_text("close_kind")?) {
        (None, _) => SessionStatus::Open,
        (Some(at), Some(kind)) if kind == "normal" => SessionStatus::Closed {
            at,
            kind: CloseKind::Normal,
        },
        (Some(at), Some(kind)) if kind == "forced" => SessionStatus::Closed {
            at,
            kind: CloseKind::Forced {
                by: sql::opt_id(row, "forced_by")?
                    .ok_or_else(|| DbError::Corrupt("forced close without forced_by".into()))?,
            },
        },
        (Some(_), kind) => {
            return Err(DbError::Corrupt(format!("invalid close_kind {kind:?}")));
        }
    };
    Ok(SessionRecord {
        session: CashierSession::from_parts(
            sql::id(row, "id")?,
            sql::id(row, "user_id")?,
            sql::id(row, "register_id")?,
            sql::money(row, "opening_cash")?,
            sql::ts(row, "opened_at")?,
            status,
        ),
        number: row.text("number")?,
        branch: sql::id(row, "branch_id")?,
        expected_at_close: row.opt_i64("expected_cash")?.map(Money::from_minor),
    })
}
