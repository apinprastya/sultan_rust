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
//! - [`ClientError`]: problem+json answers become [`Problem`], which keeps the stable `code` and
//!   the extension members (`locked_until`, `permission`, `limit`, `conflict`, `session_id`, …).
//!
//! Every method returns a `'static` future: implementations copy what they need before the
//! future starts, so the UI can spawn it or block on it. The fake does all its work when the
//! method is called and returns a ready future, which keeps UI tests deterministic.
//!
//! Amounts are minor units (`i64`, Rupiah minor unit = 1, `docs/plan.md` §1); times are unix ms.

pub mod error;
pub mod fake;
pub mod http;
pub mod types;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

pub use error::{ClientError, Problem};
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

    /// `GET /events` (WebSocket): calls `sink` for each event until the socket closes.
    fn watch_events(&self, token: &str, sink: EventSink) -> BoxFuture<()>;
}
