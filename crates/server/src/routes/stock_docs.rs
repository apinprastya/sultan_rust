//! Stock documents of a branch (spec 2.8 §7, §8, §11; phase N3): transfers between branches,
//! internal transfers, adjustments, opening stock and opnames. Each write checks its key at the
//! branch it acts for (a body without `branch_id` is the session's branch); reads need any
//! inventory key there (see [`super::stock`]). Values are left out without `report.profit`.
//!
//! - `GET /transfers?branch_id&status` (either end), `GET /transfers/{id}`,
//!   `PUT /transfers/{id}` `{from_branch_id, to_branch_id, to_location_id?, ship_from_location_id?,
//!   lines}`: a request (`stock.transfer` at the receiver), or with `ship_from_location_id` one
//!   shipped at once (`stock.transfer` at the sender). `POST …/{id}/ship`
//!   `{location_id?, lines?: [{line_no, qty_thousandths}]}` (the sender's location), `…/receive`
//!   `{location_id?, lines?}` (the receiver's; more than shipped is `422 invalid_field` `qty`, a
//!   shortage is a transit loss at the sender), `…/cancel` (a request only; either end). A branch
//!   this node does not hold is `409 other_node`. Publishes `transfer.changed` to both branches.
//! - `GET /internal-transfers?branch_id`, `GET /internal-transfers/{id}`,
//!   `PUT /internal-transfers/{id}` (posts at once, `stock.transfer`).
//! - `GET /adjustments?branch_id`, `GET /adjustments/{id}`, `PUT /adjustments/{id}` (posts at
//!   once, `stock.adjust`); the same for `/opening-stock`.
//! - `GET /opnames?branch_id`, `GET /opnames/{id}` (lines with snapshot, movements since,
//!   counted and variance), `PUT /opnames/{id}` `{location_id?, scope: full|cycle,
//!   variant_ids?, category_ids?}` (start, `stock.count`; `409 opname_open` naming the other
//!   opname), `POST …/{id}/counts` `{entries: [{id, variant_id, unit_id, qty_thousandths,
//!   replace}]}` (`stock.count`; an entry is stored once by its ID), `POST …/{id}/post`
//!   `{uncounted: set_zero|leave}` (`stock.count` **and** `stock.adjust`), `…/cancel`
//!   (`stock.count`).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::Permission;
use sultan_core::ids::{
    AdjustmentId, BranchId, CategoryId, InternalTransferId, LocationId, OpeningStockId, OpnameId,
    StockReasonId, TransferId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::Uncounted;
use sultan_db::stock::{
    self, Adjustment, CountInput, InternalTransfer, NewAdjustment, NewAdjustmentLine,
    NewInternalTransfer, NewOpeningLine, NewOpeningStock, NewOpname, NewTransfer, NewTransferLine,
    OpeningStock, Opname, OpnameScope, Transfer, TransferStatus, Written,
};
use utoipa::{IntoParams, ToSchema};

use super::stock::{
    CostView, Fail, LineQtyBody, MovementDto, NegativeDto, QtyLineBody, branch_or_session,
    doc_movements, negative_after, opt_day_text, parse_opt_day, publish_doc_stock, qty, require_at,
    require_read, run, status_of,
};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, UserSession};
use crate::state::AppState;

/// Query of the branch document lists.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ListQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
    /// One status (transfers: `requested`, `shipped`, `received`, `cancelled`).
    pub status: Option<String>,
}

fn ms(t: Option<sultan_core::time::Timestamp>) -> Option<i64> {
    t.map(sultan_core::time::Timestamp::unix_millis)
}

fn lines_of(lines: &[QtyLineBody]) -> Result<Vec<NewTransferLine>, ApiError> {
    lines
        .iter()
        .map(|l| {
            Ok(NewTransferLine {
                variant: parse_id(&l.variant_id, "variant_id")?,
                unit: parse_id(&l.unit_id, "unit_id")?,
                qty: qty(l.qty_thousandths),
            })
        })
        .collect()
}

fn line_qtys(lines: &[LineQtyBody]) -> Vec<(u32, sultan_core::catalog::Qty)> {
    lines
        .iter()
        .map(|l| (l.line_no, qty(l.qty_thousandths)))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// Transfers between branches

/// Body of `PUT /transfers/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct TransferBody {
    /// The sending branch.
    pub from_branch_id: String,
    /// The receiving branch.
    pub to_branch_id: String,
    /// The location to receive into (default: the receiving default).
    #[serde(default)]
    pub to_location_id: Option<String>,
    /// Ship at once from this location (no request).
    #[serde(default)]
    pub ship_from_location_id: Option<String>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<QtyLineBody>,
}

/// Body of `POST /transfers/{id}/ship` and `/receive`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct MoveBody {
    /// The sender's location (ship) or the receiver's (receive); default as stored, else the
    /// receiving default.
    #[serde(default)]
    pub location_id: Option<String>,
    /// Quantities per line in the line's unit; a line left out ships (receives) in full.
    #[serde(default)]
    pub lines: Vec<LineQtyBody>,
}

/// A lot travelling with a transfer line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TravelLotDto {
    /// Batch.
    pub batch: String,
    /// `YYYY-MM-DD`.
    pub expiry: String,
    /// Base units.
    pub qty_thousandths: i64,
}

/// A transfer line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TransferLineDto {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Requested quantity in the unit.
    pub qty_thousandths: i64,
    /// Shipped base units.
    pub shipped_qty_thousandths: Option<i64>,
    /// Received base units.
    pub received_qty_thousandths: Option<i64>,
    /// Lost base units (transit loss at the sender).
    pub lost_qty_thousandths: Option<i64>,
    /// Shipped value; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub shipped_value: Option<Option<i64>>,
    /// Received value; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub received_value: Option<Option<i64>>,
    /// Lost value; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub lost_value: Option<Option<i64>>,
    /// The lots that travel.
    pub lots: Vec<TravelLotDto>,
}

/// A transfer between branches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TransferDto {
    /// ID.
    pub id: String,
    /// `JKT01-TR-261126-01` (the creating branch's).
    pub number: String,
    /// The sending branch.
    pub from_branch_id: String,
    /// The sender's location (once shipped).
    pub from_location_id: Option<String>,
    /// The receiving branch.
    pub to_branch_id: String,
    /// The receiver's location.
    pub to_location_id: Option<String>,
    /// `requested`, `shipped`, `received` or `cancelled`.
    pub status: String,
    /// Note.
    pub note: Option<String>,
    /// Who created it.
    pub user_id: String,
    /// Unix ms.
    pub shipped_at: Option<i64>,
    /// Unix ms.
    pub received_at: Option<i64>,
    /// The lines.
    pub lines: Vec<TransferLineDto>,
    /// The ledger rows (single reads and action answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
    /// Locations and variants left below zero (ship answer, §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub negative_after: Vec<NegativeDto>,
}

impl TransferDto {
    fn of(t: &Transfer, view: CostView) -> Self {
        let q =
            |v: Option<sultan_core::catalog::Qty>| v.map(sultan_core::catalog::Qty::thousandths);
        Self {
            id: t.id.to_string(),
            number: t.number.clone(),
            from_branch_id: t.from_branch.to_string(),
            from_location_id: t.from_location.map(|l| l.to_string()),
            to_branch_id: t.to_branch.to_string(),
            to_location_id: t.to_location.map(|l| l.to_string()),
            status: t.status.as_str().to_owned(),
            note: t.note.clone(),
            user_id: t.user.to_string(),
            shipped_at: ms(t.shipped_at),
            received_at: ms(t.received_at),
            lines: t
                .lines
                .iter()
                .map(|l| TransferLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    shipped_qty_thousandths: q(l.shipped_qty),
                    received_qty_thousandths: q(l.received_qty),
                    lost_qty_thousandths: q(l.lost_qty),
                    shipped_value: view.opt(l.shipped_value),
                    received_value: view.opt(l.received_value),
                    lost_value: view.opt(l.lost_value),
                    lots: l
                        .lots
                        .iter()
                        .map(|(b, e, n)| TravelLotDto {
                            batch: b.clone(),
                            expiry: super::stock::day_text(*e),
                            qty_thousandths: n.thousandths(),
                        })
                        .collect(),
                })
                .collect(),
            movements: Vec::new(),
            negative_after: Vec::new(),
        }
    }
}

/// Cost view of a transfer: `report.profit` at either end.
fn transfer_view(caller: &Caller, t: &Transfer) -> CostView {
    CostView(CostView::at(caller, t.from_branch).0 || CostView::at(caller, t.to_branch).0)
}

fn publish_transfer(state: &AppState, t: &Transfer) {
    let data = json!({
        "transfer_id": t.id.to_string(),
        "number": t.number,
        "status": t.status.as_str(),
        "from_branch_id": t.from_branch.to_string(),
        "to_branch_id": t.to_branch.to_string(),
    });
    state.events.publish(Event::for_branch(
        "transfer.changed",
        t.from_branch,
        data.clone(),
    ));
    state
        .events
        .publish(Event::for_branch("transfer.changed", t.to_branch, data));
}

/// `GET /api/v1/transfers?branch_id&status`: newest first, either end.
///
/// # Errors
/// `403 permission_denied`, `422 invalid_field` (`status`).
#[utoipa::path(get, path = "/api/v1/transfers", tag = "stock",
    security(("bearer" = [])), params(ListQuery),
    responses((status = 200, body = [TransferDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_transfers(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<TransferDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let status = query
        .status
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| {
            TransferStatus::from_key(s)
                .ok_or_else(|| ApiError::invalid_field("status", "unknown `status`"))
        })
        .transpose()?;
    let list = stock::list_transfers(&state.db, Some(branch), status).await?;
    Ok(Json(
        list.iter()
            .map(|t| TransferDto::of(t, transfer_view(&caller, t)))
            .collect(),
    ))
}

async fn load_transfer(state: &AppState, id: TransferId) -> Result<Transfer, ApiError> {
    stock::get_transfer(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("transfer"))
}

/// `GET /api/v1/transfers/{id}`: with its movements at both ends.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/transfers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Transfer ID")),
    responses((status = 200, body = TransferDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_transfer(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<TransferDto>, ApiError> {
    let t = load_transfer(&state, parse_id(&id, "id")?).await?;
    let caller = Caller::load(&state.db, &session).await?;
    if require_read(&caller, t.from_branch).is_err() {
        require_read(&caller, t.to_branch)?;
    }
    let view = transfer_view(&caller, &t);
    let mut dto = TransferDto::of(&t, view);
    dto.movements = doc_movements(&state.db, *t.id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `PUT /api/v1/transfers/{id}`: a request, or a transfer shipped at once (see the module
/// docs). The same ID again answers the stored transfer.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 other_node`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/transfers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = TransferBody,
    responses((status = 201, body = TransferDto), (status = 200, body = TransferDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_transfer(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<TransferBody>,
) -> Result<(StatusCode, Json<TransferDto>), ApiError> {
    let id: TransferId = parse_put_id(&id)?;
    let from: BranchId = parse_id(&body.from_branch_id, "from_branch_id")?;
    let to: BranchId = parse_id(&body.to_branch_id, "to_branch_id")?;
    let ship_from = parse_opt_id::<LocationId>(
        body.ship_from_location_id.as_deref(),
        "ship_from_location_id",
    )?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(
        &caller,
        if ship_from.is_some() { from } else { to },
        Permission::StockTransfer,
    )?;
    let t = NewTransfer {
        id,
        from_branch: from,
        to_branch: to,
        to_location: parse_opt_id(body.to_location_id.as_deref(), "to_location_id")?,
        ship_from,
        note: body.note,
        user: session.user.id,
        lines: lines_of(&body.lines)?,
    };
    let now = state.now();
    let (state_ref, t_ref) = (&state, &t);
    let (written, stored, negative) = run("transfer put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, stored) = stock::create_transfer(&mut tx, t_ref, now).await?;
        let negative = negative_after(&mut tx, *id.as_uuid()).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored, negative))
    })
    .await?;
    let view = transfer_view(&caller, &stored);
    let mut dto = TransferDto::of(&stored, view);
    dto.negative_after = negative;
    if written != Written::Unchanged {
        publish_transfer(&state, &stored);
        if stored.status == TransferStatus::Shipped {
            publish_doc_stock(&state, *id.as_uuid()).await;
        }
    }
    dto.movements = doc_movements(&state.db, *id.as_uuid(), view).await?;
    Ok((status_of(written), Json(dto)))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransferAction {
    Ship,
    Receive,
    Cancel,
}

async fn transfer_action(
    state: &AppState,
    session: &UserSession,
    id: &str,
    action: TransferAction,
    body: MoveBody,
) -> Result<Json<TransferDto>, ApiError> {
    let id: TransferId = parse_id(id, "id")?;
    let t = load_transfer(state, id).await?;
    let caller = Caller::load(&state.db, session).await?;
    match action {
        TransferAction::Ship => require_at(&caller, t.from_branch, Permission::StockTransfer)?,
        TransferAction::Receive => require_at(&caller, t.to_branch, Permission::StockTransfer)?,
        TransferAction::Cancel => {
            if require_at(&caller, t.to_branch, Permission::StockTransfer).is_err() {
                require_at(&caller, t.from_branch, Permission::StockTransfer)?;
            }
        }
    }
    let location = parse_opt_id::<LocationId>(body.location_id.as_deref(), "location_id")?;
    let lines = line_qtys(&body.lines);
    let (now, user) = (state.now(), session.user.id);
    let lines_ref = &lines;
    let (written, stored, negative) = run("transfer action", || async move {
        let mut tx = state.db.begin().await?;
        let (written, stored) = match action {
            TransferAction::Ship => {
                stock::ship_transfer(&mut tx, id, location, lines_ref, user, now).await?
            }
            TransferAction::Receive => {
                stock::receive_transfer(&mut tx, id, location, lines_ref, user, now).await?
            }
            TransferAction::Cancel => stock::cancel_transfer(&mut tx, id, user, now).await?,
        };
        let negative = negative_after(&mut tx, *id.as_uuid()).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, stored, negative))
    })
    .await?;
    let view = transfer_view(&caller, &stored);
    let mut dto = TransferDto::of(&stored, view);
    dto.negative_after = negative;
    if written != Written::Unchanged {
        publish_transfer(state, &stored);
        if action != TransferAction::Cancel {
            publish_doc_stock(state, *id.as_uuid()).await;
        }
    }
    dto.movements = doc_movements(&state.db, *id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `POST /api/v1/transfers/{id}/ship` (`stock.transfer` at the sender).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (not requested),
/// `422 invalid_field`.
#[utoipa::path(post, path = "/api/v1/transfers/{id}/ship", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Transfer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = MoveBody,
    responses((status = 200, body = TransferDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn ship(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<TransferDto>, ApiError> {
    let body: MoveBody = super::stock::optional_body(&body)?;
    transfer_action(&state, &session, &id, TransferAction::Ship, body).await
}

/// `POST /api/v1/transfers/{id}/receive` (`stock.transfer` at the receiver).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (not shipped),
/// `422 invalid_field` (`qty` above shipped).
#[utoipa::path(post, path = "/api/v1/transfers/{id}/receive", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Transfer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = MoveBody,
    responses((status = 200, body = TransferDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn receive(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<TransferDto>, ApiError> {
    let body: MoveBody = super::stock::optional_body(&body)?;
    transfer_action(&state, &session, &id, TransferAction::Receive, body).await
}

/// `POST /api/v1/transfers/{id}/cancel`: a request only (`stock.transfer` at either end).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (shipped or received).
#[utoipa::path(post, path = "/api/v1/transfers/{id}/cancel", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Transfer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = TransferDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn cancel_transfer(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<TransferDto>, ApiError> {
    transfer_action(
        &state,
        &session,
        &id,
        TransferAction::Cancel,
        MoveBody::default(),
    )
    .await
}

// ---------------------------------------------------------------------------------------------
// Internal transfers

/// Body of `PUT /internal-transfers/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct InternalTransferBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// From this location.
    pub from_location_id: String,
    /// To this location.
    pub to_location_id: String,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<QtyLineBody>,
}

/// A line of a posted-at-once document (internal transfer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct InternalLineDto {
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
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Value moved; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
}

/// An internal transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct InternalTransferDto {
    /// ID.
    pub id: String,
    /// `JKT01-IT-261126-01`.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// From.
    pub from_location_id: String,
    /// To.
    pub to_location_id: String,
    /// Note.
    pub note: Option<String>,
    /// Who posted it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: i64,
    /// The lines.
    pub lines: Vec<InternalLineDto>,
    /// The ledger rows (single reads and posting answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
    /// Locations and variants left below zero (posting answer, §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub negative_after: Vec<NegativeDto>,
}

impl InternalTransferDto {
    fn of(t: &InternalTransfer, view: CostView) -> Self {
        Self {
            id: t.id.to_string(),
            number: t.number.clone(),
            branch_id: t.branch.to_string(),
            from_location_id: t.from_location.to_string(),
            to_location_id: t.to_location.to_string(),
            note: t.note.clone(),
            user_id: t.user.to_string(),
            posted_at: t.posted_at.unix_millis(),
            lines: t
                .lines
                .iter()
                .map(|l| InternalLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    base_qty_thousandths: l.base_qty.thousandths(),
                    value: view.opt(l.value),
                })
                .collect(),
            movements: Vec::new(),
            negative_after: Vec::new(),
        }
    }
}

/// `GET /api/v1/internal-transfers?branch_id`: newest first.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/internal-transfers", tag = "stock",
    security(("bearer" = [])), params(ListQuery),
    responses((status = 200, body = [InternalTransferDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_internal(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<InternalTransferDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let list = stock::list_internal_transfers(&state.db, branch).await?;
    Ok(Json(
        list.iter()
            .map(|t| InternalTransferDto::of(t, view))
            .collect(),
    ))
}

/// `GET /api/v1/internal-transfers/{id}`.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/internal-transfers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Internal transfer ID")),
    responses((status = 200, body = InternalTransferDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_internal(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<InternalTransferDto>, ApiError> {
    let id: InternalTransferId = parse_id(&id, "id")?;
    let t = stock::get_internal_transfer(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("internal transfer"))?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, t.branch)?;
    let view = CostView::at(&caller, t.branch);
    let mut dto = InternalTransferDto::of(&t, view);
    dto.movements = doc_movements(&state.db, *id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `PUT /api/v1/internal-transfers/{id}`: posts at once (`stock.transfer`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/internal-transfers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = InternalTransferBody,
    responses((status = 201, body = InternalTransferDto), (status = 200, body = InternalTransferDto),
        (status = 403, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_internal(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<InternalTransferBody>,
) -> Result<(StatusCode, Json<InternalTransferDto>), ApiError> {
    let id: InternalTransferId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::StockTransfer)?;
    let t = NewInternalTransfer {
        id,
        branch,
        from_location: parse_id(&body.from_location_id, "from_location_id")?,
        to_location: parse_id(&body.to_location_id, "to_location_id")?,
        note: body.note,
        user: session.user.id,
        lines: lines_of(&body.lines)?,
    };
    let view = CostView::at(&caller, branch);
    let now = state.now();
    let (state_ref, t_ref) = (&state, &t);
    let (written, dto) = run("internal transfer post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, stored) = stock::post_internal_transfer(&mut tx, t_ref, now).await?;
        let mut dto = InternalTransferDto::of(&stored, view);
        dto.negative_after = negative_after(&mut tx, *id.as_uuid()).await?;
        dto.movements = doc_movements(&mut tx, *id.as_uuid(), view).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, dto))
    })
    .await?;
    if written != Written::Unchanged {
        publish_doc_stock(&state, *id.as_uuid()).await;
    }
    Ok((status_of(written), Json(dto)))
}

// ---------------------------------------------------------------------------------------------
// Adjustments and opening stock

/// A line of an adjustment body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AdjustmentLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Signed quantity in the unit, thousandths (≠ 0).
    pub qty_thousandths: i64,
    /// The stock reason (its direction must allow the sign).
    pub reason_id: String,
    /// A note (required by Koreksi).
    #[serde(default)]
    pub note: Option<String>,
    /// Unit cost per line unit for an increase while the variant has no cost yet.
    #[serde(default)]
    pub unit_cost: Option<i64>,
}

/// Body of `PUT /adjustments/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct AdjustmentBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// The location (default: the receiving default).
    #[serde(default)]
    pub location_id: Option<String>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<AdjustmentLineBody>,
}

/// An adjustment line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AdjustmentLineDto {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Signed quantity in the unit.
    pub qty_thousandths: i64,
    /// The reason.
    pub reason_id: String,
    /// The note.
    pub note: Option<String>,
    /// The typed unit cost; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub unit_cost: Option<Option<i64>>,
    /// Signed base units.
    pub base_qty_thousandths: i64,
    /// The movement's value; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
}

/// A posted adjustment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct AdjustmentDto {
    /// ID.
    pub id: String,
    /// `JKT01-ADJ-261126-01`.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// Note.
    pub note: Option<String>,
    /// Who posted it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: i64,
    /// The lines.
    pub lines: Vec<AdjustmentLineDto>,
    /// The ledger rows (single reads and posting answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
    /// Locations and variants left below zero (posting answer, §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub negative_after: Vec<NegativeDto>,
}

impl AdjustmentDto {
    fn of(a: &Adjustment, view: CostView) -> Self {
        Self {
            id: a.id.to_string(),
            number: a.number.clone(),
            branch_id: a.branch.to_string(),
            location_id: a.location.to_string(),
            note: a.note.clone(),
            user_id: a.user.to_string(),
            posted_at: a.posted_at.unix_millis(),
            lines: a
                .lines
                .iter()
                .map(|l| AdjustmentLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    reason_id: l.reason.to_string(),
                    note: l.note.clone(),
                    unit_cost: view.0.then(|| l.unit_cost.map(Money::minor)),
                    base_qty_thousandths: l.base_qty.thousandths(),
                    value: view.opt(l.value),
                })
                .collect(),
            movements: Vec::new(),
            negative_after: Vec::new(),
        }
    }
}

/// `GET /api/v1/adjustments?branch_id`: newest first.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/adjustments", tag = "stock",
    security(("bearer" = [])), params(ListQuery),
    responses((status = 200, body = [AdjustmentDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_adjustments(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<AdjustmentDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let list = stock::list_adjustments(&state.db, branch).await?;
    Ok(Json(
        list.iter().map(|a| AdjustmentDto::of(a, view)).collect(),
    ))
}

/// `GET /api/v1/adjustments/{id}`.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/adjustments/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Adjustment ID")),
    responses((status = 200, body = AdjustmentDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_adjustment(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<AdjustmentDto>, ApiError> {
    let id: AdjustmentId = parse_id(&id, "id")?;
    let a = stock::get_adjustment(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("adjustment"))?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, a.branch)?;
    let view = CostView::at(&caller, a.branch);
    let mut dto = AdjustmentDto::of(&a, view);
    dto.movements = doc_movements(&state.db, *id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `PUT /api/v1/adjustments/{id}`: posts at once (`stock.adjust`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `422 invalid_field` (`qty`, `reason`, `note`,
/// `unit_cost`).
#[utoipa::path(put, path = "/api/v1/adjustments/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = AdjustmentBody,
    responses((status = 201, body = AdjustmentDto), (status = 200, body = AdjustmentDto),
        (status = 403, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_adjustment(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<AdjustmentBody>,
) -> Result<(StatusCode, Json<AdjustmentDto>), ApiError> {
    let id: AdjustmentId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::StockAdjust)?;
    let mut lines = Vec::with_capacity(body.lines.len());
    for l in body.lines {
        lines.push(NewAdjustmentLine {
            variant: parse_id(&l.variant_id, "variant_id")?,
            unit: parse_id(&l.unit_id, "unit_id")?,
            qty: qty(l.qty_thousandths),
            reason: parse_id::<StockReasonId>(&l.reason_id, "reason_id")?,
            note: l.note,
            unit_cost: l.unit_cost.map(Money::from_minor),
        });
    }
    let a = NewAdjustment {
        id,
        branch,
        location: parse_opt_id(body.location_id.as_deref(), "location_id")?,
        note: body.note,
        user: session.user.id,
        lines,
    };
    let view = CostView::at(&caller, branch);
    let now = state.now();
    let (state_ref, a_ref) = (&state, &a);
    let (written, dto) = run("adjustment post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, stored) = stock::post_adjustment(&mut tx, a_ref, now).await?;
        let mut dto = AdjustmentDto::of(&stored, view);
        dto.negative_after = negative_after(&mut tx, *id.as_uuid()).await?;
        dto.movements = doc_movements(&mut tx, *id.as_uuid(), view).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, dto))
    })
    .await?;
    if written != Written::Unchanged {
        publish_doc_stock(&state, *id.as_uuid()).await;
    }
    Ok((status_of(written), Json(dto)))
}

/// A line of an opening stock body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OpeningLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit, thousandths (> 0).
    pub qty_thousandths: i64,
    /// Unit cost per line unit (Rp, ≥ 0).
    pub unit_cost: i64,
    /// Batch (tracked products).
    #[serde(default)]
    pub batch: Option<String>,
    /// Expiry `YYYY-MM-DD` (required for tracked products).
    #[serde(default)]
    pub expiry: Option<String>,
}

/// Body of `PUT /opening-stock/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OpeningBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// The location (default: the receiving default).
    #[serde(default)]
    pub location_id: Option<String>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<OpeningLineBody>,
}

/// An opening stock line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OpeningLineDto {
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
    /// Unit cost; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub unit_cost: Option<Option<i64>>,
    /// Batch.
    pub batch: Option<String>,
    /// `YYYY-MM-DD`.
    pub expiry: Option<String>,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Value; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
    /// The lot.
    pub lot_id: Option<String>,
}

/// A posted opening stock document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OpeningStockDto {
    /// ID.
    pub id: String,
    /// `JKT01-OS-261126-01`.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// Note.
    pub note: Option<String>,
    /// Who posted it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: i64,
    /// The lines.
    pub lines: Vec<OpeningLineDto>,
    /// The ledger rows (single reads and posting answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
}

impl OpeningStockDto {
    fn of(o: &OpeningStock, view: CostView) -> Self {
        Self {
            id: o.id.to_string(),
            number: o.number.clone(),
            branch_id: o.branch.to_string(),
            location_id: o.location.to_string(),
            note: o.note.clone(),
            user_id: o.user.to_string(),
            posted_at: o.posted_at.unix_millis(),
            lines: o
                .lines
                .iter()
                .map(|l| OpeningLineDto {
                    line_no: l.line_no,
                    variant_id: l.variant.to_string(),
                    unit_id: l.unit.to_string(),
                    factor_thousandths: l.factor.thousandths(),
                    qty_thousandths: l.qty.thousandths(),
                    unit_cost: view.amount(l.unit_cost),
                    batch: l.batch.clone(),
                    expiry: opt_day_text(l.expiry),
                    base_qty_thousandths: l.base_qty.thousandths(),
                    value: view.opt(l.value),
                    lot_id: l.lot.map(|x| x.to_string()),
                })
                .collect(),
            movements: Vec::new(),
        }
    }
}

/// `GET /api/v1/opening-stock?branch_id`: newest first.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/opening-stock", tag = "stock",
    security(("bearer" = [])), params(ListQuery),
    responses((status = 200, body = [OpeningStockDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_opening(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<OpeningStockDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let list = stock::list_opening_stock(&state.db, branch).await?;
    Ok(Json(
        list.iter().map(|o| OpeningStockDto::of(o, view)).collect(),
    ))
}

/// `GET /api/v1/opening-stock/{id}`.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/opening-stock/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Opening stock ID")),
    responses((status = 200, body = OpeningStockDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_opening(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<OpeningStockDto>, ApiError> {
    let id: OpeningStockId = parse_id(&id, "id")?;
    let o = stock::get_opening_stock(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("opening stock"))?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, o.branch)?;
    let view = CostView::at(&caller, o.branch);
    let mut dto = OpeningStockDto::of(&o, view);
    dto.movements = doc_movements(&state.db, *id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `PUT /api/v1/opening-stock/{id}`: posts at once (`stock.adjust`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/opening-stock/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = OpeningBody,
    responses((status = 201, body = OpeningStockDto), (status = 200, body = OpeningStockDto),
        (status = 403, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_opening(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<OpeningBody>,
) -> Result<(StatusCode, Json<OpeningStockDto>), ApiError> {
    let id: OpeningStockId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::StockAdjust)?;
    let mut lines = Vec::with_capacity(body.lines.len());
    for l in body.lines {
        lines.push(NewOpeningLine {
            variant: parse_id(&l.variant_id, "variant_id")?,
            unit: parse_id(&l.unit_id, "unit_id")?,
            qty: qty(l.qty_thousandths),
            unit_cost: Money::from_minor(l.unit_cost),
            batch: l.batch,
            expiry: parse_opt_day("expiry", l.expiry.as_deref())?,
        });
    }
    let o = NewOpeningStock {
        id,
        branch,
        location: parse_opt_id(body.location_id.as_deref(), "location_id")?,
        note: body.note,
        user: session.user.id,
        lines,
    };
    let view = CostView::at(&caller, branch);
    let now = state.now();
    let (state_ref, o_ref) = (&state, &o);
    let (written, dto) = run("opening stock post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, stored) = stock::post_opening_stock(&mut tx, o_ref, now).await?;
        let mut dto = OpeningStockDto::of(&stored, view);
        dto.movements = doc_movements(&mut tx, *id.as_uuid(), view).await?;
        tx.commit().await?;
        Ok::<_, Fail>((written, dto))
    })
    .await?;
    if written != Written::Unchanged {
        publish_doc_stock(&state, *id.as_uuid()).await;
    }
    Ok((status_of(written), Json(dto)))
}

// ---------------------------------------------------------------------------------------------
// Opnames

/// Body of `PUT /opnames/{id}` (start).
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct OpnameBody {
    /// The branch (default: the session's).
    #[serde(default)]
    pub branch_id: Option<String>,
    /// The location (default: the receiving default).
    #[serde(default)]
    pub location_id: Option<String>,
    /// `full` or `cycle`.
    pub scope: String,
    /// A cycle count's variants.
    #[serde(default)]
    pub variant_ids: Vec<String>,
    /// A cycle count's categories (their own products).
    #[serde(default)]
    pub category_ids: Vec<String>,
    /// A note.
    #[serde(default)]
    pub note: Option<String>,
}

/// One count entry.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CountEntryBody {
    /// The client's `UUIDv7`: an entry is stored once.
    pub id: String,
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// A scan adds (may be negative to take one back); a typed quantity replaces.
    pub qty_thousandths: i64,
    /// A typed quantity (replaces the line's count).
    #[serde(default)]
    pub replace: bool,
}

/// Body of `POST /opnames/{id}/counts`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct CountsBody {
    /// The entries, in order.
    pub entries: Vec<CountEntryBody>,
}

/// Body of `POST /opnames/{id}/post`.
#[derive(Debug, Clone, Default, Deserialize, ToSchema)]
pub struct PostOpnameBody {
    /// In a full count, what happens to uncounted variants with stock: `set_zero` or `leave`
    /// (default).
    #[serde(default)]
    pub uncounted: Option<String>,
}

/// An opname line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OpnameLineDto {
    /// The variant.
    pub variant_id: String,
    /// On hand at the start.
    pub snapshot_thousandths: i64,
    /// Net movements since the start (other than this opname's); 0 once posted.
    pub since_thousandths: i64,
    /// The counted quantity; `null` = not counted.
    pub counted_thousandths: Option<i64>,
    /// counted − (snapshot + since), for counted lines.
    pub variance_thousandths: Option<i64>,
    /// The posted `count` movement's value; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
}

/// An opname.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OpnameDto {
    /// ID.
    pub id: String,
    /// `JKT01-OP-261126-01`.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// A full count (else cycle).
    pub full: bool,
    /// `counting`, `posted` or `cancelled`.
    pub status: String,
    /// `set_zero` or `leave` once posted.
    pub uncounted: Option<String>,
    /// Note.
    pub note: Option<String>,
    /// Who started it.
    pub user_id: String,
    /// Unix ms (the snapshot).
    pub started_at: i64,
    /// Unix ms.
    pub posted_at: Option<i64>,
    /// The lines (empty in lists).
    pub lines: Vec<OpnameLineDto>,
    /// The ledger rows (posted, single reads and posting answers).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movements: Vec<MovementDto>,
    /// Locations and variants left below zero (posting answer, §4).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub negative_after: Vec<NegativeDto>,
}

impl OpnameDto {
    fn of(o: &Opname, view: CostView) -> Self {
        let q =
            |v: Option<sultan_core::catalog::Qty>| v.map(sultan_core::catalog::Qty::thousandths);
        Self {
            id: o.id.to_string(),
            number: o.number.clone(),
            branch_id: o.branch.to_string(),
            location_id: o.location.to_string(),
            full: o.full,
            status: o.status.clone(),
            uncounted: o.uncounted.map(|u| u.as_str().to_owned()),
            note: o.note.clone(),
            user_id: o.user.to_string(),
            started_at: o.started_at.unix_millis(),
            posted_at: ms(o.posted_at),
            lines: o
                .lines
                .iter()
                .map(|l| OpnameLineDto {
                    variant_id: l.variant.to_string(),
                    snapshot_thousandths: l.snapshot.thousandths(),
                    since_thousandths: l.since.thousandths(),
                    counted_thousandths: q(l.counted),
                    variance_thousandths: q(l.variance),
                    value: view.opt(l.value),
                })
                .collect(),
            movements: Vec::new(),
            negative_after: Vec::new(),
        }
    }
}

/// `GET /api/v1/opnames?branch_id`: newest first, without lines.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/opnames", tag = "stock",
    security(("bearer" = [])), params(ListQuery),
    responses((status = 200, body = [OpnameDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_opnames(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<OpnameDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let list = stock::list_opnames(&state.db, branch).await?;
    Ok(Json(list.iter().map(|o| OpnameDto::of(o, view)).collect()))
}

async fn load_opname(state: &AppState, id: OpnameId) -> Result<Opname, ApiError> {
    stock::get_opname(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("opname"))
}

/// `GET /api/v1/opnames/{id}`: with the live variance of each line.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/opnames/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Opname ID")),
    responses((status = 200, body = OpnameDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_opname(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<OpnameDto>, ApiError> {
    let o = load_opname(&state, parse_id(&id, "id")?).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, o.branch)?;
    let view = CostView::at(&caller, o.branch);
    let mut dto = OpnameDto::of(&o, view);
    dto.movements = doc_movements(&state.db, *o.id.as_uuid(), view).await?;
    Ok(Json(dto))
}

/// `PUT /api/v1/opnames/{id}`: starts an opname (`stock.count`); the same ID again answers it.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 opname_open`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/opnames/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = OpnameBody,
    responses((status = 201, body = OpnameDto), (status = 200, body = OpnameDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_opname(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<OpnameBody>,
) -> Result<(StatusCode, Json<OpnameDto>), ApiError> {
    let id: OpnameId = parse_put_id(&id)?;
    let branch = branch_or_session(&session, body.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, branch, Permission::StockCount)?;
    let scope = match body.scope.as_str() {
        "full" => OpnameScope::Full,
        "cycle" => OpnameScope::Cycle {
            variants: body
                .variant_ids
                .iter()
                .map(|v| parse_id::<VariantId>(v, "variant_ids"))
                .collect::<Result<_, _>>()?,
            categories: body
                .category_ids
                .iter()
                .map(|c| parse_id::<CategoryId>(c, "category_ids"))
                .collect::<Result<_, _>>()?,
        },
        _ => {
            return Err(ApiError::invalid_field(
                "scope",
                "`scope` is `full` or `cycle`",
            ));
        }
    };
    let n = NewOpname {
        id,
        branch,
        location: parse_opt_id(body.location_id.as_deref(), "location_id")?,
        scope,
        note: body.note,
        user: session.user.id,
    };
    let now = state.now();
    let (state_ref, n_ref) = (&state, &n);
    let (written, stored) = run("opname start", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::start_opname(&mut tx, n_ref, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((
        status_of(written),
        Json(OpnameDto::of(&stored, CostView::at(&caller, branch))),
    ))
}

/// `POST /api/v1/opnames/{id}/counts`: appends count entries (`stock.count`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (posted or cancelled) /
/// `opname_open`, `422 invalid_field` (`variant` outside a cycle count, `qty` below 0).
#[utoipa::path(post, path = "/api/v1/opnames/{id}/counts", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Opname ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = CountsBody,
    responses((status = 200, body = OpnameDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn counts(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CountsBody>,
) -> Result<Json<OpnameDto>, ApiError> {
    let id: OpnameId = parse_id(&id, "id")?;
    let o = load_opname(&state, id).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, o.branch, Permission::StockCount)?;
    let mut entries = Vec::with_capacity(body.entries.len());
    for e in &body.entries {
        entries.push(CountInput {
            id: parse_id(&e.id, "id")?,
            variant: parse_id(&e.variant_id, "variant_id")?,
            unit: parse_id(&e.unit_id, "unit_id")?,
            qty: qty(e.qty_thousandths),
            replace: e.replace,
        });
    }
    let (now, user, state_ref, entries_ref) = (state.now(), session.user.id, &state, &entries);
    let stored = run("opname counts", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::add_counts(&mut tx, id, entries_ref, user, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok(Json(OpnameDto::of(
        &stored,
        CostView::at(&caller, o.branch),
    )))
}

/// `POST /api/v1/opnames/{id}/post` (`stock.count` and `stock.adjust`).
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (cancelled), `422 invalid_field`
/// (`uncounted`).
#[utoipa::path(post, path = "/api/v1/opnames/{id}/post", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Opname ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = PostOpnameBody,
    responses((status = 200, body = OpnameDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn post_opname(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    body: axum::body::Bytes,
) -> Result<Json<OpnameDto>, ApiError> {
    let body: PostOpnameBody = super::stock::optional_body(&body)?;
    let id: OpnameId = parse_id(&id, "id")?;
    let o = load_opname(&state, id).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, o.branch, Permission::StockCount)?;
    require_at(&caller, o.branch, Permission::StockAdjust)?;
    let choice = match body.uncounted.as_deref() {
        None => Uncounted::Leave,
        Some(key) => Uncounted::from_key(key).ok_or_else(|| {
            ApiError::invalid_field("uncounted", "`uncounted` is `set_zero` or `leave`")
        })?,
    };
    let view = CostView::at(&caller, o.branch);
    let (now, user, state_ref) = (state.now(), session.user.id, &state);
    let (written, dto) = run("opname post", || async move {
        let mut tx = state_ref.db.begin().await?;
        let (written, stored) = stock::post_opname(&mut tx, id, choice, user, None, now).await?;
        let mut dto = OpnameDto::of(&stored, view);
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

/// `POST /api/v1/opnames/{id}/cancel` (`stock.count`): frees its locks.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`, `409 conflict` (posted).
#[utoipa::path(post, path = "/api/v1/opnames/{id}/cancel", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Opname ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = OpnameDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn cancel_opname(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<OpnameDto>, ApiError> {
    let id: OpnameId = parse_id(&id, "id")?;
    let o = load_opname(&state, id).await?;
    let caller = Caller::load(&state.db, &session).await?;
    require_at(&caller, o.branch, Permission::StockCount)?;
    let (now, user, state_ref) = (state.now(), session.user.id, &state);
    let (_, stored) = run("opname cancel", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::cancel_opname(&mut tx, id, user, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok(Json(OpnameDto::of(
        &stored,
        CostView::at(&caller, o.branch),
    )))
}
