//! An in-memory [`Service`] for UI tests (plan Phase 8). It follows the server's rules closely
//! enough to drive every register screen: PIN lockout after 5 failures (spec 2.1 §4), one open
//! session per register and per user with `409 session_already_open` (§5), the blind-close report
//! (§5), and manager overrides with self-approval, limits and single-use tokens (§6). Sessions get
//! numbers like the server's (`JKT01-R1-S-<YYMMDD>-<NN>`, Asia/Jakarta, Phase 9) and are kept
//! with their close reports for [`Service::session`] / [`Service::sessions`]: a user reads their
//! own sessions, a user marked [`FakeUser::with_reports`] (Budi by default) reads all.
//!
//! Every call does its work immediately and returns a ready future, and is recorded in
//! [`FakeService::calls`] so tests can check what the UI sent (for example the `X-Override`
//! retry). [`FakeService::fail_next`] injects one error; [`FakeService::emit`] pushes an event to
//! the sink given to [`Service::watch_events`].

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use sultan_core::doc_number::{BranchTimeZone, LocalDay, session_number};
use sultan_core::time::Timestamp;

use crate::{
    Branch, CashierSession, ClientError, CloseReport, CloseRequest, EventSink, LockUser, Login,
    OverrideGrant, OverrideRequest, Problem, ProtectedAction, ProtectedResult, ServerEvent,
    Service, SessionDetail, SessionPage, SessionQuery, TenderTotal,
};

/// Failures before a lockout (spec 2.1 §4).
pub const MAX_FAILURES: u32 = 5;
/// Lockout length (spec 2.1 §4).
pub const LOCKOUT_MS: i64 = 5 * 60 * 1_000;
/// The fake's register.
pub const REGISTER_ID: &str = "register-1";
/// The fake's branch.
pub const BRANCH_ID: &str = "branch-1";

/// A user of the fake: PIN and the limit (basis points) of the demo permission `sale.discount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeUser {
    /// User ID.
    pub id: String,
    /// Display name.
    pub name: String,
    /// PIN.
    pub pin: String,
    /// Limit of `sale.discount` (basis points).
    pub discount_limit: i64,
    /// Holds `report.sales`: may read every session, not only their own.
    pub reports: bool,
}

impl FakeUser {
    /// A user.
    #[must_use]
    pub fn new(id: &str, name: &str, pin: &str, discount_limit: i64) -> Self {
        Self {
            id: id.to_owned(),
            name: name.to_owned(),
            pin: pin.to_owned(),
            discount_limit,
            reports: false,
        }
    }

    /// The same user holding `report.sales`.
    #[must_use]
    pub const fn with_reports(mut self) -> Self {
        self.reports = true;
        self
    }
}

/// One recorded call: the method and its interesting arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// Method name, e.g. `demo_protected`.
    pub method: &'static str,
    /// Arguments, e.g. the `X-Override` token or the idempotency key.
    pub detail: String,
}

#[derive(Default)]
struct State {
    users: Vec<FakeUser>,
    now: i64,
    failures: HashMap<String, u32>,
    locked_until: HashMap<String, i64>,
    tokens: HashMap<String, String>,
    serial: u32,
    session: Option<CashierSession>,
    history: Vec<(CashierSession, Option<CloseReport>)>,
    cash_sales: i64,
    opened_by_key: HashMap<String, CashierSession>,
    overrides: HashMap<String, (String, Option<i64>, String, String)>,
    calls: Vec<Call>,
    fail_next: Option<ClientError>,
    fail_on: Option<(&'static str, ClientError)>,
    sink: Option<EventSink>,
}

/// The fake service; see the module docs.
pub struct FakeService(Mutex<State>);

fn api(status: u16, code: &str) -> ClientError {
    ClientError::Api(Problem::new(status, code, code))
}

fn ready<T: Send + 'static>(result: Result<T, ClientError>) -> crate::BoxFuture<T> {
    Box::pin(std::future::ready(result))
}

impl Default for FakeService {
    fn default() -> Self {
        Self::new(vec![
            FakeUser::new("u-ani", "Ani", "1234", 1_000),
            FakeUser::new("u-budi", "Budi", "9999", 2_000).with_reports(),
            FakeUser::new("u-citra", "Citra", "5555", 1_200),
            FakeUser::new("u-dedi", "Dedi", "4321", 1_000),
        ])
    }
}

impl FakeService {
    /// A fake with these users. Defaults: Ani (cashier, 10%, PIN 1234), Budi (manager, 20%,
    /// 9999), Citra (supervisor, 12%, 5555), Dedi (cashier, 10%, 4321).
    #[must_use]
    pub fn new(users: Vec<FakeUser>) -> Self {
        Self(Mutex::new(State {
            users,
            now: 1_800_000_000_000,
            ..State::default()
        }))
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the fake's clock (unix ms), used for lockouts and open times.
    pub fn set_now(&self, now: i64) {
        self.state().now = now;
    }

    /// Cash sales the close report adds to the expected cash.
    pub fn set_cash_sales(&self, amount: i64) {
        self.state().cash_sales = amount;
    }

    /// Makes the next call fail with `error`.
    pub fn fail_next(&self, error: ClientError) {
        self.state().fail_next = Some(error);
    }

    /// Makes the next call of `method` fail with `error`.
    pub fn fail_on(&self, method: &'static str, error: ClientError) {
        self.state().fail_on = Some((method, error));
    }

    /// Opens a session for `user_id` directly (someone else on this register).
    pub fn open_for(&self, user_id: &str, opening_cash: i64) -> CashierSession {
        let mut s = self.state();
        let session = s.new_session(user_id, opening_cash);
        s.session = Some(session.clone());
        s.history.push((session.clone(), None));
        session
    }

    /// The open session, if any.
    #[must_use]
    pub fn session(&self) -> Option<CashierSession> {
        self.state().session.clone()
    }

    /// The calls so far.
    #[must_use]
    pub fn calls(&self) -> Vec<Call> {
        self.state().calls.clone()
    }

    /// The calls of one method.
    #[must_use]
    pub fn calls_of(&self, method: &str) -> Vec<String> {
        self.calls()
            .into_iter()
            .filter(|c| c.method == method)
            .map(|c| c.detail)
            .collect()
    }

    /// Whether a sink is registered by [`Service::watch_events`].
    #[must_use]
    pub fn watching(&self) -> bool {
        self.state().sink.is_some()
    }

    /// Sends `event` to the registered sink, if any.
    pub fn emit(&self, event: ServerEvent) {
        let sink = self.state().sink.clone();
        if let Some(sink) = sink {
            sink(event);
        }
    }

    /// Records a call and takes an injected failure.
    fn begin(
        &self,
        method: &'static str,
        detail: String,
    ) -> (MutexGuard<'_, State>, Option<ClientError>) {
        let mut s = self.state();
        s.calls.push(Call { method, detail });
        let mut failure = s.fail_next.take();
        if failure.is_none() && s.fail_on.as_ref().is_some_and(|(m, _)| *m == method) {
            failure = s.fail_on.take().map(|(_, e)| e);
        }
        (s, failure)
    }
}

impl State {
    fn next(&mut self, prefix: &str) -> String {
        self.serial += 1;
        format!("{prefix}-{}", self.serial)
    }

    fn user(&self, id: &str) -> Option<&FakeUser> {
        self.users.iter().find(|u| u.id == id)
    }

    fn caller(&self, token: &str) -> Result<FakeUser, ClientError> {
        self.tokens
            .get(token)
            .and_then(|id| self.user(id))
            .cloned()
            .ok_or_else(|| api(401, "invalid_session"))
    }

    fn new_session(&mut self, user_id: &str, opening_cash: i64) -> CashierSession {
        let day = LocalDay::of(
            Timestamp::from_unix_millis(self.now),
            BranchTimeZone::default(),
        );
        let prefix = format!("JKT01-R1-S-{}-", day.yymmdd());
        let seq = self
            .history
            .iter()
            .filter(|(s, _)| s.number.starts_with(&prefix))
            .count()
            + 1;
        CashierSession {
            id: self.next("session"),
            number: session_number("JKT01", "R1", day, u32::try_from(seq).unwrap_or(u32::MAX)),
            user_id: user_id.to_owned(),
            register_id: REGISTER_ID.to_owned(),
            branch_id: BRANCH_ID.to_owned(),
            opening_cash,
            opened_at: self.now,
            status: "open".to_owned(),
            closed_at: None,
            expected_cash: None,
        }
    }

    /// Whether `user` may read `session` (own, or `report.sales`).
    fn may_read(user: &FakeUser, session: &CashierSession) -> bool {
        user.reports || session.user_id == user.id
    }

    /// Lockout and PIN check (spec 2.1 §4).
    fn verify(&mut self, user: &FakeUser, pin: &str) -> Result<(), ClientError> {
        let now = self.now;
        if let Some(until) = self
            .locked_until
            .get(&user.id)
            .copied()
            .filter(|u| *u > now)
        {
            return Err(Problem::new(423, "locked_out", "locked")
                .with("locked_until", until)
                .into());
        }
        if user.pin == pin {
            self.failures.remove(&user.id);
            return Ok(());
        }
        let failures = self.failures.entry(user.id.clone()).or_default();
        *failures += 1;
        if *failures >= MAX_FAILURES {
            *failures = 0;
            let until = now + LOCKOUT_MS;
            self.locked_until.insert(user.id.clone(), until);
            return Err(Problem::new(423, "locked_out", "locked")
                .with("locked_until", until)
                .into());
        }
        Err(api(401, "invalid_credentials"))
    }
}

impl Service for FakeService {
    fn lock_users(&self) -> crate::BoxFuture<Vec<LockUser>> {
        let (s, failure) = self.begin("lock_users", String::new());
        if let Some(e) = failure {
            return ready(Err(e));
        }
        ready(Ok(s
            .users
            .iter()
            .map(|u| LockUser {
                id: u.id.clone(),
                display_name: u.name.clone(),
            })
            .collect()))
    }

    fn pin_login(&self, user_id: &str, pin: &str) -> crate::BoxFuture<Login> {
        let (mut s, failure) = self.begin("pin_login", user_id.to_owned());
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let Some(user) = s.user(user_id).cloned() else {
            return ready(Err(api(401, "invalid_credentials")));
        };
        if let Err(e) = s.verify(&user, pin) {
            return ready(Err(e));
        }
        let token = s.next("token");
        s.tokens.insert(token.clone(), user.id.clone());
        ready(Ok(Login {
            token,
            user_id: user.id,
            display_name: user.name,
            branch_id: BRANCH_ID.to_owned(),
            register_id: Some(REGISTER_ID.to_owned()),
            expires_at: s.now + 30 * 60 * 1_000,
        }))
    }

    fn logout(&self, token: &str) -> crate::BoxFuture<()> {
        let (mut s, failure) = self.begin("logout", token.to_owned());
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let Some(user) = s.tokens.remove(token) else {
            return ready(Err(api(401, "invalid_session")));
        };
        let sink = s.sink.clone();
        drop(s);
        // Like the server: the user's sockets hear about it.
        if let Some(sink) = sink {
            sink(ServerEvent {
                kind: "session.revoked".to_owned(),
                data: serde_json::json!({"user_id": user, "reason": "logout", "this_session": true}),
            });
        }
        ready(Ok(()))
    }

    fn branch(&self, token: &str, branch_id: &str) -> crate::BoxFuture<Branch> {
        let (s, failure) = self.begin("branch", branch_id.to_owned());
        let result = failure.map_or_else(|| s.caller(token).map(|_| ()), Err);
        ready(result.map(|()| Branch {
            id: branch_id.to_owned(),
            code: "JKT01".to_owned(),
            name: "Toko Jakarta".to_owned(),
            timezone: "Asia/Jakarta".to_owned(),
        }))
    }

    fn current_session(&self, token: &str) -> crate::BoxFuture<Option<CashierSession>> {
        let (s, failure) = self.begin("current_session", String::new());
        let result = failure.map_or_else(|| s.caller(token).map(|_| ()), Err);
        ready(result.map(|()| s.session.clone()))
    }

    fn open_session(
        &self,
        token: &str,
        opening_cash: i64,
        key: &str,
    ) -> crate::BoxFuture<CashierSession> {
        let (mut s, failure) = self.begin("open_session", format!("{opening_cash} key={key}"));
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let user = match s.caller(token) {
            Ok(u) => u,
            Err(e) => return ready(Err(e)),
        };
        if let Some(replayed) = s.opened_by_key.get(key) {
            return ready(Ok(replayed.clone()));
        }
        if opening_cash < 0 {
            return ready(Err(Problem::new(422, "invalid_field", "negative")
                .with("field", "opening_cash")
                .into()));
        }
        if let Some(open) = &s.session {
            let conflict = if open.user_id == user.id {
                "user"
            } else {
                "register"
            };
            return ready(Err(Problem::new(409, "session_already_open", "open")
                .with("conflict", conflict)
                .with("session_id", open.id.clone())
                .into()));
        }
        let session = s.new_session(&user.id, opening_cash);
        s.session = Some(session.clone());
        s.history.push((session.clone(), None));
        s.opened_by_key.insert(key.to_owned(), session.clone());
        ready(Ok(session))
    }

    fn close_session(
        &self,
        token: &str,
        session_id: &str,
        body: &CloseRequest,
        key: &str,
    ) -> crate::BoxFuture<CloseReport> {
        let detail = format!("{session_id} counted={} key={key}", body.counted_cash);
        let (mut s, failure) = self.begin("close_session", detail);
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let user = match s.caller(token) {
            Ok(u) => u,
            Err(e) => return ready(Err(e)),
        };
        let Some(mut session) = s.session.clone().filter(|o| o.id == session_id) else {
            return ready(Err(Problem::new(409, "session_closed", "closed")
                .with("session_id", session_id)
                .into()));
        };
        if session.user_id != user.id {
            return ready(Err(api(403, "not_session_owner")));
        }
        let expected = session.opening_cash + s.cash_sales;
        "closed".clone_into(&mut session.status);
        session.closed_at = Some(s.now);
        session.expected_cash = Some(expected);
        s.session = None;
        let tenders = if s.cash_sales > 0 {
            vec![TenderTotal {
                tender: "cash".to_owned(),
                sales: s.cash_sales,
                refunds: 0,
                net: s.cash_sales,
                count: 3,
            }]
        } else {
            Vec::new()
        };
        let report = CloseReport {
            forced: false,
            branch_code: "JKT01".to_owned(),
            branch_name: "Toko Jakarta".to_owned(),
            timezone: "Asia/Jakarta".to_owned(),
            register_code: "R1".to_owned(),
            register_name: "Register 1".to_owned(),
            cashier_name: user.name,
            forced_by_name: None,
            opening_cash: session.opening_cash,
            cash_sales: s.cash_sales,
            cash_refunds: 0,
            cash_in: 0,
            cash_out: 0,
            expected_cash: expected,
            counted_cash: body.counted_cash,
            over_short: body.counted_cash - expected,
            transactions: if s.cash_sales > 0 { 3 } else { 0 },
            tenders,
            counts: body.counts.clone().unwrap_or_default(),
            session,
        };
        if let Some(entry) = s.history.iter_mut().find(|(h, _)| h.id == session_id) {
            *entry = (report.session.clone(), Some(report.clone()));
        }
        ready(Ok(report))
    }

    fn session(&self, token: &str, session_id: &str) -> crate::BoxFuture<SessionDetail> {
        let (s, failure) = self.begin("session", session_id.to_owned());
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let user = match s.caller(token) {
            Ok(u) => u,
            Err(e) => return ready(Err(e)),
        };
        let Some((session, report)) = s.history.iter().find(|(h, _)| h.id == session_id) else {
            return ready(Err(api(404, "not_found")));
        };
        if !State::may_read(&user, session) {
            return ready(Err(Problem::new(403, "permission_denied", "denied")
                .with("permission", "report.sales")
                .into()));
        }
        ready(Ok(SessionDetail {
            session: session.clone(),
            close_report: report.clone(),
        }))
    }

    fn sessions(&self, token: &str, query: &SessionQuery) -> crate::BoxFuture<SessionPage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        let (s, failure) = self.begin("sessions", detail);
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let user = match s.caller(token) {
            Ok(u) => u,
            Err(e) => return ready(Err(e)),
        };
        let limit = query.limit.unwrap_or(100) as usize;
        let matches = |c: &CashierSession| {
            State::may_read(&user, c)
                && query.user_id.as_ref().is_none_or(|u| *u == c.user_id)
                && query
                    .register_id
                    .as_ref()
                    .is_none_or(|r| *r == c.register_id)
                && query.branch_id.as_ref().is_none_or(|b| *b == c.branch_id)
                && query.status.as_ref().is_none_or(|st| *st == c.status)
                && query.from.is_none_or(|f| c.opened_at >= f)
                && query.until.is_none_or(|u| c.opened_at < u)
        };
        // Newest first; the cursor is the last ID of the previous page.
        let newest_first = s.history.iter().rev().map(|(c, _)| c);
        let after_cursor: Vec<&CashierSession> = match &query.before_id {
            None => newest_first.collect(),
            Some(cursor) => newest_first
                .skip_while(|c| c.id != *cursor)
                .skip(1)
                .collect(),
        };
        let items: Vec<CashierSession> = after_cursor
            .into_iter()
            .filter(|c| matches(c))
            .take(limit)
            .cloned()
            .collect();
        let next_before_id = items
            .last()
            .filter(|_| items.len() == limit)
            .map(|c| c.id.clone());
        ready(Ok(SessionPage {
            items,
            next_before_id,
        }))
    }

    fn request_override(
        &self,
        token: &str,
        body: &OverrideRequest,
    ) -> crate::BoxFuture<OverrideGrant> {
        let (mut s, failure) = self.begin("request_override", body.approver_id.clone());
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let requester = match s.caller(token) {
            Ok(u) => u,
            Err(e) => return ready(Err(e)),
        };
        if body.approver_id == requester.id {
            return ready(Err(api(403, "self_approval")));
        }
        let Some(approver) = s.user(&body.approver_id).cloned() else {
            return ready(Err(api(401, "invalid_credentials")));
        };
        if let Err(e) = s.verify(&approver, &body.approver_pin) {
            return ready(Err(e));
        }
        if body.value.unwrap_or(0) > approver.discount_limit {
            return ready(Err(Problem::new(403, "override_limit_exceeded", "limit")
                .with("permission", body.action.clone())
                .with("limit", approver.discount_limit)
                .into()));
        }
        let token = s.next("override");
        s.overrides.insert(
            token.clone(),
            (
                body.action.clone(),
                body.value,
                body.context_ref.clone(),
                approver.id.clone(),
            ),
        );
        ready(Ok(OverrideGrant {
            token,
            action: body.action.clone(),
            value: body.value,
            context_ref: body.context_ref.clone(),
            requester_id: requester.id,
            approver_id: approver.id,
            expires_at: s.now + 120_000,
        }))
    }

    fn demo_protected(
        &self,
        token: &str,
        body: &ProtectedAction,
        override_token: Option<&str>,
    ) -> crate::BoxFuture<ProtectedResult> {
        let detail = format!("override={}", override_token.unwrap_or("-"));
        let (mut s, failure) = self.begin("demo_protected", detail);
        if let Some(e) = failure {
            return ready(Err(e));
        }
        let user = match s.caller(token) {
            Ok(u) => u,
            Err(e) => return ready(Err(e)),
        };
        let result = |approved_by: Option<String>| ProtectedResult {
            action: body.action.clone(),
            value: body.value,
            context_ref: body.context_ref.clone(),
            approved_by,
        };
        if body.value.unwrap_or(0) <= user.discount_limit {
            return ready(Ok(result(None)));
        }
        let Some(presented) = override_token else {
            return ready(Err(Problem::new(403, "needs_override", "needs override")
                .with("permission", body.action.clone())
                .with("limit", user.discount_limit)
                .into()));
        };
        match s.overrides.remove(presented) {
            Some((action, value, context, approver))
                if action == body.action && value == body.value && context == body.context_ref =>
            {
                ready(Ok(result(Some(approver))))
            }
            _ => ready(Err(api(403, "invalid_override"))),
        }
    }

    fn watch_events(&self, token: &str, sink: EventSink) -> crate::BoxFuture<()> {
        let (mut s, failure) = self.begin("watch_events", String::new());
        if let Some(e) = failure {
            return ready(Err(e));
        }
        if let Err(e) = s.caller(token) {
            return ready(Err(e));
        }
        s.sink = Some(sink);
        ready(Ok(()))
    }
}

#[cfg(test)]
mod tests {
    use futures_util::FutureExt;

    use super::*;

    fn now<T>(future: crate::BoxFuture<T>) -> Result<T, ClientError> {
        future.now_or_never().expect("the fake is always ready")
    }

    fn login(fake: &FakeService, user: &str, pin: &str) -> String {
        now(fake.pin_login(user, pin)).unwrap().token
    }

    fn close(fake: &FakeService, token: &str, id: &str) -> CloseReport {
        let body = CloseRequest {
            counted_cash: 0,
            counts: None,
        };
        now(fake.close_session(token, id, &body, "k")).unwrap()
    }

    /// 2027-01-15T16:59:59.999Z = 23:59:59.999 WIB.
    const LATE: i64 = 1_800_032_399_999;

    #[test]
    fn sessions_are_numbered_per_local_day() {
        let fake = FakeService::default();
        fake.set_now(LATE);
        let ani = login(&fake, "u-ani", "1234");
        #[rustfmt::skip]
        let steps = [
            (LATE, "JKT01-R1-S-270115-01"),
            (LATE, "JKT01-R1-S-270115-02"),
            (LATE + 1, "JKT01-R1-S-270116-01"),
        ];
        for (n, (at, number)) in steps.into_iter().enumerate() {
            fake.set_now(at);
            let opened = now(fake.open_session(&ani, 0, &format!("open-{n}"))).unwrap();
            assert_eq!(opened.number, number);
            let report = close(&fake, &ani, &opened.id);
            assert_eq!(report.session.closed_at, Some(at));
            assert_eq!(report.cashier_name, "Ani");
        }
    }

    #[test]
    fn session_detail_and_list_follow_the_read_rule() {
        let fake = FakeService::default();
        let ani = login(&fake, "u-ani", "1234");
        let budi = login(&fake, "u-budi", "9999");
        let dedi = login(&fake, "u-dedi", "4321");
        let first = now(fake.open_session(&ani, 5, "a")).unwrap();
        let report = close(&fake, &ani, &first.id);
        fake.set_now(1_800_000_060_000);
        let second = fake.open_for("u-dedi", 7);

        // Own sessions, or every session with `report.sales`.
        let detail = now(Service::session(&fake, &ani, &first.id)).unwrap();
        assert_eq!(detail.close_report, Some(report.clone()));
        assert_eq!(detail.session, report.session);
        let detail = now(Service::session(&fake, &budi, &second.id)).unwrap();
        assert_eq!(detail.close_report, None);
        let err = now(Service::session(&fake, &ani, &second.id)).unwrap_err();
        assert_eq!(
            err.problem().map(|p| p.code.as_str()),
            Some("permission_denied")
        );
        let err = now(Service::session(&fake, &ani, "nope")).unwrap_err();
        assert_eq!(err.problem().map(|p| p.code.as_str()), Some("not_found"));
        assert!(now(Service::session(&fake, "bad", &first.id)).is_err());

        let ids = |token: &str, query: SessionQuery| -> (Vec<String>, Option<String>) {
            let page = now(fake.sessions(token, &query)).unwrap();
            (
                page.items.into_iter().map(|s| s.id).collect(),
                page.next_before_id,
            )
        };
        let q = SessionQuery::default;
        #[rustfmt::skip]
        let cases = [
            (&budi, q(), vec![&second.id, &first.id], None),
            (&ani, q(), vec![&first.id], None),
            (&dedi, q(), vec![&second.id], None),
            (&budi, SessionQuery { status: Some("open".into()), ..q() }, vec![&second.id], None),
            (&budi, SessionQuery { user_id: Some("u-ani".into()), ..q() }, vec![&first.id], None),
            (&budi, SessionQuery { register_id: Some("x".into()), ..q() }, vec![], None),
            (&budi, SessionQuery { branch_id: Some(BRANCH_ID.into()), ..q() }, vec![&second.id, &first.id], None),
            (&budi, SessionQuery { from: Some(1_800_000_060_000), ..q() }, vec![&second.id], None),
            (&budi, SessionQuery { until: Some(1_800_000_060_000), ..q() }, vec![&first.id], None),
            (&budi, SessionQuery { limit: Some(1), ..q() }, vec![&second.id], Some(&second.id)),
            (&budi, SessionQuery { limit: Some(1), before_id: Some(second.id.clone()), ..q() }, vec![&first.id], Some(&first.id)),
        ];
        for (token, query, expected, next) in cases {
            let expected: Vec<String> = expected.into_iter().cloned().collect();
            assert_eq!(
                ids(token, query.clone()),
                (expected, next.cloned()),
                "{query:?}"
            );
        }
        assert!(now(fake.sessions("bad", &q())).is_err());
        assert_eq!(
            fake.calls_of("sessions").len(),
            12,
            "11 cases and the bad token"
        );
        fake.fail_next(ClientError::Network("down".into()));
        assert!(now(fake.sessions(&budi, &q())).is_err());
        fake.fail_next(ClientError::Network("down".into()));
        assert!(now(Service::session(&fake, &budi, &first.id)).is_err());
    }
}
