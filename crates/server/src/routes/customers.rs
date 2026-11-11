//! Customers (spec `docs/spec/2.5-customers.md` §2, §6–§9, §12–§13; phase M3).
//!
//! - `PUT /customers/{id}`: sign-up (`201`, `customer.create`) or update (`200`,
//!   `customer.edit`). Setting or changing the group, tier, valid-until date or manual-tier flag
//!   also needs `customer.manage`, on create too (the till cannot set them). An identical replay
//!   answers `200` and needs only `customer.create` or `customer.edit`, so a cashier's retried
//!   sign-up works. Input is checked with `core::customer` (phone → `+62…`, `invalid_phone`; tax id,
//!   `invalid_tax_id`; email lower case; name 1–100). `code` is optional: without it the server
//!   draws a random code (`CustomerCode::generate` with the OS RNG) and draws again on a clash, at
//!   most [`CODE_ATTEMPTS`] times (then `500`); a code the client sends that is taken is
//!   `409 conflict` (the client draws a new one). On an update the stored code is kept.
//! - `GET /customers/{id}`: any user session; an archived (merged, erased) customer needs
//!   `customer.manage`.
//! - `GET /customers/lookup?phone=…` / `?code=…`: the till's lookup (spec 2.5 §6), any user
//!   session (a device token alone is `401`). A merged customer's code or phone finds the customer
//!   it was merged into; archived and erased customers are `404`.
//! - `GET /customers?q=&page=&limit=`: name search over name and company name, `customer.search`,
//!   at most 20 per page. Without `q` it lists every customer by name (the back office's list),
//!   `include_archived=true` adds archived ones and needs `customer.manage`.
//! - `GET /customers/duplicates` ("Possible duplicates") and `GET /customers/negative-balances`
//!   ("Negative balances", spec 2.5 §11): `customer.manage`.
//! - `POST /customers/{id}/archive`, `/unarchive`, `/merge {from}` (`customer.manage`) and
//!   `/erase` (`customer.erase`), with `Idempotency-Key`.
//! - `GET /customers/{id}/history`: **an empty page until sales exist**; it fills in with spec 2.6.
//!
//! **Node roles** (spec 2.5 §12): on a branch node only sign-up (a `PUT` that creates, or its
//! replay) is allowed; every other write is `403 read_only_replica`. Forwarding a till's profile
//! edit to HQ (and `503 hq_unreachable`) comes with sync. Reads work everywhere.
//!
//! Customer permissions count from any of the caller's role assignments
//! ([`Caller::require_anywhere`]; spec 2.5 §18 item 16, decided 2026-11-12), like `catalog.manage`:
//! customers are company-wide master data. Every write stores the change, one audit row and one outbox row
//! per customer in one transaction (`sultan_db::customers`), reruns the transaction after a lost
//! race ([`run`]) and publishes `customer.changed {customer_id}` to every socket when something
//! changed.

use std::future::Future;

use argon2::password_hash::rand_core::{OsRng, RngCore};
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sultan_core::access::Permission;
use sultan_core::customer::{
    Birthday as CoreBirthday, CustomerCode, CustomerError, Phone, TaxId, normalize_email,
    normalize_name,
};
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::time::Timestamp;
use sultan_db::DbError;
use sultan_db::customers::{
    self as cust, Birthday, Customer, CustomerDbError, CustomerGroup, CustomerRecord,
    MembershipTier, Written,
};
use sultan_db::org;
use utoipa::{IntoParams, ToSchema};
use uuid::Uuid;

use super::catalog::{actor, written_status};
use super::{parse_id, parse_opt_id, parse_put_id};
use crate::access::Caller;
use crate::error::ApiError;
use crate::events::Event;
use crate::extract::{ApiJson, ApiQuery, RequireHq, UserSession};
use crate::retry::{self, Failure};
use crate::state::AppState;

/// How often a sign-up draws a new code after a clash (spec 2.5 §2: up to 5 times, then `500`).
pub const CODE_ATTEMPTS: u32 = 5;
/// Page size of the name search and the list (spec 2.5 §6: at most 20 results).
pub const SEARCH_PAGE: u32 = cust::MAX_SEARCH_PAGE;
/// Page size of the duplicates and negative-balances lists.
pub const REVIEW_PAGE: u32 = 50;

/// Why a customer transaction failed: a repository error (maybe a lost race worth a rerun) or an
/// answer decided by the handler.
#[derive(Debug)]
pub(crate) enum Fail {
    /// A repository error.
    Db(CustomerDbError),
    /// Any other answer.
    Api(ApiError),
}

impl From<CustomerDbError> for Fail {
    fn from(error: CustomerDbError) -> Self {
        Self::Db(error)
    }
}

impl From<DbError> for Fail {
    fn from(error: DbError) -> Self {
        Self::Db(error.into())
    }
}

impl From<ApiError> for Fail {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

impl From<CustomerError> for Fail {
    fn from(error: CustomerError) -> Self {
        Self::Api(error.into())
    }
}

impl From<Fail> for ApiError {
    fn from(fail: Fail) -> Self {
        match fail {
            Fail::Db(error) => error.into(),
            Fail::Api(error) => error,
        }
    }
}

/// Runs a whole write transaction again while it loses races (see
/// [`super::catalog::retrying`]); `409 conflict` when every attempt lost.
pub(crate) async fn run<T, F, Fut>(op: F) -> Result<T, Fail>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Fail>>,
{
    retry::run(
        "customer write",
        op,
        |f| matches!(f, Fail::Db(e) if e.is_retryable()),
    )
    .await
    .map_err(|failure| match failure {
        Failure::Final(fail) => fail,
        Failure::Exhausted(Fail::Db(CustomerDbError::Db(e @ DbError::Conflict(_)))) => {
            Fail::Api(e.into())
        }
        Failure::Exhausted(_) => Fail::Api(retry::conflict()),
    })
}

/// Publishes `customer.changed` unless the write changed nothing.
pub(crate) fn publish_changed(state: &AppState, written: Written, customer: Uuid) {
    if written != Written::Unchanged {
        state.events.publish(Event::customer_changed(customer));
    }
}

/// `YYYYMMDD` → `YYYY-MM-DD`.
fn date_text(day: u32) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        day / 10_000,
        day / 100 % 100,
        day % 100
    )
}

/// `YYYY-MM-DD` → `YYYYMMDD`; `422 invalid_field` otherwise.
fn parse_date(field: &str, text: &str) -> Result<u32, ApiError> {
    let invalid = || ApiError::invalid_field(field, format!("`{field}` must be a YYYY-MM-DD date"));
    let mut parts = text.trim().splitn(3, '-');
    let mut next = || parts.next().and_then(|p| p.parse::<u32>().ok());
    let (Some(y), Some(m), Some(d)) = (next(), next(), next()) else {
        return Err(invalid());
    };
    if !(1..=9999).contains(&y) {
        return Err(invalid());
    }
    // A birthday with a year is exactly a calendar date check.
    CoreBirthday::new(m, d, i32::try_from(y).ok()).map_err(|_| invalid())?;
    Ok(y * 10_000 + m * 100 + d)
}

/// The writable fields of a customer (`PUT /customers/{id}`). Blank text means "none".
#[derive(Debug, Clone, Default, Deserialize, Serialize, ToSchema)]
pub struct CustomerBody {
    /// Member code (`K7QH-29XM`, dash optional, any case). Optional on create (the server draws
    /// one); ignored on update (the code never changes).
    #[serde(default)]
    pub code: Option<String>,
    /// Name, 1–100 characters after trimming.
    pub name: String,
    /// Phone, normalized to `+62…` (`0812-3456-789` → `+628123456789`); unique among
    /// non-archived customers.
    #[serde(default)]
    pub phone: Option<String>,
    /// Email, stored lower case; not unique.
    #[serde(default)]
    pub email: Option<String>,
    /// Birthday month (1–12); with `birth_day`.
    #[serde(default)]
    pub birth_month: Option<u32>,
    /// Birthday day of the month; 29 February is allowed.
    #[serde(default)]
    pub birth_day: Option<u32>,
    /// Birth year, optional even with a birthday.
    #[serde(default)]
    pub birth_year: Option<i32>,
    /// Address.
    #[serde(default)]
    pub address: Option<String>,
    /// Company name.
    #[serde(default)]
    pub company_name: Option<String>,
    /// NPWP or NIK: 15 or 16 digits (` .-` separators are dropped).
    #[serde(default)]
    pub tax_id: Option<String>,
    /// Tax address.
    #[serde(default)]
    pub tax_address: Option<String>,
    /// Staff notes (not printed).
    #[serde(default)]
    pub notes: Option<String>,
    /// Customer group (`customer.manage` to set or change).
    #[serde(default)]
    pub group_id: Option<String>,
    /// Membership tier (`customer.manage`).
    #[serde(default)]
    pub tier_id: Option<String>,
    /// Last day the tier applies, `YYYY-MM-DD` (`customer.manage`; needs `tier_id`).
    #[serde(default)]
    pub tier_valid_until: Option<String>,
    /// Never changed by the automatic tier job (`customer.manage`).
    #[serde(default)]
    pub manual_tier: bool,
}

/// `Some(text)` unless blank.
fn present(text: Option<&str>) -> Option<&str> {
    text.map(str::trim).filter(|t| !t.is_empty())
}

/// The body checked with `core::customer`: the optional code and the profile (code empty).
fn parse_body(
    body: &CustomerBody,
    id: Uuid,
    session: &UserSession,
    now: Timestamp,
) -> Result<(Option<CustomerCode>, Customer), ApiError> {
    let code = present(body.code.as_deref())
        .map(CustomerCode::parse)
        .transpose()?;
    let phone = present(body.phone.as_deref())
        .map(Phone::parse)
        .transpose()?;
    let tax_id = present(body.tax_id.as_deref())
        .map(TaxId::parse)
        .transpose()?;
    let email = present(body.email.as_deref())
        .map(normalize_email)
        .transpose()?;
    let birthday = match (body.birth_month, body.birth_day) {
        (None, None) if body.birth_year.is_none() => None,
        (Some(month), Some(day)) => {
            let b = CoreBirthday::new(month, day, body.birth_year)?;
            let year = b
                .year()
                .map(|y| u16::try_from(y).ok().filter(|y| (1..=9999).contains(y)))
                .map(|y| y.ok_or_else(|| ApiError::invalid_field("birth_year", "out of range")))
                .transpose()?;
            Some(Birthday {
                // `CoreBirthday::new` checked 1–12 and 1–31.
                month: u8::try_from(b.month()).unwrap_or_default(),
                day: u8::try_from(b.day()).unwrap_or_default(),
                year,
            })
        }
        _ => {
            return Err(ApiError::invalid_field(
                "birthday",
                "`birth_month` and `birth_day` go together (`birth_year` needs them)",
            ));
        }
    };
    let tier = parse_opt_id::<Uuid>(present(body.tier_id.as_deref()), "tier_id")?;
    let tier_valid_until = present(body.tier_valid_until.as_deref())
        .map(|d| parse_date("tier_valid_until", d))
        .transpose()?;
    if tier.is_none() && tier_valid_until.is_some() {
        return Err(ApiError::invalid_field(
            "tier_valid_until",
            "`tier_valid_until` needs `tier_id`",
        ));
    }
    let text = |t: &Option<String>| present(t.as_deref()).map(str::to_owned);
    let customer = Customer {
        id,
        code: String::new(),
        name: normalize_name(&body.name)?,
        phone: phone.map(|p| p.as_str().to_owned()),
        email,
        birthday,
        address: text(&body.address),
        company_name: text(&body.company_name),
        tax_id: tax_id.map(|t| t.as_str().to_owned()),
        tax_address: text(&body.tax_address),
        notes: text(&body.notes),
        group: parse_opt_id(present(body.group_id.as_deref()), "group_id")?,
        tier,
        tier_valid_until,
        manual_tier: body.manual_tier,
        signed_up_branch: Some(session.session.branch),
        signed_up_at: now,
    };
    Ok((code, customer))
}

/// The fields only `customer.manage` sets (spec 2.5 §7).
fn tier_part(c: &Customer) -> (Option<Uuid>, Option<Uuid>, Option<u32>, bool) {
    (c.group, c.tier, c.tier_valid_until, c.manual_tier)
}

/// Who may write `new` over `old` (see the module docs).
fn check_put(
    caller: &Caller,
    hq: bool,
    old: Option<&CustomerRecord>,
    new: &Customer,
) -> Result<(), ApiError> {
    let Some(old) = old else {
        caller.require_anywhere(Permission::CustomerCreate)?;
        if tier_part(new) != (None, None, None, false) {
            caller.require_anywhere(Permission::CustomerManage)?;
        }
        return Ok(());
    };
    let mut same = new.clone();
    same.code.clone_from(&old.customer.code);
    same.signed_up_branch = old.customer.signed_up_branch;
    same.signed_up_at = old.customer.signed_up_at;
    if same == old.customer {
        return caller
            .require_any_anywhere(&[Permission::CustomerCreate, Permission::CustomerEdit]);
    }
    if !hq {
        return Err(ApiError::read_only_replica());
    }
    caller.require_anywhere(Permission::CustomerEdit)?;
    if tier_part(&old.customer) != tier_part(new) {
        caller.require_anywhere(Permission::CustomerManage)?;
    }
    Ok(())
}

/// A customer as the API shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CustomerDto {
    /// ID.
    pub id: String,
    /// Member code as stored, e.g. `K7QH29XM`.
    pub code: String,
    /// The code as shown and printed, e.g. `K7QH-29XM`.
    pub display_code: String,
    /// Name (`Erased customer` after an erase).
    pub name: String,
    /// Phone, `+62…`.
    pub phone: Option<String>,
    /// The phone as the receipt shows it, e.g. `0812****789`.
    pub masked_phone: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Birthday month.
    pub birth_month: Option<u8>,
    /// Birthday day.
    pub birth_day: Option<u8>,
    /// Birth year.
    pub birth_year: Option<u16>,
    /// Address.
    pub address: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// NPWP or NIK (digits).
    pub tax_id: Option<String>,
    /// Tax address.
    pub tax_address: Option<String>,
    /// Staff notes.
    pub notes: Option<String>,
    /// Group.
    pub group_id: Option<String>,
    /// The group's name.
    pub group_name: Option<String>,
    /// Membership tier.
    pub tier_id: Option<String>,
    /// The tier's name.
    pub tier_name: Option<String>,
    /// Last day the tier applies, `YYYY-MM-DD`.
    pub tier_valid_until: Option<String>,
    /// Whether the tier applies today at the session's branch (not archived, not expired; spec
    /// 2.5 §4); `null` without a tier. `false` → show "Gold (expired …)".
    pub tier_applies: Option<bool>,
    /// Manual tier.
    pub manual_tier: bool,
    /// Branch where the customer signed up.
    pub signed_up_branch_id: Option<String>,
    /// Sign-up time (unix ms).
    pub signed_up_at: i64,
    /// A phone that clashed at sync ("Possible duplicates").
    pub phone_conflict: Option<String>,
    /// Customer balance (saved change), minor units.
    pub balance: i64,
    /// Archived (also after a merge or an erase).
    pub archived: bool,
    /// When it was archived.
    pub archived_at: Option<i64>,
    /// The customer it was merged into.
    pub merged_into_id: Option<String>,
    /// When its personal data was erased.
    pub erased_at: Option<i64>,
    /// Created (unix ms).
    pub created_at: i64,
    /// Last changed (unix ms).
    pub updated_at: i64,
}

/// Group and tier names and today's date at the session's branch, for [`CustomerDto`]s.
pub(crate) struct Names {
    groups: Vec<CustomerGroup>,
    tiers: Vec<MembershipTier>,
    today: i64,
}

impl Names {
    /// Loads the groups and tiers (archived ones included) and today's date in the time zone of
    /// the session's branch.
    pub(crate) async fn load(state: &AppState, session: &UserSession) -> Result<Self, ApiError> {
        let tz = org::get_branch(&state.db, session.session.branch)
            .await?
            .and_then(|b| b.timezone.parse::<BranchTimeZone>().ok())
            .unwrap_or_default();
        Ok(Self {
            groups: cust::list_groups(&state.db, true).await?,
            tiers: cust::list_tiers(&state.db, true).await?,
            today: LocalDay::of(state.now(), tz).key(),
        })
    }

    pub(crate) fn dto(&self, r: &CustomerRecord) -> CustomerDto {
        let c = &r.customer;
        let group = c.group.and_then(|g| self.groups.iter().find(|x| x.id == g));
        let tier = c.tier.and_then(|t| self.tiers.iter().find(|x| x.id == t));
        let tier_applies = c.tier.map(|_| {
            tier.is_some_and(|t| t.archived_at.is_none())
                && c.tier_valid_until
                    .is_none_or(|until| i64::from(until) >= self.today)
        });
        let id = |u: Option<Uuid>| u.map(|u| u.to_string());
        CustomerDto {
            id: c.id.to_string(),
            code: c.code.clone(),
            display_code: CustomerCode::parse(&c.code)
                .map_or_else(|_| c.code.clone(), |code| code.to_string()),
            name: c.name.clone(),
            phone: c.phone.clone(),
            masked_phone: c
                .phone
                .as_deref()
                .and_then(|p| Phone::parse(p).ok())
                .map(|p| p.masked()),
            email: c.email.clone(),
            birth_month: c.birthday.map(|b| b.month),
            birth_day: c.birthday.map(|b| b.day),
            birth_year: c.birthday.and_then(|b| b.year),
            address: c.address.clone(),
            company_name: c.company_name.clone(),
            tax_id: c.tax_id.clone(),
            tax_address: c.tax_address.clone(),
            notes: c.notes.clone(),
            group_id: id(c.group),
            group_name: group.map(|g| g.name.clone()),
            tier_id: id(c.tier),
            tier_name: tier.map(|t| t.name.clone()),
            tier_valid_until: c.tier_valid_until.map(date_text),
            tier_applies,
            manual_tier: c.manual_tier,
            signed_up_branch_id: c.signed_up_branch.map(|b| b.to_string()),
            signed_up_at: c.signed_up_at.unix_millis(),
            phone_conflict: r.phone_conflict.clone(),
            balance: r.balance.minor(),
            archived: r.archived_at.is_some(),
            archived_at: r.archived_at.map(Timestamp::unix_millis),
            merged_into_id: id(r.merged_into),
            erased_at: r.erased_at.map(Timestamp::unix_millis),
            created_at: r.created_at.unix_millis(),
            updated_at: r.updated_at.unix_millis(),
        }
    }
}

/// A random code from the OS RNG (spec 2.5 §2).
fn draw_code() -> CustomerCode {
    CustomerCode::generate(|| {
        let mut byte = [0_u8; 1];
        OsRng.fill_bytes(&mut byte);
        byte[0]
    })
}

/// `PUT /api/v1/customers/{id}`: sign up (`201`) or update (`200`) a customer (spec 2.5 §7).
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied` / `read_only_replica`, `409 phone_taken`
/// (with the other customer's `customer_code` and `masked_phone`) / `conflict` (a sent code is
/// taken) / `archived` / `merged`, `422 invalid_phone` / `invalid_tax_id` / `invalid_code` /
/// `invalid_field` / `invalid_reference`.
#[utoipa::path(put, path = "/api/v1/customers/{id}", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Client-generated UUIDv7")),
    request_body = CustomerBody,
    responses((status = 201, body = CustomerDto), (status = 200, body = CustomerDto),
        (status = 403, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn put(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<CustomerBody>,
) -> Result<(StatusCode, Json<CustomerDto>), ApiError> {
    let id: Uuid = parse_put_id(&id)?;
    let now = state.now();
    let (sent_code, customer) = parse_body(&body, id, &session, now)?;
    let caller = Caller::load(&state.db, &session).await?;
    let hq = state.node_role.owns_master_data();
    let actor = actor(&session);
    let mut attempt = 0;
    let (written, stored) = loop {
        attempt += 1;
        let mut customer = customer.clone();
        sent_code
            .unwrap_or_else(draw_code)
            .as_str()
            .clone_into(&mut customer.code);
        let (state, actor, caller, customer) = (&state, &actor, &caller, &customer);
        let result = run(|| async move {
            let mut tx = state.db.begin().await?;
            let old = cust::get_customer(&mut tx, id).await?;
            check_put(caller, hq, old.as_ref(), customer)?;
            let written = cust::put_customer(&mut tx, customer, actor, now).await?;
            let stored = cust::get_customer(&mut tx, id)
                .await?
                .ok_or(CustomerDbError::NotFound("customer"))?;
            tx.commit().await?;
            Ok((written, stored))
        })
        .await;
        match result {
            Err(Fail::Db(CustomerDbError::CodeTaken { .. }))
                if sent_code.is_none() && attempt < CODE_ATTEMPTS => {}
            Err(Fail::Db(CustomerDbError::CodeTaken { .. })) if sent_code.is_none() => {
                return Err(ApiError::internal());
            }
            other => break other?,
        }
    };
    publish_changed(&state, written, id);
    let names = Names::load(&state, &session).await?;
    Ok((written_status(written), Json(names.dto(&stored))))
}

/// `GET /api/v1/customers/{id}`.
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied` (archived without `customer.manage`),
/// `404 not_found`.
#[utoipa::path(get, path = "/api/v1/customers/{id}", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Customer ID")),
    responses((status = 200, body = CustomerDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn get(
    State(state): State<AppState>,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<CustomerDto>, ApiError> {
    let id: Uuid = parse_id(&id, "id")?;
    let record = cust::get_customer(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("customer"))?;
    if !record.is_active() {
        Caller::load(&state.db, &session)
            .await?
            .require_anywhere(Permission::CustomerManage)?;
    }
    Ok(Json(Names::load(&state, &session).await?.dto(&record)))
}

/// Query of `GET /customers/lookup`: exactly one of `phone` and `code`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct LookupQuery {
    /// Phone in any accepted form (`0812-3456-789`).
    pub phone: Option<String>,
    /// Member code, typed or scanned (`k7qh-29xm`, dash optional).
    pub code: Option<String>,
}

/// `GET /api/v1/customers/lookup?phone=…|code=…`: the till's lookup (spec 2.5 §6).
///
/// # Errors
/// `401` (also a device token without a session), `422 invalid_field` (neither or both given) /
/// `invalid_phone` / `invalid_code`, `404 not_found` (unknown, archived or erased).
#[utoipa::path(get, path = "/api/v1/customers/lookup", tag = "customers",
    security(("bearer" = [], "register_token" = []), ("bearer" = [])),
    params(LookupQuery),
    responses((status = 200, body = CustomerDto), (status = 404, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn lookup(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<LookupQuery>,
) -> Result<Json<CustomerDto>, ApiError> {
    let found = match (
        present(query.phone.as_deref()),
        present(query.code.as_deref()),
    ) {
        (Some(phone), None) => {
            cust::lookup_by_phone(&state.db, Phone::parse(phone)?.as_str()).await?
        }
        (None, Some(code)) => {
            cust::lookup_by_code(&state.db, CustomerCode::parse(code)?.as_str()).await?
        }
        _ => {
            return Err(ApiError::invalid_field(
                "phone",
                "give exactly one of `phone` and `code`",
            ));
        }
    };
    let record = found.ok_or_else(|| ApiError::not_found("customer"))?;
    Ok(Json(Names::load(&state, &session).await?.dto(&record)))
}

/// Query of `GET /customers`.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct ListQuery {
    /// Text to find in the name or company name (case and accents ignored). Empty: every
    /// customer by name.
    #[serde(default)]
    pub q: String,
    /// Page, from 1 (default 1).
    pub page: Option<u32>,
    /// Page size, 1–20 (default 20).
    pub limit: Option<u32>,
    /// Without `q` only: include archived customers (needs `customer.manage`).
    #[serde(default)]
    pub include_archived: bool,
}

/// A page of customers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CustomerPage {
    /// The customers, by name.
    pub items: Vec<CustomerDto>,
    /// The next page's number; `null` when this page was not full.
    pub next_page: Option<u32>,
}

/// `(offset, limit)` of a 1-based page.
fn page_window(page: Option<u32>, limit: u32) -> Result<(u32, u32), ApiError> {
    let page = page.unwrap_or(1);
    if page == 0 {
        return Err(ApiError::invalid_field("page", "`page` starts at 1"));
    }
    Ok(((page - 1).saturating_mul(limit), limit))
}

fn page_of(
    names: &Names,
    records: &[CustomerRecord],
    page: Option<u32>,
    limit: u32,
) -> CustomerPage {
    let full = records.len() == limit as usize;
    CustomerPage {
        items: records.iter().map(|r| names.dto(r)).collect(),
        next_page: full.then(|| page.unwrap_or(1) + 1),
    }
}

/// `GET /api/v1/customers?q=&page=`: name search (spec 2.5 §6) or, without `q`, the list.
///
/// # Errors
/// `401`, `403 permission_denied` (`customer.search`; `customer.manage` for
/// `include_archived`), `422 invalid_field` (`page`, `limit`).
#[utoipa::path(get, path = "/api/v1/customers", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(ListQuery),
    responses((status = 200, body = CustomerPage), (status = 403, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn list(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<Json<CustomerPage>, ApiError> {
    let caller = Caller::load(&state.db, &session).await?;
    caller.require_anywhere(Permission::CustomerSearch)?;
    let limit = query.limit.unwrap_or(SEARCH_PAGE);
    if !(1..=SEARCH_PAGE).contains(&limit) {
        return Err(ApiError::invalid_field(
            "limit",
            format!("`limit` must be 1 to {SEARCH_PAGE}"),
        ));
    }
    let (offset, limit) = page_window(query.page, limit)?;
    let records = if query.q.trim().is_empty() {
        if query.include_archived {
            caller.require_anywhere(Permission::CustomerManage)?;
        }
        cust::list_customers(&state.db, query.include_archived, offset, limit).await?
    } else {
        cust::search_customers(&state.db, &query.q, offset, limit).await?
    };
    let names = Names::load(&state, &session).await?;
    Ok(Json(page_of(&names, &records, query.page, limit)))
}

/// Query of the review lists.
#[derive(Debug, Clone, Default, Deserialize, IntoParams)]
pub struct PageQuery {
    /// Page, from 1 (default 1); 50 per page.
    pub page: Option<u32>,
}

/// `GET /api/v1/customers/duplicates`: "Possible duplicates" (spec 2.5 §7), by clashing phone.
///
/// # Errors
/// `401`, `403 permission_denied` (`customer.manage`), `422 invalid_field` (`page`).
#[utoipa::path(get, path = "/api/v1/customers/duplicates", tag = "customers",
    security(("bearer" = [])), params(PageQuery),
    responses((status = 200, body = CustomerPage), (status = 403, body = crate::openapi::Problem)))]
pub async fn duplicates(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<PageQuery>,
) -> Result<Json<CustomerPage>, ApiError> {
    review(&state, &session, query.page, false).await
}

/// `GET /api/v1/customers/negative-balances`: "Negative balances" (spec 2.5 §11), most negative
/// first, archived customers included.
///
/// # Errors
/// `401`, `403 permission_denied` (`customer.manage`), `422 invalid_field` (`page`).
#[utoipa::path(get, path = "/api/v1/customers/negative-balances", tag = "customers",
    security(("bearer" = [])), params(PageQuery),
    responses((status = 200, body = CustomerPage), (status = 403, body = crate::openapi::Problem)))]
pub async fn negative_balances(
    State(state): State<AppState>,
    session: UserSession,
    ApiQuery(query): ApiQuery<PageQuery>,
) -> Result<Json<CustomerPage>, ApiError> {
    review(&state, &session, query.page, true).await
}

async fn review(
    state: &AppState,
    session: &UserSession,
    page: Option<u32>,
    negative: bool,
) -> Result<Json<CustomerPage>, ApiError> {
    Caller::load(&state.db, session)
        .await?
        .require_anywhere(Permission::CustomerManage)?;
    let (offset, limit) = page_window(page, REVIEW_PAGE)?;
    let records = if negative {
        cust::list_negative_balances(&state.db, offset, limit).await?
    } else {
        cust::list_duplicates(&state.db, offset, limit).await?
    };
    let names = Names::load(state, session).await?;
    Ok(Json(page_of(&names, &records, page, limit)))
}

/// Which state change [`set_state`] makes.
#[derive(Clone, Copy)]
enum Change {
    Archive,
    Unarchive,
    Erase,
}

/// `POST /api/v1/customers/{id}/archive` (spec 2.5 §7): hidden from lookup and the phone check;
/// sales and the balance stay. A second archive changes nothing.
///
/// # Errors
/// `401`, `403 permission_denied` (`customer.manage`) / `read_only_replica`, `404 not_found`
/// (also an erased customer).
#[utoipa::path(post, path = "/api/v1/customers/{id}/archive", tag = "customers",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Customer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = CustomerDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem)))]
pub async fn archive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<CustomerDto>, ApiError> {
    set_state(&state, &session, &id, Change::Archive).await
}

/// `POST /api/v1/customers/{id}/unarchive` (spec 2.5 §7).
///
/// # Errors
/// `401`, `403 permission_denied` (`customer.manage`) / `read_only_replica`, `404 not_found`,
/// `409 phone_taken` (another customer took the phone meanwhile) / `merged`.
#[utoipa::path(post, path = "/api/v1/customers/{id}/unarchive", tag = "customers",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Customer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = CustomerDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn unarchive(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<CustomerDto>, ApiError> {
    set_state(&state, &session, &id, Change::Unarchive).await
}

/// `POST /api/v1/customers/{id}/erase` (spec 2.5 §7, personal-data request): blanks the personal
/// data, archives the record and sets `erased_at`. Cannot be undone; a second erase is `404`.
/// The audit entry records that it happened and who did it, not the erased values.
///
/// # Errors
/// `401`, `403 permission_denied` (`customer.erase`) / `read_only_replica`, `404 not_found`,
/// `409 balance_not_zero` (with `balance`: pay it out first).
#[utoipa::path(post, path = "/api/v1/customers/{id}/erase", tag = "customers",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "Customer ID"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    responses((status = 200, body = CustomerDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem)))]
pub async fn erase(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<CustomerDto>, ApiError> {
    set_state(&state, &session, &id, Change::Erase).await
}

async fn set_state(
    state: &AppState,
    session: &UserSession,
    id: &str,
    change: Change,
) -> Result<Json<CustomerDto>, ApiError> {
    let id: Uuid = parse_id(id, "id")?;
    let permission = match change {
        Change::Erase => Permission::CustomerErase,
        Change::Archive | Change::Unarchive => Permission::CustomerManage,
    };
    Caller::load(&state.db, session)
        .await?
        .require_anywhere(permission)?;
    let (actor, now) = (&actor(session), state.now());
    let (written, stored) = run(|| async move {
        let mut tx = state.db.begin().await?;
        let written = match change {
            Change::Archive => cust::archive_customer(&mut tx, id, actor, now).await?,
            Change::Unarchive => cust::unarchive_customer(&mut tx, id, actor, now).await?,
            Change::Erase => {
                cust::erase_customer(&mut tx, id, actor, now).await?;
                Written::Updated
            }
        };
        let stored = cust::get_customer(&mut tx, id)
            .await?
            .ok_or(CustomerDbError::NotFound("customer"))?;
        tx.commit().await?;
        Ok((written, stored))
    })
    .await?;
    publish_changed(state, written, id);
    Ok(Json(Names::load(state, session).await?.dto(&stored)))
}

/// Body of `POST /customers/{id}/merge`.
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct MergeBody {
    /// The customer merged into `{id}` (B), which is archived with `merged_into_id`.
    pub from: String,
}

/// `POST /api/v1/customers/{id}/merge {from}` (spec 2.5 §8): merges B (`from`) into A (`{id}`).
/// A keeps its fields and fills empty ones from B, B's balance moves to A, B is archived and its
/// code and phone find A from then on. Answers A.
///
/// # Errors
/// `401`, `400 invalid_body`, `403 permission_denied` (`customer.manage`) / `read_only_replica`,
/// `404 not_found`, `409 merged` (either was merged already) / `archived`, `422 invalid_field`
/// (A = B).
#[utoipa::path(post, path = "/api/v1/customers/{id}/merge", tag = "customers",
    security(("bearer" = [])),
    params(("id" = String, Path, description = "The customer that stays (A)"),
        ("Idempotency-Key" = Option<String>, Header, description = "Retry key (docs/plan.md §1)")),
    request_body = MergeBody,
    responses((status = 200, body = CustomerDto), (status = 403, body = crate::openapi::Problem),
        (status = 404, body = crate::openapi::Problem), (status = 409, body = crate::openapi::Problem),
        (status = 422, body = crate::openapi::Problem)))]
pub async fn merge(
    State(state): State<AppState>,
    _hq: RequireHq,
    session: UserSession,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<MergeBody>,
) -> Result<Json<CustomerDto>, ApiError> {
    let into: Uuid = parse_id(&id, "id")?;
    let from: Uuid = parse_id(&body.from, "from")?;
    Caller::load(&state.db, &session)
        .await?
        .require_anywhere(Permission::CustomerManage)?;
    let (state, actor, now) = (&state, &actor(&session), state.now());
    let stored = run(|| async move {
        let mut tx = state.db.begin().await?;
        let stored = cust::merge_customers(&mut tx, into, from, actor, now).await?;
        tx.commit().await?;
        Ok(stored)
    })
    .await?;
    publish_changed(state, Written::Updated, into);
    publish_changed(state, Written::Updated, from);
    Ok(Json(Names::load(state, &session).await?.dto(&stored)))
}

/// One receipt in a customer's history (filled with spec 2.6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct HistoryItemDto {
    /// The sale or return document.
    pub document_id: String,
    /// Its number.
    pub number: String,
    /// Branch.
    pub branch_id: String,
    /// When it was posted (unix ms).
    pub posted_at: i64,
    /// Total, minor units.
    pub total: i64,
}

/// A page of purchase history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct HistoryPage {
    /// Documents, newest first.
    pub items: Vec<HistoryItemDto>,
    /// The cursor of the next page; `null` at the end.
    pub next_before_id: Option<String>,
}

/// `GET /api/v1/customers/{id}/history` (spec 2.5 §9): the customer's posted sales and returns.
/// **There are no sales yet**: this answers an empty page for a known customer and fills in with
/// spec 2.6.
///
/// # Errors
/// `401`, `400 invalid_body`, `404 not_found`.
#[utoipa::path(get, path = "/api/v1/customers/{id}/history", tag = "customers",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    params(("id" = String, Path, description = "Customer ID")),
    responses((status = 200, body = HistoryPage), (status = 404, body = crate::openapi::Problem)))]
pub async fn history(
    State(state): State<AppState>,
    _session: UserSession,
    Path(id): Path<String>,
) -> Result<Json<HistoryPage>, ApiError> {
    let id: Uuid = parse_id(&id, "id")?;
    cust::get_customer(&state.db, id)
        .await?
        .ok_or_else(|| ApiError::not_found("customer"))?;
    Ok(Json(HistoryPage {
        items: Vec::new(),
        next_before_id: None,
    }))
}

/// The audit `before`/`after` of a balance change.
pub(crate) fn balance_json(balance: i64) -> Value {
    json!({ "balance": balance })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(parse_date("d", "2026-12-31").unwrap(), 20_261_231);
        assert_eq!(parse_date("d", " 2028-02-29 ").unwrap(), 20_280_229);
        for bad in [
            "2027-02-29",
            "2026-13-01",
            "2026-12",
            "0-01-01",
            "x-1-1",
            "2026-1-1-1",
        ] {
            assert_eq!(
                parse_date("d", bad).unwrap_err().code(),
                "invalid_field",
                "{bad}"
            );
        }
        assert_eq!(date_text(20_261_231), "2026-12-31");
        assert_eq!(date_text(9_990_101), "0999-01-01");
    }

    #[test]
    fn fail_conversions() {
        let api: ApiError = Fail::from(DbError::NotFound("x")).into();
        assert_eq!(api.code(), "not_found");
        let api: ApiError = Fail::from(CustomerError::Merged).into();
        assert_eq!(api.code(), "merged");
        let api: ApiError = Fail::from(ApiError::internal()).into();
        assert_eq!(api.code(), "internal");
        assert_eq!(balance_json(5), json!({ "balance": 5 }));
        let code = draw_code();
        assert!(CustomerCode::parse(code.as_str()).is_ok());
    }

    #[tokio::test]
    async fn run_gives_up_with_a_conflict() {
        let err: ApiError = run(|| async {
            Result::<(), Fail>::Err(Fail::Db(CustomerDbError::Db(DbError::Conflict(
                "race".into(),
            ))))
        })
        .await
        .unwrap_err()
        .into();
        assert_eq!((err.code(), err.detail()), ("conflict", "race"));
        let err: ApiError = run(|| async {
            Result::<(), Fail>::Err(Fail::Db(CustomerDbError::Db(DbError::sqlite_busy())))
        })
        .await
        .unwrap_err()
        .into();
        assert_eq!(err.code(), "conflict");
    }
}
