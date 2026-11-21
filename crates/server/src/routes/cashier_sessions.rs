//! Cashier sessions: whose money is in the drawer (spec 2.1 §5, plan Phases 6 and 9).
//!
//! - `POST /cashier-sessions {opening_cash}` → `201`: needs a PIN session at the register
//!   ([`RegisterSession`]) and `session.open_close`. At most one open session per register and
//!   one per user (`core::cashier_session`; a database index is the safety net):
//!   `409 session_already_open` with `conflict` (`register` / `user`) and `session_id`.
//!   The session gets its **number** `<BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>` in the same
//!   transaction: the branch-local open date and the register's sequence of that day
//!   (`sultan_db::cashier_sessions::insert`). A lost race on the number or the open-session index
//!   reruns the whole transaction with a short jittered backoff ([`crate::retry::run`], at most
//!   [`crate::retry::ATTEMPTS`] times), so the loser gets the proper `409` from the `core` check
//!   instead of a database error; if every attempt still lost a race the answer is `409` too
//!   (`session_already_open` after a unique-key race, `conflict` otherwise), never `500`.
//! - `GET /cashier-sessions/current`: the open session on this register (whoever it belongs to,
//!   so the lock screen can show it), or `404 not_found`.
//! - `POST /cashier-sessions/{id}/close {counted_cash, counts?}` → the **close report**. A blind
//!   close: the cashier enters the count first; only the report shows expected cash and over/short.
//!   Only the session's own cashier, at its register, with `session.open_close`
//!   (`403 permission_denied` otherwise); a closed session is `409 session_closed`.
//! - `POST /cashier-sessions/{id}/force-close {counted_cash, counts?}` → the close report marked
//!   `forced`: a manager closes an abandoned session. Needs `session.force_close` **at the
//!   session's branch**; works from a register or from back office.
//! - `GET /cashier-sessions/{id}` (Phase 9): the session and, once closed, its **stored** close
//!   report, exactly as the close returned it (plus the names a printout needs). Reprinting a
//!   close report (spec 2.1 §5, later) uses this endpoint.
//! - `GET /cashier-sessions` (Phase 9): a page of sessions, newest first. Filters `branch_id`,
//!   `register_id`, `user_id`, `status` (`open` / `closed`), `from` / `until` (open time, unix
//!   ms, `from` inclusive, `until` exclusive); paging like `GET /audit`: `limit` (default 100, at
//!   most 500) and `before_id` = the previous page's `next_before_id`.
//!
//! **Who may read a session** (Phase 9): its own cashier always; anyone else needs
//! `report.sales` **at the session's branch** (close reports are cash reports; seeded for
//! Manager and Viewer). `GET /{id}` answers `403 permission_denied` otherwise; the list only
//! contains the sessions the caller may read.
//!
//! **Blind close** (spec 2.1 §5, §9 case 16): while a session is open, its responses carry
//! `expected_cash` only when the caller holds `session.view_expected` at the session's branch;
//! otherwise the field is absent. A closed session's report shows everything to anyone who may
//! read it.
//!
//! `counts` is the optional count per denomination; when given, its total must equal
//! `counted_cash`. Expected cash and the per-tender totals come from the sales seam
//! ([`crate::movements`]): sales, returns, post-voids, cash in / out / drops and payouts.
//!
//! **Cash management** (spec 2.7 §3, §4, phase K3): the close and the force close take an optional
//! `float_kept` (0 up to the count, default the opening cash capped at the count; `422
//! invalid_field` otherwise), stored with the close-time missing-nota count. The close report adds
//! the `cash` block (per kind and reason, reversals, drawer opens, X reads, missing notas) and the
//! `float` block (kept, to the safe, the proposed opening, the float difference) next to the 2.1
//! block and the `sales` block ([`super::cash::session_figures`]). An open stores
//! `opening_proposed` when the typed opening cash differs from the float the register's last
//! session kept (spec 2.7 §18 reading 2); sessions show `opening_proposed` and `float_kept`.
//!
//! Every open, close and force close is audited (`cashier_session.open`, `.close`,
//! `.force_close`, spec 2.1 §7) and pushed as `cashier_session.opened` / `.closed` to the
//! branch's event sockets. The `POST`s accept `Idempotency-Key`, so a retried open returns the
//! original `201` instead of `409`.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sultan_core::access::{AccessError, BranchScope, Permission};
use sultan_core::cash::{SessionFigures, float_difference};
use sultan_core::cashier_session::{
    CashierSession, CloseKind, CloseReport, SessionError, SessionStatus, close_report,
};
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, UserId};
use sultan_core::money::Money;
use sultan_core::sale::{CountAmount, SessionEvent};
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::cash::{self as cash_db, SessionCashState};
use sultan_db::cashier_sessions::{
    self, DenominationCount, ReportContext, SessionFilter, SessionRecord, Visibility,
};
use sultan_db::{DbError, Tx};
use utoipa::{IntoParams, ToSchema};

use super::cash::{CashReportDto, session_figures};
use super::{parse_id, parse_opt_id, to_json};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, RegisterSession, UserSession};
use crate::retry::{self, Failure};
use crate::state::AppState;

/// Default page size of `GET /cashier-sessions`.
pub const DEFAULT_LIMIT: u32 = 100;
/// Largest page size of `GET /cashier-sessions`.
pub const MAX_LIMIT: u32 = 500;
/// Who may read other cashiers' sessions (see the module docs).
pub const READ_PERMISSION: Permission = Permission::ReportSales;

/// Body of `POST /cashier-sessions`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OpenSession {
    /// Cash in the drawer at open, in minor units (≥ 0).
    pub opening_cash: i64,
}

/// Notes or coins of one denomination counted at close.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CountDto {
    /// Face value in minor units (> 0), e.g. `50000`.
    pub denomination: i64,
    /// Number counted.
    pub quantity: u32,
}

/// Body of `close` and `force-close`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CloseSession {
    /// Cash counted in the drawer, in minor units (≥ 0).
    pub counted_cash: i64,
    /// Optional count per denomination; its total must equal `counted_cash`.
    #[serde(default)]
    pub counts: Option<Vec<CountDto>>,
    /// Cash kept in the drawer as the next session's float (spec 2.7 §3): 0 up to the counted
    /// cash; default the opening cash (at most the counted cash). The rest goes to the safe.
    #[serde(default)]
    pub float_kept: Option<i64>,
}

/// A cashier session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SessionDto {
    /// Session ID.
    pub id: String,
    /// Session number: `<BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>`, e.g. `JKT01-R2-S-261107-01`. The
    /// date is the branch-local open date; `NN` counts per register and local day.
    pub number: String,
    /// The cashier.
    pub user_id: String,
    /// The register.
    pub register_id: String,
    /// The register's branch.
    pub branch_id: String,
    /// Cash at open (minor units).
    pub opening_cash: i64,
    /// Open time (unix ms).
    pub opened_at: i64,
    /// `open` or `closed`.
    pub status: String,
    /// Close time, once closed.
    pub closed_at: Option<i64>,
    /// `normal` or `forced`, once closed.
    pub close_kind: Option<String>,
    /// The manager who force-closed it.
    pub forced_by: Option<String>,
    /// Expected cash in the drawer: the live figure while open, **only for callers holding
    /// `session.view_expected`** (blind close, spec 2.1 §5); the figure of the close once closed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_cash: Option<i64>,
    /// The proposed opening cash (the float kept by the register's last session), stored when the
    /// typed opening cash differed from it (spec 2.7 §3).
    pub opening_proposed: Option<i64>,
    /// The cash kept as float at the close (spec 2.7 §3); `null` while open or before 2.7.
    pub float_kept: Option<i64>,
}

/// Totals of one tender in the close report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TenderDto {
    /// `cash`, `card`, `qris` or `transfer`.
    pub tender: String,
    /// Sum of sales.
    pub sales: i64,
    /// Sum of refunds.
    pub refunds: i64,
    /// Sales minus refunds.
    pub net: i64,
    /// Number of sales and refunds.
    pub count: u32,
}

/// The close report (spec 2.1 §5). Amounts in minor units. Everything a printed report needs, so
/// a reprint (later) can use `GET /cashier-sessions/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CloseReportDto {
    /// The closed session (with its number, open and close times).
    pub session: SessionDto,
    /// Whether a manager force-closed it.
    pub forced: bool,
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
    /// Display name of the manager who force-closed it, if forced.
    pub forced_by_name: Option<String>,
    /// Cash at open.
    pub opening_cash: i64,
    /// Cash sales.
    pub cash_sales: i64,
    /// Cash refunds.
    pub cash_refunds: i64,
    /// Petty cash in.
    pub cash_in: i64,
    /// Petty cash out.
    pub cash_out: i64,
    /// `opening_cash + cash_sales − cash_refunds + cash_in − cash_out`.
    pub expected_cash: i64,
    /// What was counted.
    pub counted_cash: i64,
    /// `counted_cash − expected_cash`: positive = over, negative = short.
    pub over_short: i64,
    /// Number of sales and refunds.
    pub transactions: u32,
    /// Totals per tender that had a sale or refund.
    pub tenders: Vec<TenderDto>,
    /// The count per denomination, if entered (largest first).
    pub counts: Vec<CountDto>,
    /// The sales figures (spec 2.6 §10), computed from the session's documents, which never
    /// change after the close.
    pub sales: SalesReportDto,
    /// The cash movements per kind and reason, drawer opens, X reads and the cash outs missing a
    /// nota at the close (spec 2.7 §4).
    pub cash: super::cash::CashReportDto,
    /// The float kept, to the safe, the proposed opening and the float difference (spec 2.7 §3).
    pub float: FloatDto,
}

/// The float of a session (spec 2.7 §3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct FloatDto {
    /// Cash kept in the drawer for the next session; `null` for a session closed before 2.7.
    pub float_kept: Option<i64>,
    /// `counted − float_kept`, taken to the safe; `null` with `float_kept`.
    pub to_safe: Option<i64>,
    /// The proposed opening cash, when the cashier typed another amount at open.
    pub opening_proposed: Option<i64>,
    /// `opening_cash − opening_proposed` (0 without a proposal).
    pub float_difference: i64,
}

/// A count and an amount.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CountAmountDto {
    /// How many.
    pub count: u32,
    /// The sum, minor units.
    pub amount: i64,
}

impl From<CountAmount> for CountAmountDto {
    fn from(c: CountAmount) -> Self {
        Self {
            count: c.count,
            amount: c.amount.minor(),
        }
    }
}

/// Sales and refunds of one tender type in the sales figures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SalesTenderDto {
    /// The tender type (`null` for the exchange tender).
    pub tender_type_id: Option<String>,
    /// Its kind.
    pub kind: String,
    /// Sale tenders.
    pub sales: CountAmountDto,
    /// Refunds paid with it (returns and post-voids).
    pub refunds: CountAmountDto,
}

/// The session's sales figures (spec 2.6 §10, `core::sale::session_report`, with the cash
/// movements of spec 2.7 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SalesReportDto {
    /// Cash at open.
    #[serde(default)]
    pub opening_cash: i64,
    /// Σ cash received on sale tenders.
    pub cash_received: i64,
    /// Σ change handed back.
    pub change_given: i64,
    /// Σ cash paid out for returns and post-voids.
    pub cash_refunds: i64,
    /// Σ cash rounding of the documents (already inside the cash figures).
    pub cash_rounding: i64,
    /// Petty cash in.
    pub cash_in: i64,
    /// Cash out (net of reversals, spec 2.7).
    pub cash_out: i64,
    /// Safe drops (net of reversals, spec 2.7 §4).
    #[serde(default)]
    pub drops: i64,
    /// Customer balance payouts.
    pub payouts: i64,
    /// `opening_cash + cash_received − change_given − cash_refunds + cash_in − cash_out − drops −
    /// payouts` (spec 2.7 §4).
    pub expected_cash: i64,
    /// Per tender type.
    pub tenders: Vec<SalesTenderDto>,
    /// Removed cart lines.
    pub line_voids: CountAmountDto,
    /// Cleared carts.
    pub cart_voids: CountAmountDto,
    /// Post-voids paid in this session.
    pub post_voids: CountAmountDto,
    /// Returns paid in this session.
    pub returns: CountAmountDto,
    /// Line discounts.
    pub line_discounts: CountAmountDto,
    /// Cart discounts.
    pub cart_discounts: CountAmountDto,
    /// Price overrides (how much they lowered the lines).
    pub price_overrides: CountAmountDto,
    /// Change saved to customer balances.
    pub saved_change: i64,
    /// Paid with customer balances.
    pub balance_paid: i64,
}

/// The line and cart voids of `session` (audit entries of `POST /cart-voids`).
pub(crate) async fn till_voids(
    tx: &mut Tx,
    session: CashierSessionId,
    events: &mut Vec<SessionEvent>,
) -> Result<(), ApiError> {
    const PAGE: u32 = 500;
    for (action, line) in [
        (super::sales::LINE_VOID, true),
        (super::sales::CART_VOID, false),
    ] {
        let mut before_id = None;
        loop {
            let filter = audit::AuditFilter {
                action: Some(action.to_owned()),
                target: Some(format!("cashier_session:{session}")),
                before_id,
                limit: PAGE,
                ..audit::AuditFilter::default()
            };
            let page = audit::query(&mut *tx, &filter).await?;
            for entry in &page {
                let amount = Money::from_minor(
                    entry
                        .after
                        .as_ref()
                        .and_then(|a| a.get("amount"))
                        .and_then(Value::as_i64)
                        .unwrap_or_default(),
                );
                events.push(if line {
                    SessionEvent::LineVoid(amount)
                } else {
                    SessionEvent::CartVoid(amount)
                });
            }
            if page.len() < PAGE as usize {
                break;
            }
            before_id = page.last().map(|e| e.id);
        }
    }
    Ok(())
}

/// `GET /cashier-sessions/{id}`: the session and, once closed, its stored close report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SessionDetailDto {
    /// The session (blind-close rule for `expected_cash` while open).
    pub session: SessionDto,
    /// The close report as returned by the close; `null` while open.
    pub close_report: Option<CloseReportDto>,
}

/// Query parameters of `GET /cashier-sessions`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct SessionQuery {
    /// Only this branch.
    pub branch_id: Option<String>,
    /// Only this register.
    pub register_id: Option<String>,
    /// Only this cashier.
    pub user_id: Option<String>,
    /// `open` or `closed`.
    pub status: Option<String>,
    /// Opened at or after this time (unix ms).
    pub from: Option<i64>,
    /// Opened before this time (unix ms).
    pub until: Option<i64>,
    /// Page cursor: `next_before_id` of the previous page.
    pub before_id: Option<String>,
    /// Page size, 1–500 (default 100).
    pub limit: Option<u32>,
}

/// One page of `GET /cashier-sessions`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SessionPage {
    /// Sessions, newest first.
    pub items: Vec<SessionDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

fn session_dto(record: &SessionRecord, expected: Option<Money>) -> SessionDto {
    let session = &record.session;
    let (status, closed_at, close_kind, forced_by) = match session.status() {
        SessionStatus::Open => ("open", None, None, None),
        SessionStatus::Closed { at, kind } => {
            let (kind, by) = match kind {
                CloseKind::Normal => ("normal", None),
                CloseKind::Forced { by } => ("forced", Some(by.to_string())),
            };
            ("closed", Some(at.unix_millis()), Some(kind.to_owned()), by)
        }
    };
    SessionDto {
        id: session.id().to_string(),
        number: record.number.clone(),
        user_id: session.user().to_string(),
        register_id: session.register().to_string(),
        branch_id: record.branch.to_string(),
        opening_cash: session.opening_cash().minor(),
        opened_at: session.opened_at().unix_millis(),
        status: status.to_owned(),
        closed_at,
        close_kind,
        forced_by,
        expected_cash: expected.map(Money::minor),
        opening_proposed: None,
        float_kept: None,
    }
}

/// The float of a closed session from its stored columns (spec 2.7 §3).
fn float_dto(state: &SessionCashState, opening: Money, counted: Money) -> FloatDto {
    FloatDto {
        float_kept: state.float_kept.map(Money::minor),
        to_safe: state
            .float_kept
            .map(|kept| counted.saturating_sub(kept).minor()),
        opening_proposed: state.opening_proposed.map(Money::minor),
        float_difference: float_difference(opening, state.opening_proposed)
            .unwrap_or_default()
            .minor(),
    }
}

/// The parts of a close report besides the stored 2.1 block.
struct CloseExtras {
    figures: SessionFigures,
    state: SessionCashState,
}

fn report_dto(
    record: &SessionRecord,
    context: ReportContext,
    report: &CloseReport,
    counts: &[DenominationCount],
    extras: &CloseExtras,
) -> CloseReportDto {
    let mut session = session_dto(record, Some(report.expected_cash));
    session.opening_proposed = extras.state.opening_proposed.map(Money::minor);
    session.float_kept = extras.state.float_kept.map(Money::minor);
    CloseReportDto {
        sales: SalesReportDto::from(&extras.figures.sales),
        cash: CashReportDto::from(&extras.figures),
        float: float_dto(&extras.state, report.opening_cash, report.counted_cash),
        session,
        forced: matches!(
            record.session.status(),
            SessionStatus::Closed {
                kind: CloseKind::Forced { .. },
                ..
            }
        ),
        branch_code: context.branch_code,
        branch_name: context.branch_name,
        timezone: context.timezone,
        register_code: context.register_code,
        register_name: context.register_name,
        cashier_name: context.cashier_name,
        forced_by_name: context.forced_by_name,
        opening_cash: report.opening_cash.minor(),
        cash_sales: report.cash_sales.minor(),
        cash_refunds: report.cash_refunds.minor(),
        cash_in: report.cash_in.minor(),
        cash_out: report.cash_out.minor(),
        expected_cash: report.expected_cash.minor(),
        counted_cash: report.counted_cash.minor(),
        over_short: report.over_short.minor(),
        transactions: report.transactions,
        tenders: report
            .tenders
            .iter()
            .map(|(tender, t)| TenderDto {
                tender: tender.as_str().to_owned(),
                sales: t.sales.minor(),
                refunds: t.refunds.minor(),
                net: t.net.minor(),
                count: t.count,
            })
            .collect(),
        counts: counts
            .iter()
            .map(|c| CountDto {
                denomination: c.denomination.minor(),
                quantity: c.quantity,
            })
            .collect(),
    }
}

/// The float kept at a close (spec 2.7 §3, `core::cash::float_kept`): the typed one, or the
/// opening cash capped at the count.
fn float_kept(opening: Money, counted: Money, typed: Option<i64>) -> Result<Money, ApiError> {
    Ok(sultan_core::cash::float_kept(
        opening,
        counted,
        typed.map(Money::from_minor),
    )?)
}

/// A non-negative amount field.
fn amount(field: &str, value: i64) -> Result<Money, ApiError> {
    if value < 0 {
        return Err(ApiError::invalid_field(
            field,
            format!("`{field}` must not be negative"),
        ));
    }
    Ok(Money::from_minor(value))
}

/// Validates the close body: a non-negative count, and per-denomination counts (positive,
/// distinct denominations) that add up to it. Returns them largest first.
fn parse_close(body: &CloseSession) -> Result<(Money, Vec<DenominationCount>), ApiError> {
    let counted = amount("counted_cash", body.counted_cash)?;
    let Some(counts) = &body.counts else {
        return Ok((counted, Vec::new()));
    };
    let invalid = |detail: &str| ApiError::invalid_field("counts", detail);
    let mut seen = BTreeSet::new();
    let mut total = Some(0_i64);
    let mut parsed = Vec::with_capacity(counts.len());
    for c in counts {
        if c.denomination <= 0 {
            return Err(invalid("a denomination must be positive"));
        }
        if !seen.insert(c.denomination) {
            return Err(invalid("a denomination is listed twice"));
        }
        total =
            total.and_then(|t| t.checked_add(c.denomination.checked_mul(i64::from(c.quantity))?));
        parsed.push(DenominationCount {
            denomination: Money::from_minor(c.denomination),
            quantity: c.quantity,
        });
    }
    if total != Some(counted.minor()) {
        return Err(invalid("the counts do not add up to `counted_cash`"));
    }
    parsed.sort_by_key(|c| std::cmp::Reverse(c.denomination));
    Ok((counted, parsed))
}

/// Expected cash of an open session right now (for `session.view_expected` holders).
async fn expected_now(
    state: &AppState,
    tx: &mut Tx,
    session: &CashierSession,
) -> Result<Money, ApiError> {
    let movements = crate::movements::session_movements(state, tx, session.id()).await?;
    Ok(close_report(session.opening_cash(), Money::ZERO, &movements)?.expected_cash)
}

/// The session DTO as `caller` may see it: once closed, the expected cash of the close; while
/// open, the live expected cash only with `session.view_expected` at the session's branch.
async fn visible_dto(
    state: &AppState,
    tx: &mut Tx,
    caller: &Caller,
    record: &SessionRecord,
) -> Result<SessionDto, ApiError> {
    let expected = if !record.session.is_open() {
        record.expected_at_close
    } else if caller
        .at_scope(BranchScope::Branch(record.branch))
        .contains(Permission::SessionViewExpected)
    {
        Some(expected_now(state, tx, &record.session).await?)
    } else {
        None
    };
    let mut dto = session_dto(record, expected);
    if let Some(cash) = cash_db::session_cash_state(&mut *tx, record.session.id()).await? {
        dto.opening_proposed = cash.opening_proposed.map(Money::minor);
        dto.float_kept = cash.float_kept.map(Money::minor);
    }
    Ok(dto)
}

/// Whether `caller` (logged in as `user`) may read `record` (see the module docs).
fn may_read(caller: &Caller, user: UserId, record: &SessionRecord) -> bool {
    record.session.user() == user
        || caller
            .at_scope(BranchScope::Branch(record.branch))
            .contains(READ_PERMISSION)
}

fn audit(
    user: &UserSession,
    now: Timestamp,
    action: &str,
    record: &SessionRecord,
    before: Option<Value>,
    after: Value,
) -> AuditEntry {
    let mut entry = AuditEntry::new(now, action);
    entry.branch = Some(record.branch);
    entry.register = user.session.register.or(Some(record.session.register()));
    entry.user = Some(user.user.id);
    entry.target = Some(format!("cashier_session:{}", record.session.id()));
    entry.before = before;
    entry.after = Some(after);
    entry
}

fn event_data(record: &SessionRecord) -> Value {
    json!({
        "session_id": record.session.id().to_string(),
        "number": record.number,
        "register_id": record.session.register().to_string(),
        "branch_id": record.branch.to_string(),
        "user_id": record.session.user().to_string(),
    })
}

/// The outcome of one open attempt: run again after a lost race, or answer.
#[derive(Debug)]
enum Attempt {
    Retry(DbError),
    Fail(ApiError),
}

impl Attempt {
    /// The answer once no more attempts are made. A race still lost is a `409`: the open-session
    /// index is the safety net (spec 2.1 §5), anything else (a busy database) is `conflict`.
    fn into_answer(self) -> ApiError {
        match self {
            Self::Fail(error) => error,
            Self::Retry(DbError::Conflict(_)) => ApiError::new(
                StatusCode::CONFLICT,
                "session_already_open",
                "a cashier session is already open on this register or for this user",
            ),
            Self::Retry(_) => retry::conflict(),
        }
    }
}

impl From<ApiError> for Attempt {
    fn from(error: ApiError) -> Self {
        Self::Fail(error)
    }
}

impl From<DbError> for Attempt {
    fn from(error: DbError) -> Self {
        if error.is_retryable() {
            Self::Retry(error)
        } else {
            Self::Fail(error.into())
        }
    }
}

impl From<SessionError> for Attempt {
    fn from(error: SessionError) -> Self {
        Self::Fail(error.into())
    }
}

/// One open transaction: the `core` check, the insert with its number, the audit entry.
async fn try_open(
    state: &AppState,
    caller: &RegisterSession,
    access: &Caller,
    opening: Money,
    now: Timestamp,
) -> Result<(SessionDto, SessionRecord), Attempt> {
    let user = caller.user.user.id;
    let register = caller.register.register;
    let mut tx = state.db.begin().await?;
    let existing = cashier_sessions::open_conflicts(&mut tx, user, register).await?;
    let session = CashierSession::open(
        CashierSessionId::generate(),
        user,
        register,
        opening,
        now,
        &existing,
    )?;
    // No proposal without a float kept by the register's last session (spec 2.7 §18).
    let last = cash_db::last_float_kept(&mut tx, register).await?;
    let number = cashier_sessions::insert(&mut tx, &session).await?;
    let proposed = last.and_then(|kept| {
        sultan_core::cash::opening_proposed(sultan_core::cash::proposed_float(Some(kept)), opening)
    });
    if let Some(p) = proposed {
        cash_db::set_opening_proposed(&mut tx, session.id(), p).await?;
    }
    let record = SessionRecord {
        session,
        number,
        branch: caller.register.branch,
        expected_at_close: None,
    };
    let dto = visible_dto(state, &mut tx, access, &record).await?;
    let mut after = session_dto(&record, None);
    after.opening_proposed = proposed.map(Money::minor);
    let mut after = to_json(&after);
    after["float_difference"] = json!(
        float_difference(opening, proposed)
            .unwrap_or_default()
            .minor()
    );
    let entry = audit(
        &caller.user,
        now,
        "cashier_session.open",
        &record,
        None,
        after,
    );
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok((dto, record))
}

/// `POST /api/v1/cashier-sessions` → `201` (spec 2.1 §5).
///
/// # Errors
/// `401`, `403 register_session_required` / `permission_denied`, `409 session_already_open`,
/// `422 invalid_field`.
#[utoipa::path(post, path = "/api/v1/cashier-sessions", tag = "cashier",
    security(("bearer" = [], "register_token" = [])),
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = OpenSession,
    responses((status = 201, body = SessionDto), (status = 403, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn open(
    State(state): State<AppState>,
    caller: RegisterSession,
    ApiJson(body): ApiJson<OpenSession>,
) -> Result<(StatusCode, Json<SessionDto>), ApiError> {
    let access = Caller::load(&state.db, &caller.user).await?;
    access.require(Permission::SessionOpenClose)?;
    let opening = amount("opening_cash", body.opening_cash)?;
    let now = state.now();
    let (dto, record) = retry::run(
        "cashier session open",
        || try_open(&state, &caller, &access, opening, now),
        |attempt| matches!(attempt, Attempt::Retry(_)),
    )
    .await
    .map_err(|failure| match failure {
        Failure::Final(attempt) | Failure::Exhausted(attempt) => attempt.into_answer(),
    })?;
    state.events.publish(Event::for_branch(
        "cashier_session.opened",
        record.branch,
        event_data(&record),
    ));
    Ok((StatusCode::CREATED, Json(dto)))
}

/// `GET /api/v1/cashier-sessions/current`: the open session on this register.
///
/// # Errors
/// `401`, `403 register_session_required`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/cashier-sessions/current", tag = "cashier",
    security(("bearer" = [], "register_token" = [])),
    responses((status = 200, body = SessionDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn current(
    State(state): State<AppState>,
    caller: RegisterSession,
) -> Result<Json<SessionDto>, ApiError> {
    let access = Caller::load(&state.db, &caller.user).await?;
    let mut tx = state.db.begin_read().await?;
    let record = cashier_sessions::open_for_register(&mut tx, caller.register.register)
        .await?
        .ok_or_else(|| ApiError::not_found("open cashier session"))?;
    let dto = visible_dto(&state, &mut tx, &access, &record).await?;
    tx.commit().await?;
    Ok(Json(dto))
}

/// `GET /api/v1/cashier-sessions/{id}`: the session and, once closed, its stored close report
/// (reprint, spec 2.1 §5). Its own cashier, or `report.sales` at the session's branch.
///
/// # Errors
/// `401`, `400 invalid_body` (bad ID), `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/cashier-sessions/{id}", tag = "cashier",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cashier session ID")),
    responses((status = 200, body = SessionDetailDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
) -> Result<Json<SessionDetailDto>, ApiError> {
    let id: CashierSessionId = parse_id(&id, "id")?;
    let access = Caller::load(&state.db, &caller).await?;
    let mut tx = state.db.begin_read().await?;
    let record = cashier_sessions::get(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    if !may_read(&access, caller.user.id, &record) {
        return Err(AccessError::PermissionDenied {
            permission: READ_PERMISSION,
        }
        .into());
    }
    let session = visible_dto(&state, &mut tx, &access, &record).await?;
    let close_report = match cashier_sessions::stored_close(&mut tx, id).await? {
        None => None,
        Some(stored) => {
            let context = cashier_sessions::report_context(&mut tx, id)
                .await?
                .ok_or_else(|| ApiError::not_found("cashier session"))?;
            let extras = stored_extras(&mut tx, &record).await?;
            Some(report_dto(
                &record,
                context,
                &stored.report,
                &stored.counts,
                &extras,
            ))
        }
    };
    tx.commit().await?;
    Ok(Json(SessionDetailDto {
        session,
        close_report,
    }))
}

/// The cash columns and figures of a closed session: the missing-nota count and the reason names
/// stored at its close (0 and the live names before 2.7), everything else recomputed from rows
/// that no longer change.
async fn stored_extras(tx: &mut Tx, record: &SessionRecord) -> Result<CloseExtras, ApiError> {
    let state = cash_db::session_cash_state(&mut *tx, record.session.id())
        .await?
        .unwrap_or_default();
    let figures = session_figures(tx, record, Some(state.missing_nota.unwrap_or(0))).await?;
    Ok(CloseExtras { figures, state })
}

/// The stored close of `record` as `core` figures, with its float (spec 2.7 §3, §4), for the
/// close report text and the Z. A session closed before 2.7 has no float kept: 0 kept, 0 to the
/// safe. `None` while open.
///
/// # Errors
/// The database error.
pub(crate) async fn stored_close_figures(
    tx: &mut Tx,
    record: &SessionRecord,
) -> Result<Option<(sultan_core::cash::CloseFigures, CloseReport)>, ApiError> {
    let Some(stored) = cashier_sessions::stored_close(&mut *tx, record.session.id()).await? else {
        return Ok(None);
    };
    let extras = stored_extras(tx, record).await?;
    let report = stored.report;
    let float_kept = extras.state.float_kept.unwrap_or_default();
    let figures = sultan_core::cash::CloseFigures {
        session: extras.figures,
        counted_cash: report.counted_cash,
        over_short: report.over_short,
        float: sultan_core::cash::FloatFigures {
            float_kept,
            to_safe: if extras.state.float_kept.is_some() {
                report.counted_cash.saturating_sub(float_kept)
            } else {
                Money::ZERO
            },
            opening_proposed: extras.state.opening_proposed,
            float_difference: float_difference(report.opening_cash, extras.state.opening_proposed)
                .unwrap_or_default(),
        },
    };
    Ok(Some((figures, report)))
}

/// `GET /api/v1/cashier-sessions`: the sessions the caller may read, newest first.
///
/// # Errors
/// `401`, `400 invalid_body` (bad ID) / `invalid_query` (bad number), `422 invalid_field`
/// (`status` or `limit`).
#[utoipa::path(get, path = "/api/v1/cashier-sessions", tag = "cashier",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(SessionQuery),
    responses((status = 200, body = SessionPage), (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    caller: UserSession,
    ApiQuery(query): ApiQuery<SessionQuery>,
) -> Result<Json<SessionPage>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {MAX_LIMIT}"),
        ));
    }
    let open = match query.status.as_deref() {
        None => None,
        Some("open") => Some(true),
        Some("closed") => Some(false),
        Some(_) => {
            return Err(ApiError::invalid_field(
                "status",
                "`status` must be `open` or `closed`",
            ));
        }
    };
    let access = Caller::load(&state.db, &caller).await?;
    let visibility = match access.branches_with(READ_PERMISSION) {
        None => Visibility::All,
        Some(branches) => Visibility::OwnAnd {
            user: caller.user.id,
            branches,
        },
    };
    let filter = SessionFilter {
        visibility,
        branch: parse_opt_id::<BranchId>(query.branch_id.as_deref(), "branch_id")?,
        register: parse_opt_id::<RegisterId>(query.register_id.as_deref(), "register_id")?,
        user: parse_opt_id::<UserId>(query.user_id.as_deref(), "user_id")?,
        open,
        from: query.from.map(Timestamp::from_unix_millis),
        until: query.until.map(Timestamp::from_unix_millis),
        before_id: parse_opt_id(query.before_id.as_deref(), "before_id")?,
        limit,
    };
    let mut tx = state.db.begin_read().await?;
    let records = cashier_sessions::list(&mut tx, &filter).await?;
    let mut items = Vec::with_capacity(records.len());
    for record in &records {
        items.push(visible_dto(&state, &mut tx, &access, record).await?);
    }
    tx.commit().await?;
    let full = records.len() == limit as usize;
    let next_before_id = records
        .last()
        .filter(|_| full)
        .map(|r| r.session.id().to_string());
    Ok(Json(SessionPage {
        items,
        next_before_id,
    }))
}

/// Closes `record`'s session (already changed by `core`) with its report, audits, commits and
/// publishes `cashier_session.closed`.
async fn finish_close(
    state: &AppState,
    mut tx: Tx,
    who: &UserSession,
    (record, report, counts): (&SessionRecord, &CloseReport, &[DenominationCount]),
    float_kept: Money,
    action: &str,
    before: Value,
) -> Result<CloseReportDto, ApiError> {
    let session = &record.session;
    let figures = session_figures(&mut tx, record, None).await?;
    cashier_sessions::close(&mut tx, session, report, counts)
        .await
        .map_err(|e| match e {
            DbError::NotFound(_) => ApiError::from(SessionError::AlreadyClosed {
                session: session.id(),
            }),
            other => other.into(),
        })?;
    cash_db::record_close_cash(&mut tx, session.id(), float_kept, figures.missing_nota).await?;
    cash_db::record_close_reasons(&mut tx, session.id(), &figures.cash.reasons).await?;
    let context = cashier_sessions::report_context(&mut tx, session.id())
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    let state_after = cash_db::session_cash_state(&mut tx, session.id())
        .await?
        .unwrap_or_default();
    let extras = CloseExtras {
        figures,
        state: state_after,
    };
    let dto = report_dto(record, context, report, counts, &extras);
    let now = state.now();
    let entry = audit(who, now, action, record, Some(before), to_json(&dto));
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    let mut data = event_data(record);
    data["forced"] = dto.forced.into();
    state.events.publish(Event::for_branch(
        "cashier_session.closed",
        record.branch,
        data,
    ));
    Ok(dto)
}

/// `POST /api/v1/cashier-sessions/{id}/close`: blind close by the session's cashier → the close
/// report (spec 2.1 §5).
///
/// # Errors
/// `401`, `403 register_session_required` / `permission_denied`, `404 not_found`,
/// `409 session_closed`, `422 invalid_field` / `amount_overflow`.
#[utoipa::path(post, path = "/api/v1/cashier-sessions/{id}/close", tag = "cashier",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cashier session ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = CloseSession,
    responses((status = 200, body = CloseReportDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn close(
    State(state): State<AppState>,
    caller: RegisterSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CloseSession>,
) -> Result<Json<CloseReportDto>, ApiError> {
    Caller::load(&state.db, &caller.user)
        .await?
        .require(Permission::SessionOpenClose)?;
    let id: CashierSessionId = parse_id(&id, "id")?;
    let (counted, counts) = parse_close(&body)?;
    let mut tx = state.db.begin().await?;
    let mut record = cashier_sessions::get(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    let before = to_json(&session_dto(&record, None));
    let movements = crate::movements::session_movements(&state, &mut tx, id).await?;
    let opening = record.session.opening_cash();
    let report = record
        .session
        .close(caller.user.user.id, counted, &movements, state.now())?;
    if record.session.register() != caller.register.register {
        return Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "permission_denied",
            "the session is open on another register; close it there",
        ));
    }
    let kept = float_kept(opening, counted, body.float_kept)?;
    let dto = finish_close(
        &state,
        tx,
        &caller.user,
        (&record, &report, &counts),
        kept,
        "cashier_session.close",
        before,
    )
    .await?;
    Ok(Json(dto))
}

/// `POST /api/v1/cashier-sessions/{id}/force-close`: a manager closes an abandoned session; the
/// report is marked forced (spec 2.1 §5). Needs `session.force_close` at the session's branch.
///
/// # Errors
/// `401`, `403 permission_denied`, `404 not_found`, `409 session_closed`, `422 invalid_field` /
/// `amount_overflow`.
#[utoipa::path(post, path = "/api/v1/cashier-sessions/{id}/force-close", tag = "cashier",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cashier session ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = CloseSession,
    responses((status = 200, body = CloseReportDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn force_close(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CloseSession>,
) -> Result<Json<CloseReportDto>, ApiError> {
    let id: CashierSessionId = parse_id(&id, "id")?;
    let (counted, counts) = parse_close(&body)?;
    let access = Caller::load(&state.db, &caller).await?;
    let mut tx = state.db.begin().await?;
    let mut record = cashier_sessions::get(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    access
        .at_scope(BranchScope::Branch(record.branch))
        .require(Permission::SessionForceClose, None)?;
    let before = to_json(&session_dto(&record, None));
    let movements = crate::movements::session_movements(&state, &mut tx, id).await?;
    let opening = record.session.opening_cash();
    let report = record
        .session
        .force_close(caller.user.id, counted, &movements, state.now())?;
    let kept = float_kept(opening, counted, body.float_kept)?;
    let dto = finish_close(
        &state,
        tx,
        &caller,
        (&record, &report, &counts),
        kept,
        "cashier_session.force_close",
        before,
    )
    .await?;
    Ok(Json(dto))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_race_lost_to_the_end_is_a_conflict() {
        let answer = |attempt: Attempt| {
            let error = attempt.into_answer();
            (error.status().as_u16(), error.code().to_owned())
        };
        let conflict = Attempt::from(DbError::Conflict("index".into()));
        assert!(matches!(conflict, Attempt::Retry(_)));
        assert_eq!(answer(conflict), (409, "session_already_open".into()));
        let busy = Attempt::from(DbError::sqlite_busy());
        assert!(matches!(busy, Attempt::Retry(_)));
        assert_eq!(answer(busy), (409, "conflict".into()));
        let fail = Attempt::from(DbError::Corrupt("x".into()));
        assert!(matches!(fail, Attempt::Fail(_)));
        assert_eq!(answer(fail), (500, "internal".into()));
        let timed_out = Attempt::from(DbError::Sqlx(sqlx::Error::PoolTimedOut));
        assert!(matches!(timed_out, Attempt::Fail(_)));
    }

    /// The open loop as `open` runs it: `SQLITE_BUSY` on every attempt is retried
    /// [`retry::ATTEMPTS`] times and answered `409 conflict`; an error that is not a lost race
    /// is answered after one attempt.
    #[tokio::test]
    async fn open_retries_busy_and_answers_other_errors_at_once() {
        use std::cell::Cell;

        async fn open_loop(calls: &Cell<u32>, error: fn() -> DbError) -> (u32, u16, String) {
            calls.set(0);
            let failure = retry::run(
                "test open",
                || async move {
                    calls.set(calls.get() + 1);
                    Result::<(), Attempt>::Err(error().into())
                },
                |attempt| matches!(attempt, Attempt::Retry(_)),
            )
            .await
            .unwrap_err();
            let (Failure::Final(attempt) | Failure::Exhausted(attempt)) = failure;
            let error = attempt.into_answer();
            (
                calls.get(),
                error.status().as_u16(),
                error.code().to_owned(),
            )
        }

        let calls = Cell::new(0);
        assert_eq!(
            open_loop(&calls, DbError::sqlite_busy).await,
            (retry::ATTEMPTS, 409, "conflict".into())
        );
        assert_eq!(
            open_loop(&calls, || DbError::Sqlx(sqlx::Error::PoolTimedOut)).await,
            (1, 500, "internal".into())
        );
    }

    fn body(counted: i64, counts: Option<Vec<(i64, u32)>>) -> CloseSession {
        CloseSession {
            counted_cash: counted,
            counts: counts.map(|c| {
                c.into_iter()
                    .map(|(denomination, quantity)| CountDto {
                        denomination,
                        quantity,
                    })
                    .collect()
            }),
            float_kept: None,
        }
    }

    #[test]
    fn close_body_rules() {
        #[rustfmt::skip]
        let cases = [
            ("no counts", body(5, None), Ok(vec![])),
            ("counts add up, sorted largest first",
                body(120_000, Some(vec![(20_000, 1), (50_000, 2)])), Ok(vec![50_000, 20_000])),
            ("negative count", body(-1, None), Err("counted_cash")),
            ("counts do not add up", body(10, Some(vec![(5, 1)])), Err("counts")),
            ("zero denomination", body(0, Some(vec![(0, 1)])), Err("counts")),
            ("duplicate denomination", body(10, Some(vec![(5, 1), (5, 1)])), Err("counts")),
            ("overflow", body(10, Some(vec![(i64::MAX, 2)])), Err("counts")),
        ];
        for (name, body, expected) in cases {
            let got = parse_close(&body)
                .map(|(_, counts)| {
                    counts
                        .iter()
                        .map(|c| c.denomination.minor())
                        .collect::<Vec<_>>()
                })
                .inspect_err(|e| {
                    assert_eq!(e.code(), "invalid_field", "{name}");
                });
            match expected {
                Ok(denominations) => assert_eq!(got.unwrap(), denominations, "{name}"),
                Err(field) => {
                    let err = got.unwrap_err();
                    assert_eq!(err.status().as_u16(), 422, "{name}");
                    assert!(format!("{err:?}").contains(field), "{name}");
                }
            }
        }
    }

    #[test]
    fn attempts_classify_errors() {
        assert!(matches!(
            Attempt::from(DbError::Conflict("x".into())),
            Attempt::Retry(_)
        ));
        assert!(matches!(
            Attempt::from(DbError::NotFound("register")),
            Attempt::Fail(e) if e.code() == "not_found"
        ));
        assert!(matches!(
            Attempt::from(SessionError::NoOpenSession),
            Attempt::Fail(e) if e.code() == "no_open_session"
        ));
        assert!(matches!(
            Attempt::from(ApiError::not_found("x")),
            Attempt::Fail(_)
        ));
    }
}
