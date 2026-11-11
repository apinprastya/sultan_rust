//! What the register needs from the server, as one trait (plan Phase 8, spec 2.1 §2, §4–§6).
//!
//! - [`Service`]: list the lock-screen users, PIN login and logout, the branch name, the current /
//!   open / close cashier session, past sessions and their stored close reports (Phase 9; the
//!   source of a later reprint), manager overrides, the demo endpoint protected by
//!   `X-Override`, and the events WebSocket. The register UI (`crates/ui`) depends on this trait
//!   only, so every screen flow can be tested with [`fake::FakeService`].
//! - [`http::HttpService`]: the real implementation over `reqwest` (device token in
//!   `X-Register-Token`, session token as `Authorization: Bearer`, `Idempotency-Key` on the cashier
//!   session `POST`s) and `tokio-tungstenite` for `GET /api/v1/events`.
//! - The register's barcode field resolves codes with [`Service::lookup`]
//!   (`GET /catalog/lookup`, phase C5); its answer is [`Lookup`].
//! - [`ClientError`]: problem+json answers become [`Problem`], which keeps the stable `code` and
//!   the extension members (`locked_until`, `permission`, `limit`, `conflict`, `session_id`, …).
//!
//! Every method returns a `'static` future: implementations copy what they need before the
//! future starts, so the UI can spawn it or block on it. The fake does all its work when the
//! method is called and returns a ready future, which keeps UI tests deterministic.
//!
//! - [`BackOffice`] extends [`Service`] with what the back-office window needs (phase C4, spec 2.2
//!   §12): password login, `GET /auth/me`, branches, and the catalog (products, categories,
//!   brands, tax classes, settings, generators, branch blocks, images, paged search, and the
//!   hard delete of never-used products, categories and brands, C3b). Its types are in
//!   [`catalog`]. A back-office [`HttpService`] ([`HttpService::user`]) has no device token.
//!
//! Amounts are minor units (`i64`, Rupiah minor unit = 1, `docs/plan.md` §1); times are unix ms.

pub mod catalog;
pub mod error;
pub mod fake;
pub mod http;
pub mod types;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use catalog::{
    Brand, BrandBody, CatalogSettings, Category, CategoryBody, CategoryNode, Generate, ImageInfo,
    Lookup, Me, Product, ProductPage, ProductQuery, ProductSummary, SearchHit, SearchPage,
    TaxClass,
};
pub use error::{ClientError, Duplicate, FieldError, Problem};
pub use http::{HttpService, api_base};
pub use types::{
    Branch, CashierSession, CloseReport, CloseRequest, Count, LockUser, Login, OverrideGrant,
    OverrideRequest, ProtectedAction, ProtectedResult, ServerEvent, SessionDetail, SessionPage,
    SessionQuery, TenderTotal,
};

/// A boxed, sendable future of a service call.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = Result<T, ClientError>> + Send + 'static>>;

/// Receives the events of [`Service::watch_events`].
pub type EventSink = Arc<dyn Fn(ServerEvent) + Send + Sync>;

/// The register's view of the server. `token` is the PIN session token of [`Login`].
pub trait Service: Send + Sync {
    /// `GET /registers/me/users`: the lock-screen tiles.
    fn lock_users(&self) -> BoxFuture<Vec<LockUser>>;

    /// `POST /auth/pin`.
    fn pin_login(&self, user_id: &str, pin: &str) -> BoxFuture<Login>;

    /// `POST /auth/logout`.
    fn logout(&self, token: &str) -> BoxFuture<()>;

    /// `GET /branches/{id}`: the branch name for the header.
    fn branch(&self, token: &str, branch_id: &str) -> BoxFuture<Branch>;

    /// `GET /cashier-sessions/current`: the open session on this register (anyone's), or `None`.
    fn current_session(&self, token: &str) -> BoxFuture<Option<CashierSession>>;

    /// `POST /cashier-sessions {opening_cash}` with `Idempotency-Key: key`.
    fn open_session(&self, token: &str, opening_cash: i64, key: &str) -> BoxFuture<CashierSession>;

    /// `POST /cashier-sessions/{id}/close` (blind close) with `Idempotency-Key: key`.
    fn close_session(
        &self,
        token: &str,
        session_id: &str,
        body: &CloseRequest,
        key: &str,
    ) -> BoxFuture<CloseReport>;

    /// `GET /cashier-sessions/{id}`: a session and, once closed, its stored close report. The
    /// caller's own sessions, or any at a branch where they hold `report.sales`.
    fn session(&self, token: &str, session_id: &str) -> BoxFuture<SessionDetail>;

    /// `GET /cashier-sessions?…`: the sessions the caller may read, newest first, one page.
    fn sessions(&self, token: &str, query: &SessionQuery) -> BoxFuture<SessionPage>;

    /// `POST /overrides`: a manager approves; the answer carries the single-use token.
    fn request_override(&self, token: &str, body: &OverrideRequest) -> BoxFuture<OverrideGrant>;

    /// `POST /demo/override-protected`, with `X-Override` when `override_token` is given.
    fn demo_protected(
        &self,
        token: &str,
        body: &ProtectedAction,
        override_token: Option<&str>,
    ) -> BoxFuture<ProtectedResult>;

    /// `GET /events` (WebSocket): calls `sink` for each event until the socket closes. An empty
    /// `token` connects with the device token alone (C3b): only company-wide events such as
    /// `catalog.changed` arrive then.
    fn watch_events(&self, token: &str, sink: EventSink) -> BoxFuture<()>;

    /// `GET /catalog/lookup?code=`: resolves a scanned or typed code (spec 2.2 §6). The
    /// assortment is the register's branch (from the device token). Errors: `404 not_found`,
    /// `422 archived` / `not_sold_here` / `not_sellable` / `scale_value_zero`.
    fn lookup(&self, token: &str, code: &str) -> BoxFuture<Lookup>;
}

/// What the back office needs from the server (phase C4, spec 2.2 §12). `token` is the password
/// session of [`BackOffice::password_login`]; [`Service::logout`] and [`Service::watch_events`]
/// work with it too.
pub trait BackOffice: Service {
    /// `POST /auth/password`.
    fn password_login(&self, username: &str, password: &str) -> BoxFuture<Login>;

    /// `GET /auth/me`: the user and their permission keys.
    fn me(&self, token: &str) -> BoxFuture<Me>;

    /// `GET /branches`: the branches the user may see (for the branch-block checklist).
    fn branches(&self, token: &str) -> BoxFuture<Vec<Branch>>;

    /// `GET /products?…`: one page.
    fn products(&self, token: &str, query: &ProductQuery) -> BoxFuture<ProductPage>;

    /// `GET /products/{id}`.
    fn product(&self, token: &str, id: &str) -> BoxFuture<Product>;

    /// `PUT /products/{id}`: create or replace the whole document.
    fn put_product(&self, token: &str, id: &str, body: &Product) -> BoxFuture<Product>;

    /// `POST /products/{id}/archive` (or `/unarchive`) with `Idempotency-Key: key`.
    fn set_archived(&self, token: &str, id: &str, archived: bool, key: &str) -> BoxFuture<Product>;

    /// `GET /categories[?include_archived=true]`: the tree.
    fn categories(&self, token: &str, include_archived: bool) -> BoxFuture<Vec<CategoryNode>>;

    /// `PUT /categories/{id}`.
    fn put_category(&self, token: &str, id: &str, body: &CategoryBody) -> BoxFuture<Category>;

    /// `GET /brands[?include_archived=true]`.
    fn brands(&self, token: &str, include_archived: bool) -> BoxFuture<Vec<Brand>>;

    /// `PUT /brands/{id}`.
    fn put_brand(&self, token: &str, id: &str, body: &BrandBody) -> BoxFuture<Brand>;

    /// `GET /tax-classes`.
    fn tax_classes(&self, token: &str) -> BoxFuture<Vec<TaxClass>>;

    /// `GET /settings/catalog`.
    fn catalog_settings(&self, token: &str) -> BoxFuture<CatalogSettings>;

    /// `POST /catalog/generate/{product-code|barcode}` with `Idempotency-Key: key`: the value.
    fn generate(&self, token: &str, what: Generate, key: &str) -> BoxFuture<String>;

    /// `GET /products/{id}/branch-blocks`: the blocked branch IDs.
    fn branch_blocks(&self, token: &str, id: &str) -> BoxFuture<Vec<String>>;

    /// `PUT /products/{id}/branch-blocks`: replaces the set; answers the stored set.
    fn put_branch_blocks(
        &self,
        token: &str,
        id: &str,
        branch_ids: &[String],
    ) -> BoxFuture<Vec<String>>;

    /// `GET /products/{id}/image`: the JPEG bytes, or `None` without an image.
    fn product_image(&self, token: &str, id: &str) -> BoxFuture<Option<Vec<u8>>>;

    /// `PUT /products/{id}/image` (PNG, JPEG or WebP bytes; the server resizes).
    fn put_product_image(&self, token: &str, id: &str, bytes: Vec<u8>) -> BoxFuture<ImageInfo>;

    /// `DELETE /products/{id}/image`.
    fn delete_product_image(&self, token: &str, id: &str) -> BoxFuture<()>;

    /// `GET /catalog/search?q=&limit=&after_variant_id=`: one page of sellable variants (the
    /// bundle component picker); `after` is the previous page's `next_after_variant_id`.
    fn search(
        &self,
        token: &str,
        q: &str,
        limit: u32,
        after: Option<&str>,
    ) -> BoxFuture<SearchPage>;

    /// `DELETE /products/{id}`: hard-deletes a never-used product (spec 2.2 §9); a used one is
    /// `409 in_use` (archive it instead).
    fn delete_product(&self, token: &str, id: &str) -> BoxFuture<()>;

    /// `DELETE /categories/{id}`: a category without products or children; else `409 in_use`.
    fn delete_category(&self, token: &str, id: &str) -> BoxFuture<()>;

    /// `DELETE /brands/{id}`: a brand without products; else `409 in_use`.
    fn delete_brand(&self, token: &str, id: &str) -> BoxFuture<()>;
}
