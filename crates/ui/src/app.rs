//! Glue between the Slint window and the [`Controller`] (plan Phase 8). Depends on
//! [`sultan_client::Service`] only, so tests drive every flow with the fake service.
//!
//! - Every Slint callback becomes a [`Msg`]; [`Controller::update`] returns [`Effect`]s; each
//!   effect is started as a service call and its result comes back as another [`Msg`]. After
//!   each batch the whole view is rendered from the controller (`render`).
//! - [`Runner::Spawn`] (the real register): calls run on the tokio runtime; results are queued
//!   and the UI thread is woken through `slint::Weak::upgrade_in_event_loop`, which calls the
//!   window's `pump` callback.
//! - [`Runner::Block`] (tests): calls are awaited on the spot, so a test sees the outcome as soon
//!   as the key press returns. Events (`watch_events`) are spawned in both modes; tests deliver
//!   queued events with [`App::pump`].
//! - A one-second `slint::Timer` refreshes the clock and the lockout countdown; tests call
//!   [`App::tick`] with their own clock.

use std::cell::{Cell, Ref, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use sultan_client::{CloseReport, EventSink, Service};
use tokio::runtime::Handle;

use crate::amount::{rupiah, signed_rupiah};
use crate::controller::{Controller, Effect, Msg, Notice, Screen, TILE_COLUMNS, Tone, value_text};
use crate::count::{CountForm, DENOMINATIONS, is_coin};
use crate::input::{Key, date_time, time_of_day};
use crate::scan::{ScanField, match_kind, qty_text};
use crate::{
    Chip, CloseView, DenomRow, HomeView, LockView, MainWindow, Note, OpenView, OverrideView,
    ReportLine, ReportView, ScanView, UserTile,
};

/// How service calls run; see the module docs.
#[derive(Debug, Clone)]
pub enum Runner {
    /// Spawn on the runtime, deliver results through the event loop (the real register).
    Spawn(Handle),
    /// Block on each call (headless tests).
    Block(Handle),
}

impl Runner {
    pub(crate) const fn handle(&self) -> &Handle {
        match self {
            Self::Spawn(h) | Self::Block(h) => h,
        }
    }
}

/// The current time (unix ms).
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

/// The system clock.
#[must_use]
pub fn system_clock() -> Clock {
    Arc::new(|| chrono::Utc::now().timestamp_millis())
}

/// What the window shows besides the service data.
#[derive(Clone)]
pub struct Options {
    /// The register in the header, e.g. `R1 · Kasir 1`.
    pub register_label: String,
    /// `id` or `en`; `None` keeps Slint's choice from the system locale.
    pub language: Option<String>,
    /// The clock.
    pub clock: Clock,
}

impl Options {
    /// Options with the system clock and the system language.
    #[must_use]
    pub fn new(register_label: impl Into<String>) -> Self {
        Self {
            register_label: register_label.into(),
            language: None,
            clock: system_clock(),
        }
    }
}

type Inbox = Arc<Mutex<VecDeque<Msg>>>;
type Wake = Arc<dyn Fn() + Send + Sync>;

fn push(inbox: &Inbox, msg: Msg) {
    inbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(msg);
}

/// The models behind the window's lists, kept so that unchanged lists are not rebuilt.
struct Models {
    users: Rc<VecModel<UserTile>>,
    managers: Rc<VecModel<UserTile>>,
    chips: Rc<VecModel<Chip>>,
    actions: Rc<VecModel<Chip>>,
    denoms: Rc<VecModel<DenomRow>>,
    report_lines: Rc<VecModel<ReportLine>>,
    report_tenders: Rc<VecModel<ReportLine>>,
}

/// Replaces the rows of `model` if they changed.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.iter().ne(rows.iter().cloned()) {
        model.set_vec(rows);
    }
}

struct State {
    ui: slint::Weak<MainWindow>,
    controller: RefCell<Controller>,
    inbox: Inbox,
    wake: Wake,
    service: Arc<dyn Service>,
    runner: Runner,
    options: Options,
    models: Models,
    draining: Cell<bool>,
}

/// The register window and its state.
pub struct App {
    /// The window.
    pub ui: MainWindow,
    state: Rc<State>,
    _timer: slint::Timer,
}

impl App {
    /// The controller (read-only), for tests.
    #[must_use]
    pub fn controller(&self) -> Ref<'_, Controller> {
        self.state.controller.borrow()
    }

    /// Handles queued results and events (what `pump` does on the event loop).
    pub fn pump(&self) {
        self.state.drain();
        self.state.render();
    }

    /// Refreshes the clock and countdowns (what the one-second timer does).
    pub fn tick(&self) {
        self.state.render();
    }

    /// Sends a message as if the window had.
    pub fn dispatch(&self, msg: Msg) -> bool {
        self.state.dispatch(msg)
    }
}

type MsgFuture = Pin<Box<dyn Future<Output = Msg> + Send>>;

/// Starts the service call of `effect`. Calls happen now (the fake does its work here); the
/// returned future only waits for the answer.
fn start(service: &dyn Service, effect: Effect) -> Option<MsgFuture> {
    macro_rules! call {
        ($future:expr, $msg:path) => {{
            let future = $future;
            Some(Box::pin(async move { $msg(future.await) }) as MsgFuture)
        }};
    }
    match effect {
        Effect::LoadUsers => call!(service.lock_users(), Msg::Users),
        Effect::Login { user_id, pin } => call!(service.pin_login(&user_id, &pin), Msg::LoggedIn),
        Effect::LoadBranch { token, branch_id } => {
            call!(service.branch(&token, &branch_id), Msg::Branch)
        }
        Effect::LoadCurrent { token } => call!(service.current_session(&token), Msg::Current),
        Effect::Open { token, amount, key } => {
            call!(service.open_session(&token, amount, &key), Msg::Opened)
        }
        Effect::Close {
            token,
            session_id,
            body,
            key,
        } => {
            let future = service.close_session(&token, &session_id, &body, &key);
            Some(Box::pin(async move { Msg::Closed(future.await.map(Box::new)) }) as MsgFuture)
        }
        Effect::RequestOverride { token, body } => {
            call!(service.request_override(&token, &body), Msg::Approved)
        }
        Effect::Protected {
            token,
            body,
            override_token,
        } => call!(
            service.demo_protected(&token, &body, override_token.as_deref()),
            Msg::Protected
        ),
        Effect::Logout { token } => call!(service.logout(&token), Msg::LoggedOut),
        Effect::Lookup { token, code, seq } => {
            let future = service.lookup(&token, &code);
            Some(Box::pin(async move { Msg::Looked(seq, future.await) }) as MsgFuture)
        }
        Effect::WatchEvents { .. } => None,
    }
}

impl State {
    /// Handles `msg` and everything it leads to, then renders. Returns whether a key was used.
    fn dispatch(&self, msg: Msg) -> bool {
        let step = self.controller.borrow_mut().update(msg);
        self.run(step.effects);
        self.drain();
        self.render();
        step.handled
    }

    /// Handles queued messages until none are left.
    fn drain(&self) {
        if self.draining.replace(true) {
            return;
        }
        loop {
            let next = self
                .inbox
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop_front();
            let Some(msg) = next else { break };
            let step = self.controller.borrow_mut().update(msg);
            self.run(step.effects);
        }
        self.draining.set(false);
    }

    fn run(&self, effects: Vec<Effect>) {
        for effect in effects {
            if let Effect::WatchEvents { token, generation } = &effect {
                let inbox = self.inbox.clone();
                let wake = self.wake.clone();
                let generation = *generation;
                let sink: EventSink = Arc::new(move |event| {
                    push(&inbox, Msg::Event(generation, event));
                    wake();
                });
                let watching = self.service.watch_events(token, sink);
                // Ends when the socket closes (logout) or fails; nothing to report.
                self.runner.handle().spawn(async move {
                    let _ = watching.await;
                });
                continue;
            }
            let Some(future) = start(self.service.as_ref(), effect) else {
                continue;
            };
            match &self.runner {
                Runner::Block(handle) => push(&self.inbox, handle.block_on(future)),
                Runner::Spawn(handle) => {
                    let inbox = self.inbox.clone();
                    let wake = self.wake.clone();
                    handle.spawn(async move {
                        push(&inbox, future.await);
                        wake();
                    });
                }
            }
        }
    }

    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let c = self.controller.borrow();
        let now = (self.options.clock)();
        render(&ui, &c, now, &self.models, &self.options);
    }
}

fn chips(shortcuts: &[crate::controller::Shortcut]) -> Vec<Chip> {
    shortcuts
        .iter()
        .map(|s| Chip {
            keys: s.key.label().into(),
            label: s.label.into(),
            id: s.action.id().into(),
        })
        .collect()
}

fn note(notice: Option<&Notice>, now: i64) -> Note {
    notice
        .and_then(|n| n.shown(now))
        .map_or_else(Note::default, |(code, arg, tone)| Note {
            code: code.into(),
            arg: arg.into(),
            tone: tone.value(),
        })
}

/// Up to two initials: `Budi Santoso` → `BS`, `ani` → `A`.
#[must_use]
pub fn initials(name: &str) -> String {
    name.split_whitespace()
        .filter_map(|w| w.chars().next())
        .take(2)
        .flat_map(char::to_uppercase)
        .collect()
}

fn tile(name: &str) -> UserTile {
    UserTile {
        name: name.into(),
        initials: initials(name).into(),
    }
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

fn report_lines(r: &CloseReport) -> (Vec<ReportLine>, Vec<ReportLine>) {
    let line = |label: &str, value: String, strong: bool| ReportLine {
        label: label.into(),
        value: value.into(),
        tone: 0,
        strong,
    };
    let lines = vec![
        line("opening_cash", rupiah(r.opening_cash), false),
        line("cash_sales", rupiah(r.cash_sales), false),
        line("cash_refunds", rupiah(r.cash_refunds), false),
        line("cash_in", rupiah(r.cash_in), false),
        line("cash_out", rupiah(r.cash_out), false),
        line("expected_cash", rupiah(r.expected_cash), true),
        line("counted_cash", rupiah(r.counted_cash), true),
        line("transactions", r.transactions.to_string(), false),
    ];
    let tenders = r
        .tenders
        .iter()
        .map(|t| {
            line(
                &t.tender,
                format!("{} · {}×", rupiah(t.net), t.count),
                false,
            )
        })
        .collect();
    (lines, tenders)
}

/// The barcode field and the shown item (phase C5). No price: only the amount printed on a
/// price scale label.
fn scan_view(scan: &ScanField, now: i64) -> ScanView {
    let mut view = ScanView {
        code: scan.code.as_str().into(),
        looking: scan.looking().unwrap_or_default().into(),
        ..ScanView::default()
    };
    let Some(shown) = &scan.shown else {
        return view;
    };
    view.scanned = shown.code.as_str().into();
    match &shown.result {
        Err(notice) => {
            view.state = 2;
            view.note = note(Some(notice), now);
        }
        Ok(item) => {
            view.state = 1;
            view.name = if item.product_name.is_empty() {
                &item.name
            } else {
                &item.product_name
            }
            .into();
            view.variant = item.variant_label.as_str().into();
            view.sku = item.sku.as_str().into();
            view.unit = item.unit_name.as_str().into();
            view.qty = item
                .qty_thousandths
                .map(|q| {
                    format!("{} {}", qty_text(q), item.unit_name)
                        .trim_end()
                        .to_owned()
                })
                .unwrap_or_default()
                .into();
            view.plu = item.plu.map(|p| p.to_string()).unwrap_or_default().into();
            view.kind = item.product_type.as_str().into();
            view.r#match = match_kind(item).into();
            view.matched = if item.matched_code.is_empty() {
                &shown.code
            } else {
                &item.matched_code
            }
            .into();
            view.amount = item.amount.map(rupiah).unwrap_or_default().into();
            view.open_price = item.open_price;
            view.requires_note = item.requires_note;
        }
    }
    view
}

fn denominations(form: &CountForm) -> Vec<DenomRow> {
    DENOMINATIONS
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let row = i + 1;
            let qty = form.quantity(row);
            DenomRow {
                label: rupiah(*d).into(),
                qty: if qty > 0 {
                    qty.to_string().into()
                } else {
                    SharedString::new()
                },
                subtotal: if qty > 0 {
                    rupiah(form.subtotal(row)).into()
                } else {
                    SharedString::new()
                },
                coin: is_coin(*d),
            }
        })
        .collect()
}

#[allow(clippy::too_many_lines)] // one assignment per window property
fn render(ui: &MainWindow, c: &Controller, now: i64, models: &Models, options: &Options) {
    ui.set_screen(c.screen.name().into());
    ui.set_busy(c.busy);
    ui.set_clock(date_time(now).into());
    ui.set_register_label(options.register_label.as_str().into());
    ui.set_branch_label(
        c.branch
            .as_ref()
            .map(|b| format!("{} · {}", b.code, b.name))
            .unwrap_or_default()
            .into(),
    );
    ui.set_user_name(
        c.login
            .as_ref()
            .map(|l| l.display_name.as_str())
            .unwrap_or_default()
            .into(),
    );
    sync(&models.chips, chips(&c.shortcuts()));
    sync(&models.actions, chips(&Controller::home_actions()));
    sync(
        &models.users,
        c.users.iter().map(|u| tile(&u.display_name)).collect(),
    );

    match &c.screen {
        Screen::Lock(l) => {
            let pad = l.pad.as_ref();
            let pad_name = pad
                .and_then(|p| c.users.get(p.user))
                .map(|u| u.display_name.as_str())
                .unwrap_or_default();
            ui.set_lock(LockView {
                cursor: index(l.cursor),
                pad_open: pad.is_some(),
                pad_name: pad_name.into(),
                pad_initials: initials(pad_name).into(),
                pin_length: index(pad.map_or(0, |p| p.pin.len())),
                note: note(pad.map_or(l.notice.as_ref(), |p| p.notice.as_ref()), now),
            });
        }
        Screen::Open(o) => {
            let blocked = o.blocked.as_ref();
            ui.set_open(OpenView {
                amount: rupiah(o.amount.value()).into(),
                blocked: blocked.is_some(),
                blocked_name: blocked
                    .map(|s| c.user_name(&s.user_id).unwrap_or(&s.user_id).to_owned())
                    .unwrap_or_default()
                    .into(),
                blocked_since: blocked
                    .map(|s| time_of_day(s.opened_at))
                    .unwrap_or_default()
                    .into(),
                blocked_number: blocked
                    .map(|s| s.number.as_str())
                    .unwrap_or_default()
                    .into(),
                note: note(o.notice.as_ref(), now),
            });
        }
        Screen::Home(h) => {
            // Opening cash only: expected cash is never shown before the close (spec 2.1 §5).
            let session = c.session.as_ref();
            ui.set_home(HomeView {
                number: session
                    .map(|s| s.number.as_str())
                    .unwrap_or_default()
                    .into(),
                since: session
                    .map(|s| time_of_day(s.opened_at))
                    .unwrap_or_default()
                    .into(),
                opening: session
                    .map(|s| rupiah(s.opening_cash))
                    .unwrap_or_default()
                    .into(),
                note: note(h.notice.as_ref(), now),
            });
            ui.set_scan(scan_view(&h.scan, now));
        }
        Screen::Close(close) => {
            sync(&models.denoms, denominations(&close.form));
            ui.set_closing(CloseView {
                cursor: index(close.form.cursor),
                total: rupiah(close.form.counted()).into(),
                total_locked: close.form.uses_denominations(),
                confirm: close.confirm,
                note: note(close.notice.as_ref(), now),
            });
        }
        Screen::Report(report) => {
            let (lines, tenders) = report_lines(report);
            ui.set_report(ReportView {
                number: report.session.number.as_str().into(),
                over_short: signed_rupiah(report.over_short).into(),
                tone: Tone::of_amount(report.over_short).value(),
                forced: report.forced,
                has_tenders: !tenders.is_empty(),
            });
            sync(&models.report_lines, lines);
            sync(&models.report_tenders, tenders);
        }
    }
    if !matches!(c.screen, Screen::Report(_)) {
        sync(&models.report_lines, Vec::new());
        sync(&models.report_tenders, Vec::new());
    }

    let approvers: Vec<UserTile> = c
        .approvers()
        .iter()
        .map(|u| tile(&u.display_name))
        .collect();
    sync(&models.managers, approvers);
    ui.set_approval(
        c.approval
            .as_ref()
            .map_or_else(OverrideView::default, |a| OverrideView {
                open: true,
                action: a.action.action.as_str().into(),
                value: value_text(&a.action.action, a.action.value).into(),
                limit: value_text(&a.action.action, a.limit).into(),
                cursor: index(a.cursor),
                pin_length: index(a.pin.len()),
                note: note(a.notice.as_ref(), now),
            }),
    );
}

/// Wires the window's callbacks to `state` (weakly: the window must not keep it alive).
fn connect(ui: &MainWindow, state: &Rc<State>) {
    ui.on_key({
        let state = Rc::downgrade(state);
        move |text, control, shift| {
            state
                .upgrade()
                .is_some_and(|s| s.dispatch(Msg::Key(Key::parse(&text, control, shift))))
        }
    });
    ui.on_action({
        let state = Rc::downgrade(state);
        move |id| {
            if let (Some(s), Some(action)) =
                (state.upgrade(), crate::controller::Action::from_id(&id))
            {
                s.dispatch(Msg::Action(action));
            }
        }
    });
    ui.on_pick({
        let state = Rc::downgrade(state);
        move |kind, i| {
            let (Some(s), Ok(i)) = (state.upgrade(), usize::try_from(i)) else {
                return;
            };
            let msg = match kind.as_str() {
                "user" => Msg::PickUser(i),
                "approver" => Msg::PickApprover(i),
                "row" => Msg::PickRow(i),
                _ => return,
            };
            s.dispatch(msg);
        }
    });
    ui.on_pump({
        let state = Rc::downgrade(state);
        move || {
            if let Some(s) = state.upgrade() {
                s.drain();
                s.render();
            }
        }
    });
}

/// Builds the window and its state, and asks for the lock-screen users.
///
/// # Errors
/// The Slint platform error (no display, no backend).
pub fn build_app(
    service: Arc<dyn Service>,
    runner: Runner,
    options: Options,
) -> Result<App, slint::PlatformError> {
    let ui = MainWindow::new()?;
    if let Some(language) = &options.language {
        // `en` (or "") is the source language; an unknown language falls back to it.
        if slint::select_bundled_translation(language).is_err() {
            let _ = slint::select_bundled_translation("en");
        }
    }
    let models = Models {
        users: Rc::default(),
        managers: Rc::default(),
        chips: Rc::default(),
        actions: Rc::default(),
        denoms: Rc::default(),
        report_lines: Rc::default(),
        report_tenders: Rc::default(),
    };
    ui.set_users(ModelRc::from(models.users.clone()));
    ui.set_managers(ModelRc::from(models.managers.clone()));
    ui.set_chips(ModelRc::from(models.chips.clone()));
    ui.set_actions(ModelRc::from(models.actions.clone()));
    ui.set_denoms(ModelRc::from(models.denoms.clone()));
    ui.set_report_lines(ModelRc::from(models.report_lines.clone()));
    ui.set_report_tenders(ModelRc::from(models.report_tenders.clone()));
    ui.set_columns(index(TILE_COLUMNS));

    let wake: Wake = match &runner {
        Runner::Spawn(_) => {
            let weak = ui.as_weak();
            Arc::new(move || {
                let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_pump());
            })
        }
        Runner::Block(_) => Arc::new(|| {}),
    };
    let (controller, effects) = Controller::new();
    let state = Rc::new(State {
        ui: ui.as_weak(),
        controller: RefCell::new(controller),
        inbox: Inbox::default(),
        wake,
        service,
        runner,
        options,
        models,
        draining: Cell::new(false),
    });

    connect(&ui, &state);

    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, Duration::from_secs(1), {
        let state = Rc::downgrade(&state);
        move || {
            if let Some(s) = state.upgrade() {
                s.render();
            }
        }
    });

    state.run(effects);
    state.drain();
    state.render();
    Ok(App {
        ui,
        state,
        _timer: timer,
    })
}

/// Opens the register window and runs the event loop until it is closed (the real register).
///
/// # Errors
/// The Slint platform error.
pub fn run(
    service: Arc<dyn Service>,
    handle: Handle,
    options: Options,
) -> Result<(), slint::PlatformError> {
    let app = build_app(service, Runner::Spawn(handle), options)?;
    app.ui.run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initials_of_names() {
        let cases = [
            ("Budi Santoso", "BS"),
            ("ani", "A"),
            ("Siti Nur Aisyah", "SN"),
            ("", ""),
        ];
        for (name, expected) in cases {
            assert_eq!(initials(name), expected, "{name}");
        }
        assert_eq!(index(usize::MAX), i32::MAX);
    }
}
