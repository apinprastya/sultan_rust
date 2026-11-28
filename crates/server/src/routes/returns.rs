//! Returns and exchanges (spec 2.6 §8, D9, D11, D12; phase S3).
//!
//! - `PUT /returns/{id}` posts a return (client `UUIDv7`), optionally with the new sale of an
//!   exchange (`exchange: {sale_id, sale}`, posted in the same transaction). A PIN session at the
//!   register with an **open cashier session of the caller on this register**
//!   (`409 no_open_session`). The same body again → `200` with the stored return; another body →
//!   `409 conflict`. `201` on create.
//!   - **By receipt** (`sale_id`, each line names its `sale_line_no`): `refund.with_receipt`
//!     (overridable). Only a sale of this branch (`invalid_field`, `sale`; in P1 a sale of
//!     another branch is never on this node's branch), not voided (`409 conflict`), not imported
//!     (`invalid_field`, `sale`). Each line refunds what the customer paid for it
//!     (`core::sale::line_refund`, the last return takes what is left); more than sold − returned
//!     → `422 return_qty_exceeded`. Older than the stored `return_days` (default 30, 0 = no limit;
//!     `GET|PUT /settings/sales`, phase S6) →
//!     `403 return_window` until a manager override of `refund.with_receipt` is presented. The
//!     customer is the sale's (a sale without one may refund to the attached customer).
//!   - **Without receipt** (no `sale_id`): the refund is the current price for the attached
//!     customer (or walk-in), with tax; `refund.no_receipt` with its `MaxAmount` limit,
//!     overridable.
//!   - **Refund (D11):** `refund` is `cash` or `balance`. The part of the sale paid with the
//!     balance (not given back by earlier returns) goes back to the balance first; a refund to
//!     the balance stops at `max_balance` and the rest is cash; cash is rounded with the cash
//!     rounding setting.
//!   - **Exchange (D12):** the refund pays the new sale first (an `exchange` tender); the customer
//!     pays any difference with the sale's own tenders; what is left of the refund is paid back
//!     as above. The new sale is rebuilt and checked like `PUT /sales/{id}` (its overrides use
//!     the new sale's ID as `context_ref`), and it needs `sale.create`. Publishes `sale.posted`
//!     for it.
//!   - Lines are restocked, or written off with `write_off_reason` (D9).
//! - `GET /returns/{id}`, `GET /returns?…`: as for sales ([`super::sales`]).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::{Permission, Requested};
use sultan_core::catalog::Qty;
use sultan_core::customer::{Phone, ReceiptCustomer};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::ids::{ReturnId, SaleId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{self as core_pricing, LineMode};
use sultan_core::sale::{
    Label, ReceiptHeader, ReceiptWidth, ReturnReceiptInput, ReturnReceiptLine, ReturnReceiptTender,
    build_return_receipt,
};
use sultan_core::sale::{
    RefundInput, ReturnDisposition, ReturnedSoFar, SaleError, SoldLine, TenderKind, TotalsLine,
    check_return_window, compute_totals, ensure_returnable, line_refund, plan_refund,
    return_movements,
};
use sultan_db::sales::{self, NewReturn, RefundTender, Return, ReturnHeader, ReturnLine, Written};
use sultan_db::{cashier_sessions, customers, org, users};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::sale_post::{self, BuiltSale, SaleBody, body_hash, build_sale};
use super::sales::{DocQuery, RefundTenderDto, SaleDto, refund_method, refund_tender_types, run};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::AppState;

/// One returned line.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct ReturnLineBody {
    /// The sale line (by receipt), from 1.
    #[serde(default)]
    pub sale_line_no: Option<u32>,
    /// The variant (must match the sale line by receipt).
    pub variant_id: String,
    /// The unit (must match the sale line by receipt).
    pub unit_id: String,
    /// Quantity returned, thousandths (> 0).
    pub qty_thousandths: i64,
    /// Written off instead of restocked, with this reason (1–200 characters).
    #[serde(default)]
    pub write_off_reason: Option<String>,
}

/// The new sale of an exchange.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct ExchangeBody {
    /// The new sale's client `UUIDv7`.
    pub sale_id: String,
    /// The sale, as for `PUT /sales/{id}`; its tenders pay what the refund does not.
    pub sale: SaleBody,
}

/// Body of `PUT /returns/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, ToSchema)]
pub struct ReturnBody {
    /// The original sale (by receipt).
    #[serde(default)]
    pub sale_id: Option<String>,
    /// The attached customer (without a receipt, or a sale without a customer).
    #[serde(default)]
    pub customer_id: Option<String>,
    /// An optional note (at most 200 characters).
    #[serde(default)]
    pub reason: Option<String>,
    /// `cash` or `balance`.
    pub refund: String,
    /// The lines, 1–300.
    pub lines: Vec<ReturnLineBody>,
    /// An exchange sale in the same flow.
    #[serde(default)]
    pub exchange: Option<ExchangeBody>,
}

/// One stored return line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnLineDto {
    /// From 1.
    pub line_no: u32,
    /// The sale line.
    pub sale_line_no: Option<u32>,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The receipt name.
    pub name: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// The refund.
    pub refund: i64,
    /// `restock` or `write_off`.
    pub disposition: String,
    /// Why it was written off.
    pub write_off_reason: Option<String>,
}

/// A return's header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnHeaderDto {
    /// ID.
    pub id: String,
    /// Number, e.g. `JKT01-R1-RT-261116-0001`.
    pub number: String,
    /// `posted`.
    pub status: String,
    /// The original sale.
    pub sale_id: Option<String>,
    /// Branch.
    pub branch_id: String,
    /// Register.
    pub register_id: String,
    /// The session that paid it.
    pub cashier_session_id: String,
    /// Cashier.
    pub user_id: String,
    /// Customer.
    pub customer_id: Option<String>,
    /// The customer's name (phase S6).
    pub customer_name: Option<String>,
    /// The customer's member code.
    pub customer_code: Option<String>,
    /// The cashier's display name (phase S6).
    pub cashier_name: Option<String>,
    /// The register's code (phase S6).
    pub register_code: Option<String>,
    /// The note.
    pub reason: Option<String>,
    /// Σ line refunds.
    pub refund: i64,
    /// Cash rounding of the cash part.
    pub cash_rounding: i64,
    /// The new sale of an exchange.
    pub exchange_sale_id: Option<String>,
    /// Posted (unix ms).
    pub posted_at: i64,
}

impl From<&ReturnHeader> for ReturnHeaderDto {
    fn from(h: &ReturnHeader) -> Self {
        Self {
            id: h.id.to_string(),
            number: h.number.clone(),
            status: h.status.as_str().to_owned(),
            sale_id: h.sale.map(|s| s.to_string()),
            branch_id: h.branch.to_string(),
            register_id: h.register.to_string(),
            cashier_session_id: h.cashier_session.to_string(),
            user_id: h.user.to_string(),
            customer_id: h.customer.map(|c| c.to_string()),
            customer_name: h.customer_name.clone(),
            customer_code: h.customer_code.clone(),
            cashier_name: h.user_name.clone(),
            register_code: h.register_code.clone(),
            reason: h.reason.clone(),
            refund: h.refund.minor(),
            cash_rounding: h.cash_rounding.minor(),
            exchange_sale_id: h.exchange_sale.map(|s| s.to_string()),
            posted_at: h.posted_at.unix_millis(),
        }
    }
}

/// A return with its lines and refund tenders (and the exchange sale, when posted with it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnDto {
    /// The header.
    #[serde(flatten)]
    pub header: ReturnHeaderDto,
    /// The lines.
    pub lines: Vec<ReturnLineDto>,
    /// How it paid back: cash, balance, exchange.
    pub tenders: Vec<RefundTenderDto>,
    /// The exchange sale.
    pub exchange_sale: Option<SaleDto>,
}

fn return_dto(r: &Return, exchange: Option<SaleDto>) -> ReturnDto {
    ReturnDto {
        header: ReturnHeaderDto::from(&r.header),
        lines: r
            .lines
            .iter()
            .map(|l| {
                let (disposition, reason) = match &l.line.disposition {
                    ReturnDisposition::Restock => ("restock", None),
                    ReturnDisposition::WriteOff { reason } => ("write_off", Some(reason.clone())),
                };
                ReturnLineDto {
                    line_no: l.line_no,
                    sale_line_no: l.line.sale_line,
                    variant_id: l.line.variant.to_string(),
                    unit_id: l.line.unit.to_string(),
                    name: l.line.name.clone(),
                    qty_thousandths: l.line.qty.thousandths(),
                    refund: l.line.refund.minor(),
                    disposition: disposition.to_owned(),
                    write_off_reason: reason,
                }
            })
            .collect(),
        tenders: r.tenders.iter().map(RefundTenderDto::from).collect(),
        exchange_sale: exchange,
    }
}

async fn with_exchange(state: &AppState, r: &Return) -> Result<ReturnDto, ApiError> {
    let exchange = match r.header.exchange_sale {
        Some(id) => sales::get_sale(&state.db, id)
            .await?
            .map(|s| SaleDto::from(&s)),
        None => None,
    };
    Ok(return_dto(r, exchange))
}

fn disposition(line: &ReturnLineBody) -> Result<ReturnDisposition, ApiError> {
    Ok(match &line.write_off_reason {
        None => ReturnDisposition::Restock,
        Some(reason) => ReturnDisposition::WriteOff {
            reason: reason.clone(),
        }
        .validate()?,
    })
}

/// What the lines of a return come to before the refund is planned.
struct Gathered {
    lines: Vec<ReturnLine>,
    customer: Option<Uuid>,
    /// The part of the sale paid with the balance and not given back yet.
    balance_paid: Money,
    /// The permission and the forced return-window override.
    checks: Vec<(Permission, Option<Requested>, Option<ApiError>)>,
}

/// The lines of a return by receipt of `sale_id` (see the module docs).
async fn by_receipt(
    state: &AppState,
    till: sale_post::Till,
    sale_id: SaleId,
    body: &ReturnBody,
) -> Result<Gathered, ApiError> {
    let sale = sales::get_sale(&state.db, sale_id)
        .await?
        .ok_or_else(|| ApiError::not_found("sale"))?;
    let h = &sale.header;
    ensure_returnable(h.status, h.legacy_number.is_some())?;
    if h.branch != till.branch {
        return Err(ApiError::invalid_field(
            "sale",
            "only a sale of this branch can be returned by receipt",
        ));
    }
    let mut taken: Vec<(u32, Qty, Money)> = Vec::new();
    let mut lines = Vec::with_capacity(body.lines.len());
    for line in &body.lines {
        let no = line.sale_line_no.ok_or_else(|| {
            ApiError::invalid_field(
                "lines.sale_line_no",
                "a return by receipt names its sale line",
            )
        })?;
        let stored = sale.lines.iter().find(|l| l.line_no == no).ok_or_else(|| {
            ApiError::invalid_field("lines.sale_line_no", format!("the sale has no line {no}"))
        })?;
        let variant: VariantId = parse_id(&line.variant_id, "lines.variant_id")?;
        let unit: UnitId = parse_id(&line.unit_id, "lines.unit_id")?;
        if stored.line.variant != variant || stored.line.unit != unit {
            return Err(ApiError::invalid_field(
                "lines",
                format!("line {no} sold another variant or unit"),
            ));
        }
        let qty = Qty::from_thousandths(line.qty_thousandths);
        let (mut so_far_qty, mut so_far_refund) = (stored.returned.qty, stored.returned.refunded);
        for (n, q, r) in &taken {
            if *n == no {
                so_far_qty = so_far_qty
                    .checked_add(*q)
                    .ok_or(SaleError::AmountOverflow)?;
                so_far_refund = so_far_refund.saturating_add(*r);
            }
        }
        let refund = line_refund(
            SoldLine {
                qty: stored.line.qty,
                amount: stored.line.totals.total,
            },
            ReturnedSoFar {
                qty: so_far_qty,
                refunded: so_far_refund,
            },
            qty,
        )?;
        taken.push((no, qty, refund));
        lines.push(ReturnLine {
            sale_line: Some(no),
            variant,
            unit,
            name: stored.line.name.clone(),
            product_type: stored.line.product_type,
            factor: stored.line.factor,
            components: stored.line.components.clone(),
            qty,
            refund,
            disposition: disposition(line)?,
        });
    }
    let branch = org::get_branch(&state.db, h.branch).await?;
    let tz: BranchTimeZone = branch
        .and_then(|b| b.timezone.parse().ok())
        .unwrap_or_default();
    let mut checks = vec![(Permission::RefundWithReceipt, None, None)];
    let return_days = sales::return_days(&state.db).await?;
    if let Err(e) = check_return_window(h.posted_at, state.now(), tz, return_days) {
        checks.push((Permission::RefundWithReceipt, None, Some(ApiError::from(e))));
    }
    let paid_with_balance = sale
        .tenders
        .iter()
        .filter(|t| t.kind == TenderKind::CustomerBalance)
        .fold(Money::ZERO, |a, t| a.saturating_add(t.amount));
    let refunded = sale
        .lines
        .iter()
        .fold(Money::ZERO, |a, l| a.saturating_add(l.returned.refunded));
    let customer = match h.customer {
        Some(c) => Some(c),
        None => parse_opt_id(body.customer_id.as_deref(), "customer_id")?,
    };
    Ok(Gathered {
        lines,
        customer,
        balance_paid: paid_with_balance.saturating_sub(paid_with_balance.min(refunded)),
        checks,
    })
}

/// The lines of a return without a receipt: today's price for the attached customer, with tax.
async fn without_receipt(
    state: &AppState,
    till: sale_post::Till,
    body: &ReturnBody,
) -> Result<Gathered, ApiError> {
    let customer: Option<Uuid> = parse_opt_id(body.customer_id.as_deref(), "customer_id")?;
    if let Some(c) = customer {
        sale_post::customer(state, c).await?;
    }
    let mut wanted = Vec::with_capacity(body.lines.len());
    for line in &body.lines {
        if line.sale_line_no.is_some() {
            return Err(ApiError::invalid_field(
                "lines.sale_line_no",
                "a return without a receipt names no sale line",
            ));
        }
        wanted.push((
            parse_id::<VariantId>(&line.variant_id, "lines.variant_id")?,
            parse_id::<UnitId>(&line.unit_id, "lines.unit_id")?,
        ));
    }
    let items = sale_post::load_items(state, &wanted, "lines", true).await?;
    let ctx = sale_post::PriceContext::load(state, till.branch).await?;
    let mut quoted = Vec::with_capacity(items.len());
    for (item, line) in items.iter().zip(&body.lines) {
        let qty = Qty::from_thousandths(line.qty_thousandths);
        sultan_core::sale::check_qty(qty, item.unit_kind)?;
        quoted.push(core_pricing::CartLine {
            item: item.sale_item(),
            qty,
            mode: LineMode::Listed,
        });
    }
    let priced = sale_post::quote(state, till.branch, customer, &ctx, &quoted).await?;
    let mut totals_lines = Vec::with_capacity(items.len());
    for (i, (item, p)) in items.iter().zip(&priced).enumerate() {
        let p = p.clone().map_err(|e| ApiError::from(e).with("line", i))?;
        let gross = sultan_core::sale::no_receipt_refund(p.unit_price, quoted[i].qty)?;
        totals_lines.push(TotalsLine {
            gross,
            line_discount: None,
            cart_discountable: false,
            tax_rate_bp: ctx.rate(item.tax_class),
        });
    }
    let totals = compute_totals(&totals_lines, None, ctx.settings.tax)?;
    let mut lines = Vec::with_capacity(items.len());
    for ((item, line), t) in items.iter().zip(&body.lines).zip(&totals.lines) {
        lines.push(ReturnLine {
            sale_line: None,
            variant: item.variant,
            unit: item.unit,
            name: item.name.clone(),
            product_type: item.product_type,
            factor: item.factor,
            components: item.components.clone(),
            qty: Qty::from_thousandths(line.qty_thousandths),
            refund: t.total,
            disposition: disposition(line)?,
        });
    }
    Ok(Gathered {
        lines,
        customer,
        balance_paid: Money::ZERO,
        checks: vec![(
            Permission::RefundNoReceipt,
            Some(Requested::Amount(totals.total)),
            None,
        )],
    })
}

/// `PUT /api/v1/returns/{id}` (see the module docs).
///
/// # Errors
/// `403` permission codes / `return_window`, `404 not_found` (sale), `409 no_open_session` /
/// `conflict` / `cart_stale` (exchange), `422` rule codes.
#[utoipa::path(put, path = "/api/v1/returns/{id}", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7"),
        ("X-Override" = Option<String>, Header, description = "Override token(s), comma separated (spec 2.1 §6)")),
    request_body = ReturnBody,
    responses((status = 201, body = ReturnDto), (status = 200, body = ReturnDto),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
#[allow(clippy::too_many_lines)] // one pass: lines, exchange, refund plan, checks, post
pub async fn put(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ReturnBody>,
) -> Result<(StatusCode, Json<ReturnDto>), ApiError> {
    let id: ReturnId = parse_put_id(&id)?;
    let hash = body_hash(&body);
    {
        let mut tx = state.db.begin_read().await?;
        let stored = sales::replayed_return(&mut tx, id, &hash).await?;
        tx.commit().await?;
        if let Some(stored) = stored {
            return Ok((StatusCode::OK, Json(with_exchange(&state, &stored).await?)));
        }
    }
    if body.lines.is_empty() || body.lines.len() > sultan_core::sale::MAX_LINES {
        return Err(ApiError::invalid_field(
            "lines",
            "a return has 1 to 300 lines",
        ));
    }
    let method = refund_method(&body.refund)?;
    let access = Caller::load(&state.db, &caller.user).await?;
    if body.exchange.is_some() {
        // The exchange is a new sale: it needs what `PUT /sales/{id}` needs.
        access.require(Permission::SaleCreate)?;
    }
    let till = sale_post::till(&state, &caller).await?;
    let now = state.now();
    let sale_id: Option<SaleId> = parse_opt_id(body.sale_id.as_deref(), "sale_id")?;
    let lines = match sale_id {
        Some(s) => by_receipt(&state, till, s, &body).await?,
        None => without_receipt(&state, till, &body).await?,
    };
    let refund_total = lines
        .lines
        .iter()
        .try_fold(Money::ZERO, |a, l| a.checked_add(l.refund))
        .ok_or(SaleError::AmountOverflow)?;
    let exchange: Option<(SaleId, BuiltSale)> = match &body.exchange {
        Some(ex) => {
            let ex_id: SaleId = parse_put_id(&ex.sale_id)?;
            let built = build_sale(
                &state,
                till,
                ex_id,
                &ex.sale,
                Some(refund_total),
                body_hash(&ex.sale),
                now,
            )
            .await?;
            Some((ex_id, built))
        }
        None => None,
    };
    let exchange_used = exchange
        .as_ref()
        .map_or(Money::ZERO, |(_, b)| b.exchange_used);
    let ctx = sale_post::PriceContext::load(&state, till.branch).await?;
    let settings = sale_post::balance_settings(&state).await?;
    let balance = match lines.customer {
        Some(c) => Some(sale_post::customer(&state, c).await?.balance),
        None => None,
    };
    let plan = plan_refund(RefundInput {
        amount: refund_total.saturating_sub(exchange_used),
        balance_paid: lines.balance_paid,
        method,
        balance,
        settings,
        rounding: ctx.settings.cash_rounding,
    })?;
    let mut tenders = Vec::new();
    if exchange_used > Money::ZERO {
        tenders.push(RefundTender {
            tender_type: None,
            kind: TenderKind::Exchange,
            amount: exchange_used,
        });
    }
    tenders.extend(super::sales::refund_tenders(
        refund_tender_types(&state).await?,
        plan.cash,
        plan.payment_reversed.saturating_add(plan.to_balance),
    ));
    let mut movements = Vec::new();
    for l in &lines.lines {
        movements.extend(return_movements(&l.stock_line(), &l.disposition)?);
    }
    let new = NewReturn {
        id,
        request_hash: hash,
        sale: sale_id,
        branch: till.branch,
        register: till.register,
        cashier_session: till.session,
        user: till.user,
        customer: lines.customer,
        reason: body.reason.clone(),
        lines: lines.lines,
        cash_rounding: plan.cash_rounding,
        tenders,
        balance_entries: plan.balance_entries,
        movements,
        exchange: exchange.as_ref().map(|(_, b)| b.sale.clone()),
        posted_at: now,
    };
    let context = id.to_string();
    let exchange_context = exchange
        .as_ref()
        .map(|(ex_id, _)| ex_id.to_string())
        .unwrap_or_default();
    let checks = &lines.checks;
    let (state_ref, caller_ref, access_ref, header_ref) = (&state, &caller, &access, &header);
    let (written, stored) = run("return post", || {
        let mut new = new.clone();
        let exchange = exchange.clone();
        let (context, exchange_context) = (context.clone(), exchange_context.clone());
        async move {
            let mut tx = state_ref.db.begin().await?;
            let mut actions: Vec<(Action<'_>, Option<ApiError>)> = checks
                .iter()
                .map(|(permission, value, forced)| {
                    (
                        Action {
                            permission: *permission,
                            value: *value,
                            context_ref: &context,
                        },
                        forced.clone(),
                    )
                })
                .collect();
            let own = actions.len();
            if let Some((_, built)) = &exchange {
                actions.extend(built.actions(&exchange_context));
            }
            let approvers = overrides::authorize_all(
                state_ref,
                &mut tx,
                caller_ref,
                &access_ref.permissions,
                &actions,
                header_ref,
            )
            .await?;
            if let (Some((_, mut built)), Some(sale)) = (exchange, new.exchange.as_mut()) {
                built.set_approvers(&approvers[own..]);
                *sale = built.sale;
            }
            let (written, stored) = sales::post_return(&mut tx, &new, now).await?;
            tx.commit().await?;
            Ok((written, stored))
        }
    })
    .await?;
    let dto = with_exchange(&state, &stored).await?;
    if written == Written::Unchanged {
        return Ok((StatusCode::OK, Json(dto)));
    }
    super::stock::publish_doc_stock(&state, *stored.header.id.as_uuid()).await;
    if let Some(exchange) = stored.header.exchange_sale {
        super::stock::publish_doc_stock(&state, *exchange.as_uuid()).await;
    }
    if let Some(sale) = &dto.exchange_sale {
        state.events.publish(Event::for_branch(
            "sale.posted",
            till.branch,
            serde_json::json!({
                "sale_id": sale.header.id,
                "number": sale.header.number,
                "register_id": sale.header.register_id,
                "cashier_session_id": sale.header.cashier_session_id,
                "total": sale.header.total,
                "paid": sale.header.paid,
            }),
        ));
    }
    Ok((StatusCode::CREATED, Json(dto)))
}

/// `GET /api/v1/returns/{id}`: readable like a sale ([`super::sales::may_read`]).
///
/// # Errors
/// `401`, `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/returns/{id}", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Return ID")),
    responses((status = 200, body = ReturnDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ReturnDto>, ApiError> {
    let id: ReturnId = parse_id(&id, "id")?;
    let stored = readable(&state, &caller, id).await?;
    Ok(Json(with_exchange(&state, &stored).await?))
}

/// The stored return `id`, when the caller may read it ([`super::sales::may_read`]).
async fn readable(
    state: &AppState,
    caller: &UserSession,
    id: ReturnId,
) -> Result<Return, ApiError> {
    let stored = sales::get_return(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("return"))?;
    let access = Caller::load(&state.db, caller).await?;
    if !super::sales::may_read(
        &access,
        caller.user.id,
        Some(stored.header.user),
        stored.header.branch,
    ) {
        return Err(sultan_core::access::AccessError::PermissionDenied {
            permission: Permission::ReportSales,
        }
        .into());
    }
    Ok(stored)
}

/// One page of `GET /returns`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnPage {
    /// Newest first.
    pub items: Vec<ReturnHeaderDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

/// `GET /api/v1/returns?…`: as `GET /sales` ([`super::sales::list`]).
///
/// # Errors
/// `401`, `403 permission_denied`, `422 invalid_field` (`limit`).
#[utoipa::path(get, path = "/api/v1/returns", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(DocQuery),
    responses((status = 200, body = ReturnPage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    caller: UserSession,
    ApiQuery(query): ApiQuery<DocQuery>,
) -> Result<Json<ReturnPage>, ApiError> {
    let filter = super::sales::doc_filter(&state, &caller, &query).await?;
    let items = sales::list_returns(&state.db, &filter).await?;
    let next_before_id = (items.len() == filter.limit as usize)
        .then(|| items.last().map(|h| h.id.to_string()))
        .flatten();
    Ok(Json(ReturnPage {
        items: items.iter().map(ReturnHeaderDto::from).collect(),
        next_before_id,
    }))
}

/// Query of `GET /returns/{id}/receipt` (as for a sale's receipt).
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ReturnReceiptQuery {
    /// A reprint (printed COPY, audited as `return.reprint`).
    #[serde(default)]
    pub copy: bool,
    /// 32, 42 or 48 columns (default: the branch's receipt width).
    pub width: Option<usize>,
    /// The label language: `id` or `en` (default).
    pub lang: Option<String>,
}

/// The receipt of a return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnReceiptDto {
    /// The return.
    pub return_id: String,
    /// Its number (also the barcode/QR payload).
    pub number: String,
    /// A reprint.
    pub copy: bool,
    /// The text, one entry per line.
    pub lines: Vec<String>,
    /// The text with a newline after each line.
    pub text: String,
    /// The payload of the barcode/QR under the text.
    pub barcode: String,
}

/// Builds the receipt of `ret` (`core::sale::build_return_receipt`).
#[allow(clippy::too_many_lines)] // gathers every block of the return receipt
async fn return_receipt(
    state: &AppState,
    ret: &Return,
    width: ReceiptWidth,
    copy: bool,
    lang: &str,
) -> Result<ReturnReceiptDto, ApiError> {
    let h = &ret.header;
    let branch = org::get_branch(&state.db, h.branch)
        .await?
        .ok_or_else(|| ApiError::not_found("branch"))?;
    let tz: BranchTimeZone = branch.timezone.parse().unwrap_or_default();
    let register = org::get_register(&state.db, h.register)
        .await?
        .map(|r| r.code)
        .unwrap_or_default();
    let cashier = users::get_user(&state.db, h.user)
        .await?
        .map(|u| u.display_name)
        .unwrap_or_default();
    let customer = match h.customer {
        Some(c) => customers::get_customer(&state.db, c).await?,
        None => None,
    };
    let phone = customer
        .as_ref()
        .and_then(|c| c.customer.phone.as_deref())
        .and_then(|p| Phone::parse(p).ok());
    let customer_line = customer.as_ref().map(|c| ReceiptCustomer {
        name: &c.customer.name,
        company_name: c.customer.company_name.as_deref(),
        phone: phone.as_ref(),
        tier_name: None,
    });
    let sale_number = match h.sale {
        Some(s) => sales::get_sale(&state.db, s)
            .await?
            .map(|s| s.header.number),
        None => None,
    };
    let exchange_number = match h.exchange_sale {
        Some(s) => sales::get_sale(&state.db, s)
            .await?
            .map(|s| s.header.number),
        None => None,
    };
    let wanted: Vec<_> = ret
        .lines
        .iter()
        .map(|l| (l.line.variant, l.line.unit))
        .collect();
    let items = sale_post::load_items(state, &wanted, "lines", true).await?;
    let lines: Vec<ReturnReceiptLine<'_>> = ret
        .lines
        .iter()
        .zip(&items)
        .map(|(l, item)| ReturnReceiptLine {
            name: &l.line.name,
            qty: l.line.qty,
            unit: &item.unit_name,
            refund: l.line.refund,
            write_off: matches!(l.line.disposition, ReturnDisposition::WriteOff { .. }),
        })
        .collect();
    let mut names: Vec<String> = Vec::with_capacity(ret.tenders.len());
    for t in &ret.tenders {
        let name = match t.tender_type {
            Some(id) => sales::get_tender_type(&state.db, id)
                .await?
                .map(|r| r.tender.name)
                .unwrap_or_default(),
            None => Label::Exchange.text(lang).to_owned(),
        };
        names.push(name);
    }
    let tenders: Vec<ReturnReceiptTender<'_>> = ret
        .tenders
        .iter()
        .zip(&names)
        .map(|(t, name)| ReturnReceiptTender {
            name,
            amount: t.amount,
        })
        .collect();
    let header_lines: Vec<String> = branch.receipt_header.lines().map(str::to_owned).collect();
    let footer_lines: Vec<String> = branch.receipt_footer.lines().map(str::to_owned).collect();
    let input = ReturnReceiptInput {
        width,
        header: ReceiptHeader {
            lines: &header_lines,
            branch_name: &branch.name,
            address: &branch.address,
            tax_id: branch.npwp.as_deref(),
        },
        number: &h.number,
        posted_at: h.posted_at,
        tz,
        register: &register,
        cashier: &cashier,
        customer: customer_line,
        sale_number: sale_number.as_deref(),
        lines: &lines,
        cash_rounding: h.cash_rounding,
        refund: h.refund,
        tenders: &tenders,
        exchange_number: exchange_number.as_deref(),
        footer: &footer_lines,
        copy,
    };
    let built = build_return_receipt(&input, &|l| l.text(lang).to_owned());
    Ok(ReturnReceiptDto {
        return_id: h.id.to_string(),
        number: h.number.clone(),
        copy,
        text: built.text(),
        lines: built.lines,
        barcode: built.barcode,
    })
}

/// Whether `ret` is the last return of the caller's open session on their register.
async fn last_return_of_own_session(
    state: &AppState,
    caller: &UserSession,
    ret: &Return,
) -> Result<bool, ApiError> {
    let Some(register) = caller.session.register else {
        return Ok(false);
    };
    let Some(open) = cashier_sessions::open_for_register(&state.db, register).await? else {
        return Ok(false);
    };
    if open.session.user() != caller.user.id || ret.header.cashier_session != open.session.id() {
        return Ok(false);
    }
    let filter = sales::DocFilter {
        cashier_session: Some(open.session.id()),
        limit: 1,
        ..sales::DocFilter::default()
    };
    let last = sales::list_returns(&state.db, &filter).await?;
    Ok(last.first().is_some_and(|h| h.id == ret.header.id))
}

/// `GET /api/v1/returns/{id}/receipt?copy=&width=&lang=` (phase S6): the return receipt, built
/// like a sale's. Readable like the return; a copy follows the sale reprint rules (free for the
/// last return of the caller's open session, else `sale.reprint`, overridable at a register)
/// and is audited as `return.reprint`.
///
/// # Errors
/// `401`, `403 permission_denied` / `needs_override` / `invalid_override`, `404 not_found`,
/// `422 invalid_field` (`width`).
#[utoipa::path(get, path = "/api/v1/returns/{id}/receipt", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Return ID"), ReturnReceiptQuery,
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.reprint` (spec 2.1 §6)")),
    responses((status = 200, body = ReturnReceiptDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn get_receipt(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ReturnReceiptQuery>,
) -> Result<Json<ReturnReceiptDto>, ApiError> {
    let id: ReturnId = parse_id(&id, "id")?;
    let stored = readable(&state, &caller, id).await?;
    let width = super::sales::receipt_width(&state, query.width, stored.header.branch).await?;
    let dto = return_receipt(
        &state,
        &stored,
        width,
        query.copy,
        query.lang.as_deref().unwrap_or("en"),
    )
    .await?;
    if query.copy {
        let free = last_return_of_own_session(&state, &caller, &stored).await?;
        super::sales::authorize_reprint(
            &state,
            &caller,
            &header,
            super::sales::Reprint {
                free,
                action: "return.reprint",
                target: format!("return:{id}"),
                context_ref: id.to_string(),
                branch: stored.header.branch,
                after: serde_json::json!({"number": stored.header.number}),
            },
        )
        .await?;
    }
    Ok(Json(dto))
}
