//! Event notifications: `GET /api/v1/events` (WebSocket) and the in-process broadcast hub
//! (`docs/plan.md` §1, spec 2.1 §4, §8; plan Phase 6).
//!
//! **Notifications only.** An event says *that* something changed; the client then fetches the
//! data over REST. No business write goes over the WebSocket, and the server ignores what clients
//! send (except close frames).
//!
//! # Authentication
//! The upgrade request is authenticated like any other request ([`UserSession`]): a bearer session
//! token, plus the register's device token for a PIN session. Native clients (the register app)
//! send the usual `Authorization` and `X-Register-Token` headers. Browsers cannot set headers on a
//! WebSocket, so the tokens may instead travel as subprotocols:
//! `Sec-WebSocket-Protocol: sultan.events.v1, bearer.<session token>, register.<device token>`;
//! the server then answers with the subprotocol [`SUBPROTOCOL`]. Tokens are never accepted in the
//! query string (it ends up in logs). An unauthenticated upgrade is answered with the usual
//! `401` problem and no upgrade.
//!
//! # Messages
//! Text frames `{"type": "<event>", "data": {...}}`:
//!
//! | Type | Data | Who receives it |
//! |---|---|---|
//! | `session.revoked` | `user_id`, `reason` (`logout` / `user_deactivated`), `this_session` | sockets of that user; a socket whose own session was revoked is closed after the message |
//! | `cashier_session.opened` | `session_id`, `register_id`, `branch_id`, `user_id` | callers who may see the branch (spec 2.1 §3) |
//! | `cashier_session.closed` | same, plus `forced` | callers who may see the branch |
//! | `user.changed` | `user_id` | the user, and callers who may see a branch where the user has (or had) a role |
//! | `events.lagged` | `missed` | a socket that fell behind; refetch everything |
//!
//! Before each delivery the socket's own login session is checked again; once it is revoked,
//! expired or its user deactivated, the socket is closed. Branch visibility is evaluated per
//! event, so role changes apply at once.

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade, close_code};
use axum::extract::{FromRequestParts, State};
use axum::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue};
use axum::response::Response;
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, UserId};
use sultan_db::{TokenHash, auth, users};
use tokio::sync::broadcast;

use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{REGISTER_TOKEN_HEADER, UserSession};
use crate::state::AppState;

/// The subprotocol the server selects for browser clients.
pub const SUBPROTOCOL: &str = "sultan.events.v1";

/// How many events a slow socket may fall behind before it gets `events.lagged`.
const CAPACITY: usize = 256;

/// Who may receive an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Audience {
    /// `session.revoked`: the sockets of `user`. `session` is the revoked login session, or `None`
    /// when all of the user's sessions were revoked.
    Sessions {
        /// The user.
        user: UserId,
        /// The revoked session (hash of its token), or all.
        session: Option<TokenHash>,
    },
    /// Callers who may see this branch.
    Branch(BranchId),
    /// `user.changed`: the user themselves, and callers who may see any of `scopes`.
    User {
        /// The changed user.
        user: UserId,
        /// The branch scopes of the user's assignments before and after the change.
        scopes: Vec<BranchScope>,
    },
}

/// One event.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// The `type`, e.g. `cashier_session.opened`.
    pub kind: &'static str,
    /// The `data`.
    pub data: Value,
    /// Who may receive it.
    pub audience: Audience,
}

impl Event {
    /// `session.revoked` (spec 2.1 §4).
    #[must_use]
    pub fn session_revoked(user: UserId, session: Option<TokenHash>, reason: &str) -> Self {
        Self {
            kind: "session.revoked",
            data: json!({ "user_id": user.to_string(), "reason": reason }),
            audience: Audience::Sessions { user, session },
        }
    }

    /// `user.changed` after a user `PUT` or `DELETE`.
    #[must_use]
    pub fn user_changed(user: UserId, scopes: Vec<BranchScope>) -> Self {
        Self {
            kind: "user.changed",
            data: json!({ "user_id": user.to_string() }),
            audience: Audience::User { user, scopes },
        }
    }

    /// A branch-scoped event.
    #[must_use]
    pub const fn for_branch(kind: &'static str, branch: BranchId, data: Value) -> Self {
        Self {
            kind,
            data,
            audience: Audience::Branch(branch),
        }
    }
}

/// The broadcast hub. Clones share the channel.
#[derive(Debug, Clone)]
pub struct EventHub {
    sender: broadcast::Sender<Event>,
}

impl Default for EventHub {
    fn default() -> Self {
        Self {
            sender: broadcast::channel(CAPACITY).0,
        }
    }
}

impl EventHub {
    /// Sends `event` to every connected socket (each decides whether its caller may see it).
    /// Publish **after** the transaction that made the change has committed.
    pub fn publish(&self, event: Event) {
        // An error only means nobody is listening.
        let _ = self.sender.send(event);
    }

    /// A new receiver.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}

/// [`UserSession`] for the upgrade request; tokens may also come as subprotocols (see the module
/// docs).
#[derive(Debug, Clone)]
pub struct EventsAuth(pub UserSession);

/// Copies `bearer.<token>` / `register.<token>` subprotocols into the headers the session
/// extractor reads, unless those headers are already present.
fn tokens_from_subprotocols(headers: &mut HeaderMap) {
    let offered: Vec<String> = headers
        .get_all(SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_owned())
        .collect();
    for protocol in offered {
        let (name, value) = if let Some(token) = protocol.strip_prefix("bearer.") {
            (AUTHORIZATION.as_str(), format!("Bearer {token}"))
        } else if let Some(token) = protocol.strip_prefix("register.") {
            (REGISTER_TOKEN_HEADER, token.to_owned())
        } else {
            continue;
        };
        if !headers.contains_key(name)
            && let Ok(value) = HeaderValue::from_str(&value)
        {
            headers.insert(name, value);
        }
    }
}

impl FromRequestParts<AppState> for EventsAuth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        tokens_from_subprotocols(&mut parts.headers);
        UserSession::from_request_parts(parts, state)
            .await
            .map(Self)
    }
}

/// `GET /api/v1/events`: upgrades to a WebSocket of event notifications (see the module docs).
///
/// # Errors
/// `401 invalid_session` / `invalid_device_token` before the upgrade.
#[utoipa::path(get, path = "/api/v1/events", tag = "events",
    security(("bearer" = []), ("bearer" = [], "register_token" = [])),
    responses((status = 101, description = "Switching to the WebSocket protocol; text frames \
        `{\"type\", \"data\"}`: session.revoked, cashier_session.opened, cashier_session.closed, \
        user.changed, events.lagged"),
        (status = 401, body = crate::openapi::Problem)))]
pub async fn events(
    State(state): State<AppState>,
    EventsAuth(session): EventsAuth,
    ws: WebSocketUpgrade,
) -> Response {
    let receiver = state.events.subscribe();
    ws.protocols([SUBPROTOCOL])
        .on_upgrade(move |socket| run(socket, state, session, receiver))
}

/// What one socket does with one event.
#[derive(Debug, Clone, PartialEq)]
enum Delivery {
    /// Not for this socket.
    Skip,
    /// Send this text.
    Send(String),
    /// Send this text, then close (the socket's own session was revoked).
    SendAndClose(String),
    /// Close without sending (the socket's session is no longer valid).
    Close,
}

fn message(kind: &str, data: &Value) -> String {
    json!({ "type": kind, "data": data }).to_string()
}

/// Whether the socket's login session and user are still valid.
async fn still_valid(state: &AppState, me: &UserSession) -> Result<bool, ApiError> {
    let now = state.now();
    let session_ok = auth::find_session(&state.db, &me.token_hash)
        .await?
        .is_some_and(|s| s.is_active(now));
    if !session_ok {
        return Ok(false);
    }
    Ok(users::get_user(&state.db, me.user.id)
        .await?
        .is_some_and(|u| u.active))
}

fn revoked(me: &UserSession, event: &Event, user: UserId, session: Option<TokenHash>) -> Delivery {
    if user != me.user.id {
        return Delivery::Skip;
    }
    let this_session = session.is_none_or(|s| s == me.token_hash);
    let mut data = event.data.clone();
    data["this_session"] = this_session.into();
    let text = message(event.kind, &data);
    if this_session {
        Delivery::SendAndClose(text)
    } else {
        Delivery::Send(text)
    }
}

/// Whether the socket's caller may see an event for `scopes` (spec 2.1 §3 branch visibility).
async fn sees_any(
    state: &AppState,
    me: &UserSession,
    scopes: &[BranchScope],
) -> Result<bool, ApiError> {
    let view = Caller::load(&state.db, me).await?.branch_view();
    Ok(scopes.iter().any(|s| match s {
        BranchScope::All => true,
        BranchScope::Branch(b) => view.allows(*b),
    }))
}

async fn decide(state: &AppState, me: &UserSession, event: &Event) -> Result<Delivery, ApiError> {
    let scopes = match &event.audience {
        Audience::Sessions { user, session } => return Ok(revoked(me, event, *user, *session)),
        Audience::Branch(branch) => vec![BranchScope::Branch(*branch)],
        Audience::User { scopes, .. } => scopes.clone(),
    };
    if !still_valid(state, me).await? {
        return Ok(Delivery::Close);
    }
    let own = matches!(&event.audience, Audience::User { user, .. } if *user == me.user.id);
    Ok(if own || sees_any(state, me, &scopes).await? {
        Delivery::Send(message(event.kind, &event.data))
    } else {
        Delivery::Skip
    })
}

async fn close(socket: &mut WebSocket) {
    let frame = CloseFrame {
        code: close_code::POLICY,
        reason: "session ended".into(),
    };
    let _ = socket.send(Message::Close(Some(frame))).await;
}

/// The socket loop: forwards events until the client leaves or the session ends.
async fn run(
    mut socket: WebSocket,
    state: AppState,
    me: UserSession,
    mut receiver: broadcast::Receiver<Event>,
) {
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_)) | Err(_)) | None => return,
                Some(Ok(_)) => {}
            },
            event = receiver.recv() => {
                let delivery = match event {
                    Ok(event) => decide(&state, &me, &event).await.unwrap_or(Delivery::Close),
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        Delivery::Send(message("events.lagged", &json!({ "missed": missed })))
                    }
                    Err(broadcast::error::RecvError::Closed) => Delivery::Close,
                };
                match delivery {
                    Delivery::Skip => {}
                    Delivery::Send(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            return;
                        }
                    }
                    Delivery::SendAndClose(text) => {
                        let _ = socket.send(Message::Text(text.into())).await;
                        close(&mut socket).await;
                        return;
                    }
                    Delivery::Close => {
                        close(&mut socket).await;
                        return;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subprotocol_tokens_become_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("sultan.events.v1, bearer.abc, register.dev, other"),
        );
        tokens_from_subprotocols(&mut headers);
        assert_eq!(headers[AUTHORIZATION], "Bearer abc");
        assert_eq!(headers[REGISTER_TOKEN_HEADER], "dev");

        // Real headers win over subprotocols.
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer real"));
        headers.insert(
            SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("bearer.fake"),
        );
        tokens_from_subprotocols(&mut headers);
        assert_eq!(headers[AUTHORIZATION], "Bearer real");
        assert!(!headers.contains_key(REGISTER_TOKEN_HEADER));
    }

    #[test]
    fn hub_without_listeners_does_not_fail() {
        let hub = EventHub::default();
        hub.publish(Event::user_changed(UserId::generate(), vec![]));
        let mut rx = hub.subscribe();
        let event = Event::for_branch("x", BranchId::generate(), json!({}));
        hub.clone().publish(event.clone());
        assert_eq!(rx.try_recv().unwrap(), event);
    }

    #[test]
    fn message_shape() {
        assert_eq!(
            serde_json::from_str::<Value>(&message("a.b", &json!({ "x": 1 }))).unwrap(),
            json!({ "type": "a.b", "data": { "x": 1 } })
        );
    }
}
