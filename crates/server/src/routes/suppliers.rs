//! Suppliers, supplier items and stock reasons (spec 2.8 §3, §7, I11, I15; phase N3): HQ master
//! data. Writes need [`RequireHq`] (`403 read_only_replica` on a branch node) and the key from
//! any role assignment (`supplier.manage`; `stock.adjust` for reasons); reads need any inventory
//! key ([`super::stock::READ_KEYS`]).
//!
//! - `GET /suppliers[?archived=true]`, `GET /suppliers/{id}`, `PUT /suppliers/{id}` (create
//!   `201` / overwrite `200`; the code is cleaned to upper case, a duplicate is `409 conflict`),
//!   `DELETE /suppliers/{id}` (archive).
//! - `GET /suppliers/{id}/items`, `GET|PUT /suppliers/{id}/items/{variant_id}`
//!   `{supplier_code, preferred}`: the last price per base unit is set by posted receipts and
//!   left out without `report.profit`; making an item preferred takes the flag from the
//!   variant's other supplier.
//! - `GET /stock-reasons[?archived=true]`, `PUT /stock-reasons/{id}`, `DELETE /stock-reasons/{id}`.
//!
//! Audit and outbox rows (`supplier.changed`, `supplier_item.changed`, `stock_reason.changed`)
//! come from `sultan_db::stock` in the same transaction.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use sultan_core::access::Permission;
use sultan_core::ids::{StockReasonId, SupplierId, VariantId};
use sultan_core::stock::ReasonDirection;
use sultan_db::stock::{self, StockReasonRecord, Supplier, SupplierItem};
use utoipa::{IntoParams, ToSchema};

use super::catalog::{actor, written_status};
use super::stock::{CostView, Fail, require_read_anywhere, run};
use super::{parse_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{ApiJson, ApiQuery, RequireHq, UserSession};
use crate::state::AppState;

/// Query of the HQ lists.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ArchivedQuery {
    /// Also archived (and, for reasons, inactive) records.
    #[serde(default)]
    pub archived: bool,
}

/// Body of `PUT /suppliers/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SupplierBody {
    /// 1–20 characters, stored upper case, unique.
    pub code: String,
    /// 1–120 characters.
    pub name: String,
    /// Address.
    #[serde(default)]
    pub address: Option<String>,
    /// Phone.
    #[serde(default)]
    pub phone: Option<String>,
    /// E-mail.
    #[serde(default)]
    pub email: Option<String>,
    /// NPWP.
    #[serde(default)]
    pub tax_id: Option<String>,
    /// PKP (informational).
    #[serde(default)]
    pub pkp: bool,
    /// Payment terms in days (0 = cash), 0–365.
    #[serde(default)]
    pub terms_days: u32,
    /// Lead time in days, 0–365 (default 7).
    #[serde(default = "seven")]
    pub lead_days: u32,
    /// Notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// Offered for new documents.
    #[serde(default = "yes")]
    pub active: bool,
}

const fn seven() -> u32 {
    7
}

const fn yes() -> bool {
    true
}

/// A supplier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SupplierDto {
    /// ID.
    pub id: String,
    /// Code.
    pub code: String,
    /// Name.
    pub name: String,
    /// Address.
    pub address: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// NPWP.
    pub tax_id: Option<String>,
    /// PKP.
    pub pkp: bool,
    /// Payment terms in days.
    pub terms_days: u32,
    /// Lead time in days.
    pub lead_days: u32,
    /// Notes.
    pub notes: Option<String>,
    /// Offered for new documents.
    pub active: bool,
    /// Archived.
    pub archived: bool,
}

impl From<&Supplier> for SupplierDto {
    fn from(s: &Supplier) -> Self {
        Self {
            id: s.id.to_string(),
            code: s.code.clone(),
            name: s.name.clone(),
            address: s.address.clone(),
            phone: s.phone.clone(),
            email: s.email.clone(),
            tax_id: s.tax_id.clone(),
            pkp: s.pkp,
            terms_days: s.terms_days,
            lead_days: s.lead_days,
            notes: s.notes.clone(),
            active: s.active,
            archived: s.archived_at.is_some(),
        }
    }
}

async fn reader(state: &AppState, session: &UserSession) -> Result<Caller, ApiError> {
    let caller = Caller::load(&state.db, session).await?;
    require_read_anywhere(&caller)?;
    Ok(caller)
}

async fn writer(
    state: &AppState,
    session: &UserSession,
    permission: Permission,
) -> Result<(), ApiError> {
    Caller::load(&state.db, session)
        .await?
        .require_anywhere(permission)
}

/// `GET /api/v1/suppliers[?archived=true]`: by code.
///
/// # Errors
/// `401`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/suppliers", tag = "stock",
    security(("bearer" = [])), params(ArchivedQuery),
    responses((status = 200, body = [SupplierDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ArchivedQuery>,
) -> Result<Json<Vec<SupplierDto>>, ApiError> {
    reader(&state, &session).await?;
    let list = stock::list_suppliers(&state.db, query.archived).await?;
    Ok(Json(list.iter().map(SupplierDto::from).collect()))
}

/// `GET /api/v1/suppliers/{id}`.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/suppliers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Supplier ID")),
    responses((status = 200, body = SupplierDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<SupplierDto>, ApiError> {
    let id: SupplierId = parse_id(&id, "id")?;
    reader(&state, &session).await?;
    let s = stock::get_supplier(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("supplier"))?;
    Ok(Json(SupplierDto::from(&s)))
}

/// `PUT /api/v1/suppliers/{id}` (see the module docs).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `409 conflict` / `archived`,
/// `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/suppliers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = SupplierBody,
    responses((status = 201, body = SupplierDto), (status = 200, body = SupplierDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SupplierBody>,
) -> Result<(StatusCode, Json<SupplierDto>), ApiError> {
    let id: SupplierId = parse_put_id(&id)?;
    writer(&state, &session, Permission::SupplierManage).await?;
    let supplier = Supplier {
        id,
        code: body.code,
        name: body.name,
        address: body.address,
        phone: body.phone,
        email: body.email,
        tax_id: body.tax_id,
        pkp: body.pkp,
        terms_days: body.terms_days,
        lead_days: body.lead_days,
        notes: body.notes,
        active: body.active,
        archived_at: None,
    };
    let now = state.now();
    let (state_ref, actor, supplier) = (&state, &actor(&session), &supplier);
    let (written, stored) = run("supplier put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::put_supplier(&mut tx, supplier, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((written_status(written), Json(SupplierDto::from(&stored))))
}

/// `DELETE /api/v1/suppliers/{id}`: archive.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/suppliers/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Supplier ID")),
    responses((status = 200, body = SupplierDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn delete(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<SupplierDto>, ApiError> {
    let id: SupplierId = parse_id(&id, "id")?;
    writer(&state, &session, Permission::SupplierManage).await?;
    let now = state.now();
    let (state_ref, actor) = (&state, &actor(&session));
    let (_, stored) = run("supplier archive", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::archive_supplier(&mut tx, id, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok(Json(SupplierDto::from(&stored)))
}

/// Body of `PUT /suppliers/{id}/items/{variant_id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SupplierItemBody {
    /// The supplier's item code (at most 40, unique per supplier).
    #[serde(default)]
    pub supplier_code: Option<String>,
    /// The variant's preferred supplier.
    #[serde(default)]
    pub preferred: bool,
}

/// A supplier item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SupplierItemDto {
    /// The supplier.
    pub supplier_id: String,
    /// The variant.
    pub variant_id: String,
    /// The supplier's item code.
    pub supplier_code: Option<String>,
    /// The last price per base unit; `null` = never received; left out without
    /// `report.profit`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<i64>)]
    pub last_price: Option<Option<i64>>,
    /// The unit of the last receipt line (the purchase unit of suggestions).
    pub last_unit_id: Option<String>,
    /// The variant's preferred supplier.
    pub preferred: bool,
}

impl SupplierItemDto {
    fn of(i: &SupplierItem, view: CostView) -> Self {
        Self {
            supplier_id: i.supplier.to_string(),
            variant_id: i.variant.to_string(),
            supplier_code: i.supplier_code.clone(),
            last_price: view.opt(i.last_price),
            last_unit_id: i.last_unit.map(|u| u.to_string()),
            preferred: i.preferred,
        }
    }
}

/// `GET /api/v1/suppliers/{id}/items`: the supplier's items by variant.
///
/// # Errors
/// `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/suppliers/{id}/items", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Supplier ID")),
    responses((status = 200, body = [SupplierItemDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_items(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<Vec<SupplierItemDto>>, ApiError> {
    let id: SupplierId = parse_id(&id, "id")?;
    let caller = reader(&state, &session).await?;
    let view = CostView::anywhere(&caller);
    let items = stock::list_supplier_items(&state.db, Some(id), None).await?;
    Ok(Json(
        items.iter().map(|i| SupplierItemDto::of(i, view)).collect(),
    ))
}

/// `GET /api/v1/suppliers/{id}/items/{variant_id}`.
///
/// # Errors
/// `403 permission_denied`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/suppliers/{id}/items/{variant_id}", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Supplier ID"), ("variant_id" = String, Path, description = "Variant ID")),
    responses((status = 200, body = SupplierItemDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_item(
    State(state): State<AppState>,
    session: UserSession,
    Path((id, variant)): Path<(String, String)>,
) -> Result<Json<SupplierItemDto>, ApiError> {
    let id: SupplierId = parse_id(&id, "id")?;
    let variant: VariantId = parse_id(&variant, "variant_id")?;
    let caller = reader(&state, &session).await?;
    let item = stock::get_supplier_item(&state.db, id, variant)
        .await?
        .ok_or_else(|| ApiError::not_found("supplier item"))?;
    Ok(Json(SupplierItemDto::of(
        &item,
        CostView::anywhere(&caller),
    )))
}

/// `PUT /api/v1/suppliers/{id}/items/{variant_id}` (see the module docs).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found` (supplier),
/// `409 conflict` (a code used by another item of the supplier), `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/suppliers/{id}/items/{variant_id}", tag = "stock",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Supplier ID"), ("variant_id" = String, Path, description = "Variant ID")),
    request_body = SupplierItemBody,
    responses((status = 201, body = SupplierItemDto), (status = 200, body = SupplierItemDto),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_item(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path((id, variant)): Path<(String, String)>,
    ApiJson(body): ApiJson<SupplierItemBody>,
) -> Result<(StatusCode, Json<SupplierItemDto>), ApiError> {
    let id: SupplierId = parse_id(&id, "id")?;
    let variant: VariantId = parse_id(&variant, "variant_id")?;
    let caller = Caller::load(&state.db, &session).await?;
    caller.require_anywhere(Permission::SupplierManage)?;
    let item = SupplierItem {
        supplier: id,
        variant,
        supplier_code: body.supplier_code,
        last_price: None,
        last_unit: None,
        preferred: body.preferred,
    };
    let now = state.now();
    let (state_ref, actor, item) = (&state, &actor(&session), &item);
    let (written, stored) = run("supplier item put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::put_supplier_item(&mut tx, item, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((
        written_status(written),
        Json(SupplierItemDto::of(&stored, CostView::anywhere(&caller))),
    ))
}

/// Body of `PUT /stock-reasons/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct StockReasonBody {
    /// 1–60 characters.
    pub name: String,
    /// `in`, `out` or `both`; fixed once a line used the reason.
    pub direction: String,
    /// A line with it needs a note.
    #[serde(default)]
    pub note_required: bool,
    /// Offered for new lines.
    #[serde(default = "yes")]
    pub active: bool,
    /// Position, ascending.
    #[serde(default)]
    pub sort: i32,
}

/// A stock reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct StockReasonDto {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// `in`, `out` or `both`.
    pub direction: String,
    /// A note is required.
    pub note_required: bool,
    /// Offered for new lines.
    pub active: bool,
    /// Position.
    pub sort: i32,
    /// Archived.
    pub archived: bool,
}

impl From<&StockReasonRecord> for StockReasonDto {
    fn from(r: &StockReasonRecord) -> Self {
        Self {
            id: r.id.to_string(),
            name: r.name.clone(),
            direction: r.direction.as_str().to_owned(),
            note_required: r.note_required,
            active: r.active,
            sort: r.sort,
            archived: r.archived_at.is_some(),
        }
    }
}

/// `GET /api/v1/stock-reasons[?archived=true]`: the active ones by sort, or every one.
///
/// # Errors
/// `401`, `403 permission_denied`.
#[utoipa::path(get, path = "/api/v1/stock-reasons", tag = "stock",
    security(("bearer" = [])), params(ArchivedQuery),
    responses((status = 200, body = [StockReasonDto]), (status = 403, body = crate::openapi::Problem)))]
pub async fn list_reasons(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ArchivedQuery>,
) -> Result<Json<Vec<StockReasonDto>>, ApiError> {
    reader(&state, &session).await?;
    let list = stock::list_stock_reasons(&state.db, query.archived).await?;
    Ok(Json(list.iter().map(StockReasonDto::from).collect()))
}

/// `PUT /api/v1/stock-reasons/{id}`: HQ data, `stock.adjust`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `409 archived`, `422 invalid_field`.
#[utoipa::path(put, path = "/api/v1/stock-reasons/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = StockReasonBody,
    responses((status = 201, body = StockReasonDto), (status = 200, body = StockReasonDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_reason(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<StockReasonBody>,
) -> Result<(StatusCode, Json<StockReasonDto>), ApiError> {
    let id: StockReasonId = parse_put_id(&id)?;
    writer(&state, &session, Permission::StockAdjust).await?;
    let direction = ReasonDirection::from_key(&body.direction).ok_or_else(|| {
        ApiError::invalid_field("direction", "`direction` is `in`, `out` or `both`")
    })?;
    let reason = StockReasonRecord {
        id,
        name: body.name,
        direction,
        note_required: body.note_required,
        active: body.active,
        sort: body.sort,
        archived_at: None,
    };
    let now = state.now();
    let (state_ref, actor, reason) = (&state, &actor(&session), &reason);
    let (written, stored) = run("stock reason put", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::put_stock_reason(&mut tx, reason, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok((written_status(written), Json(StockReasonDto::from(&stored))))
}

/// `DELETE /api/v1/stock-reasons/{id}`: archive.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/stock-reasons/{id}", tag = "stock",
    security(("bearer" = [])), params(("id" = String, Path, description = "Stock reason ID")),
    responses((status = 200, body = StockReasonDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn delete_reason(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<StockReasonDto>, ApiError> {
    let id: StockReasonId = parse_id(&id, "id")?;
    writer(&state, &session, Permission::StockAdjust).await?;
    let now = state.now();
    let (state_ref, actor) = (&state, &actor(&session));
    let (_, stored) = run("stock reason archive", || async move {
        let mut tx = state_ref.db.begin().await?;
        let out = stock::archive_stock_reason(&mut tx, id, actor, now).await?;
        tx.commit().await?;
        Ok::<_, Fail>(out)
    })
    .await?;
    Ok(Json(StockReasonDto::from(&stored)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        assert_eq!((seven(), yes()), (7, true));
    }
}
