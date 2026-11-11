//! The customer balance (saved change, spec 2.5 §11, §13; phase M3).
//!
//! - `GET /customers/{id}/balance?before_id=&limit=`: the cached balance and a page of ledger
//!   entries, newest first; any user session.
//! - `POST /customers/{id}/balance/payout {amount}` with `Idempotency-Key`: a manager pays the
//!   balance out in cash at the till. Needs a PIN session at the register
//!   (`403 register_session_required`), `customer.balance_payout` at its branch (not
//!   overridable, spec 2.5 §18.6) and an **open cashier session of the caller on this register**
//!   (`409 no_open_session`). `amount` > 0 pays out a positive balance (`0 < amount ≤ balance`);
//!   `amount` < 0 writes off a negative balance (`balance ≤ amount < 0`) and needs the Owner role at
//!   the branch. Anything else is `422 insufficient_balance` (`core::customer::payout`). One
//!   transaction appends the `payout` entry (with the cashier session, register, branch and user;
//!   outbox `customer.balance_entry`), updates the cached balance and writes the audit row
//!   `customer.balance_payout`. A positive payout is cash out of the drawer: the session's expected
//!   cash and its close report's `cash_out` include it ([`crate::movements::session_movements`]);
//!   a write-off moves no cash. Works on a branch node too: balance entries are transactional
//!   data that a branch creates (spec 2.5 §12). Publishes `customer.changed`.
//! - `GET /settings/customer-balance` (any session) and `PUT` (`customer.manage`, HQ only;
//!   negative limits are `422 invalid_field`).

use axum::Json;
use axum::extract::{Path, State};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::Permission;
use sultan_core::cashier_session::SessionError;
use sultan_core::customer::{BalanceSettings, CustomerError, payout as check_payout};
use sultan_core::money::Money;
use sultan_db::customers::{
    self as cust, BalanceEntry, BalanceEntryKind, CustomerBalanceSettings, CustomerDbError, Written,
};
use sultan_db::{audit, cashier_sessions, seed};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::catalog::actor;
use super::customers::{balance_json, publish_changed, run};
use super::{audit_entry, parse_id, parse_opt_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, RegisterSession, RequireHq, UserSession};
use crate::state::AppState;

/// One balance ledger entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BalanceEntryDto {
    /// ID (`UUIDv7`); also the page cursor.
    pub id: String,
    /// The customer.
    pub customer_id: String,
    /// `change_saved`, `payment`, `change_saved_reversed`, `payment_reversed`, `payout`,
    /// `merge_out` or `merge_in`.
    pub kind: String,
    /// Signed amount added to the balance, minor units.
    pub amount: i64,
    /// Branch.
    pub branch_id: Option<String>,
    /// Register (till entries).
    pub register_id: Option<String>,
    /// Acting user.
    pub user_id: Option<String>,
    /// Cashier session (till entries).
    pub cashier_session_id: Option<String>,
    /// Sale or payout document.
    pub document_id: Option<String>,
    /// When it happened (unix ms).
    pub occurred_at: i64,
}

impl From<&BalanceEntry> for BalanceEntryDto {
    fn from(e: &BalanceEntry) -> Self {
        let id = |u: Option<Uuid>| u.map(|u| u.to_string());
        Self {
            id: e.id.to_string(),
            customer_id: e.customer.to_string(),
            kind: e.kind.as_str().to_owned(),
            amount: e.amount.minor(),
            branch_id: e.branch.map(|b| b.to_string()),
            register_id: e.register.map(|r| r.to_string()),
            user_id: e.user.map(|u| u.to_string()),
            cashier_session_id: e.cashier_session.map(|s| s.to_string()),
            document_id: id(e.document),
            occurred_at: e.occurred_at.unix_millis(),
        }
    }
}

/// `GET /customers/{id}/balance`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct BalanceDto {
    /// The customer.
    pub customer_id: String,
    /// The balance, minor units (may be negative after offline spending, spec 2.5 §11).
    pub balance: i64,
    /// Entries, newest first.
    pub entries: Vec<BalanceEntryDto>,
    /// Pass as `before_id` for the next page; `null` when this page was not full.
    pub next_before_id: Option<String>,
}

/// Query of `GET /customers/{id}/balance`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct BalanceQuery {
    /// Page cursor: the previous page's `next_before_id`.
    pub before_id: Option<String>,
    /// Entries per page, 1–100 (default 50).
    pub limit: Option<u32>,
}

/// `GET /api/v1/customers/{id}/balance`: the balance and its movements (spec 2.5 §11).
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`, `422 invalid_field` (`limit`).
#[utoipa::path(get, path = "/api/v1/customers/{id}/balance", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Customer ID"), BalanceQuery),
    responses((status = 200, body = BalanceDto), (status = 404, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<BalanceQuery>,
) -> Result<Json<BalanceDto>, ApiError> {
    let id: Uuid = parse_id(&id, "id")?;
    let limit = query.limit.unwrap_or(50);
    if !(1..=cust::MAX_ENTRY_PAGE).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {}", cust::MAX_ENTRY_PAGE),
        ));
    }
    let before = parse_opt_id(query.before_id.as_deref(), "before_id")?;
    let page = cust::get_balance(&state.db, id, before, limit)
        .await?
        .ok_or_else(|| ApiError::not_found("customer"))?;
    let full = page.entries.len() == limit as usize;
    Ok(Json(BalanceDto {
        customer_id: id.to_string(),
        balance: page.balance.minor(),
        next_before_id: page
            .entries
            .last()
            .filter(|_| full)
            .map(|e| e.id.to_string()),
        entries: page.entries.iter().map(BalanceEntryDto::from).collect(),
    }))
}

/// Body of `POST /customers/{id}/balance/payout`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct PayoutBody {
    /// Cash paid out, minor units: `0 < amount ≤ balance`, or a negative amount to write off a
    /// negative balance (Owner only).
    pub amount: i64,
}

/// The answer of a payout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PayoutDto {
    /// The `payout` entry (its amount is −`amount`).
    pub entry: BalanceEntryDto,
    /// The balance after the payout.
    pub balance: i64,
}

/// `POST /api/v1/customers/{id}/balance/payout`: pays the balance out in cash (see the module
/// docs).
///
/// # Errors
/// `401`, `400 invalid_body`, `403 register_session_required` / `permission_denied`
/// (`customer.balance_payout`; the Owner role for a write-off), `404 not_found` (unknown or
/// erased customer), `409 no_open_session`, `422 insufficient_balance` (with `balance`) /
/// `invalid_field` (`amount` 0).
#[utoipa::path(post, path = "/api/v1/customers/{id}/balance/payout", tag = "customers",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Customer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = PayoutBody,
    responses((status = 200, body = PayoutDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn payout(
    State(state): State<AppState>,
    caller: RegisterSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PayoutBody>,
) -> Result<Json<PayoutDto>, ApiError> {
    let id: Uuid = parse_id(&id, "id")?;
    let access = Caller::load(&state.db, &caller.user).await?;
    access.require(Permission::CustomerBalancePayout)?;
    if body.amount < 0 && !access.has_role_at(seed::OWNER_ROLE, caller.register.branch) {
        return Err(ApiError::new(
            axum::http::StatusCode::FORBIDDEN,
            "permission_denied",
            "writing off a negative balance is for the Owner only",
        )
        .with("permission", Permission::CustomerBalancePayout.as_str())
        .with("reason", "owner_only"));
    }
    let amount = Money::from_minor(body.amount);
    let (state, caller, now) = (&state, &caller, state.now());
    let (entry, balance) = run(|| async move {
        let mut tx = state.db.begin().await?;
        let session = cashier_sessions::open_for_register(&mut tx, caller.register.register)
            .await?
            .filter(|s| s.session.user() == caller.user.user.id)
            .ok_or_else(|| ApiError::from(SessionError::NoOpenSession))?;
        let customer = cust::get_customer(&mut tx, id)
            .await?
            .filter(|c| c.erased_at.is_none())
            .ok_or(CustomerDbError::NotFound("customer"))?;
        let checked = check_payout(customer.balance, amount)?;
        let entry = BalanceEntry {
            branch: Some(caller.register.branch),
            register: Some(caller.register.register),
            user: Some(caller.user.user.id),
            cashier_session: Some(session.session.id()),
            ..BalanceEntry::new(id, BalanceEntryKind::Payout, checked.amount(), now)
        };
        let (_, balance) = cust::append_balance_entry(&mut tx, &entry, now).await?;
        // A concurrent payout may have moved the balance after the check: never past 0.
        let before = Money::from_minor(balance.minor() - entry.amount.minor());
        if (amount.minor() > 0 && balance.is_negative())
            || (amount.minor() < 0 && balance > Money::ZERO)
        {
            return Err(CustomerError::InsufficientBalance { balance: before }.into());
        }
        let mut after = balance_json(balance.minor());
        after["amount"] = json!(amount.minor());
        after["entry_id"] = json!(entry.id.to_string());
        after["cashier_session_id"] = json!(session.session.id().to_string());
        audit::append(
            &mut tx,
            &audit_entry(
                &caller.user,
                now,
                "customer.balance_payout",
                format!("customer:{id}"),
                Some(balance_json(before.minor())),
                Some(after),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok((entry, balance))
    })
    .await?;
    publish_changed(state, Written::Updated, id);
    Ok(Json(PayoutDto {
        entry: BalanceEntryDto::from(&entry),
        balance: balance.minor(),
    }))
}

/// The balance settings (`GET/PUT /settings/customer-balance`, spec 2.5 §11).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct BalanceSettingsDto {
    /// Most change saved on one sale, minor units (default 5 000; 0 turns saving change off).
    pub max_saved_per_sale: i64,
    /// Most balance a customer may reach through saved change (default 100 000).
    pub max_balance: i64,
}

impl From<CustomerBalanceSettings> for BalanceSettingsDto {
    fn from(s: CustomerBalanceSettings) -> Self {
        Self {
            max_saved_per_sale: s.max_saved_per_sale.minor(),
            max_balance: s.max_balance.minor(),
        }
    }
}

/// `GET /api/v1/settings/customer-balance`.
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/settings/customer-balance", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    responses((status = 200, body = BalanceSettingsDto), (status = 401, body = crate::openapi::Problem)))]
pub async fn get_settings(
    State(state): State<AppState>,
    _session: UserSession,
) -> Result<Json<BalanceSettingsDto>, ApiError> {
    Ok(Json(cust::get_balance_settings(&state.db).await?.into()))
}

/// `PUT /api/v1/settings/customer-balance`: replaces the limits; a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied` (`customer.manage`), `422 invalid_field`
/// (a negative limit).
#[utoipa::path(put, path = "/api/v1/settings/customer-balance", tag = "customers",
    security(("bearer" = [])), request_body = BalanceSettingsDto,
    responses((status = 200, body = BalanceSettingsDto), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_settings(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    ApiJson(body): ApiJson<BalanceSettingsDto>,
) -> Result<Json<BalanceSettingsDto>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require(Permission::CustomerManage)?;
    let checked = BalanceSettings {
        max_saved_per_sale: Money::from_minor(body.max_saved_per_sale),
        max_balance: Money::from_minor(body.max_balance),
    }
    .validate()?;
    let settings = CustomerBalanceSettings {
        max_saved_per_sale: checked.max_saved_per_sale,
        max_balance: checked.max_balance,
    };
    let (state, actor, settings, now) = (&state, &actor(&session), &settings, state.now());
    let stored = run(|| async move {
        let mut tx = state.db.begin().await?;
        cust::put_balance_settings(&mut tx, settings, actor, now).await?;
        let stored = cust::get_balance_settings(&mut tx).await?;
        tx.commit().await?;
        Ok(stored)
    })
    .await?;
    Ok(Json(stored.into()))
}
