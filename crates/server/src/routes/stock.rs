//! Inventory reads and what the inventory routes of spec 2.8 (phase N3) share.
//!
//! **Permissions** (§12): writes check their key at the document's branch
//! ([`require_at`]); reading on hand, stock cards, lots and documents needs any of
//! `supplier.manage`, `purchase.order`, `purchase.receive`, `stock.transfer`, `stock.count`,
//! `stock.adjust`, `report.profit` or `report.sales` at the branch ([`require_read`]). Without
//! `report.profit`
//! at the branch, cost and value fields are **left out** of the answers ([`CostView`]): unit cost,
//! movement and line values, landed cost, transfer values, purchase-order prices, suggested
//! prices and the supplier's last price. A field that is present and `null` means "no cost yet"
//! (§5); an absent field means "not shown to you".
//!
//! **Negative stock** (§4, I7) never blocks. Every posting that moves stock out answers
//! `negative_after`: the (location, variant) pairs its lines left below zero, so the screen can
//! warn; it is computed in the posting transaction ([`negative_after`]).
//!
//! **Events** (§15): every posting publishes `stock.changed {branch_id, variant_ids}` to the
//! callers who may see the branch ([`publish_doc_stock`]; sales, post-voids and returns too), and
//! transfers publish `transfer.changed` to both branches.
//!
//! Reads here:
//! - `GET /stock/on-hand?branch_id&location_id&variant_id&category_id&brand_id&q&filter&cursor&limit`:
//!   one row per (location, variant) with the branch's on order, in transit and reorder rule;
//!   `filter` is `negative`, `low` or `expiring`; `cursor` is the `next_cursor` of the previous
//!   page. Without `branch_id`: the session's branch.
//! - `GET /stock/card/{variant_id}?branch_id&location_id&from&until&cursor&limit`: the
//!   movements, oldest first, with the running balance (`from` / `until` in unix ms).
//! - `GET /stock/lots?branch_id&location_id&variant_id&expiring`: lots with stock, by expiry;
//!   `expiring=true` keeps those within the product's warn days (else the company's) of today,
//!   expired ones included, with their value at the average.
//! - `GET|PUT /settings/inventory` (§13): `PUT` is HQ data (`read_only_replica` on a branch node)
//!   and needs `stock.adjust`.
//! - `GET /payables?branch_id&supplier_id&status=open|all` (read-only until 2.9).
//! - `GET /registers/me/stock/{variant_id}` (I18): the register's branch on hand, no cost; the
//!   device token alone is enough, a user session without a register needs `sale.create`.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sultan_core::access::{AccessError, BranchScope, Permission};
use sultan_core::catalog::Qty;
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::ids::{BranchId, BrandId, CategoryId, LocationId, SupplierId, VariantId};
use sultan_core::money::Money;
use sultan_core::stock::{InventorySettings, StockError};
use sultan_db::stock::{self, MovementRecord, NO_LOCATION, OnHandFilter, StockDbError, Written};
use sultan_db::{DbError, Tx, org};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::catalog::actor;
use super::parse_opt_id;
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, CatalogReader, RequireHq, UserSession};
use crate::retry::{self, Failure};
use crate::state::AppState;

// ---------------------------------------------------------------------------------------------
// Shared: the retry loop, permissions, cost visibility, conversions, events.

/// A failure inside an inventory write: an API answer, or a repository error that may be
/// retried.
#[derive(Debug)]
pub enum Fail {
    /// Answer this.
    Api(ApiError),
    /// An inventory repository error; retried when it is a lost race.
    Db(StockDbError),
}

impl From<ApiError> for Fail {
    fn from(e: ApiError) -> Self {
        Self::Api(e)
    }
}

impl From<StockDbError> for Fail {
    fn from(e: StockDbError) -> Self {
        Self::Db(e)
    }
}

impl From<DbError> for Fail {
    fn from(e: DbError) -> Self {
        Self::Db(StockDbError::Db(e))
    }
}

impl From<StockError> for Fail {
    fn from(e: StockError) -> Self {
        Self::Db(StockDbError::Rule(e))
    }
}

/// Runs an inventory write with the 2.1 §5 retry loop (a lost race on a number or a key reruns
/// the whole transaction); every attempt lost → `409 conflict`.
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

/// The keys that let a caller read inventory (§12).
pub const READ_KEYS: [Permission; 8] = [
    Permission::SupplierManage,
    Permission::PurchaseOrder,
    Permission::PurchaseReceive,
    Permission::StockTransfer,
    Permission::StockCount,
    Permission::StockAdjust,
    Permission::ReportProfit,
    Permission::ReportSales,
];

fn denied(permission: Permission) -> ApiError {
    AccessError::PermissionDenied { permission }.into()
}

/// Whether `caller` holds `permission` at `branch`.
#[must_use]
pub fn holds(caller: &Caller, branch: BranchId, permission: Permission) -> bool {
    caller
        .at_scope(BranchScope::Branch(branch))
        .contains(permission)
}

/// `403 permission_denied` unless `caller` holds `permission` at `branch`. Stock keys are never
/// overridden here (only a receipt void takes an override).
///
/// # Errors
/// See above.
pub fn require_at(
    caller: &Caller,
    branch: BranchId,
    permission: Permission,
) -> Result<(), ApiError> {
    if holds(caller, branch, permission) {
        Ok(())
    } else {
        Err(denied(permission))
    }
}

/// `403 permission_denied` unless `caller` may read inventory at `branch` (§12).
///
/// # Errors
/// See above.
pub fn require_read(caller: &Caller, branch: BranchId) -> Result<(), ApiError> {
    if READ_KEYS.iter().any(|p| holds(caller, branch, *p)) {
        Ok(())
    } else {
        Err(denied(READ_KEYS[0]))
    }
}

/// `403 permission_denied` unless `caller` may read inventory at some branch (HQ data:
/// suppliers, reasons, settings).
///
/// # Errors
/// See above.
pub fn require_read_anywhere(caller: &Caller) -> Result<(), ApiError> {
    caller.require_any_anywhere(&READ_KEYS)
}

/// Whether cost and value fields are shown (§12: `report.profit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostView(pub bool);

impl CostView {
    /// At `branch`.
    #[must_use]
    pub fn at(caller: &Caller, branch: BranchId) -> Self {
        Self(holds(caller, branch, Permission::ReportProfit))
    }

    /// At some branch (HQ data such as supplier items).
    #[must_use]
    pub fn anywhere(caller: &Caller) -> Self {
        Self(caller.holds_anywhere(Permission::ReportProfit))
    }

    /// An optional amount: left out (`None`) when hidden, else `Some(null or value)`.
    #[must_use]
    pub fn opt(self, m: Option<Money>) -> Option<Option<i64>> {
        self.0.then(|| m.map(Money::minor))
    }

    /// An amount: left out when hidden.
    #[must_use]
    pub fn amount(self, m: Money) -> Option<Option<i64>> {
        self.0.then_some(Some(m.minor()))
    }
}

/// The branch a request is about: `branch_id`, else the session's branch.
///
/// # Errors
/// `400 invalid_body` for an ID that is not a UUID.
pub fn branch_or_session(
    session: &UserSession,
    branch_id: Option<&str>,
) -> Result<BranchId, ApiError> {
    Ok(parse_opt_id::<BranchId>(branch_id, "branch_id")?.unwrap_or(session.session.branch))
}

/// A quantity in thousandths.
#[must_use]
pub const fn qty(thousandths: i64) -> Qty {
    Qty::from_thousandths(thousandths)
}

/// A day as `YYYY-MM-DD`.
#[must_use]
pub fn day_text(day: LocalDay) -> String {
    super::z_reports::day_text(day)
}

/// An optional day as `YYYY-MM-DD`.
#[must_use]
pub fn opt_day_text(day: Option<LocalDay>) -> Option<String> {
    day.map(day_text)
}

/// Parses a `YYYY-MM-DD` field.
///
/// # Errors
/// `422 invalid_field` naming it.
pub fn parse_day(field: &str, value: &str) -> Result<LocalDay, ApiError> {
    let invalid = || ApiError::invalid_field(field, format!("`{field}` must be a date YYYY-MM-DD"));
    let bytes = value.trim().as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(invalid());
    }
    let digits: String = value.trim().chars().filter(|c| *c != '-').collect();
    digits
        .parse::<i64>()
        .ok()
        .and_then(LocalDay::from_key)
        .ok_or_else(invalid)
}

/// Parses an optional `YYYY-MM-DD` field.
///
/// # Errors
/// `422 invalid_field` naming it.
pub fn parse_opt_day(field: &str, value: Option<&str>) -> Result<Option<LocalDay>, ApiError> {
    value
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_day(field, v))
        .transpose()
}

/// Today at `branch` (its IANA time zone).
///
/// # Errors
/// `422 invalid_field` for an unknown branch, or the database error.
pub async fn branch_today(state: &AppState, branch: BranchId) -> Result<LocalDay, ApiError> {
    let b = org::get_branch(&state.db, branch)
        .await?
        .ok_or_else(|| ApiError::invalid_field("branch_id", "unknown branch"))?;
    let tz: BranchTimeZone = b.timezone.parse().unwrap_or_default();
    Ok(LocalDay::of(state.now(), tz))
}

/// Publishes `stock.changed {branch_id, variant_ids}` for each branch of `moves`.
pub fn publish_stock_changed(state: &AppState, moves: &[MovementRecord]) {
    let mut by_branch: BTreeMap<BranchId, BTreeSet<VariantId>> = BTreeMap::new();
    for m in moves {
        by_branch.entry(m.branch).or_default().insert(m.variant);
    }
    for (branch, variants) in by_branch {
        state.events.publish(Event::for_branch(
            "stock.changed",
            branch,
            json!({
                "branch_id": branch.to_string(),
                "variant_ids": variants.iter().map(ToString::to_string).collect::<Vec<_>>(),
            }),
        ));
    }
}

/// Publishes `stock.changed` for the movements the document `doc` wrote (after commit).
pub async fn publish_doc_stock(state: &AppState, doc: Uuid) {
    match stock::document_movements(&state.db, doc).await {
        Ok(moves) => publish_stock_changed(state, &moves),
        Err(e) => tracing::warn!(error = %e, "stock.changed not published"),
    }
}

/// A (location, variant) left below zero by a posting (§4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct NegativeDto {
    /// The location.
    pub location_id: Option<String>,
    /// The variant.
    pub variant_id: String,
    /// On hand there now (below zero).
    pub qty_thousandths: i64,
}

/// The (location, variant) pairs that `doc`'s outgoing movements left below zero, read in the
/// posting transaction.
///
/// # Errors
/// The database error.
pub async fn negative_after(tx: &mut Tx, doc: Uuid) -> Result<Vec<NegativeDto>, DbError> {
    let moves = stock::document_movements(&mut *tx, doc).await?;
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for m in moves.iter().filter(|m| m.qty.thousandths() < 0) {
        if !seen.insert((m.branch, m.location, m.variant)) {
            continue;
        }
        let on_hand = stock::location_on_hand(&mut *tx, m.branch, m.location, m.variant).await?;
        if on_hand.thousandths() < 0 {
            out.push(NegativeDto {
                location_id: m.location.map(|l| l.to_string()),
                variant_id: m.variant.to_string(),
                qty_thousandths: on_hand.thousandths(),
            });
        }
    }
    Ok(out)
}

/// A ledger row (§2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct MovementDto {
    /// `UUIDv7`, in posting order.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: Option<String>,
    /// The variant.
    pub variant_id: String,
    /// The kind (`sale`, `receipt`, `transfer_out`, …).
    pub kind: String,
    /// Base units, signed.
    pub qty_thousandths: i64,
    /// Signed Rupiah; `null` = no cost; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
    /// The lot.
    pub lot_id: Option<String>,
    /// The document kind (`sale`, `goods_receipt`, …).
    pub doc_kind: String,
    /// The document.
    pub doc_id: String,
    /// The document line.
    pub doc_line: Option<u32>,
    /// Unix ms.
    pub occurred_at: i64,
}

impl MovementDto {
    /// The row as `view` may see it.
    #[must_use]
    pub fn of(m: &MovementRecord, view: CostView) -> Self {
        Self {
            id: m.id.to_string(),
            branch_id: m.branch.to_string(),
            location_id: m.location.map(|l| l.to_string()),
            variant_id: m.variant.to_string(),
            kind: m.kind.as_str().to_owned(),
            qty_thousandths: m.qty.thousandths(),
            value: view.opt(m.value),
            lot_id: m.lot.map(|l| l.to_string()),
            doc_kind: m.doc_kind.clone(),
            doc_id: m.doc_id.to_string(),
            doc_line: m.doc_line,
            occurred_at: m.occurred_at.unix_millis(),
        }
    }
}

/// The movements of a document as `view` may see them.
///
/// # Errors
/// The database error.
pub async fn doc_movements<'c>(
    conn: impl Into<sultan_db::Conn<'c>>,
    doc: Uuid,
    view: CostView,
) -> Result<Vec<MovementDto>, ApiError> {
    Ok(stock::document_movements(conn, doc)
        .await?
        .iter()
        .map(|m| MovementDto::of(m, view))
        .collect())
}

/// A line of a request body: variant, unit and quantity in the unit.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct QtyLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit, thousandths.
    pub qty_thousandths: i64,
}

/// `(line_no, qty in the line's unit)` of a ship or receive body.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct LineQtyBody {
    /// The document line.
    pub line_no: u32,
    /// Quantity in the line's unit, thousandths.
    pub qty_thousandths: i64,
}

// ---------------------------------------------------------------------------------------------
// GET /stock/on-hand

/// Query of `GET /stock/on-hand`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct OnHandQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
    /// One location.
    pub location_id: Option<String>,
    /// One variant.
    pub variant_id: Option<String>,
    /// Products of this category.
    pub category_id: Option<String>,
    /// Products of this brand.
    pub brand_id: Option<String>,
    /// Text: in the name, or exactly the code, SKU or a barcode.
    pub q: Option<String>,
    /// `negative`, `low` or `expiring`.
    pub filter: Option<String>,
    /// The `next_cursor` of the previous page.
    pub cursor: Option<String>,
    /// Page size, default 50, at most 500.
    pub limit: Option<u32>,
}

/// One row of the on-hand list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OnHandRowDto {
    /// The branch.
    pub branch_id: String,
    /// The location (`null` = none).
    pub location_id: Option<String>,
    /// The variant.
    pub variant_id: String,
    /// Its product.
    pub product_id: Option<String>,
    /// The product code.
    pub product_code: String,
    /// The product name.
    pub product_name: String,
    /// The variant's SKU.
    pub sku: String,
    /// On hand at the location, base units.
    pub qty_thousandths: i64,
    /// On order at the branch.
    pub on_order_thousandths: i64,
    /// In transit to or from the branch.
    pub in_transit_thousandths: i64,
    /// The branch's reorder min.
    pub min_thousandths: Option<i64>,
    /// The branch's reorder max.
    pub max_thousandths: Option<i64>,
    /// The branch's unit cost per base unit; `null` = no cost yet; left out without
    /// `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub unit_cost: Option<Option<i64>>,
    /// On hand × unit cost (Rp); left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
}

/// A page of the on-hand list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct OnHandPage {
    /// The rows, by branch, location and variant.
    pub rows: Vec<OnHandRowDto>,
    /// Pass as `cursor` for the next page; `null` when this page was not full.
    pub next_cursor: Option<String>,
}

fn row_value(unit_cost: Option<Money>, qty: Qty) -> Option<i64> {
    let cost = i128::from(unit_cost?.minor());
    let v = cost * i128::from(qty.thousandths());
    let rounded = if v >= 0 {
        (v + 500) / 1000
    } else {
        (v - 500) / 1000
    };
    i64::try_from(rounded).ok()
}

fn parse_cursor(cursor: &str) -> Result<(BranchId, Vec<u8>, VariantId), ApiError> {
    let bad = || ApiError::invalid_field("cursor", "`cursor` is not a cursor of this list");
    let mut parts = cursor.split('.');
    let (Some(b), Some(l), Some(v), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let branch: BranchId = b.parse().map_err(|_| bad())?;
    let location: Uuid = l.parse().map_err(|_| bad())?;
    let variant: VariantId = v.parse().map_err(|_| bad())?;
    Ok((branch, location.as_bytes().to_vec(), variant))
}

fn cursor_of(branch: BranchId, location: Option<LocationId>, variant: VariantId) -> String {
    let location = location.map_or_else(|| Uuid::from_bytes(NO_LOCATION), |l| *l.as_uuid());
    format!("{branch}.{location}.{variant}")
}

/// `GET /api/v1/stock/on-hand` (see the module docs).
///
/// # Errors
/// `400 invalid_body` (an ID), `403 permission_denied`, `422 invalid_field` (`filter`,
/// `cursor`).
#[utoipa::path(get, path = "/api/v1/stock/on-hand", tag = "stock",
    security(("bearer" = [])), params(OnHandQuery),
    responses((status = 200, body = OnHandPage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn on_hand(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<OnHandQuery>,
) -> Result<Json<OnHandPage>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let mut filter = OnHandFilter {
        branch: Some(branch),
        location: parse_opt_id::<LocationId>(query.location_id.as_deref(), "location_id")?,
        variant: parse_opt_id::<VariantId>(query.variant_id.as_deref(), "variant_id")?,
        category: parse_opt_id::<CategoryId>(query.category_id.as_deref(), "category_id")?,
        brand: parse_opt_id::<BrandId>(query.brand_id.as_deref(), "brand_id")?,
        q: query.q.clone(),
        after: query.cursor.as_deref().map(parse_cursor).transpose()?,
        limit: query.limit.unwrap_or(50).clamp(1, 500),
        ..OnHandFilter::default()
    };
    match query
        .filter
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        None => {}
        Some("negative") => filter.negative = true,
        Some("low") => filter.low = true,
        Some("expiring") => filter.expiring = Some(branch_today(&state, branch).await?),
        Some(_) => {
            return Err(ApiError::invalid_field(
                "filter",
                "`filter` is `negative`, `low` or `expiring`",
            ));
        }
    }
    let rows = stock::list_on_hand(&state.db, &filter).await?;
    let full = u32::try_from(rows.len()).unwrap_or(u32::MAX) >= filter.limit;
    let next_cursor = rows
        .last()
        .filter(|_| full)
        .map(|r| cursor_of(r.branch, r.location, r.variant));
    Ok(Json(OnHandPage {
        rows: rows
            .iter()
            .map(|r| OnHandRowDto {
                branch_id: r.branch.to_string(),
                location_id: r.location.map(|l| l.to_string()),
                variant_id: r.variant.to_string(),
                product_id: r.product.map(|p| p.to_string()),
                product_code: r.product_code.clone(),
                product_name: r.product_name.clone(),
                sku: r.sku.clone(),
                qty_thousandths: r.qty.thousandths(),
                on_order_thousandths: r.on_order.thousandths(),
                in_transit_thousandths: r.in_transit.thousandths(),
                min_thousandths: r.rule.map(|(min, _)| min.thousandths()),
                max_thousandths: r.rule.map(|(_, max)| max.thousandths()),
                unit_cost: view.opt(r.unit_cost),
                value: view.0.then(|| row_value(r.unit_cost, r.qty)),
            })
            .collect(),
        next_cursor,
    }))
}

// ---------------------------------------------------------------------------------------------
// GET /stock/card/{variant_id}

/// Query of `GET /stock/card/{variant_id}`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct CardQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
    /// One location (else the whole branch).
    pub location_id: Option<String>,
    /// From (unix ms, inclusive).
    pub from: Option<i64>,
    /// Until (unix ms, exclusive).
    pub until: Option<i64>,
    /// The `next_cursor` of the previous page (a movement ID).
    pub cursor: Option<String>,
    /// Page size, default 100, at most 1000.
    pub limit: Option<u32>,
}

/// One row of a stock card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CardRowDto {
    /// The movement (the document link is `doc_kind` + `doc_id`).
    pub movement: MovementDto,
    /// On hand after it, base units.
    pub balance_thousandths: i64,
}

/// A page of a stock card.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CardPage {
    /// The rows, oldest first.
    pub rows: Vec<CardRowDto>,
    /// Pass as `cursor` for the next page; `null` when this page was not full.
    pub next_cursor: Option<String>,
}

/// `GET /api/v1/stock/card/{variant_id}` (see the module docs).
///
/// # Errors
/// `400 invalid_body`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/stock/card/{variant_id}", tag = "stock",
    security(("bearer" = [])), params(("variant_id" = String, Path, description = "Variant ID"), CardQuery),
    responses((status = 200, body = CardPage), (status = 403, body = crate::openapi::Problem)))]
pub async fn card(
    State(state): State<AppState>,
    session: UserSession,
    Path(variant): Path<String>,
    ApiQuery(query): ApiQuery<CardQuery>,
) -> Result<Json<CardPage>, ApiError> {
    let variant: VariantId = super::parse_id(&variant, "variant_id")?;
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let location = parse_opt_id::<LocationId>(query.location_id.as_deref(), "location_id")?;
    let after = parse_opt_id::<Uuid>(query.cursor.as_deref(), "cursor")?;
    let limit = query.limit.unwrap_or(100).clamp(1, 1_000);
    let ts = sultan_core::time::Timestamp::from_unix_millis;
    let mut tx = state.db.begin().await?;
    let rows = stock::stock_card(
        &mut tx,
        branch,
        location,
        variant,
        query.from.map(ts),
        query.until.map(ts),
        after,
        limit,
    )
    .await?;
    tx.commit().await?;
    let full = u32::try_from(rows.len()).unwrap_or(u32::MAX) >= limit;
    Ok(Json(CardPage {
        next_cursor: rows
            .last()
            .filter(|_| full)
            .map(|r| r.movement.id.to_string()),
        rows: rows
            .iter()
            .map(|r| CardRowDto {
                movement: MovementDto::of(&r.movement, view),
                balance_thousandths: r.balance.thousandths(),
            })
            .collect(),
    }))
}

// ---------------------------------------------------------------------------------------------
// GET /stock/lots

/// Query of `GET /stock/lots`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct LotsQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
    /// One location.
    pub location_id: Option<String>,
    /// One variant.
    pub variant_id: Option<String>,
    /// Only lots within the warn days, and expired ones (the Expiring list).
    #[serde(default)]
    pub expiring: bool,
}

/// A lot with stock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LotDto {
    /// The lot.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// The variant.
    pub variant_id: String,
    /// The batch number (empty when none was typed).
    pub batch: String,
    /// `YYYY-MM-DD`.
    pub expiry: String,
    /// Expired (before today at the branch).
    pub expired: bool,
    /// Base units.
    pub qty_thousandths: i64,
    /// Value at the branch's average; `null` = no cost; left out without `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub value: Option<Option<i64>>,
}

/// `GET /api/v1/stock/lots` (see the module docs).
///
/// # Errors
/// `400 invalid_body`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/stock/lots", tag = "stock",
    security(("bearer" = [])), params(LotsQuery),
    responses((status = 200, body = [LotDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn lots(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<LotsQuery>,
) -> Result<Json<Vec<LotDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let view = CostView::at(&caller, branch);
    let location = parse_opt_id::<LocationId>(query.location_id.as_deref(), "location_id")?;
    let variant = parse_opt_id::<VariantId>(query.variant_id.as_deref(), "variant_id")?;
    let today = branch_today(&state, branch).await?;
    let lots = if query.expiring {
        stock::list_expiring_lots(&state.db, Some(branch), location, variant, today).await?
    } else {
        stock::list_lots(&state.db, Some(branch), location, variant, None).await?
    };
    let mut costs = BTreeMap::new();
    let mut out = Vec::with_capacity(lots.len());
    for l in lots {
        let value = if view.0 {
            let reference = if let Some(r) = costs.get(&l.variant) {
                *r
            } else {
                let r = stock::cost_state(&state.db, branch, l.variant)
                    .await?
                    .reference;
                costs.insert(l.variant, r);
                r
            };
            Some(
                reference
                    .and_then(|r| r.value_of(l.qty).ok())
                    .map(Money::minor),
            )
        } else {
            None
        };
        out.push(LotDto {
            id: l.id.to_string(),
            branch_id: l.branch.to_string(),
            location_id: l.location.to_string(),
            variant_id: l.variant.to_string(),
            batch: l.batch.clone(),
            expiry: day_text(l.expiry),
            expired: l.expiry < today,
            qty_thousandths: l.qty.thousandths(),
            value,
        });
    }
    Ok(Json(out))
}

// ---------------------------------------------------------------------------------------------
// GET|PUT /settings/inventory

/// The inventory settings (§13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct InventorySettingsDto {
    /// The company is PKP: input PPN stays out of the cost (I5).
    pub pkp: bool,
    /// The receipts' default PPN rate in basis points (1100 = 11%).
    pub standard_ppn_rate_bp: u16,
    /// Lots expiring within these days are listed (default 30).
    pub expiry_warn_days: u32,
    /// The velocity period N (default 30).
    pub velocity_days: u32,
    /// Cover days of a suggestion (default 14).
    pub cover_days: u32,
}

impl From<InventorySettings> for InventorySettingsDto {
    fn from(s: InventorySettings) -> Self {
        Self {
            pkp: s.pkp,
            standard_ppn_rate_bp: s.standard_ppn_rate_bp,
            expiry_warn_days: s.expiry_warn_days,
            velocity_days: s.velocity_days,
            cover_days: s.cover_days,
        }
    }
}

/// `GET /api/v1/settings/inventory`.
///
/// # Errors
/// `401`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/settings/inventory", tag = "stock",
    security(("bearer" = [])),
    responses((status = 200, body = InventorySettingsDto), (status = 403, body = crate::openapi::Problem)))]
pub async fn get_settings(
    State(state): State<AppState>,
    session: UserSession,
) -> Result<Json<InventorySettingsDto>, ApiError> {
    let caller = Caller::load(&state.db, &session).await?;
    require_read_anywhere(&caller)?;
    Ok(Json(stock::get_inventory_settings(&state.db).await?.into()))
}

/// `PUT /api/v1/settings/inventory`: HQ data, `stock.adjust`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/settings/inventory", tag = "stock",
    security(("bearer" = [])), request_body = InventorySettingsDto,
    responses((status = 200, body = InventorySettingsDto), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_settings(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    ApiJson(body): ApiJson<InventorySettingsDto>,
) -> Result<Json<InventorySettingsDto>, ApiError> {
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::StockAdjust)?;
    let settings = InventorySettings {
        pkp: body.pkp,
        standard_ppn_rate_bp: body.standard_ppn_rate_bp,
        expiry_warn_days: body.expiry_warn_days,
        velocity_days: body.velocity_days,
        cover_days: body.cover_days,
    };
    let now = state.now();
    let (state_ref, actor, settings_ref) = (&state, &actor(&session), &settings);
    let stored = run("inventory settings put", || async move {
        let mut tx = state_ref.db.begin().await?;
        stock::put_inventory_settings(&mut tx, settings_ref, actor, now).await?;
        let stored = stock::get_inventory_settings(&mut tx).await?;
        tx.commit().await?;
        Ok::<_, Fail>(stored)
    })
    .await?;
    Ok(Json(stored.into()))
}

// ---------------------------------------------------------------------------------------------
// GET /payables

/// Query of `GET /payables`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct PayablesQuery {
    /// The branch (default: the session's).
    pub branch_id: Option<String>,
    /// One supplier.
    pub supplier_id: Option<String>,
    /// `open` (default: not paid, not voided) or `all`.
    pub status: Option<String>,
}

/// A payable (§6, I3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PayableDto {
    /// ID.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The receipt.
    pub receipt_id: String,
    /// The receipt's number.
    pub receipt_number: Option<String>,
    /// The invoice total (Rp).
    pub amount: i64,
    /// `YYYY-MM-DD`.
    pub due_date: String,
    /// Paid (a cash receipt is paid when posted).
    pub paid: bool,
    /// Voided with its receipt.
    pub voided: bool,
}

/// `GET /api/v1/payables`: by due date (read-only until 2.9).
///
/// # Errors
/// `403 permission_denied`, `422 invalid_field` (`status`).
#[utoipa::path(get, path = "/api/v1/payables", tag = "stock",
    security(("bearer" = [])), params(PayablesQuery),
    responses((status = 200, body = [PayableDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn payables(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<PayablesQuery>,
) -> Result<Json<Vec<PayableDto>>, ApiError> {
    let branch = branch_or_session(&session, query.branch_id.as_deref())?;
    let caller = Caller::load(&state.db, &session).await?;
    require_read(&caller, branch)?;
    let supplier = parse_opt_id::<SupplierId>(query.supplier_id.as_deref(), "supplier_id")?;
    let open = match query.status.as_deref().unwrap_or("open") {
        "open" => true,
        "all" => false,
        _ => {
            return Err(ApiError::invalid_field(
                "status",
                "`status` is `open` or `all`",
            ));
        }
    };
    let list = stock::list_payables(&state.db, Some(branch), supplier, open).await?;
    Ok(Json(
        list.into_iter()
            .map(|p| PayableDto {
                id: p.id.to_string(),
                branch_id: p.branch.to_string(),
                supplier_id: p.supplier.to_string(),
                receipt_id: p.receipt.to_string(),
                receipt_number: p.receipt_number,
                amount: p.amount.minor(),
                due_date: day_text(p.due_date),
                paid: p.paid,
                voided: p.voided,
            })
            .collect(),
    ))
}

// ---------------------------------------------------------------------------------------------
// GET /registers/me/stock/{variant_id}

/// The till's on hand of a variant (I18): no cost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TillStockDto {
    /// The variant.
    pub variant_id: String,
    /// The branch.
    pub branch_id: String,
    /// On hand at the branch over all its locations, base units.
    pub qty_thousandths: i64,
}

/// `GET /api/v1/registers/me/stock/{variant_id}` (see the module docs).
///
/// # Errors
/// `401`, `403 permission_denied` (a session without a register and without `sale.create`).
#[utoipa::path(get, path = "/api/v1/registers/me/stock/{variant_id}", tag = "stock",
    security(("register_token" = []), ("bearer" = [], "register_token" = []), ("bearer" = [])),
    params(("variant_id" = String, Path, description = "Variant ID")),
    responses((status = 200, body = TillStockDto), (status = 401, body = crate::openapi::Problem),
        (status = 403, body = crate::openapi::Problem)))]
pub async fn till_stock(
    State(state): State<AppState>,
    reader: CatalogReader,
    Path(variant): Path<String>,
) -> Result<Json<TillStockDto>, ApiError> {
    let variant: VariantId = super::parse_id(&variant, "variant_id")?;
    let branch = match (reader.register_branch(), reader.user()) {
        (Some(branch), _) => branch,
        (None, Some(user)) => {
            Caller::load(&state.db, user)
                .await?
                .require(Permission::SaleCreate)?;
            user.session.branch
        }
        (None, None) => return Err(ApiError::invalid_device_token()),
    };
    let on_hand = stock::branch_on_hand(&state.db, branch, variant).await?;
    Ok(Json(TillStockDto {
        variant_id: variant.to_string(),
        branch_id: branch.to_string(),
        qty_thousandths: on_hand.thousandths(),
    }))
}

/// A JSON body that may be left out (an empty body is the default).
///
/// # Errors
/// `400 invalid_body` for a body that is not the JSON of `T`.
pub fn optional_body<T: serde::de::DeserializeOwned + Default>(
    bytes: &[u8],
) -> Result<T, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(bytes).map_err(|e| ApiError::invalid_body(e.to_string()))
}

/// `201` for a created document, `200` otherwise.
#[must_use]
pub const fn status_of(written: Written) -> StatusCode {
    super::catalog::written_status(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_garbage() {
        let b = BranchId::generate();
        let v = VariantId::generate();
        let l = LocationId::generate();
        let text = cursor_of(b, Some(l), v);
        let (b2, l2, v2) = parse_cursor(&text).unwrap();
        assert_eq!((b2, l2, v2), (b, l.as_uuid().as_bytes().to_vec(), v));
        let none = cursor_of(b, None, v);
        assert_eq!(parse_cursor(&none).unwrap().1, NO_LOCATION.to_vec());
        for bad in ["", "a.b.c", "x", &format!("{b}.{v}")] {
            assert_eq!(
                parse_cursor(bad).unwrap_err().code(),
                "invalid_field",
                "{bad}"
            );
        }
    }

    #[test]
    fn values_round_half_up_and_costs_hide() {
        let m = Money::from_minor;
        assert_eq!(row_value(Some(m(1_150)), qty(1_500)), Some(1_725));
        assert_eq!(row_value(Some(m(333)), qty(-1_500)), Some(-500));
        assert_eq!(row_value(None, qty(1_000)), None);
        assert_eq!(CostView(false).opt(Some(m(5))), None);
        assert_eq!(CostView(true).opt(None), Some(None));
        assert_eq!(CostView(true).amount(m(7)), Some(Some(7)));
        assert_eq!(CostView(false).amount(m(7)), None);
    }

    #[test]
    fn optional_bodies() {
        let got: Vec<i64> = optional_body(b" \n").unwrap();
        assert!(got.is_empty());
        let got: Vec<i64> = optional_body(b"[1]").unwrap();
        assert_eq!(got, [1]);
        let err = optional_body::<Vec<i64>>(b"{").unwrap_err();
        assert_eq!(err.code(), "invalid_body");
    }

    #[test]
    fn days_parse() {
        assert_eq!(
            day_text(parse_day("d", "2026-11-27").unwrap()),
            "2026-11-27"
        );
        for bad in ["2026-13-01", "20261127", "2026/11/27", "x"] {
            assert_eq!(parse_day("d", bad).unwrap_err().code(), "invalid_field");
        }
        assert_eq!(parse_opt_day("d", Some(" ")).unwrap(), None);
        assert_eq!(parse_opt_day("d", None).unwrap(), None);
    }
}
