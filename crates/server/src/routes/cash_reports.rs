//! The X report, the close report text and the proposed float (spec 2.7 §3, §4, §5; phase K3).
//!
//! - `GET /cashier-sessions/{id}/x-report` (JSON) and `…/x-report/text?lang=&width=` (printable):
//!   the figures of an **open** session as of now (`409 session_closed` once closed: read the close
//!   report instead), with the expected cash but never the count or an over/short. The caller's
//!   own session at its register needs `session.view_expected`, which a manager may approve at the
//!   till (`X-Override`, `context_ref` = the session ID). Anyone else (the back office, or another
//!   user at the till) needs `report.sales` **and** `session.view_expected` at the session's branch
//!   (not overridable). Nothing is stored; every read appends the audit entry `session.x_report`
//!   on `cashier_session:<id>` with the approver, in the same transaction as the read, so the X
//!   counts itself in `x_reports` (and the close report and the Z count every X).
//! - `GET /cashier-sessions/{id}/report/text?lang=&width=&copy=`: the stored close report as text
//!   (`core::cash::build_close_report`, the reprint of spec 2.1 §5): its own cashier, or
//!   `report.sales` at the branch. An open session has no close report (`404 not_found`).
//! - `GET /registers/me/float`: the opening cash the open screen proposes on this register (§3):
//!   the float kept by its last closed session, 0 without one. `POST /cashier-sessions` stores the
//!   proposal as `opening_proposed` when the typed opening cash differs from it, and only when the
//!   last closed session kept a float (`has_proposal`; not for a register's first session or one
//!   after a session closed before 2.7).

use axum::Json;
use axum::extract::{Path, State};
use serde::Serialize;
use serde_json::json;
use sultan_core::access::{AccessError, BranchScope, Permission};
use sultan_core::cash::{
    CloseReportInput, SessionHeader, XReportInput, build_close_report, build_x_report, x_report,
};
use sultan_core::cashier_session::{CloseKind, SessionError, SessionStatus};
use sultan_core::ids::{CashierSessionId, UserId};
use sultan_core::money::Money;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::cash::{self, X_REPORT_ACTION};
use sultan_db::cashier_sessions::{self, ReportContext, SessionRecord};
use utoipa::ToSchema;

use super::cash::{CashReportDto, TextContext, TextDto, TextQuery, session_figures, user_name};
use super::cashier_sessions::{SalesReportDto, stored_close_figures};
use super::parse_id;
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiQuery, RegisterCtx, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::AppState;

/// An X report (spec 2.7 §5): never a count or an over/short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct XReportDto {
    /// The session.
    pub session_id: String,
    /// Its number.
    pub number: String,
    /// Branch code.
    pub branch_code: String,
    /// Branch name.
    pub branch_name: String,
    /// Branch IANA time zone.
    pub timezone: String,
    /// Register code.
    pub register_code: String,
    /// The cashier's display name.
    pub cashier_name: String,
    /// Opened at (unix ms).
    pub opened_at: i64,
    /// When the X was read (unix ms).
    pub read_at: i64,
    /// The manager who approved the read at the till.
    pub approved_by: Option<String>,
    /// Cash at open.
    pub opening_cash: i64,
    /// The expected cash now.
    pub expected_cash: i64,
    /// The sales block (spec 2.6 §10) as of now.
    pub sales: SalesReportDto,
    /// The cash block (§4) as of now, this read included in `x_reports`.
    pub cash: CashReportDto,
    /// The proposed opening cash, when another amount was typed (§3).
    pub opening_proposed: Option<i64>,
    /// `opening_cash − opening_proposed`.
    pub float_difference: i64,
}

/// The proposed float of `GET /registers/me/float`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProposedFloatDto {
    /// This register.
    pub register_id: String,
    /// The float kept by its last closed session, 0 without one.
    pub proposed_float: i64,
    /// Whether the last closed session kept a float: only then is a different opening cash
    /// stored as `opening_proposed` and shown as a float difference.
    pub has_proposal: bool,
}

fn denied(permission: Permission) -> ApiError {
    AccessError::PermissionDenied { permission }.into()
}

/// One X read: the open session, its figures and who approved it.
struct XRead {
    record: SessionRecord,
    context: ReportContext,
    report: sultan_core::cash::XReport,
    approved_by: Option<UserId>,
}

/// Authorizes, audits and computes an X of session `id` (see the module docs).
async fn x_read(
    state: &AppState,
    caller: &UserSession,
    header: &OverrideHeader,
    id: &str,
) -> Result<XRead, ApiError> {
    let id: CashierSessionId = parse_id(id, "id")?;
    let record = cashier_sessions::get(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    if !record.session.is_open() {
        return Err(SessionError::AlreadyClosed { session: id }.into());
    }
    let access = Caller::load(&state.db, caller).await?;
    let at_till = caller.session.register == Some(record.session.register())
        && record.session.user() == caller.user.id;
    if !at_till {
        let here = access.at_scope(BranchScope::Branch(record.branch));
        for p in [Permission::ReportSales, Permission::SessionViewExpected] {
            if !here.contains(p) {
                return Err(denied(p));
            }
        }
    }
    let now = state.now();
    let mut tx = state.db.begin().await?;
    let approved_by = if at_till {
        let at = RegisterSession {
            register: RegisterCtx {
                register: record.session.register(),
                branch: caller.session.branch,
            },
            user: caller.clone(),
        };
        overrides::authorize(
            state,
            &mut tx,
            &at,
            &access.permissions,
            Action {
                permission: Permission::SessionViewExpected,
                value: None,
                context_ref: &id.to_string(),
            },
            header,
        )
        .await?
    } else {
        None
    };
    let mut entry = AuditEntry::new(now, X_REPORT_ACTION);
    entry.branch = Some(record.branch);
    entry.register = caller.session.register;
    entry.user = Some(caller.user.id);
    entry.target = Some(format!("cashier_session:{id}"));
    entry.after = Some(json!({
        "number": record.number,
        "approved_by": approved_by.map(|a| a.to_string()),
    }));
    audit::append(&mut tx, &entry).await?;
    let figures = session_figures(&mut tx, &record, None).await?;
    let proposed = cash::session_cash_state(&mut tx, id)
        .await?
        .and_then(|s| s.opening_proposed);
    let context = cashier_sessions::report_context(&mut tx, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    tx.commit().await?;
    Ok(XRead {
        record,
        context,
        report: x_report(figures, proposed, now)?,
        approved_by,
    })
}

/// `GET /api/v1/cashier-sessions/{id}/x-report` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied` / `needs_override` / `invalid_override`, `404 not_found`,
/// `409 session_closed`.
#[utoipa::path(get, path = "/api/v1/cashier-sessions/{id}/x-report", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cashier session ID"),
        ("X-Override" = Option<String>, Header, description = "Override token for `session.view_expected` (context: the session ID)")),
    responses((status = 200, body = XReportDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn x_report_json(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    Path(id): Path<String>,
) -> Result<Json<XReportDto>, ApiError> {
    let x = x_read(&state, &caller, &header, &id).await?;
    let s = &x.report.session;
    Ok(Json(XReportDto {
        session_id: x.record.session.id().to_string(),
        number: x.record.number.clone(),
        branch_code: x.context.branch_code,
        branch_name: x.context.branch_name,
        timezone: x.context.timezone,
        register_code: x.context.register_code,
        cashier_name: x.context.cashier_name,
        opened_at: x.record.session.opened_at().unix_millis(),
        read_at: x.report.read_at.unix_millis(),
        approved_by: x.approved_by.map(|a| a.to_string()),
        opening_cash: s.sales.opening_cash.minor(),
        expected_cash: s.expected_cash().minor(),
        sales: SalesReportDto::from(&s.sales),
        cash: CashReportDto::from(s),
        opening_proposed: x.report.opening_proposed.map(Money::minor),
        float_difference: x.report.float_difference.minor(),
    }))
}

/// `GET /api/v1/cashier-sessions/{id}/x-report/text?lang=&width=` (see the module docs).
///
/// # Errors
/// As `GET …/x-report`, and `422 invalid_field` (`width`).
#[utoipa::path(get, path = "/api/v1/cashier-sessions/{id}/x-report/text", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cashier session ID"), TextQuery,
        ("X-Override" = Option<String>, Header, description = "Override token for `session.view_expected` (context: the session ID)")),
    responses((status = 200, body = TextDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn x_report_text(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<TextQuery>,
) -> Result<Json<TextDto>, ApiError> {
    // The width is checked before the read is audited.
    let id_parsed: CashierSessionId = parse_id(&id, "id")?;
    if let Some(record) = cashier_sessions::get(&state.db, id_parsed).await? {
        super::sales::receipt_width(&state, query.width, record.branch).await?;
    }
    let x = x_read(&state, &caller, &header, &id).await?;
    let ctx = TextContext::load(&state, x.record.branch, &query).await?;
    let approver = user_name(&state, x.approved_by).await?;
    let printout = ctx.build(|opts| {
        build_x_report(
            &XReportInput {
                header: ctx.header(),
                session: SessionHeader {
                    number: &x.record.number,
                    register: &x.context.register_code,
                    cashier: &x.context.cashier_name,
                    opened_at: x.record.session.opened_at(),
                    closed_at: None,
                    forced: false,
                    approved_by: approver.as_deref(),
                },
                report: &x.report,
            },
            opts,
        )
    });
    Ok(Json(TextDto::new(&printout, false)))
}

/// `GET /api/v1/cashier-sessions/{id}/report/text?lang=&width=&copy=` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied`, `404 not_found` (unknown or still open), `422 invalid_field`.
#[utoipa::path(get, path = "/api/v1/cashier-sessions/{id}/report/text", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cashier session ID"), TextQuery),
    responses((status = 200, body = TextDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn close_report_text(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<TextQuery>,
) -> Result<Json<TextDto>, ApiError> {
    let id: CashierSessionId = parse_id(&id, "id")?;
    let record = cashier_sessions::get(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cashier session"))?;
    if record.session.user() != caller.user.id {
        let access = Caller::load(&state.db, &caller).await?;
        if !access
            .at_scope(BranchScope::Branch(record.branch))
            .contains(Permission::ReportSales)
        {
            return Err(denied(Permission::ReportSales));
        }
    }
    let ctx = TextContext::load(&state, record.branch, &query).await?;
    let mut tx = state.db.begin_read().await?;
    let close = stored_close_figures(&mut tx, &record).await?;
    let context = cashier_sessions::report_context(&mut tx, id).await?;
    tx.commit().await?;
    let (Some((figures, _)), Some(context)) = (close, context) else {
        return Err(ApiError::not_found("close report"));
    };
    let (closed_at, forced) = match record.session.status() {
        SessionStatus::Closed { at, kind } => (Some(at), matches!(kind, CloseKind::Forced { .. })),
        SessionStatus::Open => (None, false),
    };
    let printout = ctx.build(|opts| {
        build_close_report(
            &CloseReportInput {
                header: ctx.header(),
                session: SessionHeader {
                    number: &record.number,
                    register: &context.register_code,
                    cashier: &context.cashier_name,
                    opened_at: record.session.opened_at(),
                    closed_at,
                    forced,
                    approved_by: context.forced_by_name.as_deref(),
                },
                report: &figures,
                copy: query.copy,
            },
            opts,
        )
    });
    Ok(Json(TextDto::new(&printout, query.copy)))
}

/// `GET /api/v1/registers/me/float`: the proposed opening cash of this register (see the module
/// docs).
///
/// # Errors
/// `401`, `403 register_session_required`.
#[utoipa::path(get, path = "/api/v1/registers/me/float", tag = "cash",
    security(("bearer" = [], "register_token" = [])),
    responses((status = 200, body = ProposedFloatDto), (status = 403, body = crate::openapi::Problem)))]
pub async fn proposed_float(
    State(state): State<AppState>,
    caller: RegisterSession,
) -> Result<Json<ProposedFloatDto>, ApiError> {
    let register = caller.register.register;
    let last = cash::last_float_kept(&state.db, register).await?;
    Ok(Json(ProposedFloatDto {
        register_id: register.to_string(),
        proposed_float: sultan_core::cash::proposed_float(last).minor(),
        has_proposal: last.is_some(),
    }))
}
