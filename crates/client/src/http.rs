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

use crate::cash::{
    CashMovement, CashMovementPage, CashMovementRequest, CashQuery, CashReason, CashReasonBody,
    CashText, PendingDay, ProposedFloat, XReport, ZQuery, ZReport, ZReportPage, ZRunRequest,
};
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
use crate::sales::{
    CartVoid, CartVoidRequest, DocQuery, HeldCart, HistoryPage, HoldRequest, Receipt, ReturnPage,
    ReturnRequest, Sale, SalePage, SaleRequest, SaleReturn, SaleSummary, SalesSettings, TenderType,
    TenderTypeBody, VoidRequest,
};
use crate::stock::{
    Adjustment, AdjustmentBody, CardPage, CardQuery, CountEntry, GoodsReceipt, InternalTransfer,
    InternalTransferBody, InventorySettings, Lot, LotQuery, MoveBody, OnHandPage, OnHandQuery,
    OpeningBody, OpeningStock, Opname, OpnameBody, OrderAction, Payable, PayableQuery,
    PurchaseOrder, PurchaseOrderBody, ReceiptBody, ReorderRule, ReorderRuleBody, StockDocQuery,
    StockReason, StockReasonBody, SuggestedLine, Supplier, SupplierBody, SupplierItem,
    SupplierItemBody, SupplierReturn, SupplierReturnBody, TillStock, Transfer, TransferAction,
    TransferBody,
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

    fn product_search(&self, token: &str, q: &str) -> crate::BoxFuture<SearchPage> {
        let request = self
            .request(Method::GET, "/catalog/search", Some(token))
            .query(&[("q", q), ("limit", "20")]);
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

    fn tender_types(&self, token: &str) -> crate::BoxFuture<Vec<TenderType>> {
        let token = (!token.is_empty()).then_some(token);
        Box::pin(send(self.request(Method::GET, "/tender-types", token)))
    }

    fn post_sale(
        &self,
        token: &str,
        id: &str,
        body: &SaleRequest,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Sale> {
        let request = self
            .request(Method::PUT, &format!("/sales/{id}"), Some(token))
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn sale(&self, token: &str, id: &str) -> crate::BoxFuture<Sale> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/sales/{id}"),
            Some(token),
        )))
    }

    fn sale_by_number(&self, token: &str, number: &str) -> crate::BoxFuture<Sale> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/sales/by-number/{number}"),
            Some(token),
        )))
    }

    fn sales(&self, token: &str, query: &DocQuery) -> crate::BoxFuture<SalePage> {
        Box::pin(send(
            self.request(Method::GET, "/sales", Some(token))
                .query(query),
        ))
    }

    fn receipt(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Receipt> {
        let mut request = self
            .request(Method::GET, &format!("/sales/{id}/receipt"), Some(token))
            .query(&[("lang", lang)]);
        if copy {
            request = request.query(&[("copy", "true")]);
        }
        Box::pin(send(with_override(request, override_token)))
    }

    fn return_receipt(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Receipt> {
        let mut request = self
            .request(Method::GET, &format!("/returns/{id}/receipt"), Some(token))
            .query(&[("lang", lang)]);
        if copy {
            request = request.query(&[("copy", "true")]);
        }
        Box::pin(send(with_override(request, override_token)))
    }

    fn void_sale(
        &self,
        token: &str,
        id: &str,
        body: &VoidRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<Sale> {
        let request = self
            .request(Method::POST, &format!("/sales/{id}/void"), Some(token))
            .header(IDEMPOTENCY_HEADER, key)
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn cart_void(
        &self,
        token: &str,
        body: &CartVoidRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CartVoid> {
        let request = self
            .request(Method::POST, "/cart-voids", Some(token))
            .header(IDEMPOTENCY_HEADER, key)
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn open_drawer(
        &self,
        token: &str,
        body: &crate::DrawerRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<crate::DrawerOpen> {
        let request = self
            .request(Method::POST, "/drawer/open", Some(token))
            .header(IDEMPOTENCY_HEADER, key)
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn cash_reasons(&self, token: &str) -> crate::BoxFuture<Vec<CashReason>> {
        Box::pin(send(self.request(
            Method::GET,
            "/cash-reasons",
            Some(token),
        )))
    }

    fn post_cash_movement(
        &self,
        token: &str,
        id: &str,
        body: &CashMovementRequest,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashMovement> {
        let request = self
            .request(Method::PUT, &format!("/cash-movements/{id}"), Some(token))
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn cash_movement(&self, token: &str, id: &str) -> crate::BoxFuture<CashMovement> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/cash-movements/{id}"),
            Some(token),
        )))
    }

    fn cash_movements(&self, token: &str, query: &CashQuery) -> crate::BoxFuture<CashMovementPage> {
        let request = self
            .request(Method::GET, "/cash-movements", Some(token))
            .query(query);
        Box::pin(send(request))
    }

    fn cash_slip(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashText> {
        let request = self
            .request(
                Method::GET,
                &format!("/cash-movements/{id}/slip"),
                Some(token),
            )
            .query(&text_query(copy, lang));
        Box::pin(send(with_override(request, override_token)))
    }

    fn payout_slip(
        &self,
        token: &str,
        customer_id: &str,
        entry_id: &str,
        copy: bool,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashText> {
        let path = format!("/customers/{customer_id}/balance/payouts/{entry_id}/slip");
        let request = self
            .request(Method::GET, &path, Some(token))
            .query(&text_query(copy, lang));
        Box::pin(send(with_override(request, override_token)))
    }

    fn x_report(
        &self,
        token: &str,
        session_id: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<XReport> {
        let request = self.request(
            Method::GET,
            &format!("/cashier-sessions/{session_id}/x-report"),
            Some(token),
        );
        Box::pin(send(with_override(request, override_token)))
    }

    fn x_report_text(
        &self,
        token: &str,
        session_id: &str,
        lang: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<CashText> {
        let request = self
            .request(
                Method::GET,
                &format!("/cashier-sessions/{session_id}/x-report/text"),
                Some(token),
            )
            .query(&[("lang", lang)]);
        Box::pin(send(with_override(request, override_token)))
    }

    fn close_report_text(
        &self,
        token: &str,
        session_id: &str,
        copy: bool,
        lang: &str,
    ) -> crate::BoxFuture<CashText> {
        let request = self
            .request(
                Method::GET,
                &format!("/cashier-sessions/{session_id}/report/text"),
                Some(token),
            )
            .query(&text_query(copy, lang));
        Box::pin(send(request))
    }

    fn proposed_float(&self, token: &str) -> crate::BoxFuture<ProposedFloat> {
        Box::pin(send(self.request(
            Method::GET,
            "/registers/me/float",
            Some(token),
        )))
    }

    fn run_z(
        &self,
        token: &str,
        body: &ZRunRequest,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<ZReport> {
        let request = self
            .request(Method::POST, "/z-reports", Some(token))
            .header(IDEMPOTENCY_HEADER, key)
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn z_report_text(
        &self,
        token: &str,
        id: &str,
        copy: bool,
        lang: &str,
    ) -> crate::BoxFuture<CashText> {
        let request = self
            .request(Method::GET, &format!("/z-reports/{id}/text"), Some(token))
            .query(&text_query(copy, lang));
        Box::pin(send(request))
    }

    fn pending_days(
        &self,
        token: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<PendingDay>> {
        let mut request = self.request(Method::GET, "/z-reports/pending", Some(token));
        if let Some(b) = branch_id {
            request = request.query(&[("branch_id", b)]);
        }
        Box::pin(send(request))
    }

    fn add_cash_photo(
        &self,
        token: &str,
        movement_id: &str,
        photo_id: &str,
        bytes: Vec<u8>,
    ) -> crate::BoxFuture<crate::NotaPhoto> {
        let request = self
            .request(
                Method::PUT,
                &format!("/cash-movements/{movement_id}/photos/{photo_id}"),
                Some(token),
            )
            .header("content-type", "application/octet-stream")
            .body(bytes);
        Box::pin(send(request))
    }

    fn cash_photo(
        &self,
        token: &str,
        movement_id: &str,
        photo_id: &str,
    ) -> crate::BoxFuture<Vec<u8>> {
        let request = self.request(
            Method::GET,
            &format!("/cash-movements/{movement_id}/photos/{photo_id}"),
            Some(token),
        );
        Box::pin(async move { exchange(request).await.map(|(_, bytes)| bytes) })
    }

    fn till_stock(&self, token: &str, variant_id: &str) -> crate::BoxFuture<TillStock> {
        let token = (!token.is_empty()).then_some(token);
        Box::pin(send(self.request(
            Method::GET,
            &format!("/registers/me/stock/{variant_id}"),
            token,
        )))
    }

    fn post_return(
        &self,
        token: &str,
        id: &str,
        body: &ReturnRequest,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<SaleReturn> {
        let request = self
            .request(Method::PUT, &format!("/returns/{id}"), Some(token))
            .json(body);
        Box::pin(send(with_override(request, override_token)))
    }

    fn hold_cart(&self, token: &str, id: &str, body: &HoldRequest) -> crate::BoxFuture<HeldCart> {
        let request = self
            .request(Method::PUT, &format!("/held-carts/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn held_carts(&self, token: &str) -> crate::BoxFuture<Vec<HeldCart>> {
        Box::pin(send(self.request(Method::GET, "/held-carts", Some(token))))
    }

    fn take_held_cart(&self, token: &str, id: &str, key: &str) -> crate::BoxFuture<HeldCart> {
        let request = self
            .request(Method::POST, &format!("/held-carts/{id}/take"), Some(token))
            .header(IDEMPOTENCY_HEADER, key);
        Box::pin(send(request))
    }

    fn delete_held_cart(
        &self,
        token: &str,
        id: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<HeldCart> {
        let request = self.request(Method::DELETE, &format!("/held-carts/{id}"), Some(token));
        Box::pin(send(with_override(request, override_token)))
    }

    fn customer_history(
        &self,
        token: &str,
        id: &str,
        before: Option<&str>,
    ) -> crate::BoxFuture<HistoryPage> {
        let mut request = self.request(
            Method::GET,
            &format!("/customers/{id}/history"),
            Some(token),
        );
        if let Some(before) = before {
            request = request.query(&[("before", before)]);
        }
        Box::pin(send(request))
    }

    fn recent_receipts(
        &self,
        token: &str,
        customer_id: &str,
    ) -> crate::BoxFuture<Vec<SaleSummary>> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/customers/{customer_id}/recent-receipts"),
            Some(token),
        )))
    }
}

/// The query of a text route: `lang`, and `copy=true` for a reprint.
fn text_query(copy: bool, lang: &str) -> Vec<(&'static str, String)> {
    let mut query = vec![("lang", lang.to_owned())];
    if copy {
        query.push(("copy", "true".to_owned()));
    }
    query
}

/// Adds `X-Override` when a token (or several, comma separated) is given.
fn with_override(request: RequestBuilder, override_token: Option<&str>) -> RequestBuilder {
    match override_token {
        Some(t) => request.header(OVERRIDE_HEADER, t),
        None => request,
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

    fn all_tender_types(&self, token: &str) -> crate::BoxFuture<Vec<TenderType>> {
        Box::pin(send(
            self.request(Method::GET, "/tender-types", Some(token))
                .query(&[("all", "true")]),
        ))
    }

    fn put_tender_type(
        &self,
        token: &str,
        id: &str,
        body: &TenderTypeBody,
    ) -> crate::BoxFuture<TenderType> {
        Box::pin(send(
            self.request(Method::PUT, &format!("/tender-types/{id}"), Some(token))
                .json(body),
        ))
    }

    fn delete_tender_type(&self, token: &str, id: &str) -> crate::BoxFuture<TenderType> {
        Box::pin(send(self.request(
            Method::DELETE,
            &format!("/tender-types/{id}"),
            Some(token),
        )))
    }

    fn returns(&self, token: &str, query: &DocQuery) -> crate::BoxFuture<ReturnPage> {
        Box::pin(send(
            self.request(Method::GET, "/returns", Some(token))
                .query(query),
        ))
    }

    fn sale_return(&self, token: &str, id: &str) -> crate::BoxFuture<SaleReturn> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/returns/{id}"),
            Some(token),
        )))
    }

    fn sales_settings(&self, token: &str) -> crate::BoxFuture<SalesSettings> {
        Box::pin(send(self.request(
            Method::GET,
            "/settings/sales",
            Some(token),
        )))
    }

    fn put_sales_settings(
        &self,
        token: &str,
        body: &SalesSettings,
    ) -> crate::BoxFuture<SalesSettings> {
        let request = self
            .request(Method::PUT, "/settings/sales", Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn all_cash_reasons(&self, token: &str) -> crate::BoxFuture<Vec<CashReason>> {
        let request = self
            .request(Method::GET, "/cash-reasons", Some(token))
            .query(&[("archived", "true")]);
        Box::pin(send(request))
    }

    fn put_cash_reason(
        &self,
        token: &str,
        id: &str,
        body: &CashReasonBody,
    ) -> crate::BoxFuture<CashReason> {
        let request = self
            .request(Method::PUT, &format!("/cash-reasons/{id}"), Some(token))
            .json(body);
        Box::pin(send(request))
    }

    fn delete_cash_reason(&self, token: &str, id: &str) -> crate::BoxFuture<CashReason> {
        Box::pin(send(self.request(
            Method::DELETE,
            &format!("/cash-reasons/{id}"),
            Some(token),
        )))
    }

    fn z_reports(&self, token: &str, query: &ZQuery) -> crate::BoxFuture<ZReportPage> {
        let request = self
            .request(Method::GET, "/z-reports", Some(token))
            .query(query);
        Box::pin(send(request))
    }

    fn z_report(&self, token: &str, id: &str) -> crate::BoxFuture<ZReport> {
        Box::pin(send(self.request(
            Method::GET,
            &format!("/z-reports/{id}"),
            Some(token),
        )))
    }

    fn force_close_session(
        &self,
        token: &str,
        session_id: &str,
        body: &CloseRequest,
        key: &str,
    ) -> crate::BoxFuture<CloseReport> {
        let request = self
            .request(
                Method::POST,
                &format!("/cashier-sessions/{session_id}/force-close"),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key)
            .json(body);
        Box::pin(send(request))
    }

    fn suppliers(&self, token: &str, include_archived: bool) -> crate::BoxFuture<Vec<Supplier>> {
        self.get_query("/suppliers", token, &[("archived", include_archived)])
    }

    fn put_supplier(
        &self,
        token: &str,
        id: &str,
        body: &SupplierBody,
    ) -> crate::BoxFuture<Supplier> {
        self.put_json(token, &format!("/suppliers/{id}"), body)
    }

    fn delete_supplier(&self, token: &str, id: &str) -> crate::BoxFuture<Supplier> {
        Box::pin(send(self.request(
            Method::DELETE,
            &format!("/suppliers/{id}"),
            Some(token),
        )))
    }

    fn supplier_items(
        &self,
        token: &str,
        supplier_id: &str,
    ) -> crate::BoxFuture<Vec<SupplierItem>> {
        self.get(token, &format!("/suppliers/{supplier_id}/items"))
    }

    fn put_supplier_item(
        &self,
        token: &str,
        supplier_id: &str,
        variant_id: &str,
        body: &SupplierItemBody,
    ) -> crate::BoxFuture<SupplierItem> {
        self.put_json(
            token,
            &format!("/suppliers/{supplier_id}/items/{variant_id}"),
            body,
        )
    }

    fn stock_reasons(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<StockReason>> {
        self.get_query("/stock-reasons", token, &[("archived", include_archived)])
    }

    fn put_stock_reason(
        &self,
        token: &str,
        id: &str,
        body: &StockReasonBody,
    ) -> crate::BoxFuture<StockReason> {
        self.put_json(token, &format!("/stock-reasons/{id}"), body)
    }

    fn delete_stock_reason(&self, token: &str, id: &str) -> crate::BoxFuture<StockReason> {
        Box::pin(send(self.request(
            Method::DELETE,
            &format!("/stock-reasons/{id}"),
            Some(token),
        )))
    }

    fn inventory_settings(&self, token: &str) -> crate::BoxFuture<InventorySettings> {
        self.get(token, "/settings/inventory")
    }

    fn put_inventory_settings(
        &self,
        token: &str,
        body: &InventorySettings,
    ) -> crate::BoxFuture<InventorySettings> {
        self.put_json(token, "/settings/inventory", body)
    }

    fn on_hand(&self, token: &str, query: &OnHandQuery) -> crate::BoxFuture<OnHandPage> {
        self.get_query("/stock/on-hand", token, query)
    }

    fn stock_card(
        &self,
        token: &str,
        variant_id: &str,
        query: &CardQuery,
    ) -> crate::BoxFuture<CardPage> {
        self.get_query(&format!("/stock/card/{variant_id}"), token, query)
    }

    fn lots(&self, token: &str, query: &LotQuery) -> crate::BoxFuture<Vec<Lot>> {
        self.get_query("/stock/lots", token, query)
    }

    fn payables(&self, token: &str, query: &PayableQuery) -> crate::BoxFuture<Vec<Payable>> {
        self.get_query("/payables", token, query)
    }

    fn purchase_orders(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<PurchaseOrder>> {
        self.get_query("/purchase-orders", token, query)
    }

    fn purchase_order(&self, token: &str, id: &str) -> crate::BoxFuture<PurchaseOrder> {
        self.get(token, &format!("/purchase-orders/{id}"))
    }

    fn put_purchase_order(
        &self,
        token: &str,
        id: &str,
        body: &PurchaseOrderBody,
    ) -> crate::BoxFuture<PurchaseOrder> {
        self.put_json(token, &format!("/purchase-orders/{id}"), body)
    }

    fn purchase_order_action(
        &self,
        token: &str,
        id: &str,
        action: OrderAction,
        key: &str,
    ) -> crate::BoxFuture<PurchaseOrder> {
        self.action(
            token,
            &format!("/purchase-orders/{id}/{}", action.as_str()),
            key,
            None,
        )
    }

    fn goods_receipts(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<GoodsReceipt>> {
        self.get_query("/goods-receipts", token, query)
    }

    fn goods_receipt(&self, token: &str, id: &str) -> crate::BoxFuture<GoodsReceipt> {
        self.get(token, &format!("/goods-receipts/{id}"))
    }

    fn put_goods_receipt(
        &self,
        token: &str,
        id: &str,
        body: &ReceiptBody,
    ) -> crate::BoxFuture<GoodsReceipt> {
        self.put_json(token, &format!("/goods-receipts/{id}"), body)
    }

    fn post_goods_receipt(
        &self,
        token: &str,
        id: &str,
        key: &str,
    ) -> crate::BoxFuture<GoodsReceipt> {
        self.action(token, &format!("/goods-receipts/{id}/post"), key, None)
    }

    fn void_goods_receipt(
        &self,
        token: &str,
        id: &str,
        key: &str,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<GoodsReceipt> {
        let mut request = self
            .request(
                Method::POST,
                &format!("/goods-receipts/{id}/void"),
                Some(token),
            )
            .header(IDEMPOTENCY_HEADER, key);
        if let Some(t) = override_token {
            request = request.header(OVERRIDE_HEADER, t);
        }
        Box::pin(send(request))
    }

    fn supplier_returns(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<SupplierReturn>> {
        self.get_query("/supplier-returns", token, query)
    }

    fn supplier_return(&self, token: &str, id: &str) -> crate::BoxFuture<SupplierReturn> {
        self.get(token, &format!("/supplier-returns/{id}"))
    }

    fn put_supplier_return(
        &self,
        token: &str,
        id: &str,
        body: &SupplierReturnBody,
    ) -> crate::BoxFuture<SupplierReturn> {
        self.put_json(token, &format!("/supplier-returns/{id}"), body)
    }

    fn post_supplier_return(
        &self,
        token: &str,
        id: &str,
        key: &str,
    ) -> crate::BoxFuture<SupplierReturn> {
        self.action(token, &format!("/supplier-returns/{id}/post"), key, None)
    }

    fn transfers(&self, token: &str, query: &StockDocQuery) -> crate::BoxFuture<Vec<Transfer>> {
        self.get_query("/transfers", token, query)
    }

    fn transfer(&self, token: &str, id: &str) -> crate::BoxFuture<Transfer> {
        self.get(token, &format!("/transfers/{id}"))
    }

    fn put_transfer(
        &self,
        token: &str,
        id: &str,
        body: &TransferBody,
    ) -> crate::BoxFuture<Transfer> {
        self.put_json(token, &format!("/transfers/{id}"), body)
    }

    fn transfer_action(
        &self,
        token: &str,
        id: &str,
        action: TransferAction,
        body: &MoveBody,
        key: &str,
    ) -> crate::BoxFuture<Transfer> {
        self.action(
            token,
            &format!("/transfers/{id}/{}", action.as_str()),
            key,
            Some(json!(body)),
        )
    }

    fn internal_transfers(
        &self,
        token: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<InternalTransfer>> {
        self.get_query("/internal-transfers", token, &branch_query(branch_id))
    }

    fn put_internal_transfer(
        &self,
        token: &str,
        id: &str,
        body: &InternalTransferBody,
    ) -> crate::BoxFuture<InternalTransfer> {
        self.put_json(token, &format!("/internal-transfers/{id}"), body)
    }

    fn adjustments(
        &self,
        token: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<Adjustment>> {
        self.get_query("/adjustments", token, &branch_query(branch_id))
    }

    fn adjustment(&self, token: &str, id: &str) -> crate::BoxFuture<Adjustment> {
        self.get(token, &format!("/adjustments/{id}"))
    }

    fn put_adjustment(
        &self,
        token: &str,
        id: &str,
        body: &AdjustmentBody,
    ) -> crate::BoxFuture<Adjustment> {
        self.put_json(token, &format!("/adjustments/{id}"), body)
    }

    fn opening_stocks(
        &self,
        token: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<OpeningStock>> {
        self.get_query("/opening-stock", token, &branch_query(branch_id))
    }

    fn put_opening_stock(
        &self,
        token: &str,
        id: &str,
        body: &OpeningBody,
    ) -> crate::BoxFuture<OpeningStock> {
        self.put_json(token, &format!("/opening-stock/{id}"), body)
    }

    fn opnames(&self, token: &str, branch_id: Option<&str>) -> crate::BoxFuture<Vec<Opname>> {
        self.get_query("/opnames", token, &branch_query(branch_id))
    }

    fn opname(&self, token: &str, id: &str) -> crate::BoxFuture<Opname> {
        self.get(token, &format!("/opnames/{id}"))
    }

    fn start_opname(&self, token: &str, id: &str, body: &OpnameBody) -> crate::BoxFuture<Opname> {
        self.put_json(token, &format!("/opnames/{id}"), body)
    }

    fn add_counts(
        &self,
        token: &str,
        id: &str,
        entries: &[CountEntry],
        key: &str,
    ) -> crate::BoxFuture<Opname> {
        self.action(
            token,
            &format!("/opnames/{id}/counts"),
            key,
            Some(json!({ "entries": entries })),
        )
    }

    fn post_opname(
        &self,
        token: &str,
        id: &str,
        uncounted: &str,
        key: &str,
    ) -> crate::BoxFuture<Opname> {
        self.action(
            token,
            &format!("/opnames/{id}/post"),
            key,
            Some(json!({ "uncounted": uncounted })),
        )
    }

    fn cancel_opname(&self, token: &str, id: &str, key: &str) -> crate::BoxFuture<Opname> {
        self.action(token, &format!("/opnames/{id}/cancel"), key, None)
    }

    fn reorder_rules(
        &self,
        token: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<ReorderRule>> {
        self.get_query("/reorder-rules", token, &branch_query(branch_id))
    }

    fn put_reorder_rule(
        &self,
        token: &str,
        branch_id: &str,
        variant_id: &str,
        body: &ReorderRuleBody,
    ) -> crate::BoxFuture<ReorderRule> {
        self.put_json(
            token,
            &format!("/reorder-rules/{branch_id}/{variant_id}"),
            body,
        )
    }

    fn reorder_suggestions(
        &self,
        token: &str,
        supplier_id: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<SuggestedLine>> {
        let mut query = branch_query(branch_id);
        query.push(("supplier_id", supplier_id.to_owned()));
        self.get_query("/reorder/suggestions", token, &query)
    }

    fn low_stock(
        &self,
        token: &str,
        branch_id: Option<&str>,
    ) -> crate::BoxFuture<Vec<SuggestedLine>> {
        self.get_query("/reorder/low-stock", token, &branch_query(branch_id))
    }
}

/// `?branch_id=` when given.
fn branch_query(branch_id: Option<&str>) -> Vec<(&'static str, String)> {
    branch_id
        .map(|b| ("branch_id", b.to_owned()))
        .into_iter()
        .collect()
}

impl HttpService {
    /// `GET {path}` with the bearer token.
    fn get<T: DeserializeOwned + 'static>(&self, token: &str, path: &str) -> crate::BoxFuture<T> {
        Box::pin(send(self.request(Method::GET, path, Some(token))))
    }

    /// `GET {path}?{query}` with the bearer token.
    fn get_query<T: DeserializeOwned + 'static, Q: serde::Serialize + ?Sized>(
        &self,
        path: &str,
        token: &str,
        query: &Q,
    ) -> crate::BoxFuture<T> {
        Box::pin(send(
            self.request(Method::GET, path, Some(token)).query(query),
        ))
    }

    /// `PUT {path}` with a JSON body.
    fn put_json<T: DeserializeOwned + 'static, B: serde::Serialize + ?Sized>(
        &self,
        token: &str,
        path: &str,
        body: &B,
    ) -> crate::BoxFuture<T> {
        Box::pin(send(
            self.request(Method::PUT, path, Some(token)).json(body),
        ))
    }

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
