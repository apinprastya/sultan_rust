//! Catalog reads for the register and back office, settings and code generators (spec 2.2 §6,
//! §7, §10–§12), plus the helpers the other catalog routes share ([`super::products`],
//! [`super::taxonomy`], [`super::product_images`]).
//!
//! - `GET /catalog/lookup?code=` and `GET /catalog/search?q=&limit=`: a user session **or** a
//!   device token ([`CatalogReader`]). The **assortment** (branch blocks, §10) is that of the
//!   register behind the request, taken from the device token, never from the query. A
//!   back-office session (no register) may pass `branch_id`, which must be a branch the user may
//!   see (`403 branch_not_assigned` otherwise); without it no assortment applies (blocked
//!   products are found / listed). Lookup errors: `404 not_found`, `422 archived` /
//!   `not_sold_here` / `not_sellable` / `scale_value_zero` (with `errors`). Search hides archived,
//!   unsellable and blocked products; it returns up to `limit` hits (1–50, default 20) and has no
//!   cursor (decided in C3: the register shows the first page only).
//! - `GET /tax-classes`: session or device.
//! - `GET /settings/catalog`: any session. `PUT /settings/catalog`: `catalog.manage`.
//! - `POST /catalog/generate/product-code` and `/barcode` → `200 {"value"}`: `catalog.manage`;
//!   they reserve the value (the counter moves past it) and accept `Idempotency-Key`.
//!
//! **Catalog writes** (every route of the catalog): HQ only (`403 read_only_replica`) and
//! `catalog.manage` held through a role assigned for **all branches** (decided in C3: the
//! catalog is company-wide, so a grant at one branch is not enough; `403 permission_denied`).
//! The repository writes the change, its audit row and its outbox row in one transaction; a
//! transaction that loses a race is run again with a short backoff ([`retrying`], at most
//! [`crate::retry::ATTEMPTS`] times; `409 conflict` when every attempt lost). Every write that
//! changed something publishes `catalog.changed` to every event socket.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sultan_core::access::Permission;
use sultan_core::catalog::{
    CatalogError, MatchSource, ProductType, ScaleLayout, ScaleValueKind, UnitKind,
};
use sultan_core::ids::BranchId;
use sultan_db::DbError;
use sultan_db::catalog::{
    self, Actor, CatalogDbError, CatalogResult, CatalogSettings, LookupItem, SearchHit, Written,
};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::parse_opt_id;
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, CatalogReader, RequireHq, UserSession};
use crate::retry::{self, Failure};
use crate::state::AppState;

/// Default page size of `GET /catalog/search`.
pub const DEFAULT_SEARCH_LIMIT: u32 = 20;

/// Runs a whole write transaction again while it fails with a retryable error (a lost race on a
/// unique key or a counter, a deadlock, a busy `SQLite` database), with a short jittered backoff,
/// at most [`retry::ATTEMPTS`] times ([`retry::run`]). Any other error is the answer; when every
/// attempt lost a race the answer is `409 conflict`.
///
/// # Errors
/// The operation's error as problem+json.
pub async fn retrying<T, F, Fut>(op: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = CatalogResult<T>>,
{
    retry::run("catalog write", op, CatalogDbError::is_retryable)
        .await
        .map_err(|failure| match failure {
            Failure::Final(error) => error.into(),
            Failure::Exhausted(error) => gave_up(error),
        })
}

/// The answer after every attempt lost a race: `409 conflict` (with the database's detail for
/// a unique-key race).
fn gave_up(error: CatalogDbError) -> ApiError {
    match error {
        CatalogDbError::Db(error @ DbError::Conflict(_)) => error.into(),
        _ => retry::conflict(),
    }
}

/// `403 permission_denied` unless the caller holds `catalog.manage` for all branches.
///
/// # Errors
/// See above, or the database error.
pub async fn require_manage(state: &AppState, session: &UserSession) -> Result<(), ApiError> {
    Caller::load(&state.db, session)
        .await?
        .require_everywhere(Permission::CatalogManage)
}

/// Whether the reader may see archived catalog entries: `catalog.view_archived` for all branches
/// (a device token alone never may).
///
/// # Errors
/// The database error.
pub async fn may_view_archived(state: &AppState, reader: &CatalogReader) -> Result<bool, ApiError> {
    match reader.user() {
        Some(user) => Ok(Caller::load(&state.db, user)
            .await?
            .holds_everywhere(Permission::CatalogViewArchived)),
        None => Ok(false),
    }
}

/// `403 permission_denied` (`catalog.view_archived`) unless [`may_view_archived`].
///
/// # Errors
/// See above.
pub async fn require_view_archived(
    state: &AppState,
    reader: &CatalogReader,
) -> Result<(), ApiError> {
    if may_view_archived(state, reader).await? {
        Ok(())
    } else {
        Err(sultan_core::access::AccessError::PermissionDenied {
            permission: Permission::CatalogViewArchived,
        }
        .into())
    }
}

/// The audit actor of a session.
#[must_use]
pub const fn actor(session: &UserSession) -> Actor {
    Actor {
        user: Some(session.user.id),
        branch: Some(session.session.branch),
        register: session.session.register,
    }
}

/// `201` for a created record, `200` for an update or an unchanged replay.
#[must_use]
pub const fn written_status(written: Written) -> StatusCode {
    match written {
        Written::Created => StatusCode::CREATED,
        Written::Updated | Written::Unchanged => StatusCode::OK,
    }
}

/// Publishes `catalog.changed {scope, …data}` unless the write changed nothing.
pub fn publish_changed(state: &AppState, written: Written, scope: &str, data: Value) {
    if written == Written::Unchanged {
        return;
    }
    let mut data = data;
    data["scope"] = scope.into();
    state.events.publish(Event::catalog_changed(data));
}

/// The wire name of a unit kind.
#[must_use]
pub const fn unit_kind_str(kind: UnitKind) -> &'static str {
    match kind {
        UnitKind::Integer => "integer",
        UnitKind::Decimal => "decimal",
    }
}

/// Parses a product type (`stock`, `weighed`, `service`, `bundle`); `422 invalid_field`.
///
/// # Errors
/// See above.
pub fn parse_type(field: &str, value: &str) -> Result<ProductType, ApiError> {
    value.parse().map_err(|_| {
        ApiError::invalid_field(
            field,
            format!("`{field}` must be stock, weighed, service or bundle"),
        )
    })
}

/// A tax class (`GET /tax-classes`). Rates arrive with spec 2.3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct TaxClassDto {
    /// ID (the seeded `Standard` is `00000000-0000-7000-8000-000000000101`).
    pub id: String,
    /// Name.
    pub name: String,
    /// Archived.
    pub archived: bool,
}

/// `GET /api/v1/tax-classes`: every tax class, by name.
///
/// # Errors
/// `401 invalid_session` / `invalid_device_token`.
#[utoipa::path(get, path = "/api/v1/tax-classes", tag = "catalog",
    security(("bearer" = []), ("register_token" = [])),
    responses((status = 200, body = [TaxClassDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn tax_classes(
    State(state): State<AppState>,
    _reader: CatalogReader,
) -> Result<Json<Vec<TaxClassDto>>, ApiError> {
    let classes = catalog::list_tax_classes(&state.db).await?;
    Ok(Json(
        classes
            .into_iter()
            .map(|t| TaxClassDto {
                id: t.id.to_string(),
                name: t.name,
                archived: t.archived,
            })
            .collect(),
    ))
}

/// Query of `GET /catalog/lookup`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct LookupQuery {
    /// The scanned or typed code: barcode, scale barcode, SKU or PLU.
    pub code: String,
    /// Back office only (a session without a register): apply this branch's assortment. Ignored
    /// for a register, whose branch comes from its device token.
    pub branch_id: Option<String>,
}

/// A lookup result (spec 2.2 §6 "Result").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct LookupDto {
    /// The product.
    pub product_id: String,
    /// The variant.
    pub variant_id: String,
    /// The unit: the barcode's unit, else the default sale unit (the base unit for a PLU).
    pub unit_id: String,
    /// Display name `<product> – <values>`.
    pub name: String,
    /// The product name alone.
    pub product_name: String,
    /// The variant's option values joined by ` / ` (`Medium / Red`); empty without options.
    pub variant_label: String,
    /// The unit's name (`pcs`, `box`, `kg`).
    pub unit_name: String,
    /// Variant SKU.
    pub sku: String,
    /// The variant's PLU (weighed products), else `null`.
    pub plu: Option<u32>,
    /// `stock`, `weighed`, `service` or `bundle`.
    #[serde(rename = "type")]
    pub product_type: String,
    /// The cashier types the price.
    pub open_price: bool,
    /// The register asks for a line note.
    pub requires_note: bool,
    /// Which step matched: `barcode`, `normalized_barcode`, `scale_barcode`, `sku` or `plu`.
    pub source: String,
    /// The code that matched, as stored: the barcode (its UPC-A ↔ EAN-13 form for
    /// `normalized_barcode`), the scale code, the upper-cased SKU or the PLU as typed.
    pub matched_code: String,
    /// Quantity in thousandths (`1000` = 1, `1250` = 1.250 kg); `null` for a price-embedded
    /// scale code (it follows from the unit price, spec 2.3).
    pub qty_thousandths: Option<i64>,
    /// Line amount of a price-embedded scale code, in minor units (Rupiah).
    pub amount: Option<i64>,
    /// The branch whose assortment was applied; `null` = none (back office without `branch_id`).
    pub branch_id: Option<String>,
}

const fn source_str(source: MatchSource) -> &'static str {
    match source {
        MatchSource::Barcode => "barcode",
        MatchSource::NormalizedBarcode => "normalized_barcode",
        MatchSource::ScaleBarcode => "scale_barcode",
        MatchSource::Sku => "sku",
        MatchSource::Plu => "plu",
    }
}

fn lookup_dto(item: &LookupItem, branch: Option<BranchId>) -> LookupDto {
    LookupDto {
        product_id: item.product.to_string(),
        variant_id: item.variant.to_string(),
        unit_id: item.unit.to_string(),
        name: item.name.clone(),
        product_name: item.product_name.clone(),
        variant_label: item.variant_label.clone(),
        unit_name: item.unit_name.clone(),
        sku: item.sku.clone(),
        plu: item.plu.map(sultan_core::catalog::Plu::value),
        product_type: item.product_type.as_str().to_owned(),
        open_price: item.open_price,
        requires_note: item.requires_note,
        source: source_str(item.source).to_owned(),
        matched_code: item.matched_code.clone(),
        qty_thousandths: item.qty.map(sultan_core::catalog::Qty::thousandths),
        amount: item.amount.map(sultan_core::money::Money::minor),
        branch_id: branch.map(|b| b.to_string()),
    }
}

/// The branch whose assortment applies (see the module docs).
///
/// # Errors
/// `400 invalid_body` (bad ID), `403 branch_not_assigned`, or the database error.
async fn assortment_branch(
    state: &AppState,
    reader: &CatalogReader,
    requested: Option<&str>,
) -> Result<Option<BranchId>, ApiError> {
    if let Some(branch) = reader.register_branch() {
        return Ok(Some(branch));
    }
    let Some(branch) = parse_opt_id::<BranchId>(requested, "branch_id")? else {
        return Ok(None);
    };
    let user = reader.user().ok_or_else(ApiError::invalid_session)?;
    if Caller::load(&state.db, user)
        .await?
        .branch_view()
        .allows(branch)
    {
        Ok(Some(branch))
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "branch_not_assigned",
            "`branch_id` is not a branch you are assigned to",
        ))
    }
}

/// The branch the repository checks blocks against: without an assortment, an ID no branch has
/// (the nil UUID), so nothing is blocked.
fn blocks_at(branch: Option<BranchId>) -> BranchId {
    branch.unwrap_or_else(|| BranchId::from_uuid(Uuid::nil()))
}

/// `GET /api/v1/catalog/lookup?code=`: resolves a scanned or typed code (spec 2.2 §6).
///
/// # Errors
/// `401`, `403 branch_not_assigned`, `404 not_found`, `422 archived` / `not_sold_here` /
/// `not_sellable` / `scale_value_zero`.
#[utoipa::path(get, path = "/api/v1/catalog/lookup", tag = "catalog",
    security(("register_token" = []), ("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(LookupQuery),
    responses((status = 200, body = LookupDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn lookup(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiQuery(query): ApiQuery<LookupQuery>,
) -> Result<Json<LookupDto>, ApiError> {
    let branch = assortment_branch(&state, &reader, query.branch_id.as_deref()).await?;
    let code = query.code.trim();
    if code.is_empty() {
        return Err(ApiError::catalog_rules(&[CatalogError::NotFound]));
    }
    let item = catalog::lookup(&state.db, code, blocks_at(branch)).await?;
    Ok(Json(lookup_dto(&item, branch)))
}

/// Query of `GET /catalog/search`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct SearchQuery {
    /// Text to find in the name, receipt name, code or brand (case and accents ignored).
    #[serde(default)]
    pub q: String,
    /// Hits, 1–50 (default 20).
    pub limit: Option<u32>,
    /// Back office only: apply this branch's assortment (see `GET /catalog/lookup`).
    pub branch_id: Option<String>,
}

/// One search hit: a sellable variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SearchHitDto {
    /// The product.
    pub product_id: String,
    /// The variant.
    pub variant_id: String,
    /// The default sale unit.
    pub unit_id: String,
    /// Display name `<product> – <values>`.
    pub name: String,
    /// Variant SKU.
    pub sku: String,
    /// Product type.
    #[serde(rename = "type")]
    pub product_type: String,
    /// The cashier types the price.
    pub open_price: bool,
    /// The register asks for a line note.
    pub requires_note: bool,
}

/// The answer of `GET /catalog/search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SearchResult {
    /// Hits: prefix matches first, then by name.
    pub items: Vec<SearchHitDto>,
    /// The branch whose assortment was applied; `null` = none.
    pub branch_id: Option<String>,
}

fn hit_dto(hit: SearchHit) -> SearchHitDto {
    SearchHitDto {
        product_id: hit.product.to_string(),
        variant_id: hit.variant.to_string(),
        unit_id: hit.unit.to_string(),
        name: hit.name,
        sku: hit.sku,
        product_type: hit.product_type.as_str().to_owned(),
        open_price: hit.open_price,
        requires_note: hit.requires_note,
    }
}

/// `GET /api/v1/catalog/search?q=`: name search for the register and back office.
///
/// # Errors
/// `401`, `403 branch_not_assigned`, `422 invalid_field` (`limit`).
#[utoipa::path(get, path = "/api/v1/catalog/search", tag = "catalog",
    security(("register_token" = []), ("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(SearchQuery),
    responses((status = 200, body = SearchResult), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn search(
    State(state): State<AppState>,
    reader: CatalogReader,
    ApiQuery(query): ApiQuery<SearchQuery>,
) -> Result<Json<SearchResult>, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_SEARCH_LIMIT);
    if !(1..=catalog::MAX_SEARCH_LIMIT).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {}", catalog::MAX_SEARCH_LIMIT),
        ));
    }
    let branch = assortment_branch(&state, &reader, query.branch_id.as_deref()).await?;
    let hits = catalog::search(&state.db, &query.q, blocks_at(branch), limit).await?;
    Ok(Json(SearchResult {
        items: hits.into_iter().map(hit_dto).collect(),
        branch_id: branch.map(|b| b.to_string()),
    }))
}

/// One scale barcode layout: EAN-13 `prefix · PLU (plu_digits) · value (value_digits) · check`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScaleLayoutDto {
    /// Two-digit prefix, 20–29.
    pub prefix: u8,
    /// Number of PLU digits.
    pub plu_digits: u8,
    /// Number of value digits (`2 + plu_digits + value_digits = 12`).
    pub value_digits: u8,
    /// `weight` (grams) or `price` (minor units).
    pub kind: String,
}

/// The catalog settings (`GET/PUT /settings/catalog`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct CatalogSettingsDto {
    /// Prefix of generated product codes (default `P`), stored upper case.
    pub product_code_prefix: String,
    /// Two-digit prefix of generated internal EAN-13 barcodes (default `29`), not a scale prefix.
    pub internal_barcode_prefix: String,
    /// Scale barcode layouts.
    pub scale_layouts: Vec<ScaleLayoutDto>,
    /// Common unit names for autocomplete.
    pub unit_names: Vec<String>,
}

impl From<&CatalogSettings> for CatalogSettingsDto {
    fn from(s: &CatalogSettings) -> Self {
        Self {
            product_code_prefix: s.product_code_prefix.clone(),
            internal_barcode_prefix: s.internal_barcode_prefix.clone(),
            scale_layouts: s
                .scale_layouts
                .iter()
                .map(|l| ScaleLayoutDto {
                    prefix: l.prefix,
                    plu_digits: l.plu_digits,
                    value_digits: l.value_digits,
                    kind: match l.kind {
                        ScaleValueKind::Weight => "weight",
                        ScaleValueKind::Price => "price",
                    }
                    .to_owned(),
                })
                .collect(),
            unit_names: s.unit_names.clone(),
        }
    }
}

impl CatalogSettingsDto {
    fn into_settings(self) -> Result<CatalogSettings, ApiError> {
        let scale_layouts = self
            .scale_layouts
            .iter()
            .map(|l| {
                let kind = match l.kind.as_str() {
                    "weight" => ScaleValueKind::Weight,
                    "price" => ScaleValueKind::Price,
                    _ => {
                        return Err(ApiError::invalid_field(
                            "scale_layouts.kind",
                            "`kind` must be weight or price",
                        ));
                    }
                };
                Ok(ScaleLayout {
                    prefix: l.prefix,
                    plu_digits: l.plu_digits,
                    value_digits: l.value_digits,
                    kind,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(CatalogSettings {
            product_code_prefix: self.product_code_prefix,
            internal_barcode_prefix: self.internal_barcode_prefix,
            scale_layouts,
            unit_names: self.unit_names,
        })
    }
}

/// `GET /api/v1/settings/catalog`.
///
/// # Errors
/// `401 invalid_session`.
#[utoipa::path(get, path = "/api/v1/settings/catalog", tag = "catalog",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    responses((status = 200, body = CatalogSettingsDto), (status = 401, body = crate::openapi::Problem)))]
pub async fn get_settings(
    State(state): State<AppState>,
    _session: UserSession,
) -> Result<Json<CatalogSettingsDto>, ApiError> {
    let settings = catalog::get_catalog_settings(&state.db).await?;
    Ok(Json(CatalogSettingsDto::from(&settings)))
}

/// `PUT /api/v1/settings/catalog`: replaces the settings; a replay changes nothing.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `422` (`invalid_code`, `invalid_scale_layout`,
/// `scale_prefix_overlap`, `invalid_field`, … with `errors`).
#[utoipa::path(put, path = "/api/v1/settings/catalog", tag = "catalog",
    security(("bearer" = [])), request_body = CatalogSettingsDto,
    responses((status = 200, body = CatalogSettingsDto), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_settings(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    ApiJson(body): ApiJson<CatalogSettingsDto>,
) -> Result<Json<CatalogSettingsDto>, ApiError> {
    require_manage(&state, &session).await?;
    let settings = body.into_settings()?;
    let (state, actor, settings, now) = (&state, &actor(&session), &settings, state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = catalog::put_catalog_settings(&mut tx, settings, actor, now).await?;
        let stored = catalog::get_catalog_settings(&mut tx).await?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish_changed(state, written, "settings", json!({}));
    Ok(Json(CatalogSettingsDto::from(&stored)))
}

/// A generated value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct GeneratedDto {
    /// The product code (`P000123`) or internal EAN-13 barcode (`2900000000017`).
    pub value: String,
}

/// `POST /api/v1/catalog/generate/product-code`: reserves the next free product code (§7).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `422 generator_exhausted` / `invalid_code`.
#[utoipa::path(post, path = "/api/v1/catalog/generate/product-code", tag = "catalog",
    security(("bearer" = [])),
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = GeneratedDto), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn generate_product_code(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
) -> Result<Json<GeneratedDto>, ApiError> {
    require_manage(&state, &session).await?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let value = retrying(|| catalog::generate_product_code(&state.db, actor, now)).await?;
    Ok(Json(GeneratedDto { value }))
}

/// `POST /api/v1/catalog/generate/barcode`: reserves the next free internal EAN-13 (§7).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `422 generator_exhausted` /
/// `scale_prefix_overlap`.
#[utoipa::path(post, path = "/api/v1/catalog/generate/barcode", tag = "catalog",
    security(("bearer" = [])),
    params(("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = GeneratedDto), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn generate_barcode(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
) -> Result<Json<GeneratedDto>, ApiError> {
    require_manage(&state, &session).await?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let value = retrying(|| catalog::generate_internal_barcode(&state.db, actor, now)).await?;
    Ok(Json(GeneratedDto { value }))
}

/// Wraps a not-found of a catalog read.
pub(crate) fn not_found(what: &'static str) -> CatalogDbError {
    CatalogDbError::NotFound(what)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[tokio::test]
    async fn retrying_reruns_retryable_errors_only() {
        // A lost race twice, then success.
        let calls = Cell::new(0);
        let calls = &calls;
        let value = retrying(|| async move {
            calls.set(calls.get() + 1);
            if calls.get() < 3 {
                Err(CatalogDbError::Db(DbError::Conflict("race".into())))
            } else {
                Ok(calls.get())
            }
        })
        .await
        .unwrap();
        assert_eq!(value, 3);

        // A rule error is answered at once.
        calls.set(0);
        let err = retrying(|| async move {
            calls.set(calls.get() + 1);
            CatalogResult::<()>::Err(CatalogDbError::rule(CatalogError::Archived))
        })
        .await
        .unwrap_err();
        assert_eq!((calls.get(), err.code()), (1, "archived"));

        // A race lost every time gives up after all attempts with `409 conflict`.
        calls.set(0);
        let err = retrying(|| async move {
            calls.set(calls.get() + 1);
            CatalogResult::<()>::Err(CatalogDbError::Db(DbError::Conflict("race".into())))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.get(), retry::ATTEMPTS);
        assert_eq!((err.status().as_u16(), err.code()), (409, "conflict"));
        assert_eq!(err.detail(), "race");
    }

    #[tokio::test]
    async fn a_busy_database_to_the_end_is_a_conflict() {
        // `SQLITE_BUSY` on every attempt: retried all the way, then `409 conflict`.
        let calls = Cell::new(0);
        let calls = &calls;
        let err = retrying(|| async move {
            calls.set(calls.get() + 1);
            CatalogResult::<()>::Err(CatalogDbError::Db(DbError::sqlite_busy()))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.get(), retry::ATTEMPTS);
        assert_eq!((err.status().as_u16(), err.code()), (409, "conflict"));

        // A database error that is not a lost race (here a pool timeout) is not retried: it
        // is the answer after one attempt.
        calls.set(0);
        let err = retrying(|| async move {
            calls.set(calls.get() + 1);
            CatalogResult::<()>::Err(CatalogDbError::Db(DbError::Sqlx(sqlx::Error::PoolTimedOut)))
        })
        .await
        .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert_eq!(err.status().as_u16(), 500);

        let err = gave_up(CatalogDbError::Db(DbError::Conflict("dup".into())));
        assert_eq!((err.status().as_u16(), err.code()), (409, "conflict"));
    }

    #[test]
    fn small_helpers() {
        assert_eq!(written_status(Written::Created), StatusCode::CREATED);
        assert_eq!(written_status(Written::Updated), StatusCode::OK);
        assert_eq!(written_status(Written::Unchanged), StatusCode::OK);
        assert_eq!(unit_kind_str(UnitKind::Decimal), "decimal");
        assert_eq!(parse_type("type", "weighed").unwrap(), ProductType::Weighed);
        assert_eq!(parse_type("type", "x").unwrap_err().code(), "invalid_field");
        assert_eq!(blocks_at(None).to_string(), Uuid::nil().to_string());
        let sources = [
            (MatchSource::Barcode, "barcode"),
            (MatchSource::NormalizedBarcode, "normalized_barcode"),
            (MatchSource::ScaleBarcode, "scale_barcode"),
            (MatchSource::Sku, "sku"),
            (MatchSource::Plu, "plu"),
        ];
        for (source, name) in sources {
            assert_eq!(source_str(source), name);
        }
        assert!(matches!(not_found("x"), CatalogDbError::NotFound("x")));
    }

    #[test]
    fn settings_kind_must_be_known() {
        let dto = CatalogSettingsDto {
            product_code_prefix: "P".into(),
            internal_barcode_prefix: "29".into(),
            scale_layouts: vec![ScaleLayoutDto {
                prefix: 21,
                plu_digits: 5,
                value_digits: 5,
                kind: "volume".into(),
            }],
            unit_names: vec![],
        };
        let err = dto.into_settings().unwrap_err();
        assert_eq!(err.code(), "invalid_field");
    }
}
