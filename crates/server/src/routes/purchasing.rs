//! Purchasing (spec 2.8 §6, §10; phase N3): purchase orders, goods receipts with their
//! payables, supplier returns, reorder rules and suggestions. Branch documents: each write
//! checks its key at the document's branch (a body without `branch_id` is the session's
//! branch), reads need any inventory key there (see [`super::stock`]). Drafts are written whole
//! by their `PUT` (`201` new, `200` overwritten or replayed); actions are `POST`s with
//! `Idempotency-Key`.
//!
//! - `GET /purchase-orders?branch_id&supplier_id&status`, `GET /purchase-orders/{id}`,
//!   `PUT /purchase-orders/{id}` (draft), `POST …/{id}/order`, `…/close` ("close the rest"),
//!   `…/cancel` (nothing received yet): `purchase.order`. Line prices are left out without
//!   `report.profit` (they come from the supplier's last price).
//! - `GET /goods-receipts?branch_id&supplier_id&status`, `GET /goods-receipts/{id}`,
//!   `PUT /goods-receipts/{id}` (draft), `POST …/{id}/post` (`purchase.receive`): stock in at the
//!   landed cost, lots, last prices, PO lines (`over_received` warns about PO lines now received
//!   beyond the order), the payable; a duplicate invoice of the supplier is `409 conflict`.
//!   `POST …/{id}/void` (`purchase.receive` + a manager): a caller holding `stock.adjust` at the
//!   branch voids under their own login; anyone else needs `X-Override` for `stock.adjust` with
//!   `context_ref` = the receipt ID, approved by a manager at a register (`POST /overrides`), else
//!   `403 needs_override`. Refused with `409 receipt_in_use` once a later movement used its cost
//!   or its payable is paid. Landed cost and unit cost are left out without `report.profit`;
//!   invoice prices, discounts, PPN and totals are the invoice's and are shown.
//! - `GET /supplier-returns?branch_id&supplier_id`, `GET /supplier-returns/{id}`,
//!   `PUT /supplier-returns/{id}` (draft), `POST …/{id}/post` (`purchase.receive`): stock out at
//!   the average, the credit stored for 2.9. Line values are left out without `report.profit`.
//! - `GET /reorder-rules?branch_id`, `PUT /reorder-rules/{branch_id}/{variant_id}` `{min, max}`
//!   (`purchase.order`), `GET /reorder/suggestions?supplier_id&branch_id` (`purchase.order`),
//!   `GET /reorder/low-stock?branch_id`.
//!
//! Every posting answers `negative_after` (§4) and publishes `stock.changed`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::{AccessError, Permission};
use sultan_core::ids::{
    BranchId, GoodsReceiptId, LocationId, PurchaseOrderId, SupplierId, SupplierReturnId, UnitId,
    VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::TaxMode;
use sultan_db::stock::{
    self, GoodsReceipt, NewGoodsReceipt, NewGoodsReceiptLine, NewPurchaseOrder,
    NewPurchaseOrderLine, NewSupplierReturn, NewSupplierReturnLine, PaymentKind, PurchaseOrder,
    PurchaseOrderStatus, ReceiptStatus, ReorderRule, SuggestedLine, SupplierReturn, Written,
};
use utoipa::{IntoParams, ToSchema};

use super::catalog::actor;
use super::stock::{
    CostView, Fail, MovementDto, NegativeDto, branch_or_session, doc_movements, negative_after,
    opt_day_text, parse_day, parse_opt_day, publish_doc_stock, qty, require_at, require_read, run,
    status_of,
};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, RegisterCtx, RegisterSession, UserSession};
use crate::overrides::{self, Action, OverrideHeader};
use crate::state::AppState;

/// Query of the document lists.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct DocQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
    /// One supplier.
    pub supplier_id: Option<String>,
    /// One status.
    pub status: Option<String>,
}

fn status_param<T>(
    value: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, ApiError> {
    value
        .filter(|v| !v.is_empty())
        .map(|v| parse(v).ok_or_else(|| ApiError::invalid_field("status", "unknown `status`")))
        .transpose()
}

// ---------------------------------------------------------------------------------------------
// Purchase orders

/// A line of a purchase order body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PurchaseOrderLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit, thousandths (> 0).
    pub qty_thousandths: i64,
    /// Price per unit (Rp).
    pub price: i64,
}

/// Body of `PUT /purchase-orders/{id}` (a draft).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PurchaseOrderBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// The supplier.
    pub supplier_id: String,
    /// The location to receive into (default: the receiving default).
    #[serde(default)]
    pub location_id: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub order_date: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(default)]
    pub expected_date: Option<String>,
    /// A note (0–500).
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<PurchaseOrderLineBody>,
}

/// A purchase order line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PurchaseOrderLineDto {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor, thousandths.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub price: Option<Option<i64>>,
    /// Ordered base units.
    pub base_qty_thousandths: i64,
    /// Received base units.
    pub received_qty_thousandths: i64,
}

/// A purchase order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PurchaseOrderDto {
    /// ID.
    pub id: String,
    /// `JKT01-PO-261126-01` once ordered.
    pub number: Option<String>,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The location to receive into.
    pub location_id: String,
    /// `draft`, `ordered`, `closed` or `cancelled`.
    pub status: String,
    /// `YYYY-MM-DD`.
    pub order_date: Option<String>,
    /// `YYYY-MM-DD`.
    pub expected_date: Option<String>,
    /// Note.
    pub note: Option<String>,
    /// Who wrote it.
    pub user_id: String,
    /// Unix ms.
    pub ordered_at: Option<i64>,
    /// Unix ms.
    pub closed_at: Option<i64>,
    /// Unix ms.
    pub cancelled_at: Option<i64>,
    /// The lines.
    pub lines: Vec<PurchaseOrderLineDto>,
}

fn ms(t: Option<sultan_core::time::Timestamp>) -> Option<i64> {
    t.map(sultan_core::time::Timestamp::unix_millis)
}

impl PurchaseOrderDto {
    fn of(o: &PurchaseOrder, view: CostView) -> Self {
        Self {
            id: o.id.to_string(),
            number: o.number.clone(),
            branch_id: o.branch.to_string(),
            supplier_id: o.supplier.to_string(),
            location_id: o.location.to_string(),
            status: o.status.as_str().to_owned(),
            order_date: opt_day_text(o.order_date),
            expected_date: opt_day_text(o.expected_date),
            note: o.note.clone(),
            user_id: o.user.to_string(),
            ordered_at: ms(o.ordered_at),
            closed_at: ms(o.closed_at),
            cancelled_at: ms(o.cancelled_at),
            lines: o
                .lines
                .iter()
                .map(|l| PurchaseOrderLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    price: view.amount(l.price),
                    base_qty_thousandths: l.base_qty.thousandths(),
                    received_qty_thousandths: l.received_qty.thousandths(),
                })
                .collect(),
        }
    }
}

/// `GET /api/v1/purchase-orders`: newest first.
///
/// # Errors
/// `403 permission_denied`, `422 invalid_field` (`status`).
#[utoipa::path(get, path = "/api/v1/purchase-orders", tag = "stock",
    security(("bearer" = [])), params(DocQuery),
    responses((status = 200, body = [PurchaseOrderDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_orders(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<DocQuery>,
) -> Result<Json<Vec<PurchaseOrderDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let supplier = parse_opt_id::<SupplierId>(query.supplier_id.as_deref(), "supplier_id")?;
    let status = status_param(query.status.as_deref(), PurchaseOrderStatus::from_key)?;
    let list = stock::list_purchase_orders(&state.db, Some(branch), supplier, status).await?;
    let view = CostView::at(&caller, branch);
    Ok(Json(
        list.iter().map(|o| PurchaseOrderDto::of(o, view)).collect(),
    ))
}

async fn load_order(state: &AppState, id: PurchaseOrderId) -> Result<PurchaseOrder, ApiError> {
    stock::get_purchase_order(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("purchase order"))
}

/// `GET /api/v1/purchase-orders/{id}`.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/purchase-orders/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Purchase order ID")),
    responses((status = 200, body = PurchaseOrderDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_order(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<PurchaseOrderDto>, ApiError> {
    let po = load_order(&state, parse_id(&id, "id")?).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, po.branch)?;
    Ok(Json(PurchaseOrderDto::of(
        &po,
        CostView::at(&caller, po.branch),
    )))
}

/// `PUT /api/v1/purchase-orders/{id}`: writes a draft whole (`purchase.order`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (not a draft) / `archived`,
/// `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/purchase-orders/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = PurchaseOrderBody,
    responses((status = 201, body = PurchaseOrderDto), (status = 200, body = PurchaseOrderDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_order(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PurchaseOrderBody>,
) -> Result<(StatusCode, Json<PurchaseOrderDto>), ApiError> {
    let id: PurchaseOrderId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::PurchaseOrder)?;
    let mut lines = Vec::with_capacity(body.lines.len());
    for l in &body.lines {
        lines.push(NewPurchaseOrderLine {
            variant: parse_id::<VariantId>(&l.variant_id, "variant_id")?,
            unit: parse_id::<UnitId>(&l.unit_id, "unit_id")?,
            qty: qty(l.qty_thousandths),
            price: Money::from_minor(l.price),
        });
    }
    let po = NewPurchaseOrder {
        id,
        branch,
        supplier: parse_id(&body.supplier_id, "supplier_id")?,
        location: parse_opt_id(body.location_id.as_deref(), "location_id")?,
        order_date: parse_opt_day("order_date", body.order_date.as_deref())?,
        expected_date: parse_opt_day("expected_date", body.expected_date.as_deref())?,
        note: body.note,
        user: session.user.id,
        lines,
    };
    let now = state.now();
    let (state_ref, po) = (&state, &po);
    let (written, stored) = run("purchase order put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::put_purchase_order(&mut tx, po, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((
        status_of(written),
        Json(PurchaseOrderDto::of(&stored, CostView::at(&caller, branch))),
    ))
}

#[derive(Clone, Copy)]
enum OrderAction {
    Order,
    Close,
    Cancel,
}

async fn order_action(
    state: &AppState,
    session: &UserSession,
    id: &str,
    action: OrderAction,
) -> Result<Json<PurchaseOrderDto>, ApiError> {
    let id: PurchaseOrderId = parse_id(id, "id")?;
    let po = load_order(state, id).await?;
    let caller = Caller::load(&state.db, session).await?;
    require_at(&caller, po.branch, Permission::PurchaseOrder)?;
    let (now, user) = (state.now(), session.user.id);
    let (_, stored) = run("purchase order action", || async move {
        let mut tx = state.db.begin().await?;
        let out = match action {
            OrderAction::Order => stock::order_purchase_order(&mut tx, id, user, now).await?,
            OrderAction::Close => stock::close_purchase_order(&mut tx, id, user, now).await?,
            OrderAction::Cancel => stock::cancel_purchase_order(&mut tx, id, user, now).await?,
        };
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok(Json(PurchaseOrderDto::of(
        &stored,
        CostView::at(&caller, po.branch),
    )))
}

/// `POST /api/v1/purchase-orders/{id}/order`: numbers a draft `-PO-` (`purchase.order`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (cancelled).
#[utoipa::path(post, path = "/api/v1/purchase-orders/{id}/order", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Purchase order ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = PurchaseOrderDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn order(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<PurchaseOrderDto>, ApiError> {
    order_action(&state, &session, &id, OrderAction::Order).await
}

/// `POST /api/v1/purchase-orders/{id}/close`: "close the rest" (`purchase.order`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (a draft or cancelled order).
#[utoipa::path(post, path = "/api/v1/purchase-orders/{id}/close", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Purchase order ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = PurchaseOrderDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn close(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<PurchaseOrderDto>, ApiError> {
    order_action(&state, &session, &id, OrderAction::Close).await
}

/// `POST /api/v1/purchase-orders/{id}/cancel`: only while nothing was received
/// (`purchase.order`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (closed or partly received).
#[utoipa::path(post, path = "/api/v1/purchase-orders/{id}/cancel", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Purchase order ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = PurchaseOrderDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn cancel(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<PurchaseOrderDto>, ApiError> {
    order_action(&state, &session, &id, OrderAction::Cancel).await
}

// ---------------------------------------------------------------------------------------------
// Goods receipts

/// A line of a goods receipt body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReceiptLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit, thousandths (> 0).
    pub qty_thousandths: i64,
    /// Price per unit before discount, in the receipt's tax mode (Rp).
    pub price: i64,
    /// Line discount (Rp).
    #[serde(default)]
    pub discount: i64,
    /// The PO it fills.
    #[serde(default)]
    pub order_id: Option<String>,
    /// The PO line it fills.
    #[serde(default)]
    pub order_line_no: Option<u32>,
    /// Batch number (tracked products), 0–40 characters.
    #[serde(default)]
    pub batch: Option<String>,
    /// Expiry `YYYY-MM-DD` (required for tracked products when posted).
    #[serde(default)]
    pub expiry: Option<String>,
}

/// Body of `PUT /goods-receipts/{id}` (a draft).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReceiptBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// The supplier.
    pub supplier_id: String,
    /// The location (default: the receiving default).
    #[serde(default)]
    pub location_id: Option<String>,
    /// The supplier's invoice number (1–60).
    pub invoice_no: String,
    /// `YYYY-MM-DD`.
    pub invoice_date: String,
    /// `credit` (default) or `cash`.
    #[serde(default)]
    pub payment: Option<String>,
    /// `YYYY-MM-DD`; default the invoice date + the supplier's terms.
    #[serde(default)]
    pub due_date: Option<String>,
    /// `included`, `added` or `none` (default).
    #[serde(default)]
    pub tax_mode: Option<String>,
    /// The PPN rate in basis points; default the company's standard rate.
    #[serde(default)]
    pub tax_rate_bp: Option<u16>,
    /// Header discount (Rp).
    #[serde(default)]
    pub header_discount: i64,
    /// Other costs (Rp), spread over the lines by value.
    #[serde(default)]
    pub other_costs: i64,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<ReceiptLineBody>,
}

/// A goods receipt line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReceiptLineDto {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor, thousandths.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit (invoice).
    pub price: i64,
    /// Line discount.
    pub discount: i64,
    /// The PO.
    pub order_id: Option<String>,
    /// The PO line.
    pub order_line_no: Option<u32>,
    /// Batch.
    pub batch: Option<String>,
    /// `YYYY-MM-DD`.
    pub expiry: Option<String>,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Its share of the header discount (posted).
    pub header_discount: Option<i64>,
    /// Its PPN (posted).
    pub tax: Option<i64>,
    /// Its share of the other costs (posted).
    pub other_costs: Option<i64>,
    /// Its landed cost (posted); left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub landed: Option<Option<i64>>,
    /// The new unit cost per line unit (landed ÷ base qty × factor, posted); left out without
    /// `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub unit_cost: Option<Option<i64>>,
    /// The lot it went into.
    pub lot_id: Option<String>,
}

/// A PO line received beyond what was ordered (a warning, §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OrderLineRef {
    /// The PO.
    pub order_id: String,
    /// The line.
    pub line_no: u32,
}

/// A goods receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReceiptDto {
    /// ID.
    pub id: String,
    /// `JKT01-GR-261126-01` once posted.
    pub number: Option<String>,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    pub location_id: String,
    /// `draft`, `posted` or `voided`.
    pub status: String,
    /// The invoice number.
    pub invoice_no: String,
    /// `YYYY-MM-DD`.
    pub invoice_date: String,
    /// `credit` or `cash`.
    pub payment: String,
    /// `YYYY-MM-DD` (set when posted).
    pub due_date: Option<String>,
    /// `included`, `added` or `none`.
    pub tax_mode: String,
    /// Basis points.
    pub tax_rate_bp: u16,
    /// Header discount.
    pub header_discount: i64,
    /// Other costs.
    pub other_costs: i64,
    /// The company's PKP setting when posted.
    pub pkp: Option<bool>,
    /// Σ line nets (posted).
    pub net: Option<i64>,
    /// PPN (posted).
    pub tax: Option<i64>,
    /// The invoice total, the payable's amount (posted).
    pub total: Option<i64>,
    /// Σ landed costs (posted); left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub landed: Option<Option<i64>>,
    /// Note.
    pub note: Option<String>,
    /// Who entered it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: Option<i64>,
    /// Unix ms.
    pub voided_at: Option<i64>,
    /// The manager who approved the void (override).
    pub approved_by: Option<String>,
    /// The lines.
    pub lines: Vec<ReceiptLineDto>,
    /// The ledger rows (single reads and posting answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
    /// PO lines now received beyond the order (posting answer).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub over_received: Vec<OrderLineRef>,
    /// Locations and variants left below zero (posting answers, §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub negative_after: Vec<NegativeDto>,
}

fn per_line_unit(landed: Option<Money>, base: i64, factor: i64) -> Option<i64> {
    let landed = i128::from(landed?.minor());
    if base == 0 {
        return None;
    }
    let v = landed * i128::from(factor);
    let b = i128::from(base);
    let rounded = (2 * v + b) / (2 * b);
    i64::try_from(rounded).ok()
}

impl ReceiptDto {
    fn of(r: &GoodsReceipt, view: CostView) -> Self {
        Self {
            id: r.id.to_string(),
            number: r.number.clone(),
            branch_id: r.branch.to_string(),
            supplier_id: r.supplier.to_string(),
            location_id: r.location.to_string(),
            status: r.status.as_str().to_owned(),
            invoice_no: r.invoice_no.clone(),
            invoice_date: super::stock::day_text(r.invoice_date),
            payment: r.payment.as_str().to_owned(),
            due_date: opt_day_text(r.due_date),
            tax_mode: r.tax_mode.as_str().to_owned(),
            tax_rate_bp: r.tax_rate_bp,
            header_discount: r.header_discount.minor(),
            other_costs: r.other_costs.minor(),
            pkp: r.pkp,
            net: r.net.map(Money::minor),
            tax: r.tax.map(Money::minor),
            total: r.total.map(Money::minor),
            landed: view.opt(r.landed),
            note: r.note.clone(),
            user_id: r.user.to_string(),
            posted_at: ms(r.posted_at),
            voided_at: ms(r.voided_at),
            approved_by: r.approved_by.map(|u| u.to_string()),
            lines: r
                .lines
                .iter()
                .map(|l| ReceiptLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    price: l.price.minor(),
                    discount: l.discount.minor(),
                    order_id: l.order_line.map(|(o, _)| o.to_string()),
                    order_line_no: l.order_line.map(|(_, n)| n),
                    batch: l.batch.clone(),
                    expiry: opt_day_text(l.expiry),
                    base_qty_thousandths: l.base_qty.thousandths(),
                    header_discount: l.header_discount.map(Money::minor),
                    tax: l.tax.map(Money::minor),
                    other_costs: l.other_costs.map(Money::minor),
                    landed: view.opt(l.landed),
                    unit_cost: view.0.then(|| {
                        per_line_unit(l.landed, l.base_qty.thousandths(), l.factor.thousandths())
                    }),
                    lot_id: l.lot.map(|x| x.to_string()),
                })
                .collect(),
            movements: Vec::new(),
            over_received: Vec::new(),
            negative_after: Vec::new(),
        }
    }
}

/// `GET /api/v1/goods-receipts`: newest first, without movements.
///
/// # Errors
/// `403 permission_denied`, `422 invalid_field` (`status`).
#[utoipa::path(get, path = "/api/v1/goods-receipts", tag = "stock",
    security(("bearer" = [])), params(DocQuery),
    responses((status = 200, body = [ReceiptDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_receipts(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<DocQuery>,
) -> Result<Json<Vec<ReceiptDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let supplier = parse_opt_id::<SupplierId>(query.supplier_id.as_deref(), "supplier_id")?;
    let status = status_param(query.status.as_deref(), ReceiptStatus::from_key)?;
    let list = stock::list_goods_receipts(&state.db, Some(branch), supplier, status).await?;
    let view = CostView::at(&caller, branch);
    Ok(Json(list.iter().map(|r| ReceiptDto::of(r, view)).collect()))
}

async fn load_receipt(state: &AppState, id: GoodsReceiptId) -> Result<GoodsReceipt, ApiError> {
    stock::get_goods_receipt(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("goods receipt"))
}

/// `GET /api/v1/goods-receipts/{id}`: with its movements.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/goods-receipts/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Goods receipt ID")),
    responses((status = 200, body = ReceiptDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_receipt(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ReceiptDto>, ApiError> {
    let r = load_receipt(&state, parse_id(&id, "id")?).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, r.branch)?;
    let view = CostView::at(&caller, r.branch);
    let mut dto = ReceiptDto::of(&r, view);
    dto.movements = doc_movements(&state.db, *r.id.as_uuid(), view).await?;
    Ok(Json(dto))
}

fn receipt_of(
    id: GoodsReceiptId,
    branch: BranchId,
    session: &UserSession,
    body: ReceiptBody,
) -> Result<NewGoodsReceipt, ApiError> {
    let mut lines = Vec::with_capacity(body.lines.len());
    for l in body.lines {
        let order_line = match (l.order_id.as_deref(), l.order_line_no) {
            (Some(o), Some(n)) => Some((parse_id::<PurchaseOrderId>(o, "order_id")?, n)),
            (None, None) => None,
            _ => {
                return Err(ApiError::invalid_field(
                    "order_line_no",
                    "`order_id` and `order_line_no` go together",
                ));
            }
        };
        lines.push(NewGoodsReceiptLine {
            variant: parse_id(&l.variant_id, "variant_id")?,
            unit: parse_id(&l.unit_id, "unit_id")?,
            qty: qty(l.qty_thousandths),
            price: Money::from_minor(l.price),
            discount: Money::from_minor(l.discount),
            order_line,
            batch: l.batch,
            expiry: parse_opt_day("expiry", l.expiry.as_deref())?,
        });
    }
    let payment = match body.payment.as_deref().unwrap_or("credit") {
        "credit" => PaymentKind::Credit,
        "cash" => PaymentKind::Cash,
        _ => {
            return Err(ApiError::invalid_field(
                "payment",
                "`payment` is `credit` or `cash`",
            ));
        }
    };
    Ok(NewGoodsReceipt {
        id,
        branch,
        supplier: parse_id(&body.supplier_id, "supplier_id")?,
        location: parse_opt_id::<LocationId>(body.location_id.as_deref(), "location_id")?,
        invoice_no: body.invoice_no,
        invoice_date: parse_day("invoice_date", &body.invoice_date)?,
        payment,
        due_date: parse_opt_day("due_date", body.due_date.as_deref())?,
        tax_mode: tax_mode(body.tax_mode.as_deref())?,
        tax_rate_bp: body.tax_rate_bp,
        header_discount: Money::from_minor(body.header_discount),
        other_costs: Money::from_minor(body.other_costs),
        note: body.note,
        user: session.user.id,
        lines,
    })
}

fn tax_mode(value: Option<&str>) -> Result<TaxMode, ApiError> {
    TaxMode::from_key(value.unwrap_or("none")).ok_or_else(|| {
        ApiError::invalid_field("tax_mode", "`tax_mode` is `included`, `added` or `none`")
    })
}

/// `PUT /api/v1/goods-receipts/{id}`: writes a draft whole (`purchase.receive`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (posted) / `archived`,
/// `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/goods-receipts/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = ReceiptBody,
    responses((status = 201, body = ReceiptDto), (status = 200, body = ReceiptDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_receipt(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ReceiptBody>,
) -> Result<(StatusCode, Json<ReceiptDto>), ApiError> {
    let id: GoodsReceiptId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::PurchaseReceive)?;
    let r = receipt_of(id, branch, &session, body)?;
    let now = state.now();
    let (state_ref, r) = (&state, &r);
    let (written, stored) = run("goods receipt put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::put_goods_receipt(&mut tx, r, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((
        status_of(written),
        Json(ReceiptDto::of(&stored, CostView::at(&caller, branch))),
    ))
}

/// `POST /api/v1/goods-receipts/{id}/post` (`purchase.receive`, see the module docs).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (voided, or the supplier's invoice
/// number is posted already), `422 invalid_field`.
#[utoipa::path(post, path = "/api/v1/goods-receipts/{id}/post", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Goods receipt ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = ReceiptDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn post_receipt(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ReceiptDto>, ApiError> {
    let id: GoodsReceiptId = parse_id(&id, "id")?;
    let r = load_receipt(&state, id).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, r.branch, Permission::PurchaseReceive)?;
    let view = CostView::at(&caller, r.branch);
    let (now, user, state_ref) = (state.now(), session.user.id, &state);
    let (written, dto) = run("goods receipt post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, posted) = stock::post_goods_receipt(&mut tx, id, user, now).await?;
        let mut dto = ReceiptDto::of(&posted.receipt, view);
        dto.over_received = posted
            .over_received
            .iter()
            .map(|(o, n)| OrderLineRef {
                order_id: o.to_string(),
                line_no: *n,
            })
            .collect();
        dto.negative_after = negative_after(&mut tx, *id.as_uuid()).await?;
        dto.movements = doc_movements(&mut tx, *id.as_uuid(), view).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, dto))
    })
    .await?;
    if written != Written::Unchanged {
        publish_doc_stock(&state, *id.as_uuid()).await;
    }
    Ok(Json(dto))
}

/// `POST /api/v1/goods-receipts/{id}/void` (see the module docs).
///
/// # Errors
/// `403 permission_denied` / `needs_override` / `invalid_override`, `404 not_found`,
/// `409 receipt_in_use` / `conflict` (a draft).
#[utoipa::path(post, path = "/api/v1/goods-receipts/{id}/void", tag = "stock",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Goods receipt ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)"),
        ("X-Override" = Option<String>, Header, description = "Override token for `stock.adjust` (context: the receipt ID)")),
    responses((status = 200, body = ReceiptDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn void_receipt(
    State(state): State<AppState>,
    session: UserSession,
    header: OverrideHeader,
    Path(id): Path<String>,
) -> Result<Json<ReceiptDto>, ApiError> {
    let id: GoodsReceiptId = parse_id(&id, "id")?;
    let r = load_receipt(&state, id).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, r.branch, Permission::PurchaseReceive)?;
    let view = CostView::at(&caller, r.branch);
    let manager = super::stock::holds(&caller, r.branch, Permission::StockAdjust);
    let (now, user, state_ref) = (state.now(), session.user.id, &state);
    let (session_ref, caller_ref, header_ref, branch) = (&session, &caller, &header, r.branch);
    let (written, dto) = run("goods receipt void", || async move {
        let mut tx = state_ref.db.begin().await?;
        let approved_by = if manager {
            None
        } else if let Some(register) = session_ref.session.register {
            let at = RegisterSession {
                register: RegisterCtx {
                    register,
                    branch: session_ref.session.branch,
                },
                user: session_ref.clone(),
            };
            overrides::authorize(
                state_ref,
                &mut tx,
                &at,
                &caller_ref.at_scope(sultan_core::access::BranchScope::Branch(branch)),
                Action {
                    permission: Permission::StockAdjust,
                    value: None,
                    context_ref: &id.to_string(),
                },
                header_ref,
            )
            .await?
        } else {
            return Err(Fail::Api(
                AccessError::NeedsOverride {
                    permission: Permission::StockAdjust,
                    limit: None,
                }
                .into(),
            ));
        };
        let (written, voided) =
            stock::void_goods_receipt(&mut tx, id, user, approved_by, now).await?;
        let mut dto = ReceiptDto::of(&voided, view);
        dto.negative_after = negative_after(&mut tx, *id.as_uuid()).await?;
        dto.movements = doc_movements(&mut tx, *id.as_uuid(), view).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, dto))
    })
    .await?;
    if written != Written::Unchanged {
        publish_doc_stock(&state, *id.as_uuid()).await;
    }
    Ok(Json(dto))
}

// ---------------------------------------------------------------------------------------------
// Supplier returns

/// A line of a supplier return body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReturnLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit, thousandths (> 0).
    pub qty_thousandths: i64,
    /// Price per unit (Rp; the receipt line's when taken from one).
    pub price: i64,
    /// The receipt it comes from.
    #[serde(default)]
    pub receipt_id: Option<String>,
    /// The receipt line.
    #[serde(default)]
    pub receipt_line_no: Option<u32>,
}

/// Body of `PUT /supplier-returns/{id}` (a draft).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReturnBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// The supplier.
    pub supplier_id: String,
    /// The location (default: the receiving default).
    #[serde(default)]
    pub location_id: Option<String>,
    /// `included`, `added` or `none` (default); lines from a receipt use the receipt's.
    #[serde(default)]
    pub tax_mode: Option<String>,
    /// Basis points; default the company's standard rate.
    #[serde(default)]
    pub tax_rate_bp: Option<u16>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<ReturnLineBody>,
}

/// A supplier return line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnLineDto {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit.
    pub price: i64,
    /// The receipt.
    pub receipt_id: Option<String>,
    /// The receipt line.
    pub receipt_line_no: Option<u32>,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// The movement's value at the average (posted); left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
}

/// A supplier return.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReturnDto {
    /// ID.
    pub id: String,
    /// `JKT01-RS-261126-01` once posted.
    pub number: Option<String>,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    pub location_id: String,
    /// `draft` or `posted`.
    pub status: String,
    /// The tax mode.
    pub tax_mode: String,
    /// Basis points.
    pub tax_rate_bp: u16,
    /// The supplier credit (posted).
    pub credit: Option<i64>,
    /// Note.
    pub note: Option<String>,
    /// Who entered it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: Option<i64>,
    /// The lines.
    pub lines: Vec<ReturnLineDto>,
    /// The ledger rows (single reads and posting answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
    /// Locations and variants left below zero (posting answer, §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub negative_after: Vec<NegativeDto>,
}

impl ReturnDto {
    fn of(r: &SupplierReturn, view: CostView) -> Self {
        Self {
            id: r.id.to_string(),
            number: r.number.clone(),
            branch_id: r.branch.to_string(),
            supplier_id: r.supplier.to_string(),
            location_id: r.location.to_string(),
            status: if r.posted { "posted" } else { "draft" }.to_owned(),
            tax_mode: r.tax_mode.as_str().to_owned(),
            tax_rate_bp: r.tax_rate_bp,
            credit: r.credit.map(Money::minor),
            note: r.note.clone(),
            user_id: r.user.to_string(),
            posted_at: ms(r.posted_at),
            lines: r
                .lines
                .iter()
                .map(|l| ReturnLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    price: l.price.minor(),
                    receipt_id: l.receipt_line.map(|(r, _)| r.to_string()),
                    receipt_line_no: l.receipt_line.map(|(_, n)| n),
                    base_qty_thousandths: l.base_qty.thousandths(),
                    value: view.opt(l.value),
                })
                .collect(),
            movements: Vec::new(),
            negative_after: Vec::new(),
        }
    }
}

/// `GET /api/v1/supplier-returns`: newest first.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/supplier-returns", tag = "stock",
    security(("bearer" = [])), params(DocQuery),
    responses((status = 200, body = [ReturnDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_returns(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<DocQuery>,
) -> Result<Json<Vec<ReturnDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let supplier = parse_opt_id::<SupplierId>(query.supplier_id.as_deref(), "supplier_id")?;
    let list = stock::list_supplier_returns(&state.db, Some(branch), supplier).await?;
    let view = CostView::at(&caller, branch);
    Ok(Json(list.iter().map(|r| ReturnDto::of(r, view)).collect()))
}

async fn load_return(state: &AppState, id: SupplierReturnId) -> Result<SupplierReturn, ApiError> {
    stock::get_supplier_return(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("supplier return"))
}

/// `GET /api/v1/supplier-returns/{id}`: with its movements.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/supplier-returns/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Supplier return ID")),
    responses((status = 200, body = ReturnDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_return(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ReturnDto>, ApiError> {
    let r = load_return(&state, parse_id(&id, "id")?).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, r.branch)?;
    let view = CostView::at(&caller, r.branch);
    let mut dto = ReturnDto::of(&r, view);
    dto.movements = doc_movements(&state.db, *r.id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `PUT /api/v1/supplier-returns/{id}`: writes a draft whole (`purchase.receive`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (posted), `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/supplier-returns/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = ReturnBody,
    responses((status = 201, body = ReturnDto), (status = 200, body = ReturnDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_return(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<ReturnBody>,
) -> Result<(StatusCode, Json<ReturnDto>), ApiError> {
    let id: SupplierReturnId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::PurchaseReceive)?;
    let mut lines = Vec::with_capacity(body.lines.len());
    for l in body.lines {
        let receipt_line = match (l.receipt_id.as_deref(), l.receipt_line_no) {
            (Some(r), Some(n)) => Some((parse_id::<GoodsReceiptId>(r, "receipt_id")?, n)),
            (None, None) => None,
            _ => {
                return Err(ApiError::invalid_field(
                    "receipt_line_no",
                    "`receipt_id` and `receipt_line_no` go together",
                ));
            }
        };
        lines.push(NewSupplierReturnLine {
            variant: parse_id(&l.variant_id, "variant_id")?,
            unit: parse_id(&l.unit_id, "unit_id")?,
            qty: qty(l.qty_thousandths),
            price: Money::from_minor(l.price),
            receipt_line,
        });
    }
    let r = NewSupplierReturn {
        id,
        branch,
        supplier: parse_id(&body.supplier_id, "supplier_id")?,
        location: parse_opt_id(body.location_id.as_deref(), "location_id")?,
        tax_mode: tax_mode(body.tax_mode.as_deref())?,
        tax_rate_bp: body.tax_rate_bp,
        note: body.note,
        user: session.user.id,
        lines,
    };
    let now = state.now();
    let (state_ref, r) = (&state, &r);
    let (written, stored) = run("supplier return put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::put_supplier_return(&mut tx, r, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((
        status_of(written),
        Json(ReturnDto::of(&stored, CostView::at(&caller, branch))),
    ))
}

/// `POST /api/v1/supplier-returns/{id}/post` (`purchase.receive`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `422 invalid_field` (`qty` above what the
/// receipt line has left).
#[utoipa::path(post, path = "/api/v1/supplier-returns/{id}/post", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Supplier return ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = ReturnDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn post_return(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ReturnDto>, ApiError> {
    let id: SupplierReturnId = parse_id(&id, "id")?;
    let r = load_return(&state, id).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, r.branch, Permission::PurchaseReceive)?;
    let view = CostView::at(&caller, r.branch);
    let (now, user, state_ref) = (state.now(), session.user.id, &state);
    let (written, dto) = run("supplier return post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, posted) = stock::post_supplier_return(&mut tx, id, user, now).await?;
        let mut dto = ReturnDto::of(&posted, view);
        dto.negative_after = negative_after(&mut tx, *id.as_uuid()).await?;
        dto.movements = doc_movements(&mut tx, *id.as_uuid(), view).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, dto))
    })
    .await?;
    if written != Written::Unchanged {
        publish_doc_stock(&state, *id.as_uuid()).await;
    }
    Ok(Json(dto))
}

// ---------------------------------------------------------------------------------------------
// Reorder

/// Query with a branch.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct BranchQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
}

/// A reorder rule (base units).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ReorderRuleDto {
    /// The branch.
    pub branch_id: String,
    /// The variant.
    pub variant_id: String,
    /// Reorder at or below this.
    pub min_thousandths: i64,
    /// Order up to this.
    pub max_thousandths: i64,
}

impl From<&ReorderRule> for ReorderRuleDto {
    fn from(r: &ReorderRule) -> Self {
        Self {
            branch_id: r.branch.to_string(),
            variant_id: r.variant.to_string(),
            min_thousandths: r.min.thousandths(),
            max_thousandths: r.max.thousandths(),
        }
    }
}

/// Body of `PUT /reorder-rules/{branch_id}/{variant_id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct ReorderRuleBody {
    /// Reorder at or below this (base units, thousandths, ≥ 0).
    pub min_thousandths: i64,
    /// Order up to this (≥ min).
    pub max_thousandths: i64,
}

/// `GET /api/v1/reorder-rules?branch_id`.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/reorder-rules", tag = "stock",
    security(("bearer" = [])), params(BranchQuery),
    responses((status = 200, body = [ReorderRuleDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_rules(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<BranchQuery>,
) -> Result<Json<Vec<ReorderRuleDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let list = stock::list_reorder_rules(&state.db, branch).await?;
    Ok(Json(list.iter().map(ReorderRuleDto::from).collect()))
}

/// `PUT /api/v1/reorder-rules/{branch_id}/{variant_id}` (`purchase.order` at the branch).
///
/// # Errors
/// `403 permission_denied`, `422 invalid_field` (`min`, `max`).
#[utoipa::path(put, path = "/api/v1/reorder-rules/{branch_id}/{variant_id}", tag = "stock",
    security(("bearer" = [])),
    params(("branch_id" = String, Path, description = "Branch ID"), ("variant_id" = String, Path, description = "Variant ID")),
    request_body = ReorderRuleBody,
    responses((status = 201, body = ReorderRuleDto), (status = 200, body = ReorderRuleDto),
        (status = 403, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_rule(
    State(state): State<AppState>,
    session: UserSession,
    Path((branch, variant)): Path<(String, String)>,
    ApiJson(body): ApiJson<ReorderRuleBody>,
) -> Result<(StatusCode, Json<ReorderRuleDto>), ApiError> {
    let branch: BranchId = parse_id(&branch, "branch_id")?;
    let variant: VariantId = parse_id(&variant, "variant_id")?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::PurchaseOrder)?;
    let rule = ReorderRule {
        branch,
        variant,
        min: qty(body.min_thousandths),
        max: qty(body.max_thousandths),
    };
    let now = state.now();
    let (state_ref, actor, rule_ref) = (&state, &actor(&session), &rule);
    let written = run("reorder rule put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let w = stock::put_reorder_rule(&mut tx, rule_ref, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(w)
    })
    .await?;
    Ok((status_of(written), Json(ReorderRuleDto::from(&rule))))
}

/// Query of `GET /reorder/suggestions`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct SuggestionQuery {
    /// The supplier.
    pub supplier_id: String,
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
}

/// A suggested PO line or a low-stock row (§10).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SuggestedLineDto {
    /// The variant.
    pub variant_id: String,
    /// Branch on hand, base units.
    pub on_hand_thousandths: i64,
    /// On order.
    pub on_order_thousandths: i64,
    /// The rule's min.
    pub min_thousandths: Option<i64>,
    /// The rule's max.
    pub max_thousandths: Option<i64>,
    /// The purchase unit (`null` in the low-stock list).
    pub unit_id: Option<String>,
    /// Whole purchase units suggested.
    pub units: i64,
    /// The same in base units.
    pub base_qty_thousandths: i64,
    /// The supplier's last price per purchase unit; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub price: Option<Option<i64>>,
}

impl SuggestedLineDto {
    fn of(l: &SuggestedLine, view: CostView) -> Self {
        Self {
            variant_id: l.variant.to_string(),
            on_hand_thousandths: l.on_hand.thousandths(),
            on_order_thousandths: l.on_order.thousandths(),
            min_thousandths: l.rule.map(|(m, _)| m.thousandths()),
            max_thousandths: l.rule.map(|(_, m)| m.thousandths()),
            unit_id: l.unit.map(|u| u.to_string()),
            units: l.units,
            base_qty_thousandths: l.base_qty.thousandths(),
            price: view.opt(l.price),
        }
    }
}

/// `GET /api/v1/reorder/suggestions?supplier_id&branch_id` (`purchase.order`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found` (supplier).
#[utoipa::path(get, path = "/api/v1/reorder/suggestions", tag = "stock",
    security(("bearer" = [])), params(SuggestionQuery),
    responses((status = 200, body = [SuggestedLineDto]), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn suggestions(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<SuggestionQuery>,
) -> Result<Json<Vec<SuggestedLineDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let supplier: SupplierId = parse_id(&query.supplier_id, "supplier_id")?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::PurchaseOrder)?;
    let lines = stock::suggest_purchase_order(&state.db, supplier, branch, state.now())
        .await
        .map_err(ApiError::from)?;
    let view = CostView::at(&caller, branch);
    Ok(Json(
        lines
            .iter()
            .map(|l| SuggestedLineDto::of(l, view))
            .collect(),
    ))
}

/// `GET /api/v1/reorder/low-stock?branch_id`: variants with a rule at or below min (on hand +
/// on order), and negative balances.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/reorder/low-stock", tag = "stock",
    security(("bearer" = [])), params(BranchQuery),
    responses((status = 200, body = [SuggestedLineDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn low_stock(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<BranchQuery>,
) -> Result<Json<Vec<SuggestedLineDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let lines = stock::low_stock(&state.db, branch).await?;
    let view = CostView::at(&caller, branch);
    Ok(Json(
        lines
            .iter()
            .map(|l| SuggestedLineDto::of(l, view))
            .collect(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_cost_per_line_unit_rounds_half_up() {
        let m = |v| Some(Money::from_minor(v));
        // 48 base units landed at 96 000 → 2 000 per base unit, 48 000 per box of 24.
        assert_eq!(per_line_unit(m(96_000), 48_000, 24_000), Some(48_000));
        assert_eq!(per_line_unit(m(1_000), 3_000, 1_000), Some(333));
        assert_eq!(per_line_unit(m(2_000), 3_000, 1_000), Some(667));
        assert_eq!(per_line_unit(None, 3_000, 1_000), None);
        assert_eq!(per_line_unit(m(1), 0, 1_000), None);
    }

    #[test]
    fn statuses_and_modes() {
        assert_eq!(
            status_param(Some("ordered"), PurchaseOrderStatus::from_key).unwrap(),
            Some(PurchaseOrderStatus::Ordered)
        );
        assert_eq!(
            status_param(Some(""), ReceiptStatus::from_key).unwrap(),
            None
        );
        assert_eq!(
            status_param(Some("x"), ReceiptStatus::from_key)
                .unwrap_err()
                .code(),
            "invalid_field"
        );
        assert_eq!(tax_mode(None).unwrap(), TaxMode::None);
        assert_eq!(tax_mode(Some("added")).unwrap(), TaxMode::Added);
        assert_eq!(tax_mode(Some("x")).unwrap_err().code(), "invalid_field");
    }
}
