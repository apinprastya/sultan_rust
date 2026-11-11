//! Event notifications: `GET /api/v1/events` (WebSocket) and the in-process broadcast hub
//! (`docs/plan.md` §1, spec 2.1 §4, §8; plan Phase 6).
//!
//! **Notifications only.** An event says *that* something changed; the client then fetches the
//! data over REST. No business write goes over the WebSocket, and the server ignores what clients
//! send (except close frames).
//!
//! # Authentication
//! The upgrade request is authenticated like any other request: a bearer session token
//! ([`UserSession`]), plus the register's device token for a PIN session. A register may also
//! connect with its device token alone (no `Authorization`; spec 2.1 §8, decided 2026-11-11, C3b):
//! such a socket receives only [`Audience::Everyone`] events (`catalog.changed`) and
//! `events.lagged`; session-, branch- and user-scoped events need a session. Native clients (the
//! register app)
//! send the usual `Authorization` and `X-Register-Token` headers. Browsers cannot set headers on a
//! WebSocket, so the tokens may instead travel as subprotocols:
//! `Sec-WebSocket-Protocol: sultan.events.v1, bearer.<session token>, register.<device token>`;
//! the server then answers with the subprotocol [`SUBPROTOCOL`]. Tokens are never accepted in the
//! query string (it ends up in logs). An unauthenticated upgrade is answered with the usual
//! `401` problem and no upgrade.
//!
//! # Messages
//! Text frames `{"type": "<event>", "data": {...}}`. Each type with its data and who receives
//! it:
//!
//! - `session.revoked`: `user_id`, `reason` (`logout` / `user_deactivated`), `this_session`.
//!   Sockets of that user; a socket whose own session was revoked is closed after the message.
//! - `cashier_session.opened`: `session_id`, `register_id`, `branch_id`, `user_id`. Callers who
//!   may see the branch (spec 2.1 §3).
//! - `cashier_session.closed`: the same, plus `forced`. Callers who may see the branch.
//! - `user.changed`: `user_id`. The user, and callers who may see a branch where the user has
//!   (or had) a role.
//! - `catalog.changed`: `scope` (`product` / `category` / `brand` / `settings`), plus
//!   `product_id`, `category_id` or `brand_id`. Every socket: the catalog is company-wide
//!   (spec 2.2 §11), so registers drop cached lookups.
//! - `events.lagged`: `missed`. A socket that fell behind; refetch everything.
//!
//! Before each delivery the socket's own login session is checked again; once it is revoked,
//! expired or its user deactivated, the socket is closed. A device-token socket checks its token
//! instead (closed once the register is unpaired or archived). Branch visibility is evaluated per
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
use sultan_db::{TokenHash, auth, devices, users};
use tokio::sync::broadcast;

use crate::access::Caller;
use crate::error::ApiError;
use crate::extract::{REGISTER_TOKEN_HEADER, RegisterCtx, UserSession};
use crate::state::AppState;
use crate::token;

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
    /// Every socket whose session is still valid (company-wide data such as the catalog).
    Everyone,
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

    /// `catalog.changed` (spec 2.2 §11) for every socket. `data` carries the `scope` and the
    /// changed entity's ID.
    #[must_use]
    pub const fn catalog_changed(data: Value) -> Self {
        Self {
            kind: "catalog.changed",
            data,
            audience: Audience::Everyone,
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

/// Who holds an events socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listener {
    /// A user session (back office, or a PIN session at a register).
    User(Box<UserSession>),
    /// A register with its device token only: [`Audience::Everyone`] events only.
    Device {
        /// The register and its branch.
        register: RegisterCtx,
        /// Hash of the device token, checked again before each delivery.
        token_hash: TokenHash,
    },
}

/// The [`Listener`] of the upgrade request; tokens may also come as subprotocols (see the module
/// docs). With `Authorization` it is a [`UserSession`]; without, `X-Register-Token` alone.
#[derive(Debug, Clone)]
pub struct EventsAuth(pub Listener);

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
        if !parts.headers.contains_key(AUTHORIZATION)
            && let Some(token) = parts.headers.get(REGISTER_TOKEN_HEADER)
        {
            let token_hash = token::hash(
                token
                    .to_str()
                    .map_err(|_| ApiError::invalid_device_token())?,
            );
            let register = RegisterCtx::from_request_parts(parts, state).await?;
            return Ok(Self(Listener::Device {
                register,
                token_hash,
            }));
        }
        UserSession::from_request_parts(parts, state)
            .await
            .map(|session| Self(Listener::User(Box::new(session))))
    }
}

/// `GET /api/v1/events`: upgrades to a WebSocket of event notifications (see the module docs).
///
/// # Errors
/// `401 invalid_session` / `invalid_device_token` before the upgrade.
#[utoipa::path(get, path = "/api/v1/events", tag = "events",
    security(("bearer" = []), ("bearer" = [], "register_token" = []), ("register_token" = [])),
    responses((status = 101, description = "Switching to the WebSocket protocol; text frames \
        `{\"type\", \"data\"}`: session.revoked, cashier_session.opened, cashier_session.closed, \
        user.changed, catalog.changed, events.lagged. A device token alone receives \
        catalog.changed and events.lagged only."),
        (status = 401, body = crate::openapi::Problem)))]
pub async fn events(
    State(state): State<AppState>,
    EventsAuth(listener): EventsAuth,
    ws: WebSocketUpgrade,
) -> Response {
    let receiver = state.events.subscribe();
    ws.protocols([SUBPROTOCOL])
        .on_upgrade(move |socket| run(socket, state, listener, receiver))
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
/// A company-wide scope ([`BranchScope::All`], e.g. [`Audience::Everyone`]) is seen by every
/// valid session, so the caller's permissions are not loaded for it.
async fn sees_any(
    state: &AppState,
    me: &UserSession,
    scopes: &[BranchScope],
) -> Result<bool, ApiError> {
    if scopes.contains(&BranchScope::All) {
        return Ok(true);
    }
    let view = Caller::load(&state.db, me).await?.branch_view();
    Ok(scopes.iter().any(|s| match s {
        BranchScope::All => true,
        BranchScope::Branch(b) => view.allows(*b),
    }))
}

/// What a device-token socket does with an event: [`Audience::Everyone`] only, while the device
/// token is still valid.
async fn decide_device(
    state: &AppState,
    token_hash: &TokenHash,
    event: &Event,
) -> Result<Delivery, ApiError> {
    if event.audience != Audience::Everyone {
        return Ok(Delivery::Skip);
    }
    if devices::find_device(&state.db, token_hash).await?.is_none() {
        return Ok(Delivery::Close);
    }
    Ok(Delivery::Send(message(event.kind, &event.data)))
}

async fn decide_for(
    state: &AppState,
    listener: &Listener,
    event: &Event,
) -> Result<Delivery, ApiError> {
    match listener {
        Listener::User(me) => decide(state, me, event).await,
        Listener::Device { token_hash, .. } => decide_device(state, token_hash, event).await,
    }
}

async fn decide(state: &AppState, me: &UserSession, event: &Event) -> Result<Delivery, ApiError> {
    let scopes = match &event.audience {
        Audience::Sessions { user, session } => return Ok(revoked(me, event, *user, *session)),
        Audience::Branch(branch) => vec![BranchScope::Branch(*branch)],
        Audience::Everyone => vec![BranchScope::All],
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
    me: Listener,
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
                    Ok(event) => decide_for(&state, &me, &event).await.unwrap_or(Delivery::Close),
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

    /// A session for an unknown user (no database row is needed: nothing is looked up).
    fn session() -> UserSession {
        let now = sultan_core::time::Timestamp::from_unix_millis(0);
        let user = UserId::generate();
        UserSession {
            token_hash: [7; 32],
            session: auth::AuthSession {
                token_hash: [7; 32],
                user,
                register: None,
                branch: BranchId::generate(),
                created_at: now,
                last_seen: now,
                expires_at: now,
                revoked_at: None,
            },
            user: users::User {
                id: user,
                username: "x".into(),
                display_name: "X".into(),
                pin_hash: None,
                password_hash: None,
                active: true,
            },
        }
    }

    #[tokio::test]
    async fn company_wide_scopes_skip_the_permission_lookup() {
        // No migrations: any permission query fails, so `Ok(true)` proves none ran.
        let db = sultan_db::Db::connect("sqlite::memory:").await.unwrap();
        let clock =
            crate::state::ManualClock::new(sultan_core::time::Timestamp::from_unix_millis(0));
        let state = AppState::new(db, crate::state::NodeRole::Hq, std::sync::Arc::new(clock));
        let me = session();
        assert!(sees_any(&state, &me, &[BranchScope::All]).await.unwrap());
        let mixed = [BranchScope::Branch(BranchId::generate()), BranchScope::All];
        assert!(sees_any(&state, &me, &mixed).await.unwrap());
        // A branch scope does look the permissions up (and fails here).
        let branch = [BranchScope::Branch(BranchId::generate())];
        assert!(sees_any(&state, &me, &branch).await.is_err());
    }

    #[test]
    fn message_shape() {
        assert_eq!(
            serde_json::from_str::<Value>(&message("a.b", &json!({ "x": 1 }))).unwrap(),
            json!({ "type": "a.b", "data": { "x": 1 } })
        );
    }
}
