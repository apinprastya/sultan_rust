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
//! - Customers (spec 2.5, phase M3; types in [`customers`]): the till's operations are on
//!   [`Service`] (lookup by phone or code, name search, sign-up, the balance and its cash
//!   payout); the back office's on [`BackOffice`] (get, edit, archive, merge, erase, duplicates,
//!   negative balances, groups, tiers, balance settings). Name search and the balance view serve
//!   both. [`Service::permissions`] (`GET /auth/me`) tells the register what to show (M5).
//! - Pricing (phase R3, spec 2.3): [`Service::quote`] prices a cart (`POST /pricing/quote`) and
//!   [`Lookup::unit_price`] is the store price of one unit; [`BackOffice`] reads and writes price
//!   lists, entries, tax rates and the pricing settings. Types in [`pricing`].
//!
//! Amounts are minor units (`i64`, Rupiah minor unit = 1, `docs/plan.md` §1); times are unix ms.

pub mod catalog;
pub mod customers;
pub mod error;
pub mod fake;
pub mod http;
pub mod pricing;
pub mod types;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use catalog::{
    Brand, BrandBody, CatalogSettings, Category, CategoryBody, CategoryNode, Generate, ImageInfo,
    Lookup, Me, Product, ProductPage, ProductQuery, ProductSummary, SearchHit, SearchPage,
    TaxClass,
};
pub use customers::{
    BalanceEntry, BalanceSettings, Customer, CustomerBalance, CustomerBody, CustomerGroup,
    CustomerPage, CustomerQuery, GroupBody, MembershipTier, Payout, TierBody,
};
pub use error::{ClientError, Duplicate, FieldError, Problem};
pub use http::{HttpService, api_base};
pub use pricing::{
    EntryPage, LineError, LineTax, PriceEntry, PriceEntryBody, PriceList, PriceListBody, PriceRule,
    PriceSource, PriceTier, PricingSettings, ProductPrices, Quote, QuoteLine, QuoteRequest,
    QuoteTotals, QuotedLine, TaxRate,
};
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

    /// `GET /customers/lookup?phone=`: the customer with this phone (any accepted form; spec 2.5
    /// §6). A merged customer's phone finds the survivor. Errors: `404 not_found`,
    /// `422 invalid_phone`.
    fn customer_by_phone(&self, token: &str, phone: &str) -> BoxFuture<Customer>;

    /// `GET /customers/lookup?code=`: the customer with this member code, typed or scanned
    /// (`k7qh-29xm`, dash optional). Errors: `404 not_found`, `422 invalid_code`.
    fn customer_by_code(&self, token: &str, code: &str) -> BoxFuture<Customer>;

    /// `GET /customers?q=&page=`: name search (`customer.search`), at most 20 per page; without
    /// `q` the list of every customer by name (the back office's list).
    fn search_customers(&self, token: &str, query: &CustomerQuery) -> BoxFuture<CustomerPage>;

    /// `PUT /customers/{id}`: sign-up at the till (`customer.create`, works on a branch node). A
    /// retry with the same `id` and body answers the same customer. Errors: `409 phone_taken`
    /// (with [`Problem::customer_code`] and [`Problem::masked_phone`]), `422 invalid_phone` /
    /// `invalid_tax_id` / `invalid_field`, `403 permission_denied`.
    fn sign_up_customer(&self, token: &str, id: &str, body: &CustomerBody) -> BoxFuture<Customer>;

    /// `GET /customers/{id}/balance[?before_id=]`: the balance and a page of its entries, newest
    /// first.
    fn customer_balance(
        &self,
        token: &str,
        id: &str,
        before: Option<&str>,
    ) -> BoxFuture<CustomerBalance>;

    /// `POST /customers/{id}/balance/payout {amount}` with `Idempotency-Key: key`: pays the
    /// balance out in cash from the open cashier session (spec 2.5 §11). Errors:
    /// `403 permission_denied` (`customer.balance_payout`) / `register_session_required`,
    /// `409 no_open_session`, `422 insufficient_balance` (with [`Problem::balance`]).
    fn balance_payout(&self, token: &str, id: &str, amount: i64, key: &str) -> BoxFuture<Payout>;

    /// `POST /pricing/quote` (spec 2.3 §10): prices the lines for the register's branch today,
    /// with the customer's lists when `customer_id` is set. Per-line problems (`no_price`, …)
    /// are in [`QuotedLine::error`]; an unknown customer is `422 invalid_reference`. An empty
    /// `token` quotes with the device token alone.
    fn quote(&self, token: &str, request: &QuoteRequest) -> BoxFuture<Quote>;

    /// `GET /auth/me`'s permission keys at the session's branch plus those held through any role
    /// assignment (customer permissions count from any assignment, spec 2.5 §18 item 16), so the
    /// register shows only what the user may do (spec 2.5 M5: name search, sign-up,
    /// balance payout). The server still checks every call.
    fn permissions(&self, token: &str) -> BoxFuture<Vec<String>>;
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

    /// `GET /customers/{id}` (an archived customer needs `customer.manage`).
    fn customer(&self, token: &str, id: &str) -> BoxFuture<Customer>;

    /// `PUT /customers/{id}`: create or edit (`customer.edit`; group, tier, valid-until and the
    /// manual flag need `customer.manage`). Errors as [`Service::sign_up_customer`], plus
    /// `409 archived` / `merged` and `422 invalid_reference`.
    fn put_customer(&self, token: &str, id: &str, body: &CustomerBody) -> BoxFuture<Customer>;

    /// `POST /customers/{id}/archive` (or `/unarchive`) with `Idempotency-Key: key`
    /// (`customer.manage`). Unarchive errors: `409 phone_taken` / `merged`.
    fn set_customer_archived(
        &self,
        token: &str,
        id: &str,
        archived: bool,
        key: &str,
    ) -> BoxFuture<Customer>;

    /// `POST /customers/{into}/merge {from}` with `Idempotency-Key: key` (`customer.manage`):
    /// answers the surviving customer. Errors: `409 merged` / `archived`, `422 invalid_field`
    /// (the same customer).
    fn merge_customer(&self, token: &str, into: &str, from: &str, key: &str)
    -> BoxFuture<Customer>;

    /// `POST /customers/{id}/erase` with `Idempotency-Key: key` (`customer.erase`): blanks the
    /// personal data; cannot be undone. Errors: `409 balance_not_zero` (with
    /// [`Problem::balance`]), `404 not_found` (already erased).
    fn erase_customer(&self, token: &str, id: &str, key: &str) -> BoxFuture<Customer>;

    /// `GET /customers/duplicates?page=`: "Possible duplicates" (`customer.manage`).
    fn customer_duplicates(&self, token: &str, page: u32) -> BoxFuture<CustomerPage>;

    /// `GET /customers/negative-balances?page=`: "Negative balances" (`customer.manage`).
    fn negative_balances(&self, token: &str, page: u32) -> BoxFuture<CustomerPage>;

    /// `GET /customer-groups[?include_archived=true]`.
    fn customer_groups(&self, token: &str, include_archived: bool)
    -> BoxFuture<Vec<CustomerGroup>>;

    /// `PUT /customer-groups/{id}` (`customer.manage`; a taken name is `409 conflict`).
    fn put_customer_group(
        &self,
        token: &str,
        id: &str,
        body: &GroupBody,
    ) -> BoxFuture<CustomerGroup>;

    /// `GET /membership-tiers[?include_archived=true]`, best first.
    fn membership_tiers(
        &self,
        token: &str,
        include_archived: bool,
    ) -> BoxFuture<Vec<MembershipTier>>;

    /// `PUT /membership-tiers/{id}` (`customer.manage`; a taken name or rank is `409 conflict`
    /// with `field`).
    fn put_membership_tier(
        &self,
        token: &str,
        id: &str,
        body: &TierBody,
    ) -> BoxFuture<MembershipTier>;

    /// `GET /settings/customer-balance`.
    fn customer_balance_settings(&self, token: &str) -> BoxFuture<BalanceSettings>;

    /// `PUT /settings/customer-balance` (`customer.manage`; a negative limit is
    /// `422 invalid_field`).
    fn put_customer_balance_settings(
        &self,
        token: &str,
        body: &BalanceSettings,
    ) -> BoxFuture<BalanceSettings>;

    /// `GET /price-lists[?include_archived=true]`: by kind and name (spec 2.3 §10).
    fn price_lists(&self, token: &str, include_archived: bool) -> BoxFuture<Vec<PriceList>>;

    /// `PUT /price-lists/{id}`: create or change a list (`price.manage`). Errors:
    /// `409 price_list_conflict`, `422 invalid_rule` / `invalid_reference` / `invalid_field`.
    fn put_price_list(&self, token: &str, id: &str, body: &PriceListBody) -> BoxFuture<PriceList>;

    /// `POST /price-lists/{id}/archive` with `Idempotency-Key: key`.
    fn archive_price_list(&self, token: &str, id: &str, key: &str) -> BoxFuture<PriceList>;

    /// `GET /price-lists/{id}/entries?variant=&after_id=`: one page of a list's entries.
    fn price_entries(
        &self,
        token: &str,
        list_id: &str,
        variant_id: Option<&str>,
        after: Option<&str>,
    ) -> BoxFuture<EntryPage>;

    /// `PUT /price-lists/{list}/entries/{id}`: create or replace an entry with its tiers.
    /// Errors: `409 price_overlap` (`entry_id`), `422 invalid_rule` / `invalid_reference` /
    /// `invalid_field`.
    fn put_price_entry(
        &self,
        token: &str,
        list_id: &str,
        id: &str,
        body: &PriceEntryBody,
    ) -> BoxFuture<PriceEntry>;

    /// `DELETE /price-lists/{list}/entries/{id}`.
    fn delete_price_entry(&self, token: &str, list_id: &str, id: &str) -> BoxFuture<()>;

    /// `GET /products/{id}/prices`: the lists and the product's entries (the Prices tab).
    fn product_prices(&self, token: &str, product_id: &str) -> BoxFuture<ProductPrices>;

    /// `GET /tax-classes/{id}/rates`: oldest first.
    fn tax_rates(&self, token: &str, tax_class_id: &str) -> BoxFuture<Vec<TaxRate>>;

    /// `PUT /tax-classes/{id}/rates`: adds or changes the rate of `rate.valid_from`; answers all
    /// rates of the class.
    fn put_tax_rate(
        &self,
        token: &str,
        tax_class_id: &str,
        rate: &TaxRate,
    ) -> BoxFuture<Vec<TaxRate>>;

    /// `GET /settings/pricing`.
    fn pricing_settings(&self, token: &str) -> BoxFuture<PricingSettings>;

    /// `PUT /settings/pricing`.
    fn put_pricing_settings(
        &self,
        token: &str,
        body: &PricingSettings,
    ) -> BoxFuture<PricingSettings>;
}
