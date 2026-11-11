//! Pricing master data (spec 2.3 §9–§10, phase R3): price lists, their entries with quantity
//! tiers, tax rates per tax class and the pricing settings. The quote and the lookup price are in
//! [`super::pricing_quote`].
//!
//! - `GET /price-lists[?kind=&target_id=&include_archived=]`, `GET /price-lists/{id}`,
//!   `GET /price-lists/{id}/entries[?variant=&after_id=&limit=]`,
//!   `GET /price-lists/{id}/entries/{entry_id}`, `GET /products/{id}/prices`,
//!   `GET /tax-classes/{id}/rates`, `GET /settings/pricing`: any user session.
//! - `PUT /price-lists/{id}`, `POST /price-lists/{id}/archive` (`Idempotency-Key`),
//!   `PUT|DELETE /price-lists/{id}/entries/{entry_id}`, `PUT /tax-classes/{id}/rates` (one rate,
//!   keyed by `valid_from`) and `PUT /settings/pricing`: HQ only (`403 read_only_replica`) and
//!   `price.manage` through any role assignment (`403 permission_denied`).
//!
//! Every write stores the change, its audit row and its outbox row in one transaction (the
//! repositories in `sultan_db::pricing`), is run again after a lost race ([`retrying`]) and, when
//! it changed something, publishes `price.changed` to every event socket
//! (`Audience::Everyone`): `{scope, variant_ids}` with `scope` one of `price_list`,
//! `price_entry`, `tax_rate`, `settings`. `variant_ids` lists the variants an entry write touched;
//! it is `null` when the change may affect every price (a list's rule, switch or archive, a tax
//! rate, the settings), so registers re-price everything.
//!
//! Wire formats: a rule is `{"pct_bp": -500}` (−5%) or `{"amount": -500}` (Rp −500); a tier is
//! `{"min_qty_thousandths", "price"}` or `{"min_qty_thousandths", "rule"}`; dates are local
//! `YYYY-MM-DD` (the selling branch's day, both ends inclusive).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sultan_core::access::Permission;
use sultan_core::catalog::Qty;
use sultan_core::ids::{ProductId, TaxClassId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{
    CashRounding, PriceEntry, PriceEntryId, PriceListId, PriceListKind, PriceTier, Rule, TierPrice,
};
use sultan_core::tax::{TaxRate, TaxSettings};
use sultan_db::DbError;
use sultan_db::catalog;
use sultan_db::pricing::{
    self, EntryFilter, ListFilter, MAX_ENTRY_PAGE, PriceEntryRecord, PriceListRecord,
    PricingDbError, PricingResult, PricingSettings, Written,
};
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::catalog::{actor, written_status};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::{Audience, Event};
use crate::extract::{ApiJson, ApiQuery, RequireHq, UserSession};
use crate::retry::{self, Failure};
use crate::state::AppState;

/// Runs a pricing write transaction again while it loses races (see
/// [`super::catalog::retrying`]); `409 conflict` when every attempt lost.
///
/// # Errors
/// The operation's error as problem+json.
pub async fn retrying<T, F, Fut>(op: F) -> Result<T, ApiError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = PricingResult<T>>,
{
    retry::run("pricing write", op, PricingDbError::is_retryable)
        .await
        .map_err(|failure| match failure {
            Failure::Final(error) => error.into(),
            Failure::Exhausted(PricingDbError::Db(error @ DbError::Conflict(_))) => error.into(),
            Failure::Exhausted(_) => retry::conflict(),
        })
}

/// `403 permission_denied` unless the caller holds `price.manage` through any role assignment.
///
/// # Errors
/// See above, or the database error.
pub async fn require_price_manage(state: &AppState, session: &UserSession) -> Result<(), ApiError> {
    Caller::load(&state.db, session)
        .await?
        .require_anywhere(Permission::PriceManage)
}

/// The `price.changed` event (spec 2.3 §9) for every socket. `variants: None` = any price may
/// have changed.
#[must_use]
pub fn price_changed(scope: &str, variants: Option<&[VariantId]>) -> Event {
    let variant_ids: Value = variants.map_or(Value::Null, |vs| {
        let mut ids: Vec<String> = vs.iter().map(ToString::to_string).collect();
        ids.sort();
        ids.dedup();
        ids.into()
    });
    Event {
        kind: "price.changed",
        data: json!({ "scope": scope, "variant_ids": variant_ids }),
        audience: Audience::Everyone,
    }
}

/// Publishes [`price_changed`] unless the write changed nothing.
fn publish(state: &AppState, written: Written, scope: &str, variants: Option<&[VariantId]>) {
    if written != Written::Unchanged {
        state.events.publish(price_changed(scope, variants));
    }
}

/// A local date `YYYY-MM-DD`.
#[must_use]
pub fn date_text(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

/// Parses an optional local date `YYYY-MM-DD`; `422 invalid_field` naming the field.
fn parse_date(field: &str, value: Option<&str>) -> Result<Option<NaiveDate>, ApiError> {
    value
        .map(|v| {
            NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d").map_err(|_| {
                ApiError::invalid_field(field, format!("`{field}` must be a date YYYY-MM-DD"))
            })
        })
        .transpose()
}

/// A rule relative to the next applicable price: exactly one of `pct_bp` (basis points, −500 =
/// −5%) or `amount` (minor units, −500 = Rp 500 off).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct RuleDto {
    /// ± basis points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pct_bp: Option<i32>,
    /// ± minor units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
}

impl RuleDto {
    /// The wire form of a rule.
    #[must_use]
    pub const fn of(rule: Rule) -> Self {
        match rule {
            Rule::Pct(bp) => Self {
                pct_bp: Some(bp),
                amount: None,
            },
            Rule::Amount(amount) => Self {
                pct_bp: None,
                amount: Some(amount.minor()),
            },
        }
    }

    fn parse(self, field: &str) -> Result<Rule, ApiError> {
        match (self.pct_bp, self.amount) {
            (Some(bp), None) => Ok(Rule::Pct(bp)),
            (None, Some(amount)) => Ok(Rule::Amount(Money::from_minor(amount))),
            _ => Err(ApiError::invalid_field(
                field,
                format!("`{field}` needs exactly one of `pct_bp` and `amount`"),
            )),
        }
    }
}

/// Body of `PUT /price-lists/{id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PriceListBody {
    /// `default` (only the seeded list), `branch`, `group`, `tier` or `customer`. Fixed once
    /// created.
    pub kind: String,
    /// The branch, customer group, membership tier or customer; `null` for `default`. Fixed once
    /// created.
    #[serde(default)]
    pub target_id: Option<String>,
    /// Name, 1–100 characters.
    pub name: String,
    /// Switched on (default). At most one live (active, not archived) list per target.
    #[serde(default = "yes")]
    pub active: bool,
    /// The list rule for items without an entry (never on the `default` list).
    #[serde(default)]
    pub rule: Option<RuleDto>,
    /// `true` archives the list (removed from resolution at once), `false` unarchives it.
    #[serde(default)]
    pub archived: bool,
}

const fn yes() -> bool {
    true
}

/// A price list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PriceListDto {
    /// ID (the seeded `default` list is `00000000-0000-7000-8000-000000000201`).
    pub id: String,
    /// `default`, `branch`, `group`, `tier` or `customer`.
    pub kind: String,
    /// The target; `null` for `default`.
    pub target_id: Option<String>,
    /// Name.
    pub name: String,
    /// Switched on.
    pub active: bool,
    /// The list rule.
    pub rule: Option<RuleDto>,
    /// Archived.
    pub archived: bool,
    /// When it was archived (unix ms).
    pub archived_at: Option<i64>,
}

impl From<&PriceListRecord> for PriceListDto {
    fn from(l: &PriceListRecord) -> Self {
        Self {
            id: l.id.to_string(),
            kind: l.kind.as_str().to_owned(),
            target_id: l.target.map(|t| t.to_string()),
            name: l.name.clone(),
            active: l.active,
            rule: l.rule.map(RuleDto::of),
            archived: l.archived_at.is_some(),
            archived_at: l.archived_at.map(sultan_core::time::Timestamp::unix_millis),
        }
    }
}

/// Query of `GET /price-lists`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ListQuery {
    /// Only lists of this kind.
    pub kind: Option<String>,
    /// Only lists for this target.
    pub target_id: Option<String>,
    /// Include archived lists.
    #[serde(default)]
    pub include_archived: bool,
}

fn parse_kind(value: &str) -> Result<PriceListKind, ApiError> {
    value.parse().map_err(|_| {
        ApiError::invalid_field(
            "kind",
            "`kind` must be default, branch, group, tier or customer",
        )
    })
}

fn parse_list_id(value: &str) -> Result<PriceListId, ApiError> {
    parse_id::<Uuid>(value, "id").map(PriceListId::from_uuid)
}

fn parse_entry_id(value: &str) -> Result<PriceEntryId, ApiError> {
    parse_id::<Uuid>(value, "entry_id").map(PriceEntryId::from_uuid)
}

/// `GET /api/v1/price-lists`: price lists by kind and name.
///
/// # Errors
/// `401`, `400 invalid_body` (`target_id`), `422 invalid_field` (`kind`).
#[utoipa::path(get, path = "/api/v1/price-lists", tag = "pricing", security(("bearer" = [])),
    params(ListQuery),
    responses((status = 200, body = [PriceListDto]), (status = 401, body = crate::openapi::Problem)))]
pub async fn list_lists(
    State(state): State<AppState>,
    _session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<Vec<PriceListDto>>, ApiError> {
    let filter = ListFilter {
        kind: query.kind.as_deref().map(parse_kind).transpose()?,
        target: parse_opt_id(query.target_id.as_deref(), "target_id")?,
        include_archived: query.include_archived,
    };
    let lists = pricing::list_price_lists(&state.db, filter).await?;
    Ok(Json(lists.iter().map(PriceListDto::from).collect()))
}

/// `GET /api/v1/price-lists/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/price-lists/{id}", tag = "pricing", security(("bearer" = [])),
    params(("id" = String, Path, description = "Price list ID")),
    responses((status = 200, body = PriceListDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_list(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<PriceListDto>, ApiError> {
    let id = parse_list_id(&id)?;
    pricing::get_price_list(&state.db, id)
        .await?
        .map(|l| Json(PriceListDto::from(&l)))
        .ok_or_else(|| ApiError::not_found("price_list"))
}

/// `PUT /api/v1/price-lists/{id}`: create (`201`) or change (`200`) a price list. A replay of the
/// same body changes nothing (`200`).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `409 price_list_conflict`
/// (another live list for the target; `price_list_id`), `422 invalid_rule` (a rule on the
/// `default` list, a % rule of −100% or less), `invalid_reference` (`target_id`),
/// `invalid_field` (name, a changed kind or target, a second `default` list, switching the
/// `default` list off or archiving it).
#[utoipa::path(put, path = "/api/v1/price-lists/{id}", tag = "pricing", security(("bearer" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = PriceListBody,
    responses((status = 201, body = PriceListDto), (status = 200, body = PriceListDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_list(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<PriceListBody>,
) -> Result<(StatusCode, Json<PriceListDto>), ApiError> {
    let raw: Uuid = parse_put_id(&id)?;
    require_price_manage(&state, &session).await?;
    let list = PriceListRecord {
        id: PriceListId::from_uuid(raw),
        kind: parse_kind(&body.kind)?,
        target: parse_opt_id(body.target_id.as_deref(), "target_id")?,
        name: body.name,
        active: body.active,
        rule: body.rule.map(|r| r.parse("rule")).transpose()?,
        archived_at: body.archived.then(|| state.now()),
    };
    let (state, actor, list, now) = (&state, &actor(&session), &list, state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = pricing::put_price_list(&mut tx, list, actor, now).await?;
        let stored = pricing::get_price_list(&mut tx, list.id)
            .await?
            .ok_or(PricingDbError::NotFound("price_list"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish(state, written, "price_list", None);
    Ok((written_status(written), Json(PriceListDto::from(&stored))))
}

/// `POST /api/v1/price-lists/{id}/archive`: archives a list (removed from resolution at once);
/// answers the list. Archiving an archived list changes nothing. Accepts `Idempotency-Key`.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `404 not_found`, `422 invalid_field` (the
/// `default` list).
#[utoipa::path(post, path = "/api/v1/price-lists/{id}/archive", tag = "pricing",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Price list ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key")),
    responses((status = 200, body = PriceListDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn archive_list(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<PriceListDto>, ApiError> {
    let id = parse_list_id(&id)?;
    require_price_manage(&state, &session).await?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = pricing::archive_price_list(&mut tx, id, actor, now).await?;
        let stored = pricing::get_price_list(&mut tx, id)
            .await?
            .ok_or(PricingDbError::NotFound("price_list"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish(state, written, "price_list", None);
    Ok(Json(PriceListDto::from(&stored)))
}

/// One quantity tier: from `min_qty_thousandths` (in the entry's unit) on, a fixed unit `price`
/// (minor units, ≥ 0) or a `rule` on the price from the lists after this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TierDto {
    /// The least quantity, thousandths (> 0, unique within the entry).
    pub min_qty_thousandths: i64,
    /// Fixed unit price; exactly one of `price` and `rule`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price: Option<i64>,
    /// Rule; exactly one of `price` and `rule`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule: Option<RuleDto>,
}

impl TierDto {
    fn of(tier: &PriceTier) -> Self {
        let (price, rule) = match tier.price {
            TierPrice::Fixed(price) => (Some(price.minor()), None),
            TierPrice::Rule(rule) => (None, Some(RuleDto::of(rule))),
        };
        Self {
            min_qty_thousandths: tier.min_qty.thousandths(),
            price,
            rule,
        }
    }

    fn parse(self) -> Result<PriceTier, ApiError> {
        let price = match (self.price, self.rule) {
            (Some(price), None) => TierPrice::Fixed(Money::from_minor(price)),
            (None, Some(rule)) => TierPrice::Rule(rule.parse("tiers.rule")?),
            _ => {
                return Err(ApiError::invalid_field(
                    "tiers",
                    "a tier needs exactly one of `price` and `rule`",
                ));
            }
        };
        Ok(PriceTier {
            min_qty: Qty::from_thousandths(self.min_qty_thousandths),
            price,
        })
    }
}

/// Body of `PUT /price-lists/{id}/entries/{entry_id}`.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct PriceEntryBody {
    /// The priced variant.
    pub variant_id: String,
    /// One of its product's units; tiers' `min_qty` are in this unit.
    pub unit_id: String,
    /// First local day (inclusive), `YYYY-MM-DD`; `null` = open.
    #[serde(default)]
    pub valid_from: Option<String>,
    /// Last local day (inclusive); `null` = open.
    #[serde(default)]
    pub valid_to: Option<String>,
    /// 1–10 tiers.
    pub tiers: Vec<TierDto>,
}

/// A price-list entry with its tiers (ordered by `min_qty`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct PriceEntryDto {
    /// ID.
    pub id: String,
    /// The list.
    pub list_id: String,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// First local day, or `null`.
    pub valid_from: Option<String>,
    /// Last local day, or `null`.
    pub valid_to: Option<String>,
    /// The tiers.
    pub tiers: Vec<TierDto>,
}

impl From<&PriceEntryRecord> for PriceEntryDto {
    fn from(r: &PriceEntryRecord) -> Self {
        let e = &r.entry;
        Self {
            id: e.id.to_string(),
            list_id: r.list.to_string(),
            variant_id: e.variant.to_string(),
            unit_id: e.unit.to_string(),
            valid_from: e.valid_from.map(date_text),
            valid_to: e.valid_to.map(date_text),
            tiers: e.tiers.iter().map(TierDto::of).collect(),
        }
    }
}

/// Query of `GET /price-lists/{id}/entries`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct EntryQuery {
    /// Only entries for this variant.
    pub variant: Option<String>,
    /// The previous page's `next_after_id`.
    pub after_id: Option<String>,
    /// Page size, 1–1000 (default 1000).
    pub limit: Option<u32>,
}

/// One page of entries, ordered by ID.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct EntryPage {
    /// The entries.
    pub entries: Vec<PriceEntryDto>,
    /// Pass as `after_id` for the next page; `null` when the page was not full.
    pub next_after_id: Option<String>,
}

/// `GET /api/v1/price-lists/{id}/entries`: one page of the list's entries (any user).
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found` (list).
#[utoipa::path(get, path = "/api/v1/price-lists/{id}/entries", tag = "pricing",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Price list ID"), EntryQuery),
    responses((status = 200, body = EntryPage), (status = 404, body = crate::openapi::Problem)))]
pub async fn list_entries(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
    ApiQuery(query): ApiQuery<EntryQuery>,
) -> Result<Json<EntryPage>, ApiError> {
    let id = parse_list_id(&id)?;
    if pricing::get_price_list(&state.db, id).await?.is_none() {
        return Err(ApiError::not_found("price_list"));
    }
    let limit = query
        .limit
        .unwrap_or(MAX_ENTRY_PAGE)
        .clamp(1, MAX_ENTRY_PAGE);
    let filter = EntryFilter {
        list: Some(id),
        variant: parse_opt_id(query.variant.as_deref(), "variant")?,
        product: None,
        after: query
            .after_id
            .as_deref()
            .map(|a| parse_id::<Uuid>(a, "after_id").map(PriceEntryId::from_uuid))
            .transpose()?,
        limit,
    };
    let entries = pricing::list_price_entries(&state.db, filter).await?;
    let full = entries.len() == usize::try_from(limit).unwrap_or(usize::MAX);
    Ok(Json(EntryPage {
        next_after_id: entries
            .last()
            .filter(|_| full)
            .map(|r| r.entry.id.to_string()),
        entries: entries.iter().map(PriceEntryDto::from).collect(),
    }))
}

/// `GET /api/v1/price-lists/{id}/entries/{entry_id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found` (also an entry of another list).
#[utoipa::path(get, path = "/api/v1/price-lists/{id}/entries/{entry_id}", tag = "pricing",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Price list ID"),
        ("entry_id" = String, Path, description = "Entry ID")),
    responses((status = 200, body = PriceEntryDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn get_entry(
    State(state): State<AppState>,
    _session: UserSession,
    Path((id, entry_id)): Path<(String, String)>,
) -> Result<Json<PriceEntryDto>, ApiError> {
    let (id, entry_id) = (parse_list_id(&id)?, parse_entry_id(&entry_id)?);
    pricing::get_price_entry(&state.db, entry_id)
        .await?
        .filter(|r| r.list == id)
        .map(|r| Json(PriceEntryDto::from(&r)))
        .ok_or_else(|| ApiError::not_found("price_entry"))
}

/// `PUT /api/v1/price-lists/{id}/entries/{entry_id}`: create (`201`) or replace (`200`) an entry
/// with its tiers. A replay changes nothing. Marks the variant used (spec 2.2 §2).
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `404 not_found` (list),
/// `409 price_overlap` (another entry of the list for the same variant + unit shares a day;
/// `entry_id` names it), `422 invalid_rule` (a rule in the `default` list, a % rule of −100% or
/// less), `invalid_reference` (`variant_id`, `unit_id`), `invalid_field` (`tiers`,
/// `tiers.min_qty_thousandths`, `tiers.price`, `valid_to`, `list_id`).
#[utoipa::path(put, path = "/api/v1/price-lists/{id}/entries/{entry_id}", tag = "pricing",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Price list ID"),
        ("entry_id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = PriceEntryBody,
    responses((status = 201, body = PriceEntryDto), (status = 200, body = PriceEntryDto),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 409, body = crate::openapi::Problem), (status = 422, body = crate::openapi::Problem)))]
pub async fn put_entry(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path((id, entry_id)): Path<(String, String)>,
    ApiJson(body): ApiJson<PriceEntryBody>,
) -> Result<(StatusCode, Json<PriceEntryDto>), ApiError> {
    let list = parse_list_id(&id)?;
    let entry_id = PriceEntryId::from_uuid(parse_put_id::<Uuid>(&entry_id)?);
    require_price_manage(&state, &session).await?;
    let entry = PriceEntry {
        id: entry_id,
        variant: parse_id::<VariantId>(&body.variant_id, "variant_id")?,
        unit: parse_id::<UnitId>(&body.unit_id, "unit_id")?,
        valid_from: parse_date("valid_from", body.valid_from.as_deref())?,
        valid_to: parse_date("valid_to", body.valid_to.as_deref())?,
        tiers: body
            .tiers
            .into_iter()
            .map(TierDto::parse)
            .collect::<Result<_, _>>()?,
    };
    let (state, actor, entry, now) = (&state, &actor(&session), &entry, state.now());
    let (written, stored, old_variant) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let old = pricing::get_price_entry(&mut tx, entry.id).await?;
        let written = pricing::put_price_entry(&mut tx, list, entry, actor, now).await?;
        let stored = pricing::get_price_entry(&mut tx, entry.id)
            .await?
            .ok_or(PricingDbError::NotFound("price_entry"))?;
        tx.commit().await?;
        Ok((written, stored, old.map(|o| o.entry.variant)))
    })
    .await?;
    let mut variants = vec![stored.entry.variant];
    variants.extend(old_variant);
    publish(state, written, "price_entry", Some(&variants));
    Ok((written_status(written), Json(PriceEntryDto::from(&stored))))
}

/// `DELETE /api/v1/price-lists/{id}/entries/{entry_id}` → `204`. Until sales exist (2.6) every
/// entry may be deleted; afterwards one used by a posted sale is ended with `valid_to` instead.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(delete, path = "/api/v1/price-lists/{id}/entries/{entry_id}", tag = "pricing",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Price list ID"),
        ("entry_id" = String, Path, description = "Entry ID")),
    responses((status = 204, description = "Deleted"), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn delete_entry(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path((id, entry_id)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    let (list, entry_id) = (parse_list_id(&id)?, parse_entry_id(&entry_id)?);
    require_price_manage(&state, &session).await?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let variant = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let old = pricing::get_price_entry(&mut tx, entry_id).await?;
        pricing::delete_price_entry(&mut tx, list, entry_id, actor, now).await?;
        tx.commit().await?;
        Ok(old.map(|o| o.entry.variant))
    })
    .await?;
    let variants: Vec<VariantId> = variant.into_iter().collect();
    publish(state, Written::Updated, "price_entry", Some(&variants));
    Ok(StatusCode::NO_CONTENT)
}

/// The prices of a product for its form (`GET /products/{id}/prices`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct ProductPricesDto {
    /// The product.
    pub product_id: String,
    /// Every non-archived list (so the form can add entries to any of them), by kind and name.
    pub lists: Vec<PriceListDto>,
    /// Every entry of any list (archived ones too) for the product's variants, by ID.
    pub entries: Vec<PriceEntryDto>,
}

/// `GET /api/v1/products/{id}/prices`: all lists' entries for the product's variants (any user).
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/products/{id}/prices", tag = "pricing",
    security(("bearer" = [])), params(("id" = String, Path, description = "Product ID")),
    responses((status = 200, body = ProductPricesDto), (status = 404, body = crate::openapi::Problem)))]
pub async fn product_prices(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<ProductPricesDto>, ApiError> {
    let product: ProductId = parse_id(&id, "id")?;
    if catalog::get_product(&state.db, product).await?.is_none() {
        return Err(ApiError::not_found("product"));
    }
    let lists = pricing::list_price_lists(&state.db, ListFilter::default()).await?;
    let mut entries = Vec::new();
    let mut after = None;
    loop {
        let page = pricing::list_price_entries(
            &state.db,
            EntryFilter {
                product: Some(product),
                after,
                limit: MAX_ENTRY_PAGE,
                ..EntryFilter::default()
            },
        )
        .await?;
        let full = page.len() == usize::try_from(MAX_ENTRY_PAGE).unwrap_or(usize::MAX);
        after = page.last().map(|r| r.entry.id);
        entries.extend(page.iter().map(PriceEntryDto::from));
        if !full {
            break;
        }
    }
    Ok(Json(ProductPricesDto {
        product_id: product.to_string(),
        lists: lists.iter().map(PriceListDto::from).collect(),
        entries,
    }))
}

/// One rate of a tax class from a local date on (spec 2.3 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct TaxRateDto {
    /// Basis points (1 100 = 11%).
    pub rate_bp: u16,
    /// First local day the rate applies, `YYYY-MM-DD`.
    pub valid_from: String,
}

impl From<TaxRate> for TaxRateDto {
    fn from(r: TaxRate) -> Self {
        Self {
            rate_bp: r.rate_bp,
            valid_from: date_text(r.valid_from),
        }
    }
}

async fn tax_class_exists(state: &AppState, id: TaxClassId) -> Result<(), ApiError> {
    let classes = catalog::list_tax_classes(&state.db).await?;
    if classes.iter().any(|c| c.id == id) {
        Ok(())
    } else {
        Err(ApiError::not_found("tax_class"))
    }
}

/// `GET /api/v1/tax-classes/{id}/rates`: the class's rates, oldest first (any user).
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/tax-classes/{id}/rates", tag = "pricing",
    security(("bearer" = [])), params(("id" = String, Path, description = "Tax class ID")),
    responses((status = 200, body = [TaxRateDto]), (status = 404, body = crate::openapi::Problem)))]
pub async fn list_rates(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaxRateDto>>, ApiError> {
    let id: TaxClassId = parse_id(&id, "id")?;
    tax_class_exists(&state, id).await?;
    let rates = pricing::list_tax_rates(&state.db, id).await?;
    Ok(Json(rates.into_iter().map(TaxRateDto::from).collect()))
}

/// `PUT /api/v1/tax-classes/{id}/rates`: adds the rate from `valid_from` (`201`) or changes the
/// rate of that day (`200`); a future date is allowed. Answers all rates of the class.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `404 not_found`,
/// `422 invalid_field` (`valid_from`).
#[utoipa::path(put, path = "/api/v1/tax-classes/{id}/rates", tag = "pricing",
    security(("bearer" = [])), params(("id" = String, Path, description = "Tax class ID")),
    request_body = TaxRateDto,
    responses((status = 201, body = [TaxRateDto]), (status = 200, body = [TaxRateDto]),
        (status = 403, body = crate::openapi::Problem), (status = 404, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_rate(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<TaxRateDto>,
) -> Result<(StatusCode, Json<Vec<TaxRateDto>>), ApiError> {
    let class: TaxClassId = parse_id(&id, "id")?;
    require_price_manage(&state, &session).await?;
    let valid_from = parse_date("valid_from", Some(&body.valid_from))?
        .ok_or_else(|| ApiError::invalid_field("valid_from", "`valid_from` is required"))?;
    let rate = TaxRate {
        rate_bp: body.rate_bp,
        valid_from,
    };
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let (written, rates) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = pricing::put_tax_rate(&mut tx, class, rate, actor, now).await?;
        let rates = pricing::list_tax_rates(&mut tx, class).await?;
        tx.commit().await?;
        Ok((written, rates))
    })
    .await?;
    publish(state, written, "tax_rate", None);
    Ok((
        written_status(written),
        Json(rates.into_iter().map(TaxRateDto::from).collect()),
    ))
}

/// The company pricing settings (spec 2.3 §7–§8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PricingSettingsDto {
    /// Prices include tax (default). Changing it does not convert stored prices.
    pub tax_inclusive: bool,
    /// The company is tax-registered (PKP, default); otherwise no tax is computed.
    pub tax_registered: bool,
    /// `none` (default), `nearest_100`, `down_100`, `nearest_500` or `down_500` (applied to cash
    /// payments with 2.6).
    pub cash_rounding: String,
}

impl From<PricingSettings> for PricingSettingsDto {
    fn from(s: PricingSettings) -> Self {
        Self {
            tax_inclusive: s.tax.inclusive,
            tax_registered: s.tax.registered,
            cash_rounding: s.cash_rounding.as_str().to_owned(),
        }
    }
}

impl PricingSettingsDto {
    fn parse(&self) -> Result<PricingSettings, ApiError> {
        let cash_rounding = CashRounding::ALL
            .into_iter()
            .find(|r| r.as_str() == self.cash_rounding)
            .ok_or_else(|| {
                ApiError::invalid_field(
                    "cash_rounding",
                    "`cash_rounding` must be none, nearest_100, down_100, nearest_500 or down_500",
                )
            })?;
        Ok(PricingSettings {
            tax: TaxSettings {
                inclusive: self.tax_inclusive,
                registered: self.tax_registered,
            },
            cash_rounding,
        })
    }
}

/// `GET /api/v1/settings/pricing` (any user).
///
/// # Errors
/// `401`.
#[utoipa::path(get, path = "/api/v1/settings/pricing", tag = "pricing", security(("bearer" = [])),
    responses((status = 200, body = PricingSettingsDto), (status = 401, body = crate::openapi::Problem)))]
pub async fn get_settings(
    State(state): State<AppState>,
    _session: UserSession,
) -> Result<Json<PricingSettingsDto>, ApiError> {
    let settings = pricing::get_pricing_settings(&state.db).await?;
    Ok(Json(settings.into()))
}

/// `PUT /api/v1/settings/pricing`: replaces the settings (audited); answers them.
///
/// # Errors
/// `403 read_only_replica` / `permission_denied`, `400 invalid_body`, `422 invalid_field`
/// (`cash_rounding`).
#[utoipa::path(put, path = "/api/v1/settings/pricing", tag = "pricing", security(("bearer" = [])),
    request_body = PricingSettingsDto,
    responses((status = 200, body = PricingSettingsDto), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put_settings(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    ApiJson(body): ApiJson<PricingSettingsDto>,
) -> Result<Json<PricingSettingsDto>, ApiError> {
    require_price_manage(&state, &session).await?;
    let settings = body.parse()?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let (written, stored) = retrying(|| async move {
        let mut tx = state.db.begin().await?;
        let written = pricing::put_pricing_settings(&mut tx, settings, actor, now).await?;
        let stored = pricing::get_pricing_settings(&mut tx).await?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish(state, written, "settings", None);
    Ok(Json(stored.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_and_tiers_parse() {
        let cases = [
            (RuleDto::default(), None),
            (
                RuleDto {
                    pct_bp: Some(-500),
                    amount: None,
                },
                Some(Rule::Pct(-500)),
            ),
            (
                RuleDto {
                    pct_bp: None,
                    amount: Some(-300),
                },
                Some(Rule::Amount(Money::from_minor(-300))),
            ),
            (
                RuleDto {
                    pct_bp: Some(1),
                    amount: Some(1),
                },
                None,
            ),
        ];
        for (dto, expected) in cases {
            assert_eq!(dto.parse("rule").ok(), expected, "{dto:?}");
            if let Some(rule) = expected {
                assert_eq!(RuleDto::of(rule), dto);
            }
        }
        let bad = TierDto {
            min_qty_thousandths: 1_000,
            price: None,
            rule: None,
        };
        assert_eq!(bad.parse().unwrap_err().code(), "invalid_field");
        let rule = TierDto {
            min_qty_thousandths: 1_000,
            price: None,
            rule: Some(RuleDto {
                pct_bp: Some(-500),
                amount: None,
            }),
        };
        assert_eq!(TierDto::of(&rule.parse().unwrap()), rule);
    }

    #[test]
    fn dates_kinds_and_settings() {
        assert_eq!(
            parse_date("d", Some(" 2026-12-01 ")).unwrap(),
            NaiveDate::from_ymd_opt(2026, 12, 1)
        );
        assert_eq!(parse_date("d", None).unwrap(), None);
        let err = parse_date("valid_to", Some("01/12/2026")).unwrap_err();
        assert_eq!((err.code(), err.status().as_u16()), ("invalid_field", 422));
        assert_eq!(parse_kind("tier").unwrap(), PriceListKind::Tier);
        assert_eq!(parse_kind("gold").unwrap_err().code(), "invalid_field");
        let dto = PricingSettingsDto {
            tax_inclusive: false,
            tax_registered: true,
            cash_rounding: "down_500".into(),
        };
        assert_eq!(PricingSettingsDto::from(dto.parse().unwrap()), dto);
        let bad = PricingSettingsDto {
            cash_rounding: "up".into(),
            ..dto
        };
        assert_eq!(bad.parse().unwrap_err().code(), "invalid_field");
    }

    #[test]
    fn price_changed_events() {
        let v = VariantId::generate();
        let e = price_changed("price_entry", Some(&[v, v]));
        assert_eq!(e.kind, "price.changed");
        assert_eq!(e.audience, Audience::Everyone);
        assert_eq!(e.data["variant_ids"], json!([v.to_string()]));
        assert_eq!(
            price_changed("settings", None).data["variant_ids"],
            Value::Null
        );
    }
}
