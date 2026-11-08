//! [`Service`] over HTTP (`reqwest`) and the events WebSocket (`tokio-tungstenite`), spec 2.1 §2,
//! §4–§6, §8.
//!
//! - Every request carries the device token as `X-Register-Token` (spec 2.1 §2); calls made in a
//!   login session add `Authorization: Bearer <session token>`.
//! - The cashier-session `POST`s send `Idempotency-Key`, so a retry after a lost answer gets the
//!   original answer instead of `409` (`docs/plan.md` §1, §9 case 17). `POST /overrides` takes no
//!   key: its answer is a secret the server does not store.
//! - Error answers are parsed as problem+json ([`Problem`]); anything else is
//!   [`ClientError::Unexpected`]. Transport failures are [`ClientError::Network`].
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

use crate::{
    Branch, CashierSession, ClientError, CloseReport, CloseRequest, EventSink, LockUser, Login,
    OverrideGrant, OverrideRequest, Problem, ProtectedAction, ProtectedResult, ServerEvent,
    Service, SessionDetail, SessionPage, SessionQuery,
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

    /// The API base URL.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.0.base
    }

    fn request(&self, method: Method, path: &str, token: Option<&str>) -> RequestBuilder {
        let request = self
            .0
            .http
            .request(method, format!("{}{path}", self.0.base))
            .header(REGISTER_TOKEN_HEADER, &self.0.device_token);
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
        let bearer = format!("Bearer {token}");
        let device = self.0.device_token.clone();
        Box::pin(async move {
            let mut request = url
                .as_str()
                .into_client_request()
                .map_err(|e| ClientError::Unexpected(e.to_string()))?;
            let headers = request.headers_mut();
            for (name, value) in [("authorization", bearer), (REGISTER_TOKEN_HEADER, device)] {
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
