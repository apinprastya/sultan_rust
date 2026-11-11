//! [`Service`] over HTTP (`reqwest`) and the events WebSocket (`tokio-tungstenite`), spec 2.1 §2,
//! §4–§6, §8.
//!
//! - Every request carries the device token as `X-Register-Token` (spec 2.1 §2); calls made in a
//!   login session add `Authorization: Bearer <session token>`. A back-office client
//!   ([`HttpService::user`], phase C4) has no device token and sends only the bearer token.
//! - The cashier-session `POST`s send `Idempotency-Key`, so a retry after a lost answer gets the
//!   original answer instead of `409` (`docs/plan.md` §1, §9 case 17). `POST /overrides` takes no
//!   key: its answer is a secret the server does not store.
//! - Error answers are parsed as problem+json ([`Problem`]); anything else is
//!   [`ClientError::Unexpected`]. Transport failures are [`ClientError::Network`].
//! - `POST /pricing/quote` with an empty session token sends only the device token.
//! - `GET /cashier-sessions/current` answering `404 not_found` means "no open session".
//! - The events socket authenticates with the same two headers (native client, `events.rs`).
//!
//! Plain HTTP only for now: the server has no TLS yet (README "Limitations").

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use reqwest::{Method, RequestBuilder, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;

use crate::catalog::{
    Brand, BrandBody, CatalogSettings, Category, CategoryBody, CategoryNode, Generate, ImageInfo,
    Lookup, Me, Product, ProductPage, ProductQuery, SearchPage, TaxClass,
};
use crate::customers::{
    BalanceSettings, Customer, CustomerBalance, CustomerBody, CustomerGroup, CustomerPage,
    CustomerQuery, GroupBody, MembershipTier, Payout, TierBody,
};
use crate::pricing::{
    EntryPage, PriceEntry, PriceEntryBody, PriceList, PriceListBody, PricingSettings,
    ProductPrices, Quote, QuoteRequest, TaxRate,
};
use crate::{
    BackOffice, Branch, CashierSession, ClientError, CloseReport, CloseRequest, EventSink,
    LockUser, Login, OverrideGrant, OverrideRequest, Problem, ProtectedAction, ProtectedResult,
    ServerEvent, Service, SessionDetail, SessionPage, SessionQuery,
};

/// Header carrying the device token (spec 2.1 §2).
pub const REGISTER_TOKEN_HEADER: &str = "x-register-token";
/// Header carrying an override token (spec 2.1 §6).
pub const OVERRIDE_HEADER: &str = "x-override";
/// Header carrying a retry key (`docs/plan.md` §1).
pub const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// The API base for a server URL: accepts `http://host:8080`, a trailing `/`, or the full
/// `http://host:8080/api/v1`.
#[must_use]
pub fn api_base(server: &str) -> String {
    let server = server.trim().trim_end_matches('/');
    let server = server.strip_suffix("/api/v1").unwrap_or(server);
    format!("{server}/api/v1")
}

/// The events WebSocket URL for an API base (`http` → `ws`, `https` → `wss`).
#[must_use]
pub fn events_url(base: &str) -> String {
    let ws = if let Some(rest) = base.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = base.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        base.to_owned()
    };
    format!("{ws}/events")
}

struct Inner {
    http: reqwest::Client,
    base: String,
    device_token: String,
}

/// The real [`Service`]: a paired register talking to its server.
#[derive(Clone)]
pub struct HttpService(Arc<Inner>);

impl std::fmt::Debug for HttpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print the device token.
        f.debug_struct("HttpService")
            .field("base", &self.0.base)
            .finish_non_exhaustive()
    }
}

fn network(error: impl std::fmt::Display) -> ClientError {
    ClientError::Network(error.to_string())
}

impl HttpService {
    /// A client for `server` (see [`api_base`]) using the register's device token.
    #[must_use]
    pub fn new(server: &str, device_token: &str) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        Self(Arc::new(Inner {
            http,
            base: api_base(server),
            device_token: device_token.trim().to_owned(),
        }))
    }

    /// A back-office client for `server`: no device token, only password sessions
    /// ([`crate::BackOffice`]).
    #[must_use]
    pub fn user(server: &str) -> Self {
        Self::new(server, "")
    }

    /// The API base URL.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.0.base
    }

    fn request(&self, method: Method, path: &str, token: Option<&str>) -> RequestBuilder {
        let mut request = self
            .0
            .http
            .request(method, format!("{}{path}", self.0.base));
        if !self.0.device_token.is_empty() {
            request = request.header(REGISTER_TOKEN_HEADER, &self.0.device_token);
        }
        match token {
            Some(t) => request.bearer_auth(t),
            None => request,
        }
    }
}

/// Sends `request` and decodes a success body as `T`; see [`problem`] for errors.
async fn send<T: DeserializeOwned>(request: RequestBuilder) -> Result<T, ClientError> {
    let (status, bytes) = exchange(request).await?;
    serde_json::from_slice(&bytes)
        .map_err(|e| ClientError::Unexpected(format!("HTTP {status}: {e}")))
}

/// Sends `request`; `Ok` with the status and body for 2xx answers.
async fn exchange(request: RequestBuilder) -> Result<(StatusCode, Vec<u8>), ClientError> {
    let response = request.send().await.map_err(network)?;
    let status = response.status();
    let bytes = response.bytes().await.map_err(network)?.to_vec();
    if status.is_success() {
        Ok((status, bytes))
    } else {
        Err(problem(status, &bytes))
    }
}

/// An error answer: problem+json → [`ClientError::Api`], anything else →
/// [`ClientError::Unexpected`].
fn problem(status: StatusCode, body: &[u8]) -> ClientError {
    serde_json::from_slice::<Problem>(body).map_or_else(
        |_| ClientError::Unexpected(format!("HTTP {status}")),
        ClientError::Api,
    )
}

impl Service for HttpService {
    fn permissions(&self, token: &str) -> crate::BoxFuture<Vec<String>> {
        let me = send::<Me>(self.request(Method::GET, "/auth/me", Some(token)));
        Box::pin(async move {
            me.await.map(|me| {
                let mut keys = me.permissions;
                for key in me
                    .permissions_anywhere
                    .into_iter()
                    .chain(me.permissions_everywhere)
                {
                    if !keys.contains(&key) {
                        keys.push(key);
                    }
                }
                keys
            })
        })
    }

    fn lock_users(&self) -> crate::BoxFuture<Vec<LockUser>> {
        let request = self.request(Method::GET, "/registers/me/users", None);
        Box::pin(send(request))
    }

    fn pin_login(&self, user_id: &str, pin: &str) -> crate::BoxFuture<Login> {
        let request = self
            .request(Method::POST, "/auth/pin", None)
            .json(&json!({ "user_id": user_id, "pin": pin }));
        Box::pin(send(request))
    }

    fn logout(&self, token: &str) -> crate::BoxFuture<()> {
        let request = self.request(Method::POST, "/auth/logout", Some(token));
        Box::pin(async move { exchange(request).await.map(|_| ()) })
    }

    fn branch(&self, token: &str, branch_id: &str) -> crate::BoxFuture<Branch> {
        let request = self.request(Method::GET, &format!("/branches/{branch_id}"), Some(token));
        Box::pin(send(request))
    }

    fn current_session(&self, token: &str) -> crate::BoxFuture<Option<CashierSession>> {
        let request = self.request(Method::GET, "/cashier-sessions/current", Some(token));
        Box::pin(async move {
            match send(request).await {
                Ok(session) => Ok(Some(session)),
                Err(ClientError::Api(p)) if p.code == "not_found" => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn open_session(
        &self,
        token: &str,
        opening_cash: i64,
        key: &str,
    ) -> crate::BoxFuture<CashierSession> {
        let request = self
            .request(Method::POST, "/cashier-sessions", Some(token))
            .header(IDEMPOTENCY_HEADER, key)
            .json(&json!({ "opening_cash": opening_cash }));
        Box::pin(send(request))
    }

    fn close_session(
        &self,
        token: &str,
        session_id: &str,
        body: &CloseRequest,
        key: &str,
    ) -> crate::BoxFuture<CloseReport> {
        let request = self
            .request(
                Method::POST,
                &format!("/cashier-sessions/{session_id}/close"),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key)
            .json(body);
        Box::pin(send(request))
    }

    fn session(&self, token: &str, session_id: &str) -> crate::BoxFuture<SessionDetail> {
        let request = self.request(
            Method::GET,
            &format!("/cashier-sessions/{session_id}"),
            Some(token),
        );
        Box::pin(send(request))
    }

    fn sessions(&self, token: &str, query: &SessionQuery) -> crate::BoxFuture<SessionPage> {
        let request = self
            .request(Method::GET, "/cashier-sessions", Some(token))
            .query(query);
        Box::pin(send(request))
    }

    fn request_override(
        &self,
        token: &str,
        body: &OverrideRequest,
    ) -> crate::BoxFuture<OverrideGrant> {
        let request = self
            .request(Method::POST, "/overrides", Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn demo_protected(
        &self,
        token: &str,
        body: &ProtectedAction,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<ProtectedResult> {
        let mut request = self
            .request(Method::POST, "/demo/override-protected", Some(token))
            .json(body);
        if let Some(t) = override_token {
            request = request.header(OVERRIDE_HEADER, t);
        }
        Box::pin(send(request))
    }

    fn watch_events(&self, token: &str, sink: EventSink) -> crate::BoxFuture<()> {
        let url = events_url(&self.0.base);
        // No session: the device token alone (C3b), so no `Authorization` header.
        let bearer = if token.is_empty() {
            String::new()
        } else {
            format!("Bearer {token}")
        };
        let device = self.0.device_token.clone();
        Box::pin(async move {
            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(|e| ClientError::Unexpected(e.to_string()))?;
            let headers = request.headers_mut();
            for (name, value) in [("authorization", bearer), (REGISTER_TOKEN_HEADER, device)] {
                if value.is_empty() {
                    continue;
                }
                let value = HeaderValue::from_str(&value)
                    .map_err(|e| ClientError::Unexpected(e.to_string()))?;
                headers.insert(name, value);
            }
            let (mut socket, _) = tokio_tungstenite::connect_async(request)
                .await
                .map_err(network)?;
            while let Some(message) = socket.next().await {
                match message.map_err(network)? {
                    Message::Text(text) => {
                        if let Ok(event) = serde_json::from_str::<ServerEvent>(&text) {
                            sink(event);
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            Ok(())
        })
    }

    fn lookup(&self, token: &str, code: &str) -> crate::BoxFuture<Lookup> {
        let request = self
            .request(Method::GET, "/catalog/lookup", Some(token))
            .query(&[("code", code)]);
        Box::pin(send(request))
    }

    fn customer_by_phone(&self, token: &str, phone: &str) -> crate::BoxFuture<Customer> {
        let request = self
            .request(Method::GET, "/customers/lookup", Some(token))
            .query(&[("phone", phone)]);
        Box::pin(send(request))
    }

    fn customer_by_code(&self, token: &str, code: &str) -> crate::BoxFuture<Customer> {
        let request = self
            .request(Method::GET, "/customers/lookup", Some(token))
            .query(&[("code", code)]);
        Box::pin(send(request))
    }

    fn search_customers(
        &self,
        token: &str,
        query: &CustomerQuery,
    ) -> crate::BoxFuture<CustomerPage> {
        let request = self
            .request(Method::GET, "/customers", Some(token))
            .query(query);
        Box::pin(send(request))
    }

    fn sign_up_customer(
        &self,
        token: &str,
        id: &str,
        body: &CustomerBody,
    ) -> crate::BoxFuture<Customer> {
        let request = self
            .request(Method::PUT, &format!("/customers/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn customer_balance(
        &self,
        token: &str,
        id: &str,
        before: Option<&str>,
    ) -> crate::BoxFuture<CustomerBalance> {
        let mut request = self.request(
            Method::GET,
            &format!("/customers/{id}/balance"),
            Some(token),
        );
        if let Some(before) = before {
            request = request.query(&[("before_id", before)]);
        }
        Box::pin(send(request))
    }

    fn balance_payout(
        &self,
        token: &str,
        id: &str,
        amount: i64,
        key: &str,
    ) -> crate::BoxFuture<Payout> {
        let request = self
            .request(
                Method::POST,
                &format!("/customers/{id}/balance/payout"),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key)
            .json(&json!({ "amount": amount }));
        Box::pin(send(request))
    }

    fn quote(&self, token: &str, request: &QuoteRequest) -> crate::BoxFuture<Quote> {
        // No session: the device token alone.
        let token = (!token.is_empty()).then_some(token);
        let request = self
            .request(Method::POST, "/pricing/quote", token)
            .json(request);
        Box::pin(send(request))
    }
}

#[derive(serde::Deserialize)]
struct Generated {
    value: String,
}

#[derive(serde::Deserialize, serde::Serialize)]
struct BranchIds {
    branch_ids: Vec<String>,
}

fn archived_query(include_archived: bool) -> &'static [(&'static str, &'static str)] {
    if include_archived {
        &[("include_archived", "true")]
    } else {
        &[]
    }
}

impl BackOffice for HttpService {
    fn password_login(&self, username: &str, password: &str) -> crate::BoxFuture<Login> {
        let request = self
            .request(Method::POST, "/auth/password", None)
            .json(&json!({ "username": username, "password": password }));
        Box::pin(send(request))
    }

    fn me(&self, token: &str) -> crate::BoxFuture<Me> {
        Box::pin(send(self.request(Method::GET, "/auth/me", Some(token))))
    }

    fn branches(&self, token: &str) -> crate::BoxFuture<Vec<Branch>> {
        Box::pin(send(self.request(Method::GET, "/branches", Some(token))))
    }

    fn products(&self, token: &str, query: &ProductQuery) -> crate::BoxFuture<ProductPage> {
        let request = self
            .request(Method::GET, "/products", Some(token))
            .query(query);
        Box::pin(send(request))
    }

    fn product(&self, token: &str, id: &str) -> crate::BoxFuture<Product> {
        let request = self.request(Method::GET, &format!("/products/{id}"), Some(token));
        Box::pin(send(request))
    }

    fn put_product(&self, token: &str, id: &str, body: &Product) -> crate::BoxFuture<Product> {
        let request = self
            .request(Method::PUT, &format!("/products/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn set_archived(
        &self,
        token: &str,
        id: &str,
        archived: bool,
        key: &str,
    ) -> crate::BoxFuture<Product> {
        let action = if archived { "archive" } else { "unarchive" };
        let request = self
            .request(
                Method::POST,
                &format!("/products/{id}/{action}"),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key);
        Box::pin(send(request))
    }

    fn categories(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<CategoryNode>> {
        let request = self
            .request(Method::GET, "/categories", Some(token))
            .query(archived_query(include_archived));
        Box::pin(send(request))
    }

    fn put_category(
        &self,
        token: &str,
        id: &str,
        body: &CategoryBody,
    ) -> crate::BoxFuture<Category> {
        let request = self
            .request(Method::PUT, &format!("/categories/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn brands(&self, token: &str, include_archived: bool) -> crate::BoxFuture<Vec<Brand>> {
        let request = self
            .request(Method::GET, "/brands", Some(token))
            .query(archived_query(include_archived));
        Box::pin(send(request))
    }

    fn put_brand(&self, token: &str, id: &str, body: &BrandBody) -> crate::BoxFuture<Brand> {
        let request = self
            .request(Method::PUT, &format!("/brands/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn tax_classes(&self, token: &str) -> crate::BoxFuture<Vec<TaxClass>> {
        Box::pin(send(self.request(Method::GET, "/tax-classes", Some(token))))
    }

    fn catalog_settings(&self, token: &str) -> crate::BoxFuture<CatalogSettings> {
        Box::pin(send(self.request(
            Method::GET,
            "/settings/catalog",
            Some(token),
        )))
    }

    fn generate(&self, token: &str, what: Generate, key: &str) -> crate::BoxFuture<String> {
        let request = self
            .request(
                Method::POST,
                &format!("/catalog/generate/{}", what.path()),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key);
        Box::pin(async move { send::<Generated>(request).await.map(|g| g.value) })
    }

    fn branch_blocks(&self, token: &str, id: &str) -> crate::BoxFuture<Vec<String>> {
        let request = self.request(
            Method::GET,
            &format!("/products/{id}/branch-blocks"),
            Some(token),
        );
        Box::pin(async move { send::<BranchIds>(request).await.map(|b| b.branch_ids) })
    }

    fn put_branch_blocks(
        &self,
        token: &str,
        id: &str,
        branch_ids: &[String],
    ) -> crate::BoxFuture<Vec<String>> {
        let request = self
            .request(
                Method::PUT,
                &format!("/products/{id}/branch-blocks"),
                Some(token),
            )
            .json(&BranchIds {
                branch_ids: branch_ids.to_vec(),
            });
        Box::pin(async move { send::<BranchIds>(request).await.map(|b| b.branch_ids) })
    }

    fn product_image(&self, token: &str, id: &str) -> crate::BoxFuture<Option<Vec<u8>>> {
        let request = self.request(Method::GET, &format!("/products/{id}/image"), Some(token));
        Box::pin(async move {
            match exchange(request).await {
                Ok((_, bytes)) => Ok(Some(bytes)),
                Err(ClientError::Api(p)) if p.code == "not_found" => Ok(None),
                Err(e) => Err(e),
            }
        })
    }

    fn put_product_image(
        &self,
        token: &str,
        id: &str,
        bytes: Vec<u8>,
    ) -> crate::BoxFuture<ImageInfo> {
        let request = self
            .request(Method::PUT, &format!("/products/{id}/image"), Some(token))
            .header("content-type", "application/octet-stream")
            .body(bytes);
        Box::pin(send(request))
    }

    fn delete_product_image(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        let request = self.request(
            Method::DELETE,
            &format!("/products/{id}/image"),
            Some(token),
        );
        Box::pin(async move { exchange(request).await.map(|_| ()) })
    }

    fn search(
        &self,
        token: &str,
        q: &str,
        limit: u32,
        after: Option<&str>,
    ) -> crate::BoxFuture<SearchPage> {
        let mut query = vec![("q", q.to_owned()), ("limit", limit.to_string())];
        if let Some(after) = after {
            query.push(("after_variant_id", after.to_owned()));
        }
        let request = self
            .request(Method::GET, "/catalog/search", Some(token))
            .query(&query);
        Box::pin(send(request))
    }

    fn delete_product(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        self.delete(token, &format!("/products/{id}"))
    }

    fn delete_category(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        self.delete(token, &format!("/categories/{id}"))
    }

    fn delete_brand(&self, token: &str, id: &str) -> crate::BoxFuture<()> {
        self.delete(token, &format!("/brands/{id}"))
    }

    fn customer(&self, token: &str, id: &str) -> crate::BoxFuture<Customer> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/customers/{id}"),
            Some(token),
        )))
    }

    fn put_customer(
        &self,
        token: &str,
        id: &str,
        body: &CustomerBody,
    ) -> crate::BoxFuture<Customer> {
        self.sign_up_customer(token, id, body)
    }

    fn set_customer_archived(
        &self,
        token: &str,
        id: &str,
        archived: bool,
        key: &str,
    ) -> crate::BoxFuture<Customer> {
        let action = if archived { "archive" } else { "unarchive" };
        self.action(token, &format!("/customers/{id}/{action}"), key, None)
    }

    fn merge_customer(
        &self,
        token: &str,
        into: &str,
        from: &str,
        key: &str,
    ) -> crate::BoxFuture<Customer> {
        let body = json!({ "from": from });
        self.action(token, &format!("/customers/{into}/merge"), key, Some(body))
    }

    fn erase_customer(&self, token: &str, id: &str, key: &str) -> crate::BoxFuture<Customer> {
        self.action(token, &format!("/customers/{id}/erase"), key, None)
    }

    fn customer_duplicates(&self, token: &str, page: u32) -> crate::BoxFuture<CustomerPage> {
        let request = self
            .request(Method::GET, "/customers/duplicates", Some(token))
            .query(&[("page", page)]);
        Box::pin(send(request))
    }

    fn negative_balances(&self, token: &str, page: u32) -> crate::BoxFuture<CustomerPage> {
        let request = self
            .request(Method::GET, "/customers/negative-balances", Some(token))
            .query(&[("page", page)]);
        Box::pin(send(request))
    }

    fn customer_groups(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<CustomerGroup>> {
        let request = self
            .request(Method::GET, "/customer-groups", Some(token))
            .query(archived_query(include_archived));
        Box::pin(send(request))
    }

    fn put_customer_group(
        &self,
        token: &str,
        id: &str,
        body: &GroupBody,
    ) -> crate::BoxFuture<CustomerGroup> {
        let request = self
            .request(Method::PUT, &format!("/customer-groups/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn price_lists(&self, token: &str, include_archived: bool) -> crate::BoxFuture<Vec<PriceList>> {
        let request = self
            .request(Method::GET, "/price-lists", Some(token))
            .query(archived_query(include_archived));
        Box::pin(send(request))
    }

    fn put_price_list(
        &self,
        token: &str,
        id: &str,
        body: &PriceListBody,
    ) -> crate::BoxFuture<PriceList> {
        let request = self
            .request(Method::PUT, &format!("/price-lists/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn archive_price_list(&self, token: &str, id: &str, key: &str) -> crate::BoxFuture<PriceList> {
        let request = self
            .request(
                Method::POST,
                &format!("/price-lists/{id}/archive"),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key);
        Box::pin(send(request))
    }

    fn price_entries(
        &self,
        token: &str,
        list_id: &str,
        variant_id: Option<&str>,
        after: Option<&str>,
    ) -> crate::BoxFuture<EntryPage> {
        let mut query = Vec::new();
        if let Some(v) = variant_id {
            query.push(("variant", v.to_owned()));
        }
        if let Some(a) = after {
            query.push(("after_id", a.to_owned()));
        }
        let request = self
            .request(
                Method::GET,
                &format!("/price-lists/{list_id}/entries"),
                Some(token),
            )
            .query(&query);
        Box::pin(send(request))
    }

    fn put_price_entry(
        &self,
        token: &str,
        list_id: &str,
        id: &str,
        body: &PriceEntryBody,
    ) -> crate::BoxFuture<PriceEntry> {
        let request = self
            .request(
                Method::PUT,
                &format!("/price-lists/{list_id}/entries/{id}"),
                Some(token),
            )
            .json(body);
        Box::pin(send(request))
    }

    fn membership_tiers(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<MembershipTier>> {
        let request = self
            .request(Method::GET, "/membership-tiers", Some(token))
            .query(archived_query(include_archived));
        Box::pin(send(request))
    }

    fn put_membership_tier(
        &self,
        token: &str,
        id: &str,
        body: &TierBody,
    ) -> crate::BoxFuture<MembershipTier> {
        let request = self
            .request(Method::PUT, &format!("/membership-tiers/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn customer_balance_settings(&self, token: &str) -> crate::BoxFuture<BalanceSettings> {
        Box::pin(send(self.request(
            Method::GET,
            "/settings/customer-balance",
            Some(token),
        )))
    }

    fn delete_price_entry(&self, token: &str, list_id: &str, id: &str) -> crate::BoxFuture<()> {
        self.delete(token, &format!("/price-lists/{list_id}/entries/{id}"))
    }

    fn product_prices(&self, token: &str, product_id: &str) -> crate::BoxFuture<ProductPrices> {
        let request = self.request(
            Method::GET,
            &format!("/products/{product_id}/prices"),
            Some(token),
        );
        Box::pin(send(request))
    }

    fn tax_rates(&self, token: &str, tax_class_id: &str) -> crate::BoxFuture<Vec<TaxRate>> {
        let request = self.request(
            Method::GET,
            &format!("/tax-classes/{tax_class_id}/rates"),
            Some(token),
        );
        Box::pin(send(request))
    }

    fn put_tax_rate(
        &self,
        token: &str,
        tax_class_id: &str,
        rate: &TaxRate,
    ) -> crate::BoxFuture<Vec<TaxRate>> {
        let request = self
            .request(
                Method::PUT,
                &format!("/tax-classes/{tax_class_id}/rates"),
                Some(token),
            )
            .json(rate);
        Box::pin(send(request))
    }

    fn pricing_settings(&self, token: &str) -> crate::BoxFuture<PricingSettings> {
        Box::pin(send(self.request(
            Method::GET,
            "/settings/pricing",
            Some(token),
        )))
    }

    fn put_customer_balance_settings(
        &self,
        token: &str,
        body: &BalanceSettings,
    ) -> crate::BoxFuture<BalanceSettings> {
        let request = self
            .request(Method::PUT, "/settings/customer-balance", Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn put_pricing_settings(
        &self,
        token: &str,
        body: &PricingSettings,
    ) -> crate::BoxFuture<PricingSettings> {
        let request = self
            .request(Method::PUT, "/settings/pricing", Some(token))
            .json(body);
        Box::pin(send(request))
    }
}

impl HttpService {
    /// `DELETE {path}`, answered `204`.
    fn delete(&self, token: &str, path: &str) -> crate::BoxFuture<()> {
        let request = self.request(Method::DELETE, path, Some(token));
        Box::pin(async move { exchange(request).await.map(|_| ()) })
    }

    /// An action `POST {path}` with `Idempotency-Key: key` and an optional JSON body.
    fn action<T: DeserializeOwned + 'static>(
        &self,
        token: &str,
        path: &str,
        key: &str,
        body: Option<serde_json::Value>,
    ) -> crate::BoxFuture<T> {
        let mut request = self
            .request(Method::POST, path, Some(token))
            .header(IDEMPOTENCY_HEADER, key);
        if let Some(body) = body {
            request = request.json(&body);
        }
        Box::pin(send(request))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let cases = [
            (
                "http://h:8080",
                "http://h:8080/api/v1",
                "ws://h:8080/api/v1/events",
            ),
            (
                "http://h:8080/",
                "http://h:8080/api/v1",
                "ws://h:8080/api/v1/events",
            ),
            (
                " https://h/api/v1/ ",
                "https://h/api/v1",
                "wss://h/api/v1/events",
            ),
            ("h:1", "h:1/api/v1", "h:1/api/v1/events"),
        ];
        for (server, base, events) in cases {
            assert_eq!(api_base(server), base, "{server}");
            assert_eq!(events_url(base), events, "{server}");
        }
        let service = HttpService::new("http://h:1", " secret\n");
        assert_eq!(service.base(), "http://h:1/api/v1");
        assert!(!format!("{service:?}").contains("secret"));
    }

    #[test]
    fn error_bodies() {
        let body = br#"{"status":423,"code":"locked_out","detail":"x","locked_until":5}"#;
        let err = problem(StatusCode::LOCKED, body);
        assert_eq!(err.problem().unwrap().locked_until(), Some(5));
        let err = problem(StatusCode::BAD_GATEWAY, b"oops");
        assert_eq!(err, ClientError::Unexpected("HTTP 502 Bad Gateway".into()));
    }
}
