//! Sales at the register (spec 2.6 §5, §7, §9, §12; phase S3).
//!
//! - `PUT /sales/{id}` posts a sale (client `UUIDv7`). A PIN session at the register
//!   (`403 register_session_required`) with `sale.create` and an **open cashier session of the
//!   caller on this register** (`409 no_open_session`). The server rebuilds the cart
//!   ([`super::sale_post`]): quote, totals (`409 cart_stale` with the fresh quote), the limited
//!   permissions with `X-Override` (several approvals: comma-separated tokens), tenders and saved
//!   change. The number is allocated in the posting transaction, which reruns after a lost race
//!   (2.1 §5, [`crate::retry`]). The same body again → `200` with the stored sale (checked before
//!   anything else, so a replay never turns stale); another body → `409 conflict`. `201` on
//!   create. Publishes `sale.posted` (branch audience).
//! - `GET /sales/{id}`, `GET /sales/by-number/{number}`: the sale with its lines, tenders and
//!   post-void. Readable by its cashier, and by anyone holding `sale.create` or `report.sales`
//!   at the sale's branch (till staff look receipts up for returns, §8).
//! - `GET /sales?…`: headers, newest first, keyset paging (`before_id`, `limit` 1–200, default
//!   50). Filters `branch_id`, `register_id`, `cashier_session_id`, `customer_id`, `from` /
//!   `until` (posting time, unix ms), `number`. Needs `report.sales` at `branch_id` (or
//!   everywhere when no branch is given), except for a list of the caller's own cashier session
//!   (`cashier_session_id`), which the till's last receipts use (F6).
//! - `GET /sales/{id}/receipt?copy=&width=&lang=`: the receipt text (`core::sale::build_receipt`,
//!   32/42/48 columns, default 32; English labels, Indonesian with `lang=id`). A voided sale prints VOID. `copy=true` prints
//!   COPY and is audited (`sale.reprint`): the last sale of the caller's open session on this
//!   register needs no permission, any other needs `sale.reprint` (overridable at the till).
//! - `POST /sales/{id}/void {reason, reason_text?, refund}` with `Idempotency-Key`: the post-void
//!   (§7, D10, D11). `sale.post_void` (overridable), an open session of the caller on this
//!   register, a sale of this branch (`invalid_field`, `sale`), not imported. `refund` is `cash`
//!   or `balance` (the sale's customer only). `409 has_returns`, `409 conflict` when voided.
//! - `POST /cart-voids {kind, cart_id, reason, reason_text?, lines}` with `Idempotency-Key`: a
//!   removed line (`line`, `sale.void_line`) or a cleared cart (`cart`, `sale.void`), both
//!   overridable with `context_ref` = `cart_id`. Line and cart voids happen before posting (D1),
//!   so the till reports each one here; the server writes the audit entry `sale.void_line` /
//!   `sale.void_cart` on the open cashier session, which the close report counts (§10).

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::{BranchScope, Permission};
use sultan_core::customer::{Phone, ReceiptCustomer};
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::ids::{BranchId, CashierSessionId, RegisterId, SaleId, UserId, VariantId};
use sultan_core::money::Money;
use sultan_core::sale::{
    Label, PriceMode, ReceiptCustomerBlock, ReceiptHeader, ReceiptInput, ReceiptLine, ReceiptSaved,
    ReceiptTender, ReceiptTotals, ReceiptWidth, RefundMethod, SaleStatus, ScaleLabel,
    TaxInvoiceBlock, TaxLine, TenderKind, VoidInput, VoidReason, build_receipt, plan_post_void,
};
use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry};
use sultan_db::sales::{
    self, CASH_TENDER_TYPE, CUSTOMER_BALANCE_TENDER_TYPE, DocFilter, MAX_DOC_PAGE, NewSaleVoid,
    RefundTender, Sale, SaleDbError, SaleHeader, Written,
};
use sultan_db::{DbError, cashier_sessions, customers, org, users};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::sale_post::{self, SaleBody, body_hash, build_sale};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, RegisterCtx, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::retry::{self, Failure};
use crate::state::AppState;

/// One line of a sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleLineDto {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The receipt name.
    pub name: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// The unit price charged.
    pub unit_price: i64,
    /// `listed`, `open_price` or `overridden`.
    pub price_mode: String,
    /// The price an override replaced.
    pub resolved_price: Option<i64>,
    /// Who approved the override (a manager), if not the cashier.
    pub override_by: Option<String>,
    /// `weight` or `price`.
    pub scale_label: Option<String>,
    /// `unit_price × qty` (or the label amount).
    pub gross: i64,
    /// The line discount.
    pub line_discount: i64,
    /// Its share of the cart discount.
    pub cart_discount_share: i64,
    /// What the customer pays for it before tax (tax-exclusive) or with tax (inclusive).
    pub amount: i64,
    /// Tax rate, basis points.
    pub tax_rate_bp: u16,
    /// Tax.
    pub tax: i64,
    /// Amount + tax when tax-exclusive, else the amount.
    pub total: i64,
    /// Returned so far, thousandths.
    pub returned_qty_thousandths: i64,
    /// Refunded so far.
    pub refunded: i64,
}

/// One tender of a sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleTenderDto {
    /// The tender type (`null` for an exchange).
    pub tender_type_id: Option<String>,
    /// `cash`, `card`, `qris`, `transfer`, `ewallet`, `customer_balance`, `other`, `exchange`.
    pub kind: String,
    /// The amount it pays.
    pub amount: i64,
    /// Cash received.
    pub cash_received: Option<i64>,
    /// Change handed back.
    pub change_given: i64,
    /// Change saved to the balance.
    pub change_saved: i64,
    /// The reference.
    pub reference: Option<String>,
}

/// A refund tender (post-void, return).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RefundTenderDto {
    /// The tender type (`null` for an exchange).
    pub tender_type_id: Option<String>,
    /// `cash`, `customer_balance` or `exchange`.
    pub kind: String,
    /// The amount.
    pub amount: i64,
}

impl From<&RefundTender> for RefundTenderDto {
    fn from(t: &RefundTender) -> Self {
        Self {
            tender_type_id: t.tender_type.map(|t| t.to_string()),
            kind: t.kind.as_str().to_owned(),
            amount: t.amount.minor(),
        }
    }
}

/// How a sale was post-voided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleVoidDto {
    /// The register that paid back.
    pub register_id: String,
    /// The session whose drawer paid back (D10).
    pub cashier_session_id: String,
    /// Who voided it.
    pub user_id: String,
    /// `customer_changed_mind`, `wrong_item`, `price_dispute`, `other`.
    pub reason: String,
    /// The text of `other`.
    pub reason_text: Option<String>,
    /// Everything the customer paid.
    pub refund: i64,
    /// Saved change taken back.
    pub change_saved_reversed: i64,
    /// Balance payment given back.
    pub payment_reversed: i64,
    /// Refunded to the balance.
    pub to_balance: i64,
    /// Cash paid out.
    pub cash: i64,
    /// Cash rounding of the cash part.
    pub cash_rounding: i64,
    /// The refund tenders.
    pub tenders: Vec<RefundTenderDto>,
    /// When (unix ms).
    pub voided_at: i64,
}

/// A sale's header (lists).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleHeaderDto {
    /// ID.
    pub id: String,
    /// Number, e.g. `JKT01-R1-261116-0001`.
    pub number: String,
    /// The old-Sultan number of an imported sale.
    pub legacy_number: Option<String>,
    /// `posted` or `voided`.
    pub status: String,
    /// Branch.
    pub branch_id: String,
    /// Register.
    pub register_id: Option<String>,
    /// Cashier session it was posted in.
    pub cashier_session_id: Option<String>,
    /// Cashier.
    pub user_id: Option<String>,
    /// Customer.
    pub customer_id: Option<String>,
    /// The customer's name (phase S6: lists show names).
    pub customer_name: Option<String>,
    /// The customer's member code.
    pub customer_code: Option<String>,
    /// The cashier's display name (phase S6).
    pub cashier_name: Option<String>,
    /// The register's code (phase S6).
    pub register_code: Option<String>,
    /// Printed as a tax invoice.
    pub tax_invoice: bool,
    /// Prices include tax.
    pub tax_inclusive: bool,
    /// Σ gross.
    pub subtotal: i64,
    /// Σ line discounts.
    pub line_discount: i64,
    /// Cart discount.
    pub cart_discount: i64,
    /// Σ tax.
    pub tax: i64,
    /// Total before cash rounding.
    pub total: i64,
    /// Cash rounding (+/−).
    pub cash_rounding: i64,
    /// What was paid: `total + cash_rounding`.
    pub paid: i64,
    /// Change handed back.
    pub change_given: i64,
    /// Change saved to the balance.
    pub change_saved: i64,
    /// Number of lines.
    pub line_count: u32,
    /// Number of posted returns.
    pub return_count: u32,
    /// Posted (unix ms).
    pub posted_at: i64,
    /// Voided (unix ms).
    pub voided_at: Option<i64>,
}

impl From<&SaleHeader> for SaleHeaderDto {
    fn from(h: &SaleHeader) -> Self {
        Self {
            id: h.id.to_string(),
            number: h.number.clone(),
            legacy_number: h.legacy_number.clone(),
            status: h.status.as_str().to_owned(),
            branch_id: h.branch.to_string(),
            register_id: h.register.map(|r| r.to_string()),
            cashier_session_id: h.cashier_session.map(|s| s.to_string()),
            user_id: h.user.map(|u| u.to_string()),
            customer_id: h.customer.map(|c| c.to_string()),
            customer_name: h.customer_name.clone(),
            customer_code: h.customer_code.clone(),
            cashier_name: h.user_name.clone(),
            register_code: h.register_code.clone(),
            tax_invoice: h.tax_invoice,
            tax_inclusive: h.tax_inclusive,
            subtotal: h.totals.subtotal.minor(),
            line_discount: h.totals.line_discount.minor(),
            cart_discount: h.totals.cart_discount.minor(),
            tax: h.totals.tax.minor(),
            total: h.totals.total.minor(),
            cash_rounding: h.cash_rounding.minor(),
            paid: h.paid().minor(),
            change_given: h.change_given.minor(),
            change_saved: h.change_saved.minor(),
            line_count: h.line_count,
            return_count: h.return_count,
            posted_at: h.posted_at.unix_millis(),
            voided_at: h.voided_at.map(Timestamp::unix_millis),
        }
    }
}

/// A sale with its lines, tenders and post-void.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SaleDto {
    /// The header.
    #[serde(flatten)]
    pub header: SaleHeaderDto,
    /// The lines.
    pub lines: Vec<SaleLineDto>,
    /// The tenders.
    pub tenders: Vec<SaleTenderDto>,
    /// The post-void, once voided.
    pub void: Option<SaleVoidDto>,
}

impl From<&Sale> for SaleDto {
    fn from(s: &Sale) -> Self {
        let lines = s
            .lines
            .iter()
            .map(|l| {
                let line = &l.line;
                let (mode, resolved) = match line.price_mode {
                    PriceMode::Listed => ("listed", None),
                    PriceMode::OpenPrice => ("open_price", None),
                    PriceMode::Overridden { resolved } => ("overridden", resolved),
                };
                SaleLineDto {
                    line_no: l.line_no,
                    variant_id: line.variant.to_string(),
                    unit_id: line.unit.to_string(),
                    name: line.name.clone(),
                    qty_thousandths: line.qty.thousandths(),
                    unit_price: line.unit_price.minor(),
                    price_mode: mode.to_owned(),
                    resolved_price: resolved.map(Money::minor),
                    override_by: line.override_by.map(|u| u.to_string()),
                    scale_label: line.scale_label.map(|l| {
                        match l {
                            ScaleLabel::Weight => "weight",
                            ScaleLabel::Price { .. } => "price",
                        }
                        .to_owned()
                    }),
                    gross: line.totals.gross.minor(),
                    line_discount: line.totals.line_discount.minor(),
                    cart_discount_share: line.totals.cart_discount_share.minor(),
                    amount: line.totals.amount.minor(),
                    tax_rate_bp: line.totals.tax_rate_bp,
                    tax: line.totals.tax.minor(),
                    total: line.totals.total.minor(),
                    returned_qty_thousandths: l.returned.qty.thousandths(),
                    refunded: l.returned.refunded.minor(),
                }
            })
            .collect();
        let tenders = s
            .tenders
            .iter()
            .map(|t| SaleTenderDto {
                tender_type_id: t.tender_type.map(|t| t.to_string()),
                kind: t.kind.as_str().to_owned(),
                amount: t.amount.minor(),
                cash_received: t.cash_received.map(Money::minor),
                change_given: t.change_given.minor(),
                change_saved: t.change_saved.minor(),
                reference: t.reference.clone(),
            })
            .collect();
        let void = s.void.as_ref().map(|v| {
            let (reason, text) = match &v.reason {
                VoidReason::Other(text) => ("other", Some(text.clone())),
                r => (r.as_str(), None),
            };
            SaleVoidDto {
                register_id: v.register.to_string(),
                cashier_session_id: v.cashier_session.to_string(),
                user_id: v.user.to_string(),
                reason: reason.to_owned(),
                reason_text: text,
                refund: v.refund.minor(),
                change_saved_reversed: v.change_saved_reversed.minor(),
                payment_reversed: v.payment_reversed.minor(),
                to_balance: v.to_balance.minor(),
                cash: v.cash.minor(),
                cash_rounding: v.cash_rounding.minor(),
                tenders: v.tenders.iter().map(RefundTenderDto::from).collect(),
                voided_at: v.voided_at.unix_millis(),
            }
        });
        Self {
            header: SaleHeaderDto::from(&s.header),
            lines,
            tenders,
            void,
        }
    }
}

/// A failure inside a posting transaction: an API answer, or a database error that may be
/// retried (2.1 §5).
#[derive(Debug)]
pub enum Fail {
    /// Answer this.
    Api(ApiError),
    /// A sales repository error; retried when it is a lost race.
    Db(SaleDbError),
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<SaleDbError> for Fail {
    fn from(e: SaleDbError) -> Self {
        Self::Db(e)
    }
}

impl From<DbError> for Fail {
    fn from(e: DbError) -> Self {
        Self::Db(SaleDbError::Db(e))
    }
}

/// Runs a posting transaction with the 2.1 §5 retry loop (5 attempts, jittered backoff of 5, 10,
/// 20 and 40 ms). Every attempt lost → `409 conflict`.
///
/// # Errors
/// The final error.
pub async fn run<T, F, Fut>(what: &'static str, op: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Fail>>,
{
    retry::run(what, op, |f| matches!(f, Fail::Db(e) if e.is_retryable()))
        .await
        .map_err(|failure| match failure {
            Failure::Final(Fail::Api(e)) | Failure::Exhausted(Fail::Api(e)) => e,
            Failure::Final(Fail::Db(e)) => e.into(),
            Failure::Exhausted(Fail::Db(_)) => retry::conflict(),
        })
}

/// The answer to a replayed `PUT` (spec 2.6 §12): `Some(stored)` for the same body, `409
/// conflict` for another, `None` when the ID is new.
async fn replayed(
    state: &AppState,
    id: SaleId,
    hash: &sultan_db::TokenHash,
) -> Result<Option<Sale>, ApiError> {
    let mut tx = state.db.begin_read().await?;
    let stored = sales::replayed_sale(&mut tx, id, hash).await?;
    tx.commit().await?;
    Ok(stored)
}

/// `sale.posted` for the branch's sockets.
fn posted_event(sale: &Sale) -> Event {
    Event::for_branch(
        "sale.posted",
        sale.header.branch,
        json!({
            "sale_id": sale.header.id.to_string(),
            "number": sale.header.number,
            "register_id": sale.header.register.map(|r| r.to_string()),
            "cashier_session_id": sale.header.cashier_session.map(|s| s.to_string()),
            "total": sale.header.totals.total.minor(),
            "paid": sale.header.paid().minor(),
        }),
    )
}

/// `PUT /api/v1/sales/{id}`: posts a sale (see the module docs).
///
/// # Errors
/// `403 register_session_required` / `permission_denied` / `needs_override` /
/// `invalid_override`, `409 no_open_session` / `cart_stale` / `conflict`, `422` rule codes.
#[utoipa::path(put, path = "/api/v1/sales/{id}", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7"),
        ("X-Override" = Option<String>, Header, description = "Override token(s), comma separated (spec 2.1 §6)")),
    request_body = SaleBody,
    responses((status = 201, body = SaleDto), (status = 200, body = SaleDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SaleBody>,
) -> Result<(StatusCode, Json<SaleDto>), ApiError> {
    let id: SaleId = parse_put_id(&id)?;
    let hash = body_hash(&body);
    if let Some(stored) = replayed(&state, id, &hash).await? {
        return Ok((StatusCode::OK, Json(SaleDto::from(&stored))));
    }
    let access = Caller::load(&state.db, &caller.user).await?;
    access.require(Permission::SaleCreate)?;
    let till = sale_post::till(&state, &caller).await?;
    let now = state.now();
    let built = build_sale(&state, till, id, &body, None, hash, now).await?;
    let context = id.to_string();
    let (state_ref, caller_ref, access_ref, header_ref) = (&state, &caller, &access, &header);
    let (written, stored) = run("sale post", || {
        let mut built = built.clone();
        let context = context.clone();
        async move {
            let mut tx = state_ref.db.begin().await?;
            let approvers = overrides::authorize_all(
                state_ref,
                &mut tx,
                caller_ref,
                &access_ref.permissions,
                &built.actions(&context),
                header_ref,
            )
            .await?;
            built.set_approvers(&approvers);
            let (written, stored) = sales::post_sale(&mut tx, &built.sale, now).await?;
            tx.commit().await?;
            Ok((written, stored))
        }
    })
    .await?;
    if written == Written::Unchanged {
        return Ok((StatusCode::OK, Json(SaleDto::from(&stored))));
    }
    state.events.publish(posted_event(&stored));
    Ok((StatusCode::CREATED, Json(SaleDto::from(&stored))))
}

/// Whether `caller` may read a document of `branch` posted by `user`.
pub(crate) fn may_read(
    caller: &Caller,
    me: UserId,
    user: Option<UserId>,
    branch: BranchId,
) -> bool {
    let here = caller.at_scope(BranchScope::Branch(branch));
    user == Some(me)
        || here.contains(Permission::ReportSales)
        || here.contains(Permission::SaleCreate)
}

fn denied() -> ApiError {
    sultan_core::access::AccessError::PermissionDenied {
        permission: Permission::ReportSales,
    }
    .into()
}

async fn readable(
    state: &AppState,
    caller: &UserSession,
    sale: Option<Sale>,
) -> Result<Sale, ApiError> {
    let sale = sale.ok_or_else(|| ApiError::not_found("sale"))?;
    let access = Caller::load(&state.db, caller).await?;
    if may_read(
        &access,
        caller.user.id,
        sale.header.user,
        sale.header.branch,
    ) {
        Ok(sale)
    } else {
        Err(denied())
    }
}

/// `GET /api/v1/sales/{id}`.
///
/// # Errors
/// `401`, `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/sales/{id}", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Sale ID")),
    responses((status = 200, body = SaleDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    caller: UserSession,
    Path(id): Path<String>,
) -> Result<Json<SaleDto>, ApiError> {
    let id: SaleId = parse_id(&id, "id")?;
    let sale = readable(&state, &caller, sales::get_sale(&state.db, id).await?).await?;
    Ok(Json(SaleDto::from(&sale)))
}

/// `GET /api/v1/sales/by-number/{number}`: the receipt lookup of returns (§8); an imported sale
/// is found by its old number too.
///
/// # Errors
/// `401`, `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/sales/by-number/{number}", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("number" = String, Path, description = "Sale number")),
    responses((status = 200, body = SaleDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn by_number(
    State(state): State<AppState>,
    caller: UserSession,
    Path(number): Path<String>,
) -> Result<Json<SaleDto>, ApiError> {
    let found = sales::get_sale_by_number(&state.db, number.trim()).await?;
    let sale = readable(&state, &caller, found).await?;
    Ok(Json(SaleDto::from(&sale)))
}

/// Query of `GET /sales` and `GET /returns`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct DocQuery {
    /// Only this branch.
    pub branch_id: Option<String>,
    /// Only this register.
    pub register_id: Option<String>,
    /// Only this cashier session.
    pub cashier_session_id: Option<String>,
    /// Only this customer.
    pub customer_id: Option<String>,
    /// Posted at or after (unix ms).
    pub from: Option<i64>,
    /// Posted before (unix ms).
    pub until: Option<i64>,
    /// Only this number.
    pub number: Option<String>,
    /// Only this status: `posted` or `voided` (phase S6).
    pub status: Option<String>,
    /// Page cursor: `next_before_id` of the previous page.
    pub before_id: Option<String>,
    /// Page size, 1–200 (default 50).
    pub limit: Option<u32>,
}

/// One page of `GET /sales`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SalePage {
    /// Newest first.
    pub items: Vec<SaleHeaderDto>,
    /// Pass as `before_id` for the next page; `null` on the last page.
    pub next_before_id: Option<String>,
}

/// Parses the list query and checks who may list (see the module docs).
pub(crate) async fn doc_filter(
    state: &AppState,
    caller: &UserSession,
    query: &DocQuery,
) -> Result<DocFilter, ApiError> {
    let limit = query.limit.unwrap_or(50);
    if !(1..=MAX_DOC_PAGE).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {MAX_DOC_PAGE}"),
        ));
    }
    let filter = DocFilter {
        branch: parse_opt_id::<BranchId>(query.branch_id.as_deref(), "branch_id")?,
        register: parse_opt_id::<RegisterId>(query.register_id.as_deref(), "register_id")?,
        cashier_session: parse_opt_id::<CashierSessionId>(
            query.cashier_session_id.as_deref(),
            "cashier_session_id",
        )?,
        customer: parse_opt_id::<Uuid>(query.customer_id.as_deref(), "customer_id")?,
        from: query.from.map(Timestamp::from_unix_millis),
        until: query.until.map(Timestamp::from_unix_millis),
        number: query.number.clone().filter(|n| !n.trim().is_empty()),
        status: match query.status.as_deref().map(str::trim) {
            None | Some("") => None,
            Some("posted") => Some(SaleStatus::Posted),
            Some("voided") => Some(SaleStatus::Voided),
            Some(_) => {
                return Err(ApiError::invalid_field(
                    "status",
                    "`status` must be posted or voided",
                ));
            }
        },
        before: parse_opt_id::<Uuid>(query.before_id.as_deref(), "before_id")?,
        limit,
    };
    if let Some(session) = filter.cashier_session
        && cashier_sessions::get(&state.db, session)
            .await?
            .is_some_and(|r| r.session.user() == caller.user.id)
    {
        return Ok(filter);
    }
    let access = Caller::load(&state.db, caller).await?;
    let allowed = match filter.branch {
        Some(b) => access
            .at_scope(BranchScope::Branch(b))
            .contains(Permission::ReportSales),
        None => access.holds_everywhere(Permission::ReportSales),
    };
    if allowed { Ok(filter) } else { Err(denied()) }
}

/// `GET /api/v1/sales?…` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied`, `422 invalid_field` (`limit`).
#[utoipa::path(get, path = "/api/v1/sales", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])), params(DocQuery),
    responses((status = 200, body = SalePage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    caller: UserSession,
    ApiQuery(query): ApiQuery<DocQuery>,
) -> Result<Json<SalePage>, ApiError> {
    let filter = doc_filter(&state, &caller, &query).await?;
    let items = sales::list_sales(&state.db, &filter).await?;
    let next_before_id = (items.len() == filter.limit as usize)
        .then(|| items.last().map(|h| h.id.to_string()))
        .flatten();
    Ok(Json(SalePage {
        items: items.iter().map(SaleHeaderDto::from).collect(),
        next_before_id,
    }))
}

/// Query of `GET /sales/{id}/receipt`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ReceiptQuery {
    /// A reprint (printed COPY, audited).
    #[serde(default)]
    pub copy: bool,
    /// 32, 42 or 48 columns (default: the branch's receipt width, `GET /settings/sales`).
    pub width: Option<usize>,
    /// The label language: `id` (Indonesian) or `en` (default); the register sends its window
    /// language (spec 2.6 §19 from S4).
    pub lang: Option<String>,
}

/// The receipt of a sale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReceiptDto {
    /// The sale.
    pub sale_id: String,
    /// Its number (also the barcode/QR payload).
    pub number: String,
    /// A reprint.
    pub copy: bool,
    /// The sale is voided.
    pub voided: bool,
    /// The text, one entry per line.
    pub lines: Vec<String>,
    /// The text with a newline after each line.
    pub text: String,
    /// The payload of the barcode/QR under the text.
    pub barcode: String,
}

/// Builds the receipt of `sale` (spec 2.6 §9).
#[allow(clippy::too_many_lines)] // gathers every block of §9
async fn receipt(
    state: &AppState,
    sale: &Sale,
    width: ReceiptWidth,
    copy: bool,
    lang: &str,
) -> Result<ReceiptDto, ApiError> {
    let h = &sale.header;
    let branch = org::get_branch(&state.db, h.branch)
        .await?
        .ok_or_else(|| ApiError::not_found("branch"))?;
    let tz: BranchTimeZone = branch.timezone.parse().unwrap_or_default();
    let register = match h.register {
        Some(r) => org::get_register(&state.db, r)
            .await?
            .map(|r| r.code)
            .unwrap_or_default(),
        None => String::new(),
    };
    let cashier = match h.user {
        Some(u) => users::get_user(&state.db, u)
            .await?
            .map(|u| u.display_name)
            .unwrap_or_default(),
        None => String::new(),
    };
    let customer = match h.customer {
        Some(c) => customers::get_customer(&state.db, c).await?,
        None => None,
    };
    let phone = customer
        .as_ref()
        .and_then(|c| c.customer.phone.as_deref())
        .and_then(|p| Phone::parse(p).ok());
    let customer_block = customer.as_ref().map(|c| {
        let c = &c.customer;
        match (h.tax_invoice, c.tax_id.as_deref()) {
            (true, Some(tax_id)) => ReceiptCustomerBlock::TaxInvoice(TaxInvoiceBlock {
                company_name: c.company_name.as_deref().unwrap_or(&c.name),
                tax_id,
                tax_address: c.tax_address.as_deref().unwrap_or_default(),
            }),
            _ => ReceiptCustomerBlock::Customer(ReceiptCustomer {
                name: &c.name,
                company_name: c.company_name.as_deref(),
                phone: phone.as_ref(),
                tier_name: None,
            }),
        }
    });
    let wanted: Vec<_> = sale
        .lines
        .iter()
        .map(|l| (l.line.variant, l.line.unit))
        .collect();
    let items = sale_post::load_items(state, &wanted, "lines", true).await?;
    let lines: Vec<ReceiptLine<'_>> = sale
        .lines
        .iter()
        .zip(&items)
        .map(|(l, item)| ReceiptLine {
            name: &l.line.name,
            qty: (l.line.qty != sultan_core::catalog::Qty::ZERO).then_some(l.line.qty),
            unit: &item.unit_name,
            unit_price: l.line.unit_price,
            gross: l.line.totals.gross,
            discount: l.line.totals.line_discount,
        })
        .collect();
    let mut by_rate: std::collections::BTreeMap<u16, (Money, Money)> =
        std::collections::BTreeMap::new();
    for l in sale.lines.iter().filter(|l| l.line.totals.tax_rate_bp > 0) {
        let e = by_rate.entry(l.line.totals.tax_rate_bp).or_default();
        e.0 = e.0.saturating_add(l.line.totals.net);
        e.1 = e.1.saturating_add(l.line.totals.tax);
    }
    let mut names: HashMap<_, String> = HashMap::new();
    for t in &sale.tenders {
        if let Some(id) = t.tender_type
            && !names.contains_key(&id)
            && let Some(r) = sales::get_tender_type(&state.db, id).await?
        {
            names.insert(id, r.tender.name);
        }
    }
    let exchange = Label::Exchange.english().to_owned();
    let tenders: Vec<ReceiptTender<'_>> = sale
        .tenders
        .iter()
        .map(|t| ReceiptTender {
            name: t
                .tender_type
                .and_then(|id| names.get(&id))
                .unwrap_or(&exchange),
            amount: t.amount,
            cash_received: t.cash_received,
            change_given: t.change_given,
            reference: t.reference.as_deref(),
        })
        .collect();
    let saved = (h.change_saved > Money::ZERO).then(|| ReceiptSaved {
        saved: h.change_saved,
        balance: customer.as_ref().map_or(Money::ZERO, |c| c.balance),
    });
    let header_lines: Vec<String> = branch.receipt_header.lines().map(str::to_owned).collect();
    let footer_lines: Vec<String> = branch.receipt_footer.lines().map(str::to_owned).collect();
    let input = ReceiptInput {
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
        customer: customer_block,
        lines: &lines,
        totals: ReceiptTotals {
            subtotal: h.totals.subtotal,
            discount: h
                .totals
                .line_discount
                .saturating_add(h.totals.cart_discount),
            tax: by_rate
                .into_iter()
                .map(|(rate_bp, (base, tax))| TaxLine { rate_bp, base, tax })
                .collect(),
            tax_inclusive: h.tax_inclusive,
            cash_rounding: h.cash_rounding,
            total: h.paid(),
        },
        tenders: &tenders,
        saved,
        footer: &footer_lines,
        copy,
        voided: h.status == SaleStatus::Voided,
    };
    let built = build_receipt(&input, &|l| l.text(lang).to_owned());
    Ok(ReceiptDto {
        sale_id: h.id.to_string(),
        number: h.number.clone(),
        copy,
        voided: input.voided,
        text: built.text(),
        lines: built.lines,
        barcode: built.barcode,
    })
}

/// The receipt width: the asked one, else the branch's (`branch_sales_settings`, default 32).
pub(crate) async fn receipt_width(
    state: &AppState,
    asked: Option<usize>,
    branch: BranchId,
) -> Result<ReceiptWidth, ApiError> {
    let cols = match asked {
        Some(cols) => cols,
        None => usize::from(sales::receipt_width(&state.db, branch).await?),
    };
    ReceiptWidth::from_cols(cols)
        .ok_or_else(|| ApiError::invalid_field("width", "`width` must be 32, 42 or 48"))
}

/// Whether `sale` is the last sale of the caller's open session on their register.
async fn last_of_own_session(
    state: &AppState,
    caller: &UserSession,
    sale: &Sale,
) -> Result<bool, ApiError> {
    let Some(register) = caller.session.register else {
        return Ok(false);
    };
    let Some(open) = cashier_sessions::open_for_register(&state.db, register).await? else {
        return Ok(false);
    };
    if open.session.user() != caller.user.id
        || sale.header.cashier_session != Some(open.session.id())
    {
        return Ok(false);
    }
    let filter = DocFilter {
        cashier_session: Some(open.session.id()),
        limit: 1,
        ..DocFilter::default()
    };
    let last = sales::list_sales(&state.db, &filter).await?;
    Ok(last.first().is_some_and(|h| h.id == sale.header.id))
}

/// A reprint to check and audit (spec 2.6 §9, §19 item 46).
pub(crate) struct Reprint {
    /// The last document of the caller's open session: no permission needed.
    pub free: bool,
    /// The audit action (`sale.reprint`, `return.reprint`).
    pub action: &'static str,
    /// The audit target (`sale:{id}`, `return:{id}`).
    pub target: String,
    /// The override's `context_ref` (the document ID).
    pub context_ref: String,
    /// The document's branch.
    pub branch: BranchId,
    /// The audit `after` (the approver is added).
    pub after: serde_json::Value,
}

/// Checks `sale.reprint` for a copy (free for the last document of the caller's open session;
/// overridable at a register; required without an override from the back office) and writes
/// its audit entry with the approver.
pub(crate) async fn authorize_reprint(
    state: &AppState,
    caller: &UserSession,
    header: &OverrideHeader,
    reprint: Reprint,
) -> Result<(), ApiError> {
    let access = Caller::load(&state.db, caller).await?;
    let mut tx = state.db.begin().await?;
    let approver = if reprint.free {
        None
    } else if let Some(register) = caller.session.register {
        let at = RegisterSession {
            register: RegisterCtx {
                register,
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
                permission: Permission::SaleReprint,
                value: None,
                context_ref: &reprint.context_ref,
            },
            header,
        )
        .await?
    } else {
        access.require(Permission::SaleReprint)?;
        None
    };
    let mut after = reprint.after;
    after["approved_by"] = json!(approver.map(|a| a.to_string()));
    let mut entry = AuditEntry::new(state.now(), reprint.action);
    entry.branch = Some(reprint.branch);
    entry.register = caller.session.register;
    entry.user = Some(caller.user.id);
    entry.target = Some(reprint.target);
    entry.after = Some(after);
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok(())
}

/// `GET /api/v1/sales/{id}/receipt?copy=&width=&lang=` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied` / `needs_override` / `invalid_override`, `404 not_found`,
/// `422 invalid_field` (`width`).
#[utoipa::path(get, path = "/api/v1/sales/{id}/receipt", tag = "sales",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Sale ID"), ReceiptQuery,
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.reprint` (spec 2.1 §6)")),
    responses((status = 200, body = ReceiptDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get_receipt(
    State(state): State<AppState>,
    caller: UserSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<ReceiptQuery>,
) -> Result<Json<ReceiptDto>, ApiError> {
    let id: SaleId = parse_id(&id, "id")?;
    let sale = readable(&state, &caller, sales::get_sale(&state.db, id).await?).await?;
    let width = receipt_width(&state, query.width, sale.header.branch).await?;
    let dto = receipt(
        &state,
        &sale,
        width,
        query.copy,
        query.lang.as_deref().unwrap_or("en"),
    )
    .await?;
    if !query.copy {
        return Ok(Json(dto));
    }
    let free = last_of_own_session(&state, &caller, &sale).await?;
    authorize_reprint(
        &state,
        &caller,
        &header,
        Reprint {
            free,
            action: "sale.reprint",
            target: format!("sale:{id}"),
            context_ref: id.to_string(),
            branch: sale.header.branch,
            after: json!({"number": sale.header.number, "voided": dto.voided}),
        },
    )
    .await?;
    Ok(Json(dto))
}

/// Body of `POST /sales/{id}/void`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct VoidBody {
    /// `customer_changed_mind`, `wrong_item`, `price_dispute` or `other`.
    pub reason: String,
    /// The text of `other` (1–200 characters).
    #[serde(default)]
    pub reason_text: Option<String>,
    /// `cash` or `balance` (the sale's customer; above `max_balance` the rest is cash).
    pub refund: String,
}

/// `cash` or `balance`.
pub(crate) fn refund_method(value: &str) -> Result<RefundMethod, ApiError> {
    match value {
        "cash" => Ok(RefundMethod::Cash),
        "balance" => Ok(RefundMethod::Balance),
        _ => Err(ApiError::invalid_field(
            "refund",
            "`refund` is `cash` or `balance`",
        )),
    }
}

pub(crate) fn void_reason(key: &str, text: Option<&str>) -> Result<VoidReason, ApiError> {
    VoidReason::parse(key, text)
        .and_then(VoidReason::validate)
        .map_err(ApiError::from)
}

/// The tender types a refund pays with: the first active cash and customer-balance types (the
/// seeded ones when none is active).
pub(crate) async fn refund_tender_types(
    state: &AppState,
) -> Result<
    (
        sultan_core::ids::TenderTypeId,
        sultan_core::ids::TenderTypeId,
    ),
    ApiError,
> {
    let list = sales::list_tender_types(&state.db, false).await?;
    let first = |kind: TenderKind, fallback| {
        list.iter()
            .find(|t| t.tender.kind == kind && t.tender.active)
            .map_or(fallback, |t| t.tender.id)
    };
    Ok((
        first(TenderKind::Cash, CASH_TENDER_TYPE),
        first(TenderKind::CustomerBalance, CUSTOMER_BALANCE_TENDER_TYPE),
    ))
}

/// The refund tenders of a plan: cash and the balance, each only when not 0.
pub(crate) fn refund_tenders(
    (cash_type, balance_type): (
        sultan_core::ids::TenderTypeId,
        sultan_core::ids::TenderTypeId,
    ),
    cash: Money,
    to_balance: Money,
) -> Vec<RefundTender> {
    let mut out = Vec::new();
    if cash > Money::ZERO {
        out.push(RefundTender {
            tender_type: Some(cash_type),
            kind: TenderKind::Cash,
            amount: cash,
        });
    }
    if to_balance > Money::ZERO {
        out.push(RefundTender {
            tender_type: Some(balance_type),
            kind: TenderKind::CustomerBalance,
            amount: to_balance,
        });
    }
    out
}

/// `POST /api/v1/sales/{id}/void` (see the module docs).
///
/// # Errors
/// `403` permission codes, `404 not_found`, `409 no_open_session` / `has_returns` / `conflict`,
/// `422 invalid_field`.
#[utoipa::path(post, path = "/api/v1/sales/{id}/void", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Sale ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)"),
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.post_void`")),
    request_body = VoidBody,
    responses((status = 200, body = SaleDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn void(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<VoidBody>,
) -> Result<Json<SaleDto>, ApiError> {
    let id: SaleId = parse_id(&id, "id")?;
    let method = refund_method(&body.refund)?;
    let reason = void_reason(&body.reason, body.reason_text.as_deref())?;
    let access = Caller::load(&state.db, &caller.user).await?;
    let till = sale_post::till(&state, &caller).await?;
    let ctx = sale_post::PriceContext::load(&state, till.branch).await?;
    let settings = sale_post::balance_settings(&state).await?;
    let types = refund_tender_types(&state).await?;
    let context = id.to_string();
    let now = state.now();
    let (state, caller, access, header) = (&state, &caller, &access, &header);
    let stored = run("sale void", || {
        let (reason, context) = (reason.clone(), context.clone());
        async move {
            let mut tx = state.db.begin().await?;
            let sale = sales::get_sale(&mut tx, id)
                .await?
                .ok_or(SaleDbError::NotFound("sale"))?;
            if sale.header.branch != till.branch || sale.header.legacy_number.is_some() {
                return Err(ApiError::invalid_field(
                    "sale",
                    "only a sale posted at this branch can be voided here",
                )
                .into());
            }
            overrides::authorize(
                state,
                &mut tx,
                caller,
                &access.permissions,
                Action {
                    permission: Permission::SalePostVoid,
                    value: None,
                    context_ref: &context,
                },
                header,
            )
            .await?;
            let balance = match sale.header.customer {
                Some(c) => customers::get_customer(&mut tx, c)
                    .await?
                    .map(|c| c.balance),
                None => None,
            };
            let plan = plan_post_void(VoidInput {
                status: sale.header.status,
                has_returns: sale.header.return_count > 0,
                tenders: &sale.tenders,
                method,
                balance,
                settings,
                rounding: ctx.settings.cash_rounding,
            })
            .map_err(ApiError::from)?;
            let to_balance = plan
                .refund
                .payment_reversed
                .saturating_add(plan.refund.to_balance);
            let void = NewSaleVoid {
                sale: id,
                branch: till.branch,
                register: till.register,
                cashier_session: till.session,
                user: till.user,
                reason,
                method,
                tenders: refund_tenders(types, plan.refund.cash, to_balance),
                plan,
                voided_at: now,
            };
            let stored = sales::void_sale(&mut tx, &void, now).await?;
            tx.commit().await?;
            Ok(stored)
        }
    })
    .await?;
    state.events.publish(Event::for_branch(
        "sale.voided",
        stored.header.branch,
        json!({ "sale_id": stored.header.id.to_string(), "number": stored.header.number }),
    ));
    Ok(Json(SaleDto::from(&stored)))
}

/// One removed line of `POST /cart-voids`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct VoidedLineBody {
    /// The variant.
    pub variant_id: String,
    /// The receipt name, for the audit entry.
    #[serde(default)]
    pub name: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// Its gross amount (≥ 0).
    pub amount: i64,
}

/// Body of `POST /cart-voids`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CartVoidBody {
    /// `line` (one removed line) or `cart` (the whole cart cleared).
    pub kind: String,
    /// The cart's ID (the sale ID it would have had): the override's `context_ref`.
    pub cart_id: String,
    /// `customer_changed_mind`, `wrong_item`, `price_dispute` or `other`.
    pub reason: String,
    /// The text of `other`.
    #[serde(default)]
    pub reason_text: Option<String>,
    /// The removed lines (one for `line`).
    pub lines: Vec<VoidedLineBody>,
}

/// The recorded void.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CartVoidDto {
    /// `line` or `cart`.
    pub kind: String,
    /// The session it counts in.
    pub cashier_session_id: String,
    /// Σ line amounts.
    pub amount: i64,
    /// The manager who approved it, if not the cashier.
    pub approved_by: Option<String>,
}

/// The audit action of a till void.
pub const LINE_VOID: &str = "sale.void_line";
/// The audit action of a cleared cart.
pub const CART_VOID: &str = "sale.void_cart";

/// `POST /api/v1/cart-voids` (see the module docs) → `201`.
///
/// # Errors
/// `403` permission codes, `409 no_open_session`, `422 invalid_field`.
#[utoipa::path(post, path = "/api/v1/cart-voids", tag = "sales",
    security(("bearer" = [], "register_token" = [])),
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)"),
        ("X-Override" = Option<String>, Header, description = "Override token for `sale.void_line` / `sale.void`")),
    request_body = CartVoidBody,
    responses((status = 201, body = CartVoidDto), (status = 403, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn cart_void(
    State(state): State<AppState>,
    caller: RegisterSession,
    header: OverrideHeader,
    ApiJson(body): ApiJson<CartVoidBody>,
) -> Result<(StatusCode, Json<CartVoidDto>), ApiError> {
    let (permission, action) = match body.kind.as_str() {
        "line" => (Permission::SaleVoidLine, LINE_VOID),
        "cart" => (Permission::SaleVoid, CART_VOID),
        _ => {
            return Err(ApiError::invalid_field(
                "kind",
                "`kind` is `line` or `cart`",
            ));
        }
    };
    let cart: Uuid = parse_id(&body.cart_id, "cart_id")?;
    let reason = void_reason(&body.reason, body.reason_text.as_deref())?;
    if body.lines.is_empty() || (permission == Permission::SaleVoidLine && body.lines.len() != 1) {
        return Err(ApiError::invalid_field(
            "lines",
            "a line void has one line, a cart void at least one",
        ));
    }
    let mut amount = Money::ZERO;
    let mut lines = Vec::with_capacity(body.lines.len());
    for l in &body.lines {
        let variant: VariantId = parse_id(&l.variant_id, "lines.variant_id")?;
        if l.amount < 0 || l.qty_thousandths < 0 {
            return Err(ApiError::invalid_field(
                "lines.amount",
                "amounts and quantities must not be negative",
            ));
        }
        amount = amount
            .checked_add(Money::from_minor(l.amount))
            .ok_or_else(|| ApiError::from(sultan_core::sale::SaleError::AmountOverflow))?;
        lines.push(json!({
            "variant_id": variant.to_string(),
            "name": l.name,
            "qty": l.qty_thousandths,
            "amount": l.amount,
        }));
    }
    let access = Caller::load(&state.db, &caller.user).await?;
    let till = sale_post::till(&state, &caller).await?;
    let mut tx = state.db.begin().await?;
    let approver = overrides::authorize(
        &state,
        &mut tx,
        &caller,
        &access.permissions,
        Action {
            permission,
            value: None,
            context_ref: &cart.to_string(),
        },
        &header,
    )
    .await?;
    let (key, text) = match &reason {
        VoidReason::Other(text) => ("other", Some(text.clone())),
        r => (r.as_str(), None),
    };
    let mut entry = AuditEntry::new(state.now(), action);
    entry.branch = Some(till.branch);
    entry.register = Some(till.register);
    entry.user = Some(till.user);
    entry.target = Some(format!("cashier_session:{}", till.session));
    entry.after = Some(json!({
        "cart_id": cart.to_string(),
        "reason": key,
        "reason_text": text,
        "amount": amount.minor(),
        "lines": lines,
        "approved_by": approver.map(|a| a.to_string()),
    }));
    audit::append(&mut tx, &entry).await?;
    tx.commit().await?;
    Ok((
        StatusCode::CREATED,
        Json(CartVoidDto {
            kind: body.kind,
            cashier_session_id: till.session.to_string(),
            amount: amount.minor(),
            approved_by: approver.map(|a| a.to_string()),
        }),
    ))
}
