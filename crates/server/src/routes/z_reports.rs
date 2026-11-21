//! The Z report: end of day per branch (spec 2.7 §6, K13–K19; phase K3).
//!
//! - `POST /z-reports {day, branch_id?}` with `Idempotency-Key` → `201`: `day` is the business day
//!   `YYYY-MM-DD` (the branch-local open date of the sessions, K14). At a register the branch is
//!   the register's (a different `branch_id` is `422 invalid_field`); in the back office
//!   `branch_id` is required. Needs `day.close` at the branch: at the till a manager may approve
//!   it (`X-Override`, `context_ref` = `<branch_id>:<day>`), in the back office it is required.
//!   Refused on an HQ node (`403 read_only_replica`: the sessions belong to the branch node, HQ
//!   reads the synced copy). In one transaction (rerun after a lost race on the number or a
//!   session link): `core::cash::check_z_run` (a `day` after today → `422 invalid_field` `day`;
//!   a session of the day still open → `409 sessions_open` with `sessions` = their numbers;
//!   nothing left → `409 nothing_to_close`), the next number `<BRANCH>-Z-<YYMMDD>-<NN>`
//!   (`seq > 1` = supplementary, K16), `core::cash::build_z` over each covered session's stored
//!   close (its figures, float, the missing notas as of now and its sales documents), the stored
//!   snapshot with its audit (`z_report.post`) and outbox (`z_report.posted`) rows, and the link of
//!   each session. Publishes `day.closed {branch_id, day}` to the branch.
//! - `GET /z-reports?branch_id&from&until&before_id&limit`: newest first (`from` / `until` are
//!   business days, inclusive). `report.sales` at the branch (without `branch_id`: at every
//!   branch).
//! - `GET /z-reports/{id}`: the stored snapshot. `report.sales` at its branch, or whoever ran it.
//! - `GET /z-reports/{id}/text?lang=&width=&copy=`: the text (`core::cash::build_z_report`) of
//!   the snapshot; never recomputed. `copy=true` prints COPY, needs `report.sales` and is audited
//!   as `z_report.reprint`.
//! - `GET /z-reports/pending?branch_id=`: the days still to close (K18), oldest first, with their
//!   open and closed (unclosed) session counts. Days before the install day of cash management
//!   are left out (`sultan_db::cash::cash_since`, spec 2.7 §17 reading 10). `report.sales` or
//!   `day.close` at the branch; at a register `branch_id` defaults to its branch.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::{AccessError, BranchScope, Permission};
use sultan_core::cash::{
    CloseFigures, SaleRef, SessionSales, ZReport, ZReportInput, ZSession, ZSessionLine, ZTotals,
    build_z, build_z_report, check_z_run,
};
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::ids::{BranchId, UserId, ZReportId};
use sultan_core::money::Money;
use sultan_core::sale::TaxLine;
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::cash::{self, MAX_CASH_PAGE, NewZReport, ZFilter, ZReportRecord};
use sultan_db::{cashier_sessions, org};
use utoipa::{IntoParams, ToSchema};

use super::cash::{
    CashReportDto, Fail, TextContext, TextDto, TextQuery, core_figures, run, session_figures,
    user_name,
};
use super::cashier_sessions::{SalesReportDto, stored_close_figures};
use super::{parse_id, parse_opt_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, RegisterCtx, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::{AppState, NodeRole};

/// Body of `POST /z-reports`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct RunZBody {
    /// The business day, `YYYY-MM-DD`.
    pub day: String,
    /// The branch (back office); at a register its own branch.
    #[serde(default)]
    pub branch_id: Option<String>,
}

/// A sale number and when it was posted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct SaleRefDto {
    /// The sale number.
    pub number: String,
    /// Posted at (unix ms).
    pub posted_at: i64,
}

/// Tax per rate: base (DPP) and tax.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TaxLineDto {
    /// The rate in basis points (`1100` = 11%).
    pub rate_bp: u16,
    /// Σ base.
    pub base: i64,
    /// Σ tax.
    pub tax: i64,
}

/// The totals of a register or of the branch in a Z (§6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ZTotalsDto {
    /// Sessions.
    pub sessions: u32,
    /// Sales posted in them.
    pub sales_count: u32,
    /// Σ gross of the sales.
    pub gross_sales: i64,
    /// Gross sales − line discounts − cart discounts.
    pub net_sales: i64,
    /// The first sale.
    pub first_sale: Option<SaleRefDto>,
    /// The last sale.
    pub last_sale: Option<SaleRefDto>,
    /// The tax per rate, sales minus returns and post-voids.
    pub tax: Vec<TaxLineDto>,
    /// Σ expected cash.
    pub expected_cash: i64,
    /// Σ counted cash.
    pub counted_cash: i64,
    /// Σ over/short.
    pub over_short: i64,
    /// Σ float kept.
    pub float_kept: i64,
    /// Σ to safe.
    pub to_safe: i64,
    /// Σ float differences.
    pub float_difference: i64,
    /// Σ drops + Σ to safe.
    pub cash_to_deposit: i64,
    /// The summed sales block.
    pub sales: SalesReportDto,
    /// The summed cash block (missing notas as of the run).
    pub cash: CashReportDto,
}

/// One session in a Z.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ZSessionDto {
    /// The session number.
    pub number: String,
    /// The register code.
    pub register: String,
    /// The cashier's name.
    pub cashier: String,
    /// Opened at (unix ms).
    pub opened_at: i64,
    /// Closed at (unix ms).
    pub closed_at: i64,
    /// Force-closed.
    pub forced: bool,
    /// Opening cash.
    pub opening_cash: i64,
    /// Expected cash.
    pub expected_cash: i64,
    /// Counted cash.
    pub counted_cash: i64,
    /// Over (+) / short (−).
    pub over_short: i64,
    /// Float kept.
    pub float_kept: i64,
    /// To the safe.
    pub to_safe: i64,
    /// Float difference at open.
    pub float_difference: i64,
}

/// A register's subtotals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ZRegisterDto {
    /// The register code.
    pub register: String,
    /// Its totals.
    pub totals: ZTotalsDto,
}

/// The stored snapshot of a Z: never recomputed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ZSnapshotDto {
    /// `JKT01-Z-261119-01`.
    pub number: String,
    /// The business day, `YYYY-MM-DD`.
    pub day: String,
    /// The sequence within the day, from 1.
    pub seq: u32,
    /// `seq > 1` (K16).
    pub supplementary: bool,
    /// When it was run (unix ms).
    pub run_at: i64,
    /// The sessions, by open time.
    pub sessions: Vec<ZSessionDto>,
    /// Subtotals per register code.
    pub registers: Vec<ZRegisterDto>,
    /// Branch totals.
    pub branch: ZTotalsDto,
}

/// A Z report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ZReportDto {
    /// ID.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// Who ran it.
    pub user_id: String,
    /// Their display name.
    pub run_by_name: String,
    /// The approving manager (override at the till).
    pub approved_by: Option<String>,
    /// Their display name.
    pub approved_by_name: Option<String>,
    /// The sessions it covers, by open time.
    pub session_ids: Vec<String>,
    /// The snapshot.
    pub report: ZSnapshotDto,
}

/// A Z in a list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ZSummaryDto {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// Branch.
    pub branch_id: String,
    /// Business day, `YYYY-MM-DD`.
    pub day: String,
    /// Sequence within the day.
    pub seq: u32,
    /// Supplementary.
    pub supplementary: bool,
    /// Sessions covered.
    pub session_count: u32,
    /// Sales.
    pub sales_count: u32,
    /// Net sales.
    pub net_sales: i64,
    /// Σ expected cash.
    pub expected_cash: i64,
    /// Σ counted cash.
    pub counted_cash: i64,
    /// Σ over/short.
    pub over_short: i64,
    /// Cash to deposit.
    pub cash_to_deposit: i64,
    /// Run at (unix ms).
    pub run_at: i64,
    /// Who ran it.
    pub user_id: String,
    /// Who approved it.
    pub approved_by: Option<String>,
}

/// One page of `GET /z-reports`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ZReportPage {
    /// Newest first.
    pub items: Vec<ZSummaryDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

/// A day still to close.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PendingDayDto {
    /// The business day, `YYYY-MM-DD`.
    pub day: String,
    /// Its sessions still open.
    pub open: u32,
    /// Its closed sessions in no Z yet.
    pub closed: u32,
}

/// Query of `GET /z-reports`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ZQuery {
    /// Only this branch.
    pub branch_id: Option<String>,
    /// Business day on or after, `YYYY-MM-DD`.
    pub from: Option<String>,
    /// Business day on or before, `YYYY-MM-DD`.
    pub until: Option<String>,
    /// Page cursor: `next_before_id` of the previous page.
    pub before_id: Option<String>,
    /// Page size, 1–200 (default 50).
    pub limit: Option<u32>,
}

/// Query of `GET /z-reports/pending`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct PendingQuery {
    /// The branch (required in the back office; a register's own by default).
    pub branch_id: Option<String>,
}

/// `YYYY-MM-DD` → a day; `422 invalid_field` naming `field` otherwise.
fn parse_day(field: &str, value: &str) -> Result<LocalDay, ApiError> {
    let invalid = || ApiError::invalid_field(field, format!("`{field}` must be a date YYYY-MM-DD"));
    let bytes = value.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(invalid());
    }
    let digits: String = value.chars().filter(|c| *c != '-').collect();
    if digits.len() != 8 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(invalid());
    }
    digits
        .parse::<i64>()
        .ok()
        .and_then(LocalDay::from_key)
        .ok_or_else(invalid)
}

/// A day as `YYYY-MM-DD`.
#[must_use]
pub fn day_text(day: LocalDay) -> String {
    let (y, m, d) = day.ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

fn sale_ref(s: &SaleRef) -> SaleRefDto {
    SaleRefDto {
        number: s.number.clone(),
        posted_at: s.posted_at.unix_millis(),
    }
}

fn totals_dto(t: &ZTotals) -> ZTotalsDto {
    ZTotalsDto {
        sessions: t.sessions,
        sales_count: t.sales_count,
        gross_sales: t.gross_sales.minor(),
        net_sales: t.net_sales.minor(),
        first_sale: t.first_sale.as_ref().map(sale_ref),
        last_sale: t.last_sale.as_ref().map(sale_ref),
        tax: t
            .tax
            .iter()
            .map(|l| TaxLineDto {
                rate_bp: l.rate_bp,
                base: l.base.minor(),
                tax: l.tax.minor(),
            })
            .collect(),
        expected_cash: t.figures.expected_cash().minor(),
        counted_cash: t.counted_cash.minor(),
        over_short: t.over_short.minor(),
        float_kept: t.float_kept.minor(),
        to_safe: t.to_safe.minor(),
        float_difference: t.float_difference.minor(),
        cash_to_deposit: t.cash_to_deposit.minor(),
        sales: SalesReportDto::from(&t.figures.sales),
        cash: CashReportDto::from(&t.figures),
    }
}

/// The snapshot of a built Z.
#[must_use]
pub fn snapshot(z: &ZReport) -> ZSnapshotDto {
    ZSnapshotDto {
        number: z.number.clone(),
        day: day_text(z.day),
        seq: z.seq,
        supplementary: z.supplementary,
        run_at: z.run_at.unix_millis(),
        sessions: z
            .sessions
            .iter()
            .map(|s| ZSessionDto {
                number: s.number.clone(),
                register: s.register.clone(),
                cashier: s.cashier.clone(),
                opened_at: s.opened_at.unix_millis(),
                closed_at: s.closed_at.unix_millis(),
                forced: s.forced,
                opening_cash: s.opening_cash.minor(),
                expected_cash: s.expected_cash.minor(),
                counted_cash: s.counted_cash.minor(),
                over_short: s.over_short.minor(),
                float_kept: s.float_kept.minor(),
                to_safe: s.to_safe.minor(),
                float_difference: s.float_difference.minor(),
            })
            .collect(),
        registers: z
            .registers
            .iter()
            .map(|(register, totals)| ZRegisterDto {
                register: register.clone(),
                totals: totals_dto(totals),
            })
            .collect(),
        branch: totals_dto(&z.branch),
    }
}

fn core_ref(s: &SaleRefDto) -> SaleRef {
    SaleRef {
        number: s.number.clone(),
        posted_at: Timestamp::from_unix_millis(s.posted_at),
    }
}

fn core_totals(t: &ZTotalsDto) -> Result<ZTotals, ApiError> {
    let m = Money::from_minor;
    Ok(ZTotals {
        figures: core_figures(&t.sales, &t.cash)?,
        sessions: t.sessions,
        sales_count: t.sales_count,
        gross_sales: m(t.gross_sales),
        net_sales: m(t.net_sales),
        first_sale: t.first_sale.as_ref().map(core_ref),
        last_sale: t.last_sale.as_ref().map(core_ref),
        tax: t
            .tax
            .iter()
            .map(|l| TaxLine {
                rate_bp: l.rate_bp,
                base: m(l.base),
                tax: m(l.tax),
            })
            .collect(),
        counted_cash: m(t.counted_cash),
        over_short: m(t.over_short),
        float_kept: m(t.float_kept),
        to_safe: m(t.to_safe),
        float_difference: m(t.float_difference),
        cash_to_deposit: m(t.cash_to_deposit),
    })
}

/// The `core` Z of a stored snapshot (for its text).
///
/// # Errors
/// `500 internal` when the snapshot does not read back.
pub fn core_z(s: &ZSnapshotDto) -> Result<ZReport, ApiError> {
    let m = Money::from_minor;
    let t = Timestamp::from_unix_millis;
    let mut registers = std::collections::BTreeMap::new();
    for r in &s.registers {
        registers.insert(r.register.clone(), core_totals(&r.totals)?);
    }
    Ok(ZReport {
        number: s.number.clone(),
        day: parse_day("day", &s.day).map_err(|_| ApiError::internal())?,
        seq: s.seq,
        supplementary: s.supplementary,
        run_at: t(s.run_at),
        sessions: s
            .sessions
            .iter()
            .map(|l| ZSessionLine {
                number: l.number.clone(),
                register: l.register.clone(),
                cashier: l.cashier.clone(),
                opened_at: t(l.opened_at),
                closed_at: t(l.closed_at),
                forced: l.forced,
                opening_cash: m(l.opening_cash),
                expected_cash: m(l.expected_cash),
                counted_cash: m(l.counted_cash),
                over_short: m(l.over_short),
                float_kept: m(l.float_kept),
                to_safe: m(l.to_safe),
                float_difference: m(l.float_difference),
            })
            .collect(),
        registers,
        branch: core_totals(&s.branch)?,
    })
}

fn stored_snapshot(record: &ZReportRecord) -> Result<ZSnapshotDto, ApiError> {
    serde_json::from_value(record.figures.clone()).map_err(|e| {
        tracing::error!("stored Z snapshot {}: {e}", record.id);
        ApiError::internal()
    })
}

async fn report_dto(state: &AppState, record: &ZReportRecord) -> Result<ZReportDto, ApiError> {
    let sessions = cash::z_report_sessions(&state.db, record.id).await?;
    Ok(ZReportDto {
        id: record.id.to_string(),
        branch_id: record.branch.to_string(),
        user_id: record.user.to_string(),
        run_by_name: user_name(state, Some(record.user))
            .await?
            .unwrap_or_default(),
        approved_by: record.approved_by.map(|u| u.to_string()),
        approved_by_name: user_name(state, record.approved_by).await?,
        session_ids: sessions.iter().map(ToString::to_string).collect(),
        report: stored_snapshot(record)?,
    })
}

fn summary(r: &ZReportRecord) -> ZSummaryDto {
    ZSummaryDto {
        id: r.id.to_string(),
        number: r.number.clone(),
        branch_id: r.branch.to_string(),
        day: day_text(r.business_day),
        seq: r.seq,
        supplementary: r.supplementary,
        session_count: r.session_count,
        sales_count: r.sales_count,
        net_sales: r.net_sales.minor(),
        expected_cash: r.expected_cash.minor(),
        counted_cash: r.counted_cash.minor(),
        over_short: r.over_short.minor(),
        cash_to_deposit: r.cash_to_deposit.minor(),
        run_at: r.run_at.unix_millis(),
        user_id: r.user.to_string(),
        approved_by: r.approved_by.map(|u| u.to_string()),
    }
}

/// The branch a till or back-office call is about: a register's own (`branch_id` must match when
/// sent), else the required `branch_id`.
fn branch_of(caller: &UserSession, branch_id: Option<&str>) -> Result<BranchId, ApiError> {
    let asked = parse_opt_id::<BranchId>(branch_id, "branch_id")?;
    match (caller.session.register, asked) {
        (Some(_), Some(b)) if b != caller.session.branch => Err(ApiError::invalid_field(
            "branch_id",
            "a register runs the Z of its own branch",
        )),
        (Some(_), _) => Ok(caller.session.branch),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(ApiError::invalid_field(
            "branch_id",
            "`branch_id` is required",
        )),
    }
}

async fn branch_tz(state: &AppState, branch: BranchId) -> Result<BranchTimeZone, ApiError> {
    let b = org::get_branch(&state.db, branch)
        .await?
        .ok_or_else(|| ApiError::invalid_field("branch_id", "unknown branch"))?;
    Ok(b.timezone.parse().unwrap_or_default())
}

/// A covered session's inputs of `core::cash::ZSession`, owned.
struct Covered {
    number: String,
    register: String,
    cashier: String,
    opened_at: Timestamp,
    closed_at: Timestamp,
    forced: bool,
    close: CloseFigures,
    missing_nota: u32,
    sales: SessionSales,
}

/// One Z run transaction (see the module docs).
async fn try_run(
    state: &AppState,
    caller: &UserSession,
    access: &Caller,
    header: &OverrideHeader,
    (branch, day, tz): (BranchId, LocalDay, BranchTimeZone),
    id: ZReportId,
) -> Result<(ZReportRecord, Option<UserId>), Fail> {
    let now = state.now();
    let mut tx = state.db.begin().await?;
    let approved_by = match caller.session.register {
        Some(register) => {
            let at = RegisterSession {
                register: RegisterCtx { register, branch },
                user: caller.clone(),
            };
            overrides::authorize(
                state,
                &mut tx,
                &at,
                &access.permissions,
                Action {
                    permission: Permission::DayClose,
                    value: None,
                    context_ref: &format!("{branch}:{}", day_text(day)),
                },
                header,
            )
            .await?
        }
        None => None,
    };
    let sessions = cash::day_sessions(&mut tx, branch, day).await?;
    let candidates: Vec<_> = sessions.iter().map(cash::DaySession::candidate).collect();
    let covered_ids = check_z_run(day, LocalDay::of(now, tz), &candidates)?;
    let number = cash::next_z_number(&mut tx, branch, day).await?;
    let mut covered = Vec::with_capacity(covered_ids.len());
    for s in sessions.iter().filter(|s| covered_ids.contains(&s.id)) {
        let record = cashier_sessions::get(&mut tx, s.id)
            .await?
            .ok_or_else(|| ApiError::not_found("cashier session"))?;
        let Some((close, _)) = stored_close_figures(&mut tx, &record).await? else {
            return Err(ApiError::not_found("close report").into());
        };
        let live = session_figures(&mut tx, &record, None).await?;
        covered.push(Covered {
            number: s.number.clone(),
            register: s.register_code.clone(),
            cashier: s.cashier_name.clone(),
            opened_at: s.opened_at,
            closed_at: s.closed_at.unwrap_or(now),
            forced: s.forced,
            close,
            missing_nota: live.missing_nota,
            sales: cash::session_sales(&mut tx, s.id).await?,
        });
    }
    let inputs: Vec<ZSession<'_>> = covered
        .iter()
        .map(|c| ZSession {
            number: &c.number,
            register: &c.register,
            cashier: &c.cashier,
            opened_at: c.opened_at,
            closed_at: c.closed_at,
            forced: c.forced,
            close: &c.close,
            missing_nota: c.missing_nota,
            sales: &c.sales,
        })
        .collect();
    let z = build_z(&number.number, day, number.seq, now, &inputs)?;
    let figures = serde_json::to_value(snapshot(&z)).unwrap_or_default();
    let new = NewZReport {
        id,
        branch,
        report: z,
        sessions: covered_ids,
        figures,
        user: caller.user.id,
        approved_by,
        register: caller.session.register,
    };
    let stored = cash::insert_z_report(&mut tx, &new, now).await?;
    tx.commit().await?;
    Ok((stored, approved_by))
}

/// `POST /api/v1/z-reports` (see the module docs) → `201`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied` / `needs_override` / `invalid_override`,
/// `409 sessions_open` / `nothing_to_close` / `conflict`, `422 invalid_field` (`day`,
/// `branch_id`).
#[utoipa::path(post, path = "/api/v1/z-reports", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)"),
        ("X-Override" = Option<String>, Header, description = "Override token for `day.close` (context: `<branch_id>:<day>`)")),
    request_body = RunZBody,
    responses((status = 201, body = ZReportDto), (status = 403, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn post(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    ApiJson(body): ApiJson<RunZBody>,
) -> Result<(StatusCode, Json<ZReportDto>), ApiError> {
    let day = parse_day("day", body.day.trim())?;
    if state.node_role == NodeRole::Hq {
        return Err(ApiError::read_only_replica());
    }
    let branch = branch_of(&caller, body.branch_id.as_deref())?;
    let tz = branch_tz(&state, branch).await?;
    let access = Caller::load(&state.db, &caller).await?;
    if caller.session.register.is_none() {
        access
            .at_scope(BranchScope::Branch(branch))
            .require(Permission::DayClose, None)?;
    }
    let id = ZReportId::generate();
    let (state_ref, caller_ref, access_ref, header_ref) = (&state, &caller, &access, &header);
    let (stored, _) = run("z report run", || {
        try_run(
            state_ref,
            caller_ref,
            access_ref,
            header_ref,
            (branch, day, tz),
            id,
        )
    })
    .await?;
    state.events.publish(Event::for_branch(
        "day.closed",
        branch,
        json!({
            "branch_id": branch.to_string(),
            "day": day_text(day),
            "z_report_id": stored.id.to_string(),
            "number": stored.number,
        }),
    ));
    Ok((
        StatusCode::CREATED,
        Json(report_dto(&state, &stored).await?),
    ))
}

fn denied(permission: Permission) -> ApiError {
    AccessError::PermissionDenied { permission }.into()
}

/// The Z `id` if `caller` may read it: `report.sales` at its branch, or they ran it.
async fn readable(
    state: &AppState,
    caller: &UserSession,
    id: &str,
) -> Result<(ZReportRecord, bool), ApiError> {
    let id: ZReportId = parse_id(id, "id")?;
    let record = cash::get_z_report(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("z report"))?;
    let access = Caller::load(&state.db, caller).await?;
    let reports = access
        .at_scope(BranchScope::Branch(record.branch))
        .contains(Permission::ReportSales);
    if reports || record.user == caller.user.id {
        Ok((record, reports))
    } else {
        Err(denied(Permission::ReportSales))
    }
}

/// `GET /api/v1/z-reports/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/z-reports/{id}", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Z report ID")),
    responses((status = 200, body = ZReportDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ZReportDto>, ApiError> {
    let (record, _) = readable(&state, &caller, &id).await?;
    Ok(Json(report_dto(&state, &record).await?))
}

/// `GET /api/v1/z-reports/{id}/text?lang=&width=&copy=` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied`, `404 not_found`, `422 invalid_field` (`width`).
#[utoipa::path(get, path = "/api/v1/z-reports/{id}/text", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Z report ID"), TextQuery),
    responses((status = 200, body = TextDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn text(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<TextQuery>,
) -> Result<Json<TextDto>, ApiError> {
    let (record, reports) = readable(&state, &caller, &id).await?;
    if query.copy && !reports {
        return Err(denied(Permission::ReportSales));
    }
    let ctx = TextContext::load(&state, record.branch, &query).await?;
    let z = core_z(&stored_snapshot(&record)?)?;
    let run_by = user_name(&state, Some(record.user))
        .await?
        .unwrap_or_default();
    let approver = user_name(&state, record.approved_by).await?;
    let printout = ctx.build(|opts| {
        build_z_report(
            &ZReportInput {
                header: ctx.header(),
                report: &z,
                run_by: &run_by,
                approved_by: approver.as_deref(),
                copy: query.copy,
            },
            opts,
        )
    });
    if query.copy {
        let mut entry = AuditEntry::new(state.now(), "z_report.reprint");
        entry.branch = Some(record.branch);
        entry.register = caller.session.register;
        entry.user = Some(caller.user.id);
        entry.target = Some(format!("z_report:{}", record.id));
        entry.after = Some(json!({ "number": record.number }));
        let mut tx = state.db.begin().await?;
        audit::append(&mut tx, &entry).await?;
        tx.commit().await?;
    }
    Ok(Json(TextDto::new(&printout, query.copy)))
}

/// `GET /api/v1/z-reports?…` (see the module docs).
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied`, `422 invalid_field`.
#[utoipa::path(get, path = "/api/v1/z-reports", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(ZQuery),
    responses((status = 200, body = ZReportPage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    caller: UserSession,
    ApiQuery(query): ApiQuery<ZQuery>,
) -> Result<Json<ZReportPage>, ApiError> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_CASH_PAGE).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {MAX_CASH_PAGE}"),
        ));
    }
    let filter = ZFilter {
        branch: parse_opt_id::<BranchId>(query.branch_id.as_deref(), "branch_id")?,
        from_day: query
            .from
            .as_deref()
            .map(|d| parse_day("from", d))
            .transpose()?,
        until_day: query
            .until
            .as_deref()
            .map(|d| parse_day("until", d))
            .transpose()?,
        before: parse_opt_id::<ZReportId>(query.before_id.as_deref(), "before_id")?,
        limit,
    };
    let access = Caller::load(&state.db, &caller).await?;
    let allowed = match filter.branch {
        Some(b) => access
            .at_scope(BranchScope::Branch(b))
            .contains(Permission::ReportSales),
        None => access.holds_everywhere(Permission::ReportSales),
    };
    if !allowed {
        return Err(denied(Permission::ReportSales));
    }
    let records = cash::list_z_reports(&state.db, &filter).await?;
    let next_before_id = (records.len() == limit as usize)
        .then(|| records.last().map(|r| r.id.to_string()))
        .flatten();
    Ok(Json(ZReportPage {
        items: records.iter().map(summary).collect(),
        next_before_id,
    }))
}

/// `GET /api/v1/z-reports/pending?branch_id=` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied`, `422 invalid_field` (`branch_id`).
#[utoipa::path(get, path = "/api/v1/z-reports/pending", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(PendingQuery),
    responses((status = 200, body = [PendingDayDto]), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn pending(
    State(state): State<AppState>,
    caller: UserSession,
    ApiQuery(query): ApiQuery<PendingQuery>,
) -> Result<Json<Vec<PendingDayDto>>, ApiError> {
    let branch = branch_of(&caller, query.branch_id.as_deref())?;
    let access = Caller::load(&state.db, &caller).await?;
    let here = access.at_scope(BranchScope::Branch(branch));
    if !here.contains(Permission::ReportSales) && !here.contains(Permission::DayClose) {
        return Err(denied(Permission::ReportSales));
    }
    let tz = branch_tz(&state, branch).await?;
    let since = cash::cash_since(&state.db)
        .await?
        .map(|at| LocalDay::of(at, tz));
    let today = LocalDay::of(state.now(), tz);
    let days = cash::pending_days(&state.db, branch, today, since).await?;
    Ok(Json(
        days.iter()
            .map(|d| PendingDayDto {
                day: day_text(d.day),
                open: d.open,
                closed: d.closed,
            })
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn days_parse_and_print() {
        let cases = [
            ("2026-11-19", Some(20_261_119)),
            ("2026-02-30", None),
            ("2026-1-19", None),
            ("20261119", None),
            ("2026/11/19", None),
            ("2026-1a-19", None),
            ("", None),
        ];
        for (text, key) in cases {
            let got = parse_day("day", text).ok().map(LocalDay::key);
            assert_eq!(got, key, "{text}");
        }
        let day = LocalDay::from_key(20_260_105).unwrap();
        assert_eq!(day_text(day), "2026-01-05");
        let err = parse_day("from", "x").unwrap_err();
        assert_eq!((err.status().as_u16(), err.code()), (422, "invalid_field"));
    }
}
