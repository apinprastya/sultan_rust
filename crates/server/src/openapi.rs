//! The `OpenAPI` 3.1 document of `/api/v1`, generated from the handlers with `utoipa`
//! (`docs/plan.md` §1) and served at `GET /api/v1/openapi.json`. The Postman collection in
//! `postman/` follows it.
//!
//! Security schemes: `bearer` (session token from `/auth/pin` or `/auth/password`) and
//! `register_token` (the `X-Register-Token` device token from pairing, spec 2.1 §2). A PIN
//! session needs both. Action `POST`s that accept `Idempotency-Key` list it as a header
//! parameter; `X-Override` (spec 2.1 §6) is listed on the endpoints protected by limits.

use axum::Json;
use serde::Serialize;
use utoipa::openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi, ToSchema};

use crate::{events, routes};

/// One failure in the `errors` list of a catalog rule problem (spec 2.2 §12).
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ProblemItem {
    /// Stable code, e.g. `invalid_check_digit`.
    pub code: String,
    /// The field, in the request body's names (`variants.sku`, `units.factor_thousandths`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Developer-facing English message.
    pub message: String,
}

/// An error body: `application/problem+json` (RFC 9457) with a stable `code` (see
/// [`ApiError`](crate::ApiError)). The optional members appear only with the codes noted on
/// them.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Problem {
    /// Always `about:blank`.
    #[serde(rename = "type")]
    pub kind: String,
    /// HTTP reason phrase.
    pub title: String,
    /// HTTP status.
    pub status: u16,
    /// Developer-facing English message.
    pub detail: String,
    /// Stable machine-readable code, e.g. `permission_denied`.
    pub code: String,
    /// `permission_denied`, `needs_override`, `override_limit_exceeded`: the permission key,
    /// e.g. `sale.discount`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<String>,
    /// `needs_override` (when the caller holds a limited grant), `override_limit_exceeded` (the
    /// approver's limit) and `permission_denied` with `reason: grant_exceeds_own`: the limit as
    /// stored, basis points (`1000` = 10%) or minor units of money.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    /// `permission_denied`: `grant_exceeds_own` when assigning more than the caller holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `locked_out`: unix ms until the lockout ends.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked_until: Option<i64>,
    /// `session_already_open`: `register` or `user`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
    /// `session_already_open`, `session_closed`: the cashier session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// `invalid_field` and catalog rule codes: the (first) field at fault.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Catalog rule problems (`422`, and `404 not_found` of a lookup): every failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ProblemItem>>,
    /// `duplicate_code` / `_sku` / `_barcode` / `_plu`: the product that already uses the value.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<String>,
    /// `duplicate_sku` / `_barcode` / `_plu`: its variant (`null` for `duplicate_code`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,
    /// `duplicate_*`: the conflicting value (a string, or the PLU number).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Object>)]
    pub value: Option<serde_json::Value>,
}

struct SecuritySchemes;

impl Modify for SecuritySchemes {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "bearer",
            SecurityScheme::Http(HttpBuilder::new().scheme(HttpAuthScheme::Bearer).build()),
        );
        components.add_security_scheme(
            "register_token",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::new("X-Register-Token"))),
        );
    }
}

/// The API description.
#[derive(OpenApi)]
#[openapi(
    info(title = "Sultan POS API", description = "REST/JSON API of a Sultan POS node \
        (docs/plan.md §1, docs/spec/2.1-organization-access.md, docs/spec/2.2-catalog.md). \
        Quantities and unit factors are integer thousandths (`*_thousandths`), money is integer \
        minor units, times are unix milliseconds."),
    paths(
        routes::auth::pin_login,
        routes::auth::password_login,
        routes::auth::logout,
        routes::registers::pair,
        routes::registers::me_users,
        routes::registers::list,
        routes::registers::get,
        routes::registers::put,
        routes::registers::archive,
        routes::registers::pairing_code,
        routes::registers::unpair,
        routes::branches::list,
        routes::branches::get,
        routes::branches::put,
        routes::branches::archive,
        routes::locations::list,
        routes::locations::get,
        routes::locations::put,
        routes::locations::archive,
        routes::users::list,
        routes::users::get,
        routes::users::put,
        routes::users::archive,
        routes::users::unlock,
        routes::roles::list,
        routes::roles::get,
        routes::roles::put,
        routes::roles::archive,
        routes::permissions::list,
        routes::audit::list,
        routes::cashier_sessions::open,
        routes::cashier_sessions::current,
        routes::cashier_sessions::get,
        routes::cashier_sessions::list,
        routes::cashier_sessions::close,
        routes::cashier_sessions::force_close,
        routes::overrides::approve,
        routes::overrides::demo_protected,
        routes::products::list,
        routes::products::get,
        routes::products::put,
        routes::products::archive,
        routes::products::unarchive,
        routes::products::get_branch_blocks,
        routes::products::put_branch_blocks,
        routes::product_images::get,
        routes::product_images::put,
        routes::product_images::delete,
        routes::taxonomy::list_categories,
        routes::taxonomy::get_category,
        routes::taxonomy::put_category,
        routes::taxonomy::list_brands,
        routes::taxonomy::get_brand,
        routes::taxonomy::put_brand,
        routes::catalog::tax_classes,
        routes::catalog::lookup,
        routes::catalog::search,
        routes::catalog::get_settings,
        routes::catalog::put_settings,
        routes::catalog::generate_product_code,
        routes::catalog::generate_barcode,
        events::events,
        openapi_json,
    ),
    components(schemas(Problem, ProblemItem)),
    modifiers(&SecuritySchemes),
    tags(
        (name = "auth", description = "Login and logout (spec 2.1 §4)"),
        (name = "registers", description = "Registers and pairing (spec 2.1 §1–2)"),
        (name = "org", description = "Branches and stock locations (spec 2.1 §1)"),
        (name = "users", description = "Users and role assignments (spec 2.1 §3)"),
        (name = "roles", description = "Roles and the permission catalog (spec 2.1 §3)"),
        (name = "audit", description = "Audit log (spec 2.1 §7)"),
        (name = "cashier", description = "Cashier sessions, numbers, blind close, force close, past close reports (spec 2.1 §5)"),
        (name = "overrides", description = "Manager override tokens and the X-Override guard (spec 2.1 §6)"),
        (name = "catalog", description = "Products, variants, units, barcodes, images, categories, brands, assortment, lookup and search (spec 2.2)"),
        (name = "events", description = "WebSocket event notifications (docs/plan.md §1)"),
        (name = "meta", description = "API description"),
    )
)]
pub struct ApiDoc;

/// `GET /api/v1/openapi.json`: this document. No authentication.
#[utoipa::path(get, path = "/api/v1/openapi.json", tag = "meta",
    responses((status = 200, description = "The OpenAPI document", content_type = "application/json")))]
pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}
