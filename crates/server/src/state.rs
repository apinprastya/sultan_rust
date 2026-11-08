//! Shared server state (plan Phase 4): the database, the node role and the clock.
//!
//! The server never reads the system time directly: every handler asks [`AppState::now`], so tests
//! drive lockout, pairing-code expiry and idle session expiry with a [`ManualClock`]. This keeps
//! the pattern of `core`, where time is passed in (`sultan_core::time`).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sultan_core::time::Timestamp;
use sultan_db::Db;

use crate::events::EventHub;
use crate::movements::{NoSales, SessionMovements};

/// What this node is (`docs/plan.md` §1). Decides whether HQ-owned master data may be written
/// here (spec 2.1 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeRole {
    /// Head office: owns master data.
    Hq,
    /// Branch server: holds a read-only replica of master data.
    Branch,
    /// HQ + branch + register in one process: it is HQ, so writes are allowed.
    Standalone,
}

impl NodeRole {
    /// Whether HQ-owned master data (branches, registers, locations, users, roles) may be written
    /// on this node. Only a branch node refuses (spec 2.1 §1: `403 read_only_replica`).
    #[must_use]
    pub const fn owns_master_data(self) -> bool {
        !matches!(self, Self::Branch)
    }
}

/// A source of the current time.
pub trait Clock: Send + Sync + fmt::Debug {
    /// The current time.
    fn now(&self) -> Timestamp;
}

/// The real wall clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        Timestamp::from_unix_millis(millis)
    }
}

/// A clock that only moves when told to. Clones share the same time, so a test keeps one clone
/// and hands another to [`AppState`].
#[derive(Debug, Clone, Default)]
pub struct ManualClock(Arc<AtomicI64>);

impl ManualClock {
    /// A clock standing at `start`.
    #[must_use]
    pub fn new(start: Timestamp) -> Self {
        Self(Arc::new(AtomicI64::new(start.unix_millis())))
    }

    /// Sets the time.
    pub fn set(&self, now: Timestamp) {
        self.0.store(now.unix_millis(), Ordering::SeqCst);
    }

    /// Moves the time forward by `millis`.
    pub fn advance(&self, millis: i64) {
        self.0.fetch_add(millis, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        Timestamp::from_unix_millis(self.0.load(Ordering::SeqCst))
    }
}

/// Default idle expiry of a login session: 8 hours, about one shift (spec 2.1 §4 makes it a
/// setting). The register's own auto-lock after N idle minutes is a client concern.
pub const DEFAULT_SESSION_IDLE_MS: i64 = 8 * 60 * 60 * 1_000;

/// State shared by all handlers.
#[derive(Debug, Clone)]
pub struct AppState {
    /// The database.
    pub db: Db,
    /// What this node is.
    pub node_role: NodeRole,
    /// Where handlers get the current time.
    pub clock: Arc<dyn Clock>,
    /// Idle expiry of login sessions; every authenticated request moves it forward (spec 2.1 §4).
    pub session_idle_ms: i64,
    /// The event hub behind `GET /events` (plan Phase 6). Clones of the state share it.
    pub events: EventHub,
    /// Sales/refunds/cash movements of a cashier session for the close report; [`NoSales`] until
    /// the sales phase (see [`crate::movements`]).
    pub movements: Arc<dyn SessionMovements>,
}

impl AppState {
    /// State with the default session idle expiry.
    #[must_use]
    pub fn new(db: Db, node_role: NodeRole, clock: Arc<dyn Clock>) -> Self {
        Self {
            db,
            node_role,
            clock,
            session_idle_ms: DEFAULT_SESSION_IDLE_MS,
            events: EventHub::default(),
            movements: Arc::new(NoSales),
        }
    }

    /// Uses `events` as the hub, so several states (e.g. one per test request) share sockets.
    #[must_use]
    pub fn with_events(mut self, events: EventHub) -> Self {
        self.events = events;
        self
    }

    /// Sets where cashier-session movements come from (the sales seam, [`crate::movements`]).
    #[must_use]
    pub fn with_movements(mut self, movements: Arc<dyn SessionMovements>) -> Self {
        self.movements = movements;
        self
    }

    /// Sets the session idle expiry.
    #[must_use]
    pub const fn with_session_idle_ms(mut self, millis: i64) -> Self {
        self.session_idle_ms = millis;
        self
    }

    /// The current time.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        self.clock.now()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_branch_nodes_refuse_master_data_writes() {
        let cases = [
            (NodeRole::Hq, true),
            (NodeRole::Standalone, true),
            (NodeRole::Branch, false),
        ];
        for (role, owns) in cases {
            assert_eq!(role.owns_master_data(), owns, "{role:?}");
        }
    }

    #[test]
    fn manual_clock_is_shared_between_clones() {
        let clock = ManualClock::new(Timestamp::from_unix_millis(1_000));
        let other = clock.clone();
        clock.advance(500);
        assert_eq!(other.now(), Timestamp::from_unix_millis(1_500));
        other.set(Timestamp::from_unix_millis(7));
        assert_eq!(clock.now(), Timestamp::from_unix_millis(7));
        assert_eq!(ManualClock::default().now().unix_millis(), 0);
    }

    #[test]
    fn system_clock_is_after_2026() {
        // 2026-01-01T00:00:00Z.
        assert!(SystemClock.now().unix_millis() > 1_767_225_600_000);
    }

    #[tokio::test]
    async fn state_reads_its_clock_and_idle_setting() {
        let db = Db::connect("sqlite::memory:").await.unwrap();
        let clock = ManualClock::new(Timestamp::from_unix_millis(42));
        let state = AppState::new(db, NodeRole::Hq, Arc::new(clock.clone()));
        assert_eq!(state.session_idle_ms, DEFAULT_SESSION_IDLE_MS);
        assert_eq!(state.now(), Timestamp::from_unix_millis(42));
        let hub = EventHub::default();
        let mut rx = hub.subscribe();
        let state = state
            .with_session_idle_ms(5)
            .with_events(hub)
            .with_movements(Arc::new(NoSales));
        assert_eq!(state.session_idle_ms, 5);
        state.events.publish(crate::events::Event::user_changed(
            sultan_core::ids::UserId::generate(),
            vec![],
        ));
        assert!(rx.try_recv().is_ok());
    }
}
