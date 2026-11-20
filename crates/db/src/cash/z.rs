//! The Z report (spec 2.7 §6, K13–K18): the sessions of a branch's business day
//! ([`day_sessions`], [`z_candidates`], [`open_day_sessions`]), the next Z number
//! ([`next_z_number`]), storing a Z and linking its sessions in one transaction
//! ([`insert_z_report`]), reading Z reports, and the days still to close ([`pending_days`], K18).
//!
//! A session's business day is the branch-local day of its open time, the `local_day` its number
//! was built from (K14, `core::cash::business_day`).

use serde_json::{Value, json};
use sultan_core::cash::{CashError, ZCandidate, ZReport};
use sultan_core::doc_number::{LocalDay, z_number};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, UserId, ZReportId};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use super::{Actor, CashDbError, CashResult, Change, clamp_limit, count, record};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// A session of a branch's business day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaySession {
    /// The session.
    pub id: CashierSessionId,
    /// Its number.
    pub number: String,
    /// The register.
    pub register: RegisterId,
    /// The register's code.
    pub register_code: String,
    /// The cashier.
    pub user: UserId,
    /// The cashier's display name.
    pub cashier_name: String,
    /// Opened at.
    pub opened_at: Timestamp,
    /// Closed at; `None` while open.
    pub closed_at: Option<Timestamp>,
    /// Force-closed (spec 2.1 §5).
    pub forced: bool,
    /// The business day (K14).
    pub business_day: LocalDay,
    /// The Z that covers it.
    pub z_report: Option<ZReportId>,
}

impl DaySession {
    /// Still open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }

    /// The session as `core::cash::check_z_run` sees it.
    #[must_use]
    pub fn candidate(&self) -> ZCandidate<'_> {
        ZCandidate {
            id: self.id,
            number: &self.number,
            business_day: self.business_day,
            open: self.is_open(),
            in_z: self.z_report.is_some(),
        }
    }
}

fn local_day(row: &dyn Row, col: &str) -> Result<LocalDay> {
    let key = row.i64(col)?;
    LocalDay::from_key(key).ok_or_else(|| DbError::Corrupt(format!("`{col}` {key} is not a day")))
}

const DAY_SELECT: &str = "SELECT s.id, s.number, s.register_id, r.code AS register_code, \
     s.user_id, u.display_name AS cashier_name, s.opened_at, s.closed_at, s.close_kind, \
     s.local_day, s.z_report_id FROM cashier_session s \
     JOIN register r ON r.id = s.register_id JOIN app_user u ON u.id = s.user_id \
     WHERE r.branch_id = ? AND s.local_day = ?";

fn map_day(row: &dyn Row) -> Result<DaySession> {
    Ok(DaySession {
        id: sql::id(row, "id")?,
        number: row.text("number")?,
        register: sql::id(row, "register_id")?,
        register_code: row.text("register_code")?,
        user: sql::id(row, "user_id")?,
        cashier_name: row.text("cashier_name")?,
        opened_at: sql::ts(row, "opened_at")?,
        closed_at: sql::opt_ts(row, "closed_at")?,
        forced: row.opt_text("close_kind")?.as_deref() == Some("forced"),
        business_day: local_day(row, "local_day")?,
        z_report: sql::opt_id(row, "z_report_id")?,
    })
}

async fn sessions_where(
    conn: Conn<'_>,
    branch: BranchId,
    day: LocalDay,
    condition: &str,
) -> Result<Vec<DaySession>> {
    let sql = format!("{DAY_SELECT}{condition} ORDER BY s.opened_at, s.id");
    sql::fetch_all(conn, &sql, args![branch, day.key()], map_day).await
}

/// Every session of `branch` whose business day is `day` (open, closed, in a Z or not), by open
/// time: the input of `core::cash::check_z_run` ([`DaySession::candidate`]).
///
/// # Errors
/// The database error.
pub async fn day_sessions<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    day: LocalDay,
) -> Result<Vec<DaySession>> {
    sessions_where(conn.into(), branch, day, "").await
}

/// The sessions a Z of `day` covers (§6): closed and in no Z yet, by open time.
///
/// # Errors
/// The database error.
pub async fn z_candidates<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    day: LocalDay,
) -> Result<Vec<DaySession>> {
    let condition = " AND s.closed_at IS NOT NULL AND s.z_report_id IS NULL";
    sessions_where(conn.into(), branch, day, condition).await
}

/// The sessions of `day` still open (K15: they block its Z), by open time.
///
/// # Errors
/// The database error.
pub async fn open_day_sessions<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    day: LocalDay,
) -> Result<Vec<DaySession>> {
    sessions_where(conn.into(), branch, day, " AND s.closed_at IS NULL").await
}

/// The number of the next Z of a branch's business day.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZNumber {
    /// The sequence within the day, from 1 (above 1 = supplementary, K16).
    pub seq: u32,
    /// `JKT01-Z-261119-01`.
    pub number: String,
}

/// The next Z number of `branch` for business `day` (K17), read in the Z's transaction; the
/// unique key `(branch_id, business_day, seq)` catches a race in [`insert_z_report`].
///
/// # Errors
/// [`CashDbError::NotFound`] (`branch`), or the database error.
pub async fn next_z_number(tx: &mut Tx, branch: BranchId, day: LocalDay) -> CashResult<ZNumber> {
    let code = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT code FROM branch WHERE id = ?",
        args![branch],
        |row| row.text("code"),
    )
    .await?
    .ok_or(CashDbError::NotFound("branch"))?;
    let last = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT MAX(seq) AS seq FROM z_report WHERE branch_id = ? AND business_day = ?",
        args![branch, day.key()],
        |row| row.opt_i64("seq"),
    )
    .await?
    .flatten()
    .unwrap_or(0);
    let seq = u32::try_from(last + 1)
        .map_err(|_| DbError::Corrupt(format!("Z sequence {last} out of range")))?;
    Ok(ZNumber {
        seq,
        number: z_number(&code, day, seq),
    })
}

/// A Z report to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewZReport {
    /// The ID.
    pub id: ZReportId,
    /// The branch.
    pub branch: BranchId,
    /// The report as `core::cash::build_z` built it, with the number of [`next_z_number`]; the
    /// headline columns come from its branch totals.
    pub report: ZReport,
    /// The sessions it covers ([`z_candidates`] after `core::cash::check_z_run`).
    pub sessions: Vec<CashierSessionId>,
    /// The whole report as JSON (the server's snapshot, printed and synced as it is).
    pub figures: Value,
    /// Who ran it.
    pub user: UserId,
    /// The manager who approved it at the till, if any.
    pub approved_by: Option<UserId>,
    /// The register it was run at (the till), if any.
    pub register: Option<RegisterId>,
}

/// A stored Z report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZReportRecord {
    /// The ID.
    pub id: ZReportId,
    /// `JKT01-Z-261119-01`.
    pub number: String,
    /// The branch.
    pub branch: BranchId,
    /// The business day.
    pub business_day: LocalDay,
    /// The sequence within the day.
    pub seq: u32,
    /// `seq > 1` (K16).
    pub supplementary: bool,
    /// Sessions covered.
    pub session_count: u32,
    /// Sales posted in them.
    pub sales_count: u32,
    /// Branch net sales.
    pub net_sales: Money,
    /// Σ expected cash.
    pub expected_cash: Money,
    /// Σ counted cash.
    pub counted_cash: Money,
    /// Σ over/short.
    pub over_short: Money,
    /// Σ drops + Σ to safe.
    pub cash_to_deposit: Money,
    /// The stored snapshot.
    pub figures: Value,
    /// When it was run.
    pub run_at: Timestamp,
    /// Who ran it.
    pub user: UserId,
    /// Who approved it.
    pub approved_by: Option<UserId>,
}

const Z_SELECT: &str = "SELECT id, number, branch_id, business_day, seq, supplementary, \
     session_count, sales_count, net_sales, expected_cash, counted_cash, over_short, \
     cash_to_deposit, figures, run_at, user_id, approved_by FROM z_report";

fn map_z(row: &dyn Row) -> Result<ZReportRecord> {
    Ok(ZReportRecord {
        id: sql::id(row, "id")?,
        number: row.text("number")?,
        branch: sql::id(row, "branch_id")?,
        business_day: local_day(row, "business_day")?,
        seq: count(row, "seq")?,
        supplementary: row.bool("supplementary")?,
        session_count: count(row, "session_count")?,
        sales_count: count(row, "sales_count")?,
        net_sales: sql::money(row, "net_sales")?,
        expected_cash: sql::money(row, "expected_cash")?,
        counted_cash: sql::money(row, "counted_cash")?,
        over_short: sql::money(row, "over_short")?,
        cash_to_deposit: sql::money(row, "cash_to_deposit")?,
        figures: sql::opt_json(row, "figures")?.unwrap_or(Value::Null),
        run_at: sql::ts(row, "run_at")?,
        user: sql::id(row, "user_id")?,
        approved_by: sql::opt_id(row, "approved_by")?,
    })
}

/// Stores a Z report in `tx` (`POST /z-reports`, §6) and marks each of its sessions with it,
/// with the audit row `z_report.post` and the outbox event `z_report.posted`. Run the checks
/// (`core::cash::check_z_run` on [`day_sessions`]) and [`next_z_number`] in the same
/// transaction.
///
/// # Errors
/// [`CashDbError::Rule`] `nothing_to_close` without sessions; a retryable [`DbError::Conflict`]
/// when another transaction took the number, or a session is no longer closed, of the day and
/// branch and in no Z (a lost race: the rerun reports the real refusal); or the database error.
pub async fn insert_z_report(
    tx: &mut Tx,
    new: &NewZReport,
    now: Timestamp,
) -> CashResult<ZReportRecord> {
    if new.sessions.is_empty() {
        return Err(CashError::NothingToClose.into());
    }
    let z = &new.report;
    let totals = &z.branch;
    let session_count = i64::try_from(new.sessions.len()).unwrap_or(i64::MAX);
    sql::execute(
        Conn::Tx(&mut *tx),
        "INSERT INTO z_report (id, number, branch_id, business_day, seq, supplementary, \
         session_count, sales_count, net_sales, expected_cash, counted_cash, over_short, \
         cash_to_deposit, figures, run_at, user_id, approved_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            new.id,
            z.number.as_str(),
            new.branch,
            z.day.key(),
            i64::from(z.seq),
            z.seq > 1,
            session_count,
            i64::from(totals.sales_count),
            totals.net_sales,
            totals.figures.expected_cash(),
            totals.counted_cash,
            totals.over_short,
            totals.cash_to_deposit,
            new.figures.to_string(),
            z.run_at,
            new.user,
            new.approved_by,
            now,
        ],
    )
    .await?;
    for session in &new.sessions {
        let linked = sql::execute(
            Conn::Tx(&mut *tx),
            "UPDATE cashier_session SET z_report_id = ? WHERE id = ? AND z_report_id IS NULL \
             AND closed_at IS NOT NULL AND local_day = ? \
             AND register_id IN (SELECT id FROM register WHERE branch_id = ?)",
            args![new.id, *session, z.day.key(), new.branch],
        )
        .await?;
        if linked == 0 {
            return Err(DbError::Conflict(format!(
                "cashier session {session} is not a closed session of the day without a Z"
            ))
            .into());
        }
    }
    let payload = json!({
        "id": new.id.to_string(),
        "number": z.number,
        "branch_id": new.branch.to_string(),
        "business_day": z.day.key(),
        "seq": z.seq,
        "supplementary": z.seq > 1,
        "run_at": z.run_at.unix_millis(),
        "user_id": new.user.to_string(),
        "approved_by": new.approved_by.map(|u| u.to_string()),
        "sessions": new.sessions.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "figures": new.figures,
    });
    let actor = Actor {
        user: Some(new.user),
        branch: Some(new.branch),
        register: new.register,
    };
    record(
        tx,
        &actor,
        now,
        Change {
            action: "z_report.post",
            target: format!("z_report:{}", new.id),
            before: None,
            after: json!({
                "number": z.number,
                "business_day": z.day.key(),
                "sessions": new.sessions.len(),
                "cash_to_deposit": totals.cash_to_deposit.minor(),
                "approved_by": new.approved_by.map(|u| u.to_string()),
            }),
            outbox: ("z_report.posted", &payload),
        },
    )
    .await?;
    get_z_report(Conn::Tx(&mut *tx), new.id)
        .await?
        .ok_or(CashDbError::NotFound("z report"))
}

/// One Z report.
///
/// # Errors
/// The database error.
pub async fn get_z_report<'c>(
    conn: impl Into<Conn<'c>>,
    id: ZReportId,
) -> Result<Option<ZReportRecord>> {
    let sql = format!("{Z_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_z).await
}

/// The sessions a Z covers, by open time.
///
/// # Errors
/// The database error.
pub async fn z_report_sessions<'c>(
    conn: impl Into<Conn<'c>>,
    id: ZReportId,
) -> Result<Vec<CashierSessionId>> {
    sql::fetch_all(
        conn.into(),
        "SELECT id FROM cashier_session WHERE z_report_id = ? ORDER BY opened_at, id",
        args![id],
        |row| sql::id(row, "id"),
    )
    .await
}

/// Filters for [`list_z_reports`]; newest first by ID (keyset paging: `before`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZFilter {
    /// Only this branch.
    pub branch: Option<BranchId>,
    /// Business day on or after.
    pub from_day: Option<LocalDay>,
    /// Business day on or before.
    pub until_day: Option<LocalDay>,
    /// Page cursor.
    pub before: Option<ZReportId>,
    /// Page size; `0` = 50, at most [`super::MAX_CASH_PAGE`].
    pub limit: u32,
}

/// Z reports matching `filter`, newest first (`GET /z-reports`).
///
/// # Errors
/// The database error.
pub async fn list_z_reports<'c>(
    conn: impl Into<Conn<'c>>,
    filter: &ZFilter,
) -> Result<Vec<ZReportRecord>> {
    let mut conditions: Vec<&str> = Vec::new();
    let mut values: Vec<Arg> = Vec::new();
    if let Some(b) = filter.branch {
        conditions.push("branch_id = ?");
        values.push(b.into());
    }
    if let Some(d) = filter.from_day {
        conditions.push("business_day >= ?");
        values.push(d.key().into());
    }
    if let Some(d) = filter.until_day {
        conditions.push("business_day <= ?");
        values.push(d.key().into());
    }
    if let Some(id) = filter.before {
        conditions.push("id < ?");
        values.push(id.into());
    }
    let clause = if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    };
    values.push(clamp_limit(filter.limit, 50, super::MAX_CASH_PAGE).into());
    let sql = format!("{Z_SELECT}{clause} ORDER BY id DESC LIMIT ?");
    sql::fetch_all(conn.into(), &sql, values, map_z).await
}

/// A business day still to close (K18).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingDay {
    /// The business day.
    pub day: LocalDay,
    /// Its sessions still open.
    pub open: u32,
    /// Its closed sessions in no Z yet.
    pub closed: u32,
}

/// The days of `branch` still to close (`GET /z-reports/pending`, K18), oldest first: days with
/// a closed session in no Z, and days before `today` (the branch-local date) with a session still
/// open. Today with only open sessions is not listed. With `since` (the branch-local day of
/// [`super::cash_since`], spec 2.7 §17 reading 10) earlier business days are left out.
///
/// # Errors
/// The database error.
pub async fn pending_days<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    today: LocalDay,
    since: Option<LocalDay>,
) -> Result<Vec<PendingDay>> {
    let since = since.map_or(0, LocalDay::key);
    sql::fetch_all(
        conn.into(),
        "SELECT s.local_day, \
         CAST(SUM(CASE WHEN s.closed_at IS NULL THEN 1 ELSE 0 END) AS SIGNED) AS open_count, \
         CAST(SUM(CASE WHEN s.closed_at IS NULL THEN 0 ELSE 1 END) AS SIGNED) AS closed_count \
         FROM cashier_session s JOIN register r ON r.id = s.register_id \
         WHERE r.branch_id = ? AND s.z_report_id IS NULL AND s.local_day >= ? \
         GROUP BY s.local_day \
         HAVING SUM(CASE WHEN s.closed_at IS NULL THEN 0 ELSE 1 END) > 0 OR s.local_day < ? \
         ORDER BY s.local_day",
        args![branch, since, today.key()],
        |row| {
            Ok(PendingDay {
                day: local_day(row, "local_day")?,
                open: count(row, "open_count")?,
                closed: count(row, "closed_count")?,
            })
        },
    )
    .await
}
