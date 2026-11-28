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
//! Phase M3 (spec 2.5) adds customers: `/customers` (sign-up and edit, lookup, name search,
//! archive, merge, erase, duplicates, negative balances, history), `/customer-groups`,
//! `/membership-tiers`, the customer balance (`/customers/{id}/balance`, cash payout) and
//! `/settings/customer-balance`, with `customer.changed` events ([`routes::customers`]).
//! Phase R3 (spec 2.3) adds pricing: `/price-lists` with their entries, `/products/{id}/prices`,
//! `/tax-classes/{id}/rates`, `/settings/pricing`, `POST /pricing/quote` and the price in
//! `/catalog/lookup`, with `price.changed` events ([`routes::pricing`],
//! [`routes::pricing_quote`]).
//!
//! Phase K3 (spec 2.7) adds cash management: `/cash-reasons`, `/cash-movements` (cash in / out /
//! safe drop with slips), the payout slip, the X report and the close report text of a cashier
//! session, the proposed float (`/registers/me/float`), the float kept at a close and `/z-reports`
//! (end of day per branch, days still to close), with `cash.moved` and `day.closed` events
//! ([`routes::cash`]).
//!
//! Phase K6 (spec 2.7 §2a) adds the nota photos of a cash movement
//! (`/cash-movements/{id}/photos/{photo_id}`, [`routes::cash_photos`]), stored as files in the
//! node's data directory ([`attachments`]), and `nota_flag_above` in `/settings/sales`.
//!
//! Phase N3 (spec 2.8) adds inventory: `/stock/on-hand`, `/stock/card/{variant_id}`,
//! `/stock/lots`, `/settings/inventory`, `/suppliers` with their items, `/stock-reasons`,
//! `/purchase-orders`, `/goods-receipts`, `/supplier-returns`, `/transfers`,
//! `/internal-transfers`, `/adjustments`, `/opening-stock`, `/opnames`, `/reorder-rules`,
//! `/reorder/suggestions`, `/reorder/low-stock`, `/payables` and the till's
//! `/registers/me/stock/{variant_id}`, with `stock.changed` and `transfer.changed` events
//! ([`routes::stock`]).
//!
//! Phase 7 adds request logging to [`router`]: one `tracing` span per request with the method and
//! the path only, and the status and latency when it finishes. Header values (`Authorization`,
//! `X-Register-Token`, `X-Override`), query strings and bodies are never logged.

pub mod access;
pub mod attachments;
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
use axum::routing::{get, post, put};
use tower_http::trace::{DefaultOnRequest, DefaultOnResponse, TraceLayer};
use tracing::Level;

use routes::{
    audit, auth, branches, cash_movements, cash_photos, cash_reasons, cash_reports,
    cashier_sessions, catalog, customer_balance, customer_groups, customers, drawer, held_carts,
    locations, overrides as override_routes, permissions, pricing, pricing_quote, product_images,
    products, purchasing, registers, returns, roles, sales, sales_settings, stock, stock_docs,
    suppliers, taxonomy, tender_types, users, z_reports,
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
        .merge(customer_actions())
        .merge(pricing_actions())
        .merge(sales_actions())
        .route("/z-reports", post(z_reports::post))
        .merge(stock_actions())
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
        .merge(customer_routes())
        .merge(pricing_routes())
        .merge(sales_routes())
        .merge(cash_routes())
        .merge(stock_routes())
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

/// The inventory action `POST`s (spec 2.8 §16); they get the `Idempotency-Key` middleware.
fn stock_actions() -> Router<AppState> {
    Router::new()
        .route("/purchase-orders/{id}/order", post(purchasing::order))
        .route("/purchase-orders/{id}/close", post(purchasing::close))
        .route("/purchase-orders/{id}/cancel", post(purchasing::cancel))
        .route("/goods-receipts/{id}/post", post(purchasing::post_receipt))
        .route("/goods-receipts/{id}/void", post(purchasing::void_receipt))
        .route("/supplier-returns/{id}/post", post(purchasing::post_return))
        .route("/transfers/{id}/ship", post(stock_docs::ship))
        .route("/transfers/{id}/receive", post(stock_docs::receive))
        .route("/transfers/{id}/cancel", post(stock_docs::cancel_transfer))
        .route("/opnames/{id}/counts", post(stock_docs::counts))
        .route("/opnames/{id}/post", post(stock_docs::post_opname))
        .route("/opnames/{id}/cancel", post(stock_docs::cancel_opname))
}

/// The inventory resources (spec 2.8 §16).
fn stock_routes() -> Router<AppState> {
    Router::new()
        .route("/stock/on-hand", get(stock::on_hand))
        .route("/stock/card/{variant_id}", get(stock::card))
        .route("/stock/lots", get(stock::lots))
        .route(
            "/settings/inventory",
            get(stock::get_settings).put(stock::put_settings),
        )
        .route("/payables", get(stock::payables))
        .route("/registers/me/stock/{variant_id}", get(stock::till_stock))
        .route("/suppliers", get(suppliers::list))
        .route(
            "/suppliers/{id}",
            get(suppliers::get)
                .put(suppliers::put)
                .delete(suppliers::delete),
        )
        .route("/suppliers/{id}/items", get(suppliers::list_items))
        .route(
            "/suppliers/{id}/items/{variant_id}",
            get(suppliers::get_item).put(suppliers::put_item),
        )
        .route("/stock-reasons", get(suppliers::list_reasons))
        .route(
            "/stock-reasons/{id}",
            put(suppliers::put_reason).delete(suppliers::delete_reason),
        )
        .route("/purchase-orders", get(purchasing::list_orders))
        .route(
            "/purchase-orders/{id}",
            get(purchasing::get_order).put(purchasing::put_order),
        )
        .route("/goods-receipts", get(purchasing::list_receipts))
        .route(
            "/goods-receipts/{id}",
            get(purchasing::get_receipt).put(purchasing::put_receipt),
        )
        .route("/supplier-returns", get(purchasing::list_returns))
        .route(
            "/supplier-returns/{id}",
            get(purchasing::get_return).put(purchasing::put_return),
        )
        .route("/reorder-rules", get(purchasing::list_rules))
        .route(
            "/reorder-rules/{branch_id}/{variant_id}",
            put(purchasing::put_rule),
        )
        .route("/reorder/suggestions", get(purchasing::suggestions))
        .route("/reorder/low-stock", get(purchasing::low_stock))
        .route("/transfers", get(stock_docs::list_transfers))
        .route(
            "/transfers/{id}",
            get(stock_docs::get_transfer).put(stock_docs::put_transfer),
        )
        .route("/internal-transfers", get(stock_docs::list_internal))
        .route(
            "/internal-transfers/{id}",
            get(stock_docs::get_internal).put(stock_docs::put_internal),
        )
        .route("/adjustments", get(stock_docs::list_adjustments))
        .route(
            "/adjustments/{id}",
            get(stock_docs::get_adjustment).put(stock_docs::put_adjustment),
        )
        .route("/opening-stock", get(stock_docs::list_opening))
        .route(
            "/opening-stock/{id}",
            get(stock_docs::get_opening).put(stock_docs::put_opening),
        )
        .route("/opnames", get(stock_docs::list_opnames))
        .route(
            "/opnames/{id}",
            get(stock_docs::get_opname).put(stock_docs::put_opname),
        )
}

/// The customer action `POST`s (spec 2.5 §13); they get the `Idempotency-Key` middleware.
fn customer_actions() -> Router<AppState> {
    Router::new()
        .route("/customers/{id}/archive", post(customers::archive))
        .route("/customers/{id}/unarchive", post(customers::unarchive))
        .route("/customers/{id}/merge", post(customers::merge))
        .route("/customers/{id}/erase", post(customers::erase))
        .route(
            "/customers/{id}/balance/payout",
            post(customer_balance::payout),
        )
}

/// The customer resources (spec 2.5 §13).
fn customer_routes() -> Router<AppState> {
    Router::new()
        .route("/customers", get(customers::list))
        .route("/customers/lookup", get(customers::lookup))
        .route("/customers/duplicates", get(customers::duplicates))
        .route(
            "/customers/negative-balances",
            get(customers::negative_balances),
        )
        .route("/customers/{id}", get(customers::get).put(customers::put))
        .route("/customers/{id}/history", get(customers::history))
        .route("/customers/{id}/balance", get(customer_balance::get))
        .route("/customer-groups", get(customer_groups::list_groups))
        .route(
            "/customer-groups/{id}",
            get(customer_groups::get_group).put(customer_groups::put_group),
        )
        .route("/membership-tiers", get(customer_groups::list_tiers))
        .route(
            "/membership-tiers/{id}",
            get(customer_groups::get_tier).put(customer_groups::put_tier),
        )
        .route(
            "/settings/customer-balance",
            get(customer_balance::get_settings).put(customer_balance::put_settings),
        )
}

/// The pricing action `POST`s (spec 2.3 §10); they get the `Idempotency-Key` middleware.
fn pricing_actions() -> Router<AppState> {
    Router::new().route("/price-lists/{id}/archive", post(pricing::archive_list))
}

/// The pricing resources and the quote (spec 2.3 §10). The quote is a read: it stores nothing,
/// so it takes no `Idempotency-Key`.
fn pricing_routes() -> Router<AppState> {
    Router::new()
        .route("/price-lists", get(pricing::list_lists))
        .route(
            "/price-lists/{id}",
            get(pricing::get_list).put(pricing::put_list),
        )
        .route("/price-lists/{id}/entries", get(pricing::list_entries))
        .route(
            "/price-lists/{id}/entries/{entry_id}",
            get(pricing::get_entry)
                .put(pricing::put_entry)
                .delete(pricing::delete_entry),
        )
        .route("/products/{id}/prices", get(pricing::product_prices))
        .route(
            "/tax-classes/{id}/rates",
            get(pricing::list_rates).put(pricing::put_rate),
        )
        .route(
            "/settings/pricing",
            get(pricing::get_settings).put(pricing::put_settings),
        )
        .route("/pricing/quote", post(pricing_quote::quote))
}

/// The span of one request: method and path only. The query string is left out because it is
/// the one part of the URI a client could put a secret in; headers and bodies are never recorded.
fn sales_actions() -> Router<AppState> {
    Router::new()
        .route("/sales/{id}/void", post(sales::void))
        .route("/cart-voids", post(sales::cart_void))
        .route("/held-carts/{id}/take", post(held_carts::take))
        .route("/drawer/open", post(drawer::open))
}

fn sales_routes() -> Router<AppState> {
    Router::new()
        .route("/tender-types", get(tender_types::list))
        .route(
            "/tender-types/{id}",
            put(tender_types::put).delete(tender_types::delete),
        )
        .route("/sales", get(sales::list))
        .route("/sales/{id}", get(sales::get).put(sales::put))
        .route("/sales/by-number/{number}", get(sales::by_number))
        .route("/sales/{id}/receipt", get(sales::get_receipt))
        .route("/returns", get(returns::list))
        .route("/returns/{id}", get(returns::get).put(returns::put))
        .route("/returns/{id}/receipt", get(returns::get_receipt))
        .route(
            "/settings/sales",
            get(sales_settings::get).put(sales_settings::put),
        )
        .route("/held-carts", get(held_carts::list))
        .route(
            "/held-carts/{id}",
            put(held_carts::put).delete(held_carts::delete),
        )
        .route(
            "/customers/{id}/recent-receipts",
            get(customers::recent_receipts),
        )
}

/// The cash resources of spec 2.7 §11 (phase K3); `POST /z-reports` is with the idempotent
/// actions.
fn cash_routes() -> Router<AppState> {
    Router::new()
        .route("/cash-reasons", get(cash_reasons::list))
        .route(
            "/cash-reasons/{id}",
            put(cash_reasons::put).delete(cash_reasons::delete),
        )
        .route("/cash-movements", get(cash_movements::list))
        .route(
            "/cash-movements/{id}",
            get(cash_movements::get).put(cash_movements::put),
        )
        .route("/cash-movements/{id}/slip", get(cash_movements::slip))
        .route(
            "/cash-movements/{id}/photos/{photo_id}",
            get(cash_photos::get).put(cash_photos::put),
        )
        .route(
            "/customers/{id}/balance/payouts/{entry_id}/slip",
            get(cash_movements::payout_slip),
        )
        .route(
            "/cashier-sessions/{id}/x-report",
            get(cash_reports::x_report_json),
        )
        .route(
            "/cashier-sessions/{id}/x-report/text",
            get(cash_reports::x_report_text),
        )
        .route(
            "/cashier-sessions/{id}/report/text",
            get(cash_reports::close_report_text),
        )
        .route("/registers/me/float", get(cash_reports::proposed_float))
        .route("/z-reports", get(z_reports::list))
        .route("/z-reports/pending", get(z_reports::pending))
        .route("/z-reports/{id}", get(z_reports::get))
        .route("/z-reports/{id}/text", get(z_reports::text))
}

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
