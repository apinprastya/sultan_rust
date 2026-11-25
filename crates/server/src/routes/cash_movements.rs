//! Cash in / out / safe drop (spec 2.7 §1, §2, §11; phase K3) and their slips.
//!
//! - `PUT /cash-movements/{id} {kind, amount, reason_id?, note?, reverses?}` → `201`: a PIN
//!   session at the register with an **open cashier session of the caller on this register**
//!   (`409 no_open_session`). `core::cash` checks the movement (amount above 0, a reason of the
//!   kind's direction for in / out and none for a drop, an active reason, the note), and the
//!   permission: `cash.in`, `cash.out` (`MaxAmount` per movement) or `cash.drop`, all overridable
//!   at the till (`X-Override`, `context_ref` = the movement ID, value = the amount for
//!   `cash.out`). A **reversal** names `reverses` (a movement of the same open session, not a
//!   reversal, not reversed yet: `409 already_reversed`) and needs a note; it takes the original's
//!   kind, amount and reason (`kind` / `amount` may be left out; when sent they must match,
//!   `422 invalid_field` `reverses`) and its permission. The same ID with the same body again →
//!   `200` with the stored movement (even after the session closed) to whoever may read it (its
//!   poster, or `report.sales` at its branch; else `403 permission_denied`); another body →
//!   `409 conflict`. A lost race on the number or the reversal reruns the transaction (2.1 §5).
//!   The movement, its audit row `cash_movement.post` and its outbox row `cash_movement.posted`
//!   are one transaction. Publishes `cash.moved {session_id}` to the branch.
//! - `GET /cash-movements/{id}` and `GET /cash-movements?branch_id&register_id&cashier_session_id&
//!   kind&reason_id&from&until&missing_nota&before_id&limit`: newest first, keyset paging
//!   (`next_before_id`). The caller's own movements (or the list of their own session), or
//!   `report.sales` at the branch (without `branch_id` the list needs it at every branch). Each
//!   movement carries its photo IDs and the `missing_nota` flag (§2a, at the company's
//!   `nota_flag_above`).
//! - `GET /cash-movements/{id}/slip?lang=&width=&copy=`: the slip text (K7, `core::cash::
//!   build_slip`). A copy is free in the movement's own open session at its register; otherwise
//!   it needs `sale.reprint` (overridable at the till, `context_ref` = the movement ID) and is
//!   audited as `cash_movement.reprint`.
//! - `GET /customers/{id}/balance/payouts/{entry_id}/slip?lang=&width=&copy=`: the same layout for
//!   a balance payout (spec 2.5 §11): its entry ID as the number, the customer (name and masked
//!   phone) and the balance after it. Any user session (like the balance itself); a copy follows
//!   the cash slip's rule, audited as `customer.payout_reprint`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::{AccessError, BranchScope, Permission};
use sultan_core::cash::{CashKind, CashMovement, SlipInput, SlipKind, build_slip};
use sultan_core::customer::Phone;
use sultan_core::ids::{BranchId, CashMovementId, CashReasonId, CashierSessionId, RegisterId};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;
use sultan_db::cash::{self, CashFilter, CashMovementRecord, MAX_CASH_PAGE, NewCashMovement};
use sultan_db::customers::{self as cust, BalanceEntryKind};
use sultan_db::{cashier_sessions, org};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::cash::{Fail, TextContext, TextDto, TextQuery, run, user_name};
use super::sale_post::{body_hash, till};
use super::sales::{Reprint, authorize_reprint};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::AppState;

/// Body of `PUT /cash-movements/{id}`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct CashMovementBody {
    /// `in`, `out` or `drop`; may be left out for a reversal (it takes the original's).
    #[serde(default)]
    pub kind: Option<String>,
    /// Whole Rupiah above 0; may be left out for a reversal.
    #[serde(default)]
    pub amount: Option<i64>,
    /// The reason (in and out), from `GET /cash-reasons`; none for a drop or a reversal.
    #[serde(default)]
    pub reason_id: Option<String>,
    /// A note, at most 200 characters; required by some reasons and by a reversal.
    #[serde(default)]
    pub note: Option<String>,
    /// The movement this one reverses (K10).
    #[serde(default)]
    pub reverses: Option<String>,
}

/// A cash movement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CashMovementDto {
    /// ID.
    pub id: String,
    /// `<BRANCH>-<REGISTER>-K-<YYMMDD>-<NN>`, e.g. `JKT01-R2-K-261119-01`.
    pub number: String,
    /// `in`, `out` or `drop`.
    pub kind: String,
    /// The amount, above 0.
    pub amount: i64,
    /// What it did to the expected cash: `+amount` for in, `−amount` for out and drop, the
    /// opposite for a reversal.
    pub effect: i64,
    /// The reason (in and out).
    pub reason_id: Option<String>,
    /// The reason's current name.
    pub reason_name: Option<String>,
    /// The note.
    pub note: Option<String>,
    /// Branch.
    pub branch_id: String,
    /// Register.
    pub register_id: String,
    /// Cashier session.
    pub cashier_session_id: String,
    /// The cashier.
    pub user_id: String,
    /// The approving manager (override).
    pub approved_by: Option<String>,
    /// The movement this one reverses.
    pub reverses: Option<String>,
    /// Its number ("reversal of …").
    pub reverses_number: Option<String>,
    /// The movement that reverses this one.
    pub reversed_by: Option<String>,
    /// When it was posted (unix ms).
    pub posted_at: i64,
    /// Its nota photos, oldest first (§2a).
    pub photo_ids: Vec<String>,
    /// A cash out of at least `nota_flag_above` without a photo, not reversed (§2a).
    pub missing_nota: bool,
}

/// One page of `GET /cash-movements`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CashMovementPage {
    /// Newest first.
    pub items: Vec<CashMovementDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

/// Query of `GET /cash-movements`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct CashQuery {
    /// Only this branch.
    pub branch_id: Option<String>,
    /// Only this register.
    pub register_id: Option<String>,
    /// Only this cashier session.
    pub cashier_session_id: Option<String>,
    /// Only this kind: `in`, `out` or `drop`.
    pub kind: Option<String>,
    /// Only this reason.
    pub reason_id: Option<String>,
    /// Posted at or after (unix ms).
    pub from: Option<i64>,
    /// Posted before (unix ms).
    pub until: Option<i64>,
    /// Only cash outs missing a nota (§2a).
    #[serde(default)]
    pub missing_nota: bool,
    /// Page cursor: `next_before_id` of the previous page.
    pub before_id: Option<String>,
    /// Page size, 1–200 (default 50).
    pub limit: Option<u32>,
}

async fn dto(
    state: &AppState,
    record: &CashMovementRecord,
    flag: Money,
) -> Result<CashMovementDto, ApiError> {
    let m = &record.movement;
    let photos = cash::list_photos(&state.db, m.id).await?;
    Ok(CashMovementDto {
        id: m.id.to_string(),
        number: record.number.clone(),
        kind: m.kind.as_str().to_owned(),
        amount: m.amount.minor(),
        effect: m.effect().minor(),
        reason_id: m.reason.map(|r| r.to_string()),
        reason_name: record.reason_name.clone(),
        note: m.note.clone(),
        branch_id: record.branch.to_string(),
        register_id: record.register.to_string(),
        cashier_session_id: m.session.to_string(),
        user_id: record.user.to_string(),
        approved_by: record.approved_by.map(|u| u.to_string()),
        reverses: m.reverses.map(|r| r.to_string()),
        reverses_number: record.reverses_number.clone(),
        reversed_by: record.reversed_by.map(|r| r.to_string()),
        posted_at: record.posted_at.unix_millis(),
        photo_ids: photos.iter().map(|p| p.id.to_string()).collect(),
        missing_nota: record.missing_nota(flag),
    })
}

async fn one_dto(
    state: &AppState,
    record: &CashMovementRecord,
) -> Result<CashMovementDto, ApiError> {
    let flag = cash::nota_flag_above(&state.db).await?;
    dto(state, record, flag).await
}

fn kind(value: &str) -> Result<CashKind, ApiError> {
    CashKind::from_key(value)
        .ok_or_else(|| ApiError::invalid_field("kind", "`kind` is `in`, `out` or `drop`"))
}

/// The movement of a `PUT` body for the caller's open `session` (see the module docs).
async fn build(
    state: &AppState,
    id: CashMovementId,
    session: CashierSessionId,
    body: &CashMovementBody,
) -> Result<CashMovement, ApiError> {
    if let Some(target) = &body.reverses {
        let target: CashMovementId = parse_id(target, "reverses")?;
        let original = cash::get_cash_movement(&state.db, target)
            .await?
            .ok_or_else(|| ApiError::invalid_field("reverses", "no such cash movement"))?;
        let o = &original.movement;
        let differs = body.kind.as_deref().is_some_and(|k| k != o.kind.as_str())
            || body.amount.is_some_and(|a| a != o.amount.minor())
            || body
                .reason_id
                .as_deref()
                .is_some_and(|r| Some(r.to_owned()) != o.reason.map(|x| x.to_string()));
        if differs {
            return Err(ApiError::invalid_field(
                "reverses",
                "a reversal has the original's kind, amount and reason",
            ));
        }
        return Ok(CashMovement::reversal(
            id,
            session,
            o,
            original.reversed_by.is_some(),
            body.note.as_deref(),
        )?);
    }
    let kind = kind(
        body.kind
            .as_deref()
            .ok_or_else(|| ApiError::invalid_field("kind", "`kind` is required"))?,
    )?;
    let amount = Money::from_minor(
        body.amount
            .ok_or_else(|| ApiError::invalid_field("amount", "`amount` is required"))?,
    );
    let reason = match &body.reason_id {
        Some(r) => {
            let id: CashReasonId = parse_id(r, "reason_id")?;
            Some(
                cash::get_cash_reason(&state.db, id)
                    .await?
                    .ok_or_else(|| ApiError::invalid_field("reason", "no such cash reason"))?
                    .for_rules(),
            )
        }
        None => None,
    };
    Ok(CashMovement::new(
        id,
        session,
        kind,
        amount,
        reason.as_ref(),
        body.note.as_deref(),
    )?)
}

/// `PUT /api/v1/cash-movements/{id}`: posts a cash in / out / drop (see the module docs).
///
/// # Errors
/// `403 register_session_required` / `permission_denied` / `needs_override` /
/// `invalid_override`, `409 no_open_session` / `already_reversed` / `conflict`,
/// `422 invalid_field` / `amount_overflow`.
#[utoipa::path(put, path = "/api/v1/cash-movements/{id}", tag = "cash",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7"),
        ("X-Override" = Option<String>, Header, description = "Override token for `cash.in` / `cash.out` / `cash.drop` (context: the movement ID)")),
    request_body = CashMovementBody,
    responses((status = 201, body = CashMovementDto), (status = 200, body = CashMovementDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CashMovementBody>,
) -> Result<(StatusCode, Json<CashMovementDto>), ApiError> {
    let id: CashMovementId = parse_put_id(&id)?;
    let hash = body_hash(&body);
    let mut tx = state.db.begin_read().await?;
    let replayed = cash::replayed_cash_movement(&mut tx, id, &hash).await?;
    tx.commit().await?;
    if let Some(stored) = replayed {
        let stored = may_read(&state, &caller.user, stored).await?;
        return Ok((StatusCode::OK, Json(one_dto(&state, &stored).await?)));
    }
    let till = till(&state, &caller).await?;
    let access = Caller::load(&state.db, &caller.user).await?;
    let movement = build(&state, id, till.session, &body).await?;
    let now = state.now();
    let context = id.to_string();
    let (state_ref, caller_ref, access_ref, header_ref) = (&state, &caller, &access, &header);
    let (movement_ref, context_ref, hash_ref) = (&movement, context.as_str(), &hash);
    let (written, stored) = run("cash movement post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let approved_by = overrides::authorize(
            state_ref,
            &mut tx,
            caller_ref,
            &access_ref.permissions,
            Action {
                permission: movement_ref.kind.permission(),
                value: movement_ref.kind.requested(movement_ref.amount),
                context_ref,
            },
            header_ref,
        )
        .await?;
        let new = NewCashMovement {
            movement: movement_ref.clone(),
            branch: till.branch,
            register: till.register,
            user: till.user,
            approved_by,
            posted_at: now,
            request_hash: *hash_ref,
        };
        let (written, stored) = cash::post_cash_movement(&mut tx, &new, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored))
    })
    .await?;
    let dto = one_dto(&state, &stored).await?;
    if written == cash::Written::Unchanged {
        return Ok((StatusCode::OK, Json(dto)));
    }
    state.events.publish(Event::for_branch(
        "cash.moved",
        stored.branch,
        json!({
            "session_id": stored.movement.session.to_string(),
            "movement_id": dto.id,
            "number": dto.number,
            "kind": dto.kind,
        }),
    ));
    Ok((StatusCode::CREATED, Json(dto)))
}

fn denied() -> ApiError {
    AccessError::PermissionDenied {
        permission: Permission::ReportSales,
    }
    .into()
}

/// The movement `id` if `caller` may read it: their own, or `report.sales` at its branch.
pub(crate) async fn readable(
    state: &AppState,
    caller: &UserSession,
    id: &str,
) -> Result<CashMovementRecord, ApiError> {
    let id: CashMovementId = parse_id(id, "id")?;
    let record = cash::get_cash_movement(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("cash movement"))?;
    may_read(state, caller, record).await
}

/// `record` when `caller` may read it: its own poster, or `report.sales` at its branch. A replayed
/// `PUT` answers through this too, so knowing a movement's ID and body is not a way around it.
async fn may_read(
    state: &AppState,
    caller: &UserSession,
    record: CashMovementRecord,
) -> Result<CashMovementRecord, ApiError> {
    if record.user == caller.user.id {
        return Ok(record);
    }
    let access = Caller::load(&state.db, caller).await?;
    if access
        .at_scope(BranchScope::Branch(record.branch))
        .contains(Permission::ReportSales)
    {
        Ok(record)
    } else {
        Err(denied())
    }
}

/// `GET /api/v1/cash-movements/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/cash-movements/{id}", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cash movement ID")),
    responses((status = 200, body = CashMovementDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
) -> Result<Json<CashMovementDto>, ApiError> {
    let record = readable(&state, &caller, &id).await?;
    Ok(Json(one_dto(&state, &record).await?))
}

/// `GET /api/v1/cash-movements?…` (see the module docs).
///
/// # Errors
/// `401`, `400 invalid_body` / `invalid_query`, `403 permission_denied`, `422 invalid_field`
/// (`kind`, `limit`).
#[utoipa::path(get, path = "/api/v1/cash-movements", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(CashQuery),
    responses((status = 200, body = CashMovementPage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    caller: UserSession,
    ApiQuery(query): ApiQuery<CashQuery>,
) -> Result<Json<CashMovementPage>, ApiError> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_CASH_PAGE).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {MAX_CASH_PAGE}"),
        ));
    }
    let flag = cash::nota_flag_above(&state.db).await?;
    let filter = CashFilter {
        branch: parse_opt_id::<BranchId>(query.branch_id.as_deref(), "branch_id")?,
        register: parse_opt_id::<RegisterId>(query.register_id.as_deref(), "register_id")?,
        cashier_session: parse_opt_id::<CashierSessionId>(
            query.cashier_session_id.as_deref(),
            "cashier_session_id",
        )?,
        kind: query.kind.as_deref().map(kind).transpose()?,
        reason: parse_opt_id::<CashReasonId>(query.reason_id.as_deref(), "reason_id")?,
        from: query.from.map(Timestamp::from_unix_millis),
        until: query.until.map(Timestamp::from_unix_millis),
        missing_nota_above: query.missing_nota.then_some(flag),
        before: parse_opt_id::<CashMovementId>(query.before_id.as_deref(), "before_id")?,
        limit,
    };
    let own_session = match filter.cashier_session {
        Some(session) => cashier_sessions::get(&state.db, session)
            .await?
            .is_some_and(|r| r.session.user() == caller.user.id),
        None => false,
    };
    if !own_session {
        let access = Caller::load(&state.db, &caller).await?;
        let allowed = match filter.branch {
            Some(b) => access
                .at_scope(BranchScope::Branch(b))
                .contains(Permission::ReportSales),
            None => access.holds_everywhere(Permission::ReportSales),
        };
        if !allowed {
            return Err(denied());
        }
    }
    let records = cash::list_cash_movements(&state.db, &filter).await?;
    let mut items = Vec::with_capacity(records.len());
    for record in &records {
        items.push(dto(&state, record, flag).await?);
    }
    let next_before_id = (records.len() == limit as usize)
        .then(|| records.last().map(|r| r.movement.id.to_string()))
        .flatten();
    Ok(Json(CashMovementPage {
        items,
        next_before_id,
    }))
}

/// Whether `session` is the caller's open session at the register they call from (a copy of its
/// slips is free).
async fn own_open_session(
    state: &AppState,
    caller: &UserSession,
    session: Option<CashierSessionId>,
) -> Result<bool, ApiError> {
    let (Some(register), Some(session)) = (caller.session.register, session) else {
        return Ok(false);
    };
    Ok(cashier_sessions::open_for_register(&state.db, register)
        .await?
        .is_some_and(|open| open.session.id() == session && open.session.user() == caller.user.id))
}

/// The register code, empty when unknown.
async fn register_code(state: &AppState, register: Option<RegisterId>) -> Result<String, ApiError> {
    Ok(match register {
        Some(r) => org::get_register(&state.db, r)
            .await?
            .map(|r| r.code)
            .unwrap_or_default(),
        None => String::new(),
    })
}

/// `GET /api/v1/cash-movements/{id}/slip?lang=&width=&copy=` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied` / `needs_override` / `invalid_override`, `404 not_found`,
/// `422 invalid_field` (`width`).
#[utoipa::path(get, path = "/api/v1/cash-movements/{id}/slip", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Cash movement ID"), TextQuery,
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.reprint` (context: the movement ID)")),
    responses((status = 200, body = TextDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn slip(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<TextQuery>,
) -> Result<Json<TextDto>, ApiError> {
    let record = readable(&state, &caller, &id).await?;
    let m = &record.movement;
    let ctx = TextContext::load(&state, record.branch, &query).await?;
    let register = register_code(&state, Some(record.register)).await?;
    let cashier = user_name(&state, Some(record.user))
        .await?
        .unwrap_or_default();
    let approver = user_name(&state, record.approved_by).await?;
    let printout = ctx.build(|opts| {
        build_slip(
            &SlipInput {
                header: ctx.header(),
                kind: SlipKind::from(m.kind),
                number: &record.number,
                posted_at: record.posted_at,
                register: &register,
                cashier: &cashier,
                approved_by: approver.as_deref(),
                reversal_of: record.reverses_number.as_deref(),
                reason: record.reason_name.as_deref(),
                note: m.note.as_deref(),
                customer: None,
                balance_after: None,
                amount: m.amount,
                copy: query.copy,
            },
            opts,
        )
    });
    if query.copy {
        let free = own_open_session(&state, &caller, Some(m.session)).await?;
        authorize_reprint(
            &state,
            &caller,
            &header,
            Reprint {
                free,
                action: "cash_movement.reprint",
                target: format!("cash_movement:{}", m.id),
                context_ref: m.id.to_string(),
                branch: record.branch,
                after: json!({ "number": record.number }),
            },
        )
        .await?;
    }
    Ok(Json(TextDto::new(&printout, query.copy)))
}

/// `GET /api/v1/customers/{id}/balance/payouts/{entry_id}/slip?lang=&width=&copy=` (see the
/// module docs).
///
/// # Errors
/// `401`, `403 needs_override` / `invalid_override` / `permission_denied` (a copy),
/// `404 not_found` (unknown customer, or the entry is not a payout of it), `422 invalid_field`.
#[utoipa::path(get, path = "/api/v1/customers/{id}/balance/payouts/{entry_id}/slip", tag = "cash",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Customer ID"),
        ("entry_id" = String, Path, description = "The payout's balance entry ID"), TextQuery,
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.reprint` (context: the entry ID)")),
    responses((status = 200, body = TextDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn payout_slip(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    Path((customer_id, entry_id)): Path<(String, String)>,
    ApiQuery(query): ApiQuery<TextQuery>,
) -> Result<Json<TextDto>, ApiError> {
    let customer_id: Uuid = parse_id(&customer_id, "id")?;
    let entry_id: Uuid = parse_id(&entry_id, "entry_id")?;
    let entry = cust::get_balance_entry(&state.db, entry_id)
        .await?
        .filter(|e| e.customer == customer_id && e.kind == BalanceEntryKind::Payout)
        .ok_or_else(|| ApiError::not_found("payout"))?;
    let customer = cust::get_customer(&state.db, customer_id)
        .await?
        .ok_or_else(|| ApiError::not_found("customer"))?;
    let branch = entry.branch.unwrap_or(caller.session.branch);
    let ctx = TextContext::load(&state, branch, &query).await?;
    let register = register_code(&state, entry.register).await?;
    let cashier = user_name(&state, entry.user).await?.unwrap_or_default();
    let balance = cust::balance_after(&state.db, customer_id, entry_id).await?;
    let c = &customer.customer;
    let line = match c.phone.as_deref().and_then(|p| Phone::parse(p).ok()) {
        Some(phone) => format!("{} · {}", c.name, phone.masked()),
        None => c.name.clone(),
    };
    let number = entry.id.to_string();
    let printout = ctx.build(|opts| {
        build_slip(
            &SlipInput {
                header: ctx.header(),
                kind: SlipKind::Payout,
                number: &number,
                posted_at: entry.occurred_at,
                register: &register,
                cashier: &cashier,
                approved_by: None,
                reversal_of: None,
                reason: None,
                note: None,
                customer: Some(&line),
                balance_after: Some(balance),
                amount: Money::from_minor(entry.amount.minor().saturating_abs()),
                copy: query.copy,
            },
            opts,
        )
    });
    if query.copy {
        let free = own_open_session(&state, &caller, entry.cashier_session).await?;
        authorize_reprint(
            &state,
            &caller,
            &header,
            Reprint {
                free,
                action: "customer.payout_reprint",
                target: format!("customer:{customer_id}"),
                context_ref: entry_id.to_string(),
                branch,
                after: json!({ "entry_id": entry_id.to_string() }),
            },
        )
        .await?;
    }
    Ok(Json(TextDto::new(&printout, query.copy)))
}
