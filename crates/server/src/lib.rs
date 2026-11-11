//! Sultan POS server: REST/JSON API under `/api/v1` (axum) plus WebSocket event notifications
//! (`docs/plan.md` §1, `docs/spec/2.1-organization-access.md`).
//!
//! Phase 4 provides the skeleton and authentication:
//! - [`AppState`] (database, node role, clock) and [`ApiError`] (problem+json with a stable code).
//! - Extractors [`RegisterCtx`](extract::RegisterCtx), [`UserSession`](extract::UserSession),
//!   [`RequireHq`](extract::RequireHq).
//! - Endpoints (spec 2.1 §8): `POST /registers/pair`, `GET /registers/me`,
//!   `GET /registers/me/users`, `POST /auth/pin`, `POST /auth/password`, `POST /auth/logout`, `POST /users/{id}/unlock`.
//!
//! Tokens are 32 random bytes (base64url); only their SHA-256 is stored ([`token`]).
//!
//! Phase 5 adds master data and administration (spec 2.1 §8): `/branches`, `/registers`,
//! `/locations`, `/users`, `/roles` (list/get, idempotent `PUT /{resource}/{uuid}`, `DELETE` =
//! archive), `POST /registers/{id}/pairing-code`, `POST /registers/{id}/unpair`,
//! `/permissions`, `/audit` and the `OpenAPI` document at `/openapi.json` ([`openapi`]).
//! Permission checks and branch visibility: [`access`].
//!
//! Phase 6 adds cashier sessions (`/cashier-sessions`, blind close, force close; spec 2.1 §5),
//! manager overrides (`POST /overrides` and the `X-Override` guard [`overrides`]; §6), the events
//! WebSocket `GET /events` ([`events`]), `Idempotency-Key` on action `POST`s ([`idempotency`];
//! §9 case 17) and the sales seam of the close report ([`movements`]).
//!
//! Phase C3 (spec 2.2) adds the catalog: `/products`, `/products/{id}/branch-blocks`,
//! `/products/{id}/image` ([`product_image`]), `/categories`, `/brands`, `/tax-classes`,
//! `/catalog/lookup`, `/catalog/search`, `/settings/catalog` and `/catalog/generate/…`, with
//! `catalog.changed` events ([`routes::catalog`]).
//!
//! Phase 7 adds request logging to [`router`]: one `tracing` span per request with the method and
//! the path only, and the status and latency when it finishes. Header values (`Authorization`,
//! `X-Register-Token`, `X-Override`), query strings and bodies are never logged.

pub mod access;
pub mod credentials;
pub mod error;
pub mod events;
pub mod extract;
pub mod idempotency;
pub mod movements;
pub mod openapi;
pub mod overrides;
pub mod product_image;
pub mod retry;
pub mod routes;
pub mod state;
pub mod token;

use axum::Router;
use axum::http::Request;
use axum::middleware::from_fn_with_state;
use axum::routing::{get, post};
use tower_http::trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use routes::{
    audit, auth, branches, cashier_sessions, catalog, locations, overrides as override_routes,
    permissions, product_images, products, registers, roles, taxonomy, users,
};

pub use error::ApiError;
pub use state::{AppState, Clock, ManualClock, NodeRole, SystemClock};

/// Crate name, used by the binary's version banner.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// The API routes; `state` is needed by the `Idempotency-Key` middleware ([`idempotency`]), which
/// wraps the action `POST`s whose responses carry no secret. Unknown paths are `404 not_found`
/// and wrong methods are `405 method_not_allowed`, both as problem+json.
pub fn routes(state: &AppState) -> Router<AppState> {
    let idempotent = Router::new()
        .route("/auth/logout", post(auth::logout))
        .route("/registers/{id}/unpair", post(registers::unpair))
        .route("/users/{id}/unlock", post(users::unlock))
        .route("/cashier-sessions", post(cashier_sessions::open))
        .route(
            "/cashier-sessions/{id}/close",
            post(cashier_sessions::close),
        )
        .route(
            "/cashier-sessions/{id}/force-close",
            post(cashier_sessions::force_close),
        )
        .route(
            "/demo/override-protected",
            post(override_routes::demo_protected),
        )
        .merge(catalog_actions())
        .route_layer(from_fn_with_state(state.clone(), idempotency::middleware));
    let api = Router::new()
        .route("/openapi.json", get(openapi::openapi_json))
        .route("/events", get(events::events))
        .route("/auth/pin", post(auth::pin_login))
        .route("/auth/password", post(auth::password_login))
        .route("/auth/me", get(auth::me))
        .route("/registers/pair", post(registers::pair))
        .route("/registers/me", get(registers::me))
        .route("/registers/me/users", get(registers::me_users))
        .route("/registers", get(registers::list))
        .route(
            "/registers/{id}",
            get(registers::get)
                .put(registers::put)
                .delete(registers::archive),
        )
        .route(
            "/registers/{id}/pairing-code",
            post(registers::pairing_code),
        )
        .route("/branches", get(branches::list))
        .route(
            "/branches/{id}",
            get(branches::get)
                .put(branches::put)
                .delete(branches::archive),
        )
        .route("/locations", get(locations::list))
        .route(
            "/locations/{id}",
            get(locations::get)
                .put(locations::put)
                .delete(locations::archive),
        )
        .route("/users", get(users::list))
        .route(
            "/users/{id}",
            get(users::get).put(users::put).delete(users::archive),
        )
        .route("/roles", get(roles::list))
        .route(
            "/roles/{id}",
            get(roles::get).put(roles::put).delete(roles::archive),
        )
        .route("/permissions", get(permissions::list))
        .route("/audit", get(audit::list))
        .route("/cashier-sessions", get(cashier_sessions::list))
        .route("/cashier-sessions/current", get(cashier_sessions::current))
        .route("/cashier-sessions/{id}", get(cashier_sessions::get))
        .route("/overrides", post(override_routes::approve))
        .merge(catalog_routes())
        .merge(idempotent)
        .method_not_allowed_fallback(async || ApiError::method_not_allowed());
    Router::new()
        .nest("/api/v1", api)
        .fallback(async || ApiError::not_found("resource"))
}

/// The catalog's action `POST`s (spec 2.2 §12); they get the `Idempotency-Key` middleware.
fn catalog_actions() -> Router<AppState> {
    Router::new()
        .route("/products/{id}/archive", post(products::archive))
        .route("/products/{id}/unarchive", post(products::unarchive))
        .route(
            "/catalog/generate/product-code",
            post(catalog::generate_product_code),
        )
        .route("/catalog/generate/barcode", post(catalog::generate_barcode))
}

/// The catalog's resources (spec 2.2 §12).
fn catalog_routes() -> Router<AppState> {
    Router::new()
        .route("/products", get(products::list))
        .route(
            "/products/{id}",
            get(products::get)
                .put(products::put)
                .delete(products::delete),
        )
        .route(
            "/products/{id}/branch-blocks",
            get(products::get_branch_blocks).put(products::put_branch_blocks),
        )
        .route(
            "/products/{id}/image",
            get(product_images::get)
                .put(product_images::put)
                .delete(product_images::delete),
        )
        .route("/categories", get(taxonomy::list_categories))
        .route(
            "/categories/{id}",
            get(taxonomy::get_category)
                .put(taxonomy::put_category)
                .delete(taxonomy::delete_category),
        )
        .route("/brands", get(taxonomy::list_brands))
        .route(
            "/brands/{id}",
            get(taxonomy::get_brand)
                .put(taxonomy::put_brand)
                .delete(taxonomy::delete_brand),
        )
        .route("/tax-classes", get(catalog::tax_classes))
        .route("/catalog/lookup", get(catalog::lookup))
        .route("/catalog/search", get(catalog::search))
        .route(
            "/settings/catalog",
            get(catalog::get_settings).put(catalog::put_settings),
        )
}

/// The span of one request: method and path only. The query string is left out because it is
/// the one part of the URI a client could put a secret in; headers and bodies are never recorded.
fn request_span<B>(request: &Request<B>) -> tracing::Span {
    tracing::info_span!("request", method = %request.method(), path = %request.uri().path())
}

/// The complete application, with request logging (see the module docs).
pub fn router(state: AppState) -> Router {
    routes(&state).with_state(state).layer(
        TraceLayer::new_for_http()
            .make_span_with(request_span)
            .on_request(DefaultOnRequest::new().level(Level::DEBUG))
            .on_response(DefaultOnResponse::new().level(Level::INFO)),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::http::Request;
    use sultan_db::Db;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn crate_name_is_set() {
        assert_eq!(super::CRATE_NAME, "sultan-server");
    }

    /// Collects formatted log output.
    #[derive(Clone, Default)]
    struct Captured(Arc<Mutex<Vec<u8>>>);

    impl Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn request_log_has_method_path_and_status_but_no_secrets() {
        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let db = Db::connect("sqlite::memory:").await.unwrap();
        db.migrate().await.unwrap();
        let state = AppState::new(db, NodeRole::Standalone, Arc::new(SystemClock));
        let request = Request::post("/api/v1/auth/password?token=SECRET-QUERY")
            .header("authorization", "Bearer SECRET-BEARER")
            .header("x-register-token", "SECRET-DEVICE")
            .header("x-override", "SECRET-OVERRIDE")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"username":"nobody","password":"SECRET-PASSWORD"}"#,
            ))
            .unwrap();
        let response = router(state).oneshot(request).await.unwrap();
        assert_eq!(response.status().as_u16(), 401);

        let log = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(log.contains("method=POST"), "{log}");
        assert!(log.contains("path=/api/v1/auth/password"), "{log}");
        assert!(log.contains("status=401"), "{log}");
        assert!(!log.contains("SECRET"), "{log}");
    }
}
