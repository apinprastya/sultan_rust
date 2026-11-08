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

/// An error body: `application/problem+json` (RFC 9457) with a stable `code` (see
/// [`ApiError`](crate::ApiError)). Some codes add members, e.g. `permission`, `field`,
/// `locked_until`.
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
        (docs/plan.md §1, docs/spec/2.1-organization-access.md)."),
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
        events::events,
        openapi_json,
    ),
    components(schemas(Problem)),
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
