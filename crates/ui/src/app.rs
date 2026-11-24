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
use sultan_client::{CloseReport, Customer, CustomerQuery, EventSink, Lookup, Service};
use tokio::runtime::Handle;

use crate::after::{Call as AfterCall, Reply as AfterReply, ReturnStep};
use crate::amount::{rupiah, signed_rupiah};
use crate::cash::{Call as CashCall, Reply as CashReply, ZStep};
use crate::controller::{Controller, Effect, Msg, Notice, Screen, TILE_COLUMNS, Tone, value_text};
use crate::count::{CountForm, DENOMINATIONS, is_coin};
use crate::customer::{Call, Dialog, Perms, Reply, receipt_line};
use crate::input::{Key, date_time, time_of_day};
use crate::scan::{CustomerPrice, ScanField, line_amount, match_kind, percent_off, qty_text};
use crate::sell::{Line, PayStep};
use crate::till::{EntryKind, Modal, Sell, VOID_REASONS, VoidTarget};
use crate::{
    CartRow, CashRow, CashView, Chip, CloseView, CustomerRow, CustomerView, DenomRow, HomeView,
    LockView, MainWindow, Note, OpenView, OverrideView, ReportLine, ReportView, ScanView, SellRow,
    SellView, UserTile,
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
    /// Opened from the Start screen: the lock screen offers F9 to go back to it (the window
    /// closes).
    pub start: bool,
    /// The cart's crash copy (spec 2.6 D2), next to `register.token`; `None` keeps none.
    pub cart_file: Option<crate::crash::CartFile>,
}

impl Options {
    /// Options with the system clock and the system language.
    #[must_use]
    pub fn new(register_label: impl Into<String>) -> Self {
        Self {
            register_label: register_label.into(),
            language: None,
            clock: system_clock(),
            start: false,
            cart_file: None,
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
    customers: Rc<VecModel<CustomerRow>>,
    cart_rows: Rc<VecModel<CartRow>>,
    actions_b: Rc<VecModel<Chip>>,
    sell_rows: Rc<VecModel<SellRow>>,
    pay_rows: Rc<VecModel<SellRow>>,
    tender_rows: Rc<VecModel<SellRow>>,
    receipt_lines: Rc<VecModel<SharedString>>,
    /// A sell dialog's chips in two rows when there are more than four.
    dialog_chips: Rc<VecModel<Chip>>,
    dialog_chips_b: Rc<VecModel<Chip>>,
    /// Spec 2.7 K4: the cash dialog's reasons and movements, and the printable text on screen.
    cash_reasons: Rc<VecModel<SellRow>>,
    cash_rows: Rc<VecModel<CashRow>>,
    text_lines: Rc<VecModel<SharedString>>,
}

/// Replaces the rows of `model` if they changed.
fn sync<T: Clone + PartialEq + 'static>(model: &VecModel<T>, rows: Vec<T>) {
    if model.iter().ne(rows.iter().cloned()) {
        model.set_vec(rows);
    }
}

/// The running event watchers of a window. Aborted when a newer one replaces them and when the
/// window goes away, so a long-lived process (the Start screen) does not leak sockets.
#[derive(Default)]
pub(crate) struct Watchers(RefCell<Vec<tokio::task::AbortHandle>>);

impl Watchers {
    /// Runs `watching` on the runtime. It ends when its socket closes (logout) or fails,
    /// which needs no report; it replaces (aborts) the watchers running before.
    pub(crate) fn start(&self, runner: &Runner, watching: sultan_client::BoxFuture<()>) {
        let task = runner.handle().spawn(async move {
            let _ = watching.await;
        });
        self.replace(task.abort_handle());
    }

    /// Aborts the running watchers and keeps `handle` as the only one.
    fn replace(&self, handle: tokio::task::AbortHandle) {
        let mut all = self.0.borrow_mut();
        for old in all.drain(..) {
            old.abort();
        }
        all.push(handle);
    }

    /// Aborts every watcher.
    pub(crate) fn stop(&self) {
        for old in self.0.borrow_mut().drain(..) {
            old.abort();
        }
    }
}

impl Drop for Watchers {
    fn drop(&mut self) {
        self.stop();
    }
}

struct State {
    watchers: Watchers,
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

/// Starts a customer call; the future answers with its [`Reply`].
fn customer_call(
    service: &dyn Service,
    token: &str,
    call: Call,
) -> Pin<Box<dyn Future<Output = Reply> + Send>> {
    macro_rules! reply {
        ($future:expr, $reply:path) => {{
            let future = $future;
            Box::pin(async move { $reply(future.await) })
        }};
    }
    match call {
        Call::Permissions => reply!(service.permissions(token), Reply::Permissions),
        Call::ByPhone(phone) => reply!(service.customer_by_phone(token, &phone), Reply::Found),
        Call::ByCode(code) => reply!(service.customer_by_code(token, &code), Reply::Found),
        Call::Search(q) => reply!(
            service.search_customers(token, &CustomerQuery::search(&q)),
            Reply::Searched
        ),
        Call::SignUp { id, body } => {
            reply!(service.sign_up_customer(token, &id, &body), Reply::SignedUp)
        }
        Call::Payout { id, amount, key } => reply!(
            service.balance_payout(token, &id, amount, &key),
            Reply::PaidOut
        ),
    }
}

/// Starts a call of [`crate::after`]; `lang` is the window language for receipts.
#[allow(clippy::too_many_lines)] // one arm per service call
fn after_call(
    service: &dyn Service,
    token: &str,
    call: AfterCall,
    lang: &str,
) -> Pin<Box<dyn Future<Output = AfterReply> + Send>> {
    macro_rules! reply {
        ($future:expr, $reply:path) => {{
            let future = $future;
            Box::pin(async move { $reply(future.await) })
        }};
    }
    match call {
        AfterCall::Hold {
            id,
            customer_id,
            note,
            payload,
            item_count,
            total,
        } => {
            let body = sultan_client::HoldRequest {
                customer_id,
                note,
                payload: serde_json::from_str(&payload).unwrap_or_default(),
                item_count,
                total,
            };
            reply!(service.hold_cart(token, &id, &body), AfterReply::Held)
        }
        AfterCall::HeldCarts => reply!(service.held_carts(token), AfterReply::HeldCarts),
        AfterCall::Take { id, key } => {
            reply!(service.take_held_cart(token, &id, &key), AfterReply::Taken)
        }
        AfterCall::DropHeld { id, override_token } => reply!(
            service.delete_held_cart(token, &id, override_token.as_deref()),
            AfterReply::Dropped
        ),
        AfterCall::SessionSales(session) => {
            let query = sultan_client::DocQuery {
                cashier_session_id: Some(session),
                limit: Some(50),
                ..sultan_client::DocQuery::default()
            };
            reply!(service.sales(token, &query), AfterReply::Sales)
        }
        AfterCall::SaleByNumber(number) => {
            reply!(service.sale_by_number(token, &number), AfterReply::Sale)
        }
        AfterCall::Sale(id) => reply!(service.sale(token, &id), AfterReply::Sale),
        AfterCall::ReturnReceipt(id) => reply!(
            service.return_receipt(token, &id, false, lang, None),
            AfterReply::ReturnReceipt
        ),
        AfterCall::Receipt {
            id,
            copy,
            override_token,
        } => reply!(
            service.receipt(token, &id, copy, lang, override_token.as_deref()),
            AfterReply::Receipt
        ),
        AfterCall::Void {
            id,
            body,
            key,
            override_token,
        } => reply!(
            service.void_sale(token, &id, &body, &key, override_token.as_deref()),
            AfterReply::Voided
        ),
        AfterCall::Return {
            id,
            body,
            override_token,
        } => reply!(
            service.post_return(token, &id, &body, override_token.as_deref()),
            AfterReply::Returned
        ),
        AfterCall::Exchange {
            id,
            body,
            override_token,
        } => reply!(
            service.post_return(token, &id, &body, override_token.as_deref()),
            AfterReply::Exchanged
        ),
        AfterCall::Recent(customer) => {
            reply!(
                service.recent_receipts(token, &customer),
                AfterReply::Recent
            )
        }
        AfterCall::History {
            customer_id,
            before,
        } => reply!(
            service.customer_history(token, &customer_id, before.as_deref()),
            AfterReply::History
        ),
        AfterCall::Lookup(code) => reply!(service.lookup(token, &code), AfterReply::Looked),
        AfterCall::Quote(seq, request) => {
            let future = service.quote(token, &request);
            Box::pin(async move { AfterReply::Quoted(seq, future.await) })
        }
        AfterCall::Drawer {
            key,
            override_token,
        } => reply!(
            service.open_drawer(
                token,
                &sultan_client::DrawerRequest::default(),
                &key,
                override_token.as_deref()
            ),
            AfterReply::Drawer
        ),
    }
}

/// Starts a call of [`crate::cash`]; `lang` is the window language for the texts.
fn cash_call(
    service: &dyn Service,
    token: &str,
    call: CashCall,
    lang: &str,
) -> Pin<Box<dyn Future<Output = CashReply> + Send>> {
    macro_rules! reply {
        ($future:expr, $reply:path) => {{
            let future = $future;
            Box::pin(async move { $reply(future.await) })
        }};
    }
    match call {
        CashCall::Reasons => reply!(service.cash_reasons(token), CashReply::Reasons),
        CashCall::Movements(session) => reply!(
            service.cash_movements(token, &crate::cash::session_query(&session)),
            CashReply::Movements
        ),
        CashCall::Post {
            id,
            body,
            override_token,
        } => {
            let future = service.post_cash_movement(token, &id, &body, override_token.as_deref());
            Box::pin(async move { CashReply::Posted(future.await.map(Box::new)) })
        }
        CashCall::Slip {
            id,
            copy,
            override_token,
        } => {
            let future = service.cash_slip(token, &id, copy, lang, override_token.as_deref());
            Box::pin(async move { CashReply::Slip(copy, future.await) })
        }
        CashCall::XText {
            session_id,
            override_token,
        } => reply!(
            service.x_report_text(token, &session_id, lang, override_token.as_deref()),
            CashReply::X
        ),
        CashCall::CloseText(session) => reply!(
            service.close_report_text(token, &session, false, lang),
            CashReply::CloseText
        ),
        CashCall::Pending => reply!(service.pending_days(token, None), CashReply::Pending),
        CashCall::RunZ {
            day,
            key,
            override_token,
        } => {
            let future = service.run_z(
                token,
                &crate::cash::run_z_request(&day),
                &key,
                override_token.as_deref(),
            );
            Box::pin(async move { CashReply::Z(future.await.map(Box::new)) })
        }
        CashCall::ZText(id) => reply!(
            service.z_report_text(token, &id, false, lang),
            CashReply::ZText
        ),
        CashCall::Float => reply!(service.proposed_float(token), CashReply::Float),
        CashCall::PayoutSlip {
            customer_id,
            entry_id,
        } => reply!(
            service.payout_slip(token, &customer_id, &entry_id, false, lang, None),
            CashReply::PayoutSlip
        ),
    }
}

/// Starts the service call of `effect`. Calls happen now (the fake does its work here); the
/// returned future only waits for the answer.
#[allow(clippy::too_many_lines)] // one arm per service call
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
        Effect::Logout { token } => call!(service.logout(&token), Msg::LoggedOut),
        Effect::Lookup {
            token,
            code,
            seq,
            generation,
        } => {
            let future = service.lookup(&token, &code);
            Some(Box::pin(async move { Msg::Looked(generation, seq, future.await) }) as MsgFuture)
        }
        Effect::Quote {
            token,
            request,
            seq,
            generation,
        } => {
            let future = service.quote(&token, &request);
            Some(Box::pin(async move { Msg::Quoted(generation, seq, future.await) }) as MsgFuture)
        }
        Effect::Customer {
            token,
            call,
            generation,
        } => {
            let future = customer_call(service, &token, call);
            Some(
                Box::pin(async move { Msg::Customer(generation, Box::new(future.await)) })
                    as MsgFuture,
            )
        }
        Effect::QuoteCart {
            token,
            request,
            seq,
            generation,
        } => {
            let future = service.quote(&token, &request);
            Some(
                Box::pin(async move { Msg::CartQuoted(generation, seq, future.await) })
                    as MsgFuture,
            )
        }
        Effect::Search {
            token,
            q,
            generation,
        } => {
            let future = service.product_search(&token, &q);
            Some(Box::pin(async move { Msg::Searched(generation, future.await) }) as MsgFuture)
        }
        Effect::CartVoid {
            token,
            body,
            key,
            override_token,
            generation,
        } => {
            let future = service.cart_void(&token, &body, &key, override_token.as_deref());
            Some(Box::pin(async move { Msg::CartVoided(generation, future.await) }) as MsgFuture)
        }
        Effect::TenderTypes { token, generation } => {
            let future = service.tender_types(&token);
            Some(Box::pin(async move { Msg::Tenders(generation, future.await) }) as MsgFuture)
        }
        Effect::PostSale {
            token,
            id,
            body,
            override_token,
            generation,
        } => {
            let future = service.post_sale(&token, &id, &body, override_token.as_deref());
            Some(
                Box::pin(async move { Msg::Posted(generation, Box::new(future.await)) })
                    as MsgFuture,
            )
        }
        Effect::WatchEvents { .. }
        | Effect::Exit
        | Effect::Receipt { .. }
        | Effect::SaveCart(_)
        | Effect::DeleteCart
        | Effect::LoadCart { .. }
        | Effect::After { .. }
        | Effect::Cash { .. } => None,
    }
}

/// The window language for the receipt labels: `id` or `en` (the bundled translation in use).
fn receipt_language(options: &Options) -> String {
    let language = options.language.clone().unwrap_or_else(|| {
        std::env::var("LANG")
            .unwrap_or_default()
            .split(['_', '.'])
            .next()
            .unwrap_or_default()
            .to_owned()
    });
    if language.starts_with("id") {
        "id".to_owned()
    } else {
        "en".to_owned()
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
                self.watchers.start(&self.runner, watching);
                continue;
            }
            if effect == Effect::Exit {
                if let Some(ui) = self.ui.upgrade() {
                    // Ends `run_event_loop` when this is the last window.
                    let _ = ui.hide();
                }
                continue;
            }
            if self.local(&effect) {
                continue;
            }
            let effect = match effect {
                Effect::After {
                    token,
                    call,
                    generation,
                } => {
                    let lang = receipt_language(&self.options);
                    let future = after_call(self.service.as_ref(), &token, call, &lang);
                    let future: MsgFuture =
                        Box::pin(async move { Msg::After(generation, Box::new(future.await)) });
                    self.spawn_or_block(future);
                    continue;
                }
                Effect::Cash {
                    token,
                    call,
                    generation,
                } => {
                    let lang = receipt_language(&self.options);
                    let future = cash_call(self.service.as_ref(), &token, call, &lang);
                    let future: MsgFuture =
                        Box::pin(async move { Msg::Cash(generation, Box::new(future.await)) });
                    self.spawn_or_block(future);
                    continue;
                }
                Effect::Receipt {
                    token,
                    id,
                    generation,
                } => {
                    let lang = receipt_language(&self.options);
                    let future = self.service.receipt(&token, &id, false, &lang, None);
                    let future: MsgFuture =
                        Box::pin(async move { Msg::ReceiptText(generation, future.await) });
                    self.spawn_or_block(future);
                    continue;
                }
                other => other,
            };
            let Some(future) = start(self.service.as_ref(), effect) else {
                continue;
            };
            self.spawn_or_block(future);
        }
    }

    fn spawn_or_block(&self, future: MsgFuture) {
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

    /// The crash-copy file effects, done on the spot (a small local file). Errors only cost the
    /// crash copy, never the sale. Returns whether `effect` was one of them.
    fn local(&self, effect: &Effect) -> bool {
        let file = self.options.cart_file.as_ref();
        match effect {
            Effect::SaveCart(json) => {
                if let Some(f) = file {
                    let _ = f.save(json);
                }
            }
            Effect::DeleteCart => {
                if let Some(f) = file {
                    let _ = f.delete();
                }
            }
            Effect::LoadCart { generation } => push(
                &self.inbox,
                Msg::CartFile(*generation, file.and_then(crate::crash::CartFile::load)),
            ),
            _ => return false,
        }
        true
    }

    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let c = self.controller.borrow();
        let now = (self.options.clock)();
        render(&ui, &c, now, &self.models, &self.options);
    }
}

/// One chip per shortcut; keys of the same action and label share one chip
/// (`Ctrl+N / Ctrl+Del`).
fn chips(shortcuts: &[crate::controller::Shortcut]) -> Vec<Chip> {
    let mut out: Vec<(crate::controller::Shortcut, String)> = Vec::new();
    for s in shortcuts {
        if let Some((_, keys)) = out
            .iter_mut()
            .find(|(o, _)| o.action == s.action && o.label == s.label)
        {
            keys.push_str(" / ");
            keys.push_str(&s.key.label());
        } else {
            out.push((*s, s.key.label()));
        }
    }
    out.into_iter()
        .map(|(s, keys)| Chip {
            keys: keys.into(),
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
    let mut lines = lines;
    // Spec 2.7 §3: the float kept and the rest to the safe (none before 2.7).
    if let Some(kept) = r.float.float_kept {
        lines.push(line("float_kept", rupiah(kept), false));
    }
    if let Some(safe) = r.float.to_safe {
        lines.push(line("to_safe", rupiah(safe), true));
    }
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

/// How the shown item is priced (spec 2.3 §5): `label` (a price-embedded scale label: the printed
/// amount is final), `open` (typed at sale), `none` (no store price), `weight` (a weight label:
/// weight × unit price) or `unit` (the price of one unit).
fn price_kind(item: &Lookup) -> &'static str {
    if item.amount.is_some() {
        "label"
    } else if item.open_price {
        "open"
    } else if item.unit_price.is_none() {
        "none"
    } else if match_kind(item) == "scale_weight" {
        "weight"
    } else {
        "unit"
    }
}

/// The customer-price part of the item panel (spec 2.3 R5): `cust_state` 0 none, 1 waiting,
/// 2 a price of its own, 3 the store price, 4 failed.
fn customer_price(
    view: &mut ScanView,
    item: &Lookup,
    price: &CustomerPrice,
    customer: Option<&Customer>,
    now: i64,
) {
    let line = match price {
        CustomerPrice::None => return,
        CustomerPrice::Waiting(_) => {
            view.cust_state = 1;
            return;
        }
        CustomerPrice::Failed(notice) => {
            view.cust_state = 4;
            view.cust_note = note(Some(notice), now);
            return;
        }
        CustomerPrice::Priced(line) => line,
    };
    let Some(unit_price) = line.unit_price else {
        view.cust_state = 4;
        view.cust_note = note(Some(&Notice::bad("no_price")), now);
        return;
    };
    view.cust_state = if Some(unit_price) == item.unit_price {
        3
    } else {
        2
    };
    view.cust_price = rupiah(unit_price).into();
    if view.price_kind == "weight" {
        view.cust_total = line.amount.map(rupiah).unwrap_or_default().into();
    }
    view.cust_diff = percent_off(unit_price, item.unit_price).into();
    if let Some(source) = &line.source {
        view.cust_source = source.kind.as_str().into();
        let name = match source.kind.as_str() {
            "tier" => customer.and_then(|c| c.tier_name.as_deref()),
            "group" => customer.and_then(|c| c.group_name.as_deref()),
            _ => None,
        };
        view.cust_name = name.unwrap_or_default().into();
    }
}

/// The barcode field and the shown item (phase C5) with its prices (phase R5): the store price
/// of one unit, the amount of a scale label, and the attached customer's price.
fn scan_view(scan: &ScanField, customer: Option<&Customer>, now: i64) -> ScanView {
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
            view.price_kind = price_kind(item).into();
            view.price = item.unit_price.map(rupiah).unwrap_or_default().into();
            if view.price_kind == "weight" {
                view.total = item
                    .unit_price
                    .zip(item.qty_thousandths)
                    .and_then(|(p, q)| line_amount(p, q))
                    .map(rupiah)
                    .unwrap_or_default()
                    .into();
            }
            customer_price(&mut view, item, &shown.customer, customer, now);
        }
    }
    view
}

/// A cart line as the table shows it (spec 2.6 §2).
fn cart_row(
    line: &Line,
    totals: Option<&sultan_core::sale::LineTotals>,
    selected: bool,
    last: bool,
) -> CartRow {
    let flag = if line.error.is_some() {
        "no_price"
    } else if line.core.is_price_label() {
        "label"
    } else if line.overridden() {
        "override"
    } else if line.item.open_price {
        "open"
    } else {
        ""
    };
    let discount = totals.map_or(0, |t| {
        t.line_discount.minor() + t.cart_discount_share.minor()
    });
    let name = if line.item.product_name.is_empty() {
        &line.item.name
    } else {
        &line.item.product_name
    };
    CartRow {
        name: name.as_str().into(),
        variant: line.item.variant_label.as_str().into(),
        qty: qty_text(line.core.qty.thousandths()).into(),
        unit: line.item.unit_name.as_str().into(),
        price: if line.error.is_some() {
            SharedString::new()
        } else {
            rupiah(line.core.unit_price.minor()).into()
        },
        was: line.resolved().map(rupiah).unwrap_or_default().into(),
        struck: line.overridden() || discount > 0,
        discount: if discount > 0 {
            format!("-{}", rupiah(discount)).into()
        } else {
            SharedString::new()
        },
        amount: totals
            .map(|t| rupiah(t.amount.minor()))
            .unwrap_or_default()
            .into(),
        flag: flag.into(),
        selected,
        last,
    }
}

fn sell_row(label: impl Into<SharedString>, detail: impl Into<SharedString>) -> SellRow {
    SellRow {
        label: label.into(),
        detail: detail.into(),
    }
}

/// The sell screen: cart rows, totals, the open dialog, the payment panel and the posted sale.
#[allow(clippy::too_many_lines)] // one block per part of the screen
fn render_sell(
    ui: &MainWindow,
    sell: &Sell,
    c: &Controller,
    notice: Option<&Notice>,
    now: i64,
    models: &Models,
) {
    let totals = sell.cart.totals().ok();
    let rows = sell
        .cart
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            cart_row(
                l,
                totals.as_ref().and_then(|t| t.lines.get(i)),
                sell.focus == Some(i),
                sell.last == Some(i),
            )
        })
        .collect();
    sync(&models.cart_rows, rows);
    let money = |f: fn(&sultan_core::sale::CartTotals) -> i64| {
        totals.as_ref().map(f).map(rupiah).unwrap_or_default()
    };
    let mut view = SellView {
        lines_focus: sell.focus.is_some(),
        check: sell.check,
        pricing: sell.cart.quoting.is_some(),
        empty: sell.cart.is_empty(),
        subtotal: money(|t| t.subtotal.minor()).into(),
        discount: money(|t| t.line_discount.minor() + t.cart_discount.minor()).into(),
        tax: if sell.cart.settings.tax.inclusive {
            SharedString::new()
        } else {
            money(|t| t.tax.minor()).into()
        },
        total: money(|t| t.total.minor()).into(),
        items: sell.cart.item_count().to_string().into(),
        tax_invoice: sell.cart.tax_invoice,
        ..SellView::default()
    };
    let mut list = Vec::new();
    let mut receipt: Vec<SharedString> = Vec::new();
    if let Some(r) = &sell.exchange {
        view.exchange = rupiah(r.refund()).into();
        view.exchange_balance = r.balance;
        if let Some(t) = &totals {
            let (pays, back) = crate::sell::exchange_net(
                r.refund(),
                t.total.minor(),
                sell.cart.settings.rounding,
                r.balance,
            );
            if back > 0 {
                view.exchange_back = rupiah(back).into();
            } else {
                view.exchange_rest = rupiah(pays).into();
            }
        }
    }
    if let Some(modal) = &sell.modal {
        let (kind, title) = match modal {
            Modal::Entry(e) => {
                view.modal_value = e.text.as_str().into();
                view.modal_pct = e.pct;
                let (title, line) = match &e.kind {
                    EntryKind::Qty(i) => ("qty", Some(*i)),
                    EntryKind::Price(i) => ("price", Some(*i)),
                    EntryKind::OpenPrice(item, _) => {
                        view.modal_name = item.name.as_str().into();
                        view.modal_hint = item.unit_price.map(rupiah).unwrap_or_default().into();
                        ("open_price", None)
                    }
                    EntryKind::LineDiscount(i) => ("line_discount", Some(*i)),
                    EntryKind::CartDiscount => ("cart_discount", None),
                };
                if let Some(l) = line.and_then(|i| sell.cart.lines.get(i)) {
                    view.modal_name = format!(
                        "{} · {} {} · {}",
                        l.item.name,
                        qty_text(l.core.qty.thousandths()),
                        l.item.unit_name,
                        rupiah(l.core.unit_price.minor())
                    )
                    .into();
                }
                ("entry", title)
            }
            Modal::Reason(r) => {
                view.modal_cursor = index(r.cursor);
                view.modal_text = r.text.as_str().into();
                list = VOID_REASONS.iter().map(|k| sell_row(*k, "")).collect();
                match r.target {
                    VoidTarget::Line(i) => {
                        view.modal_name = sell
                            .cart
                            .lines
                            .get(i)
                            .map(|l| l.item.name.clone())
                            .unwrap_or_default()
                            .into();
                        ("reason", "reason_line")
                    }
                    VoidTarget::Cart => ("reason", "reason_cart"),
                }
            }
            Modal::Units { line, cursor } => {
                view.modal_cursor = index(*cursor);
                if let Some(l) = sell.cart.lines.get(*line) {
                    view.modal_name = l.item.name.as_str().into();
                    list = l
                        .item
                        .units
                        .iter()
                        .map(|u| sell_row(u.name.as_str(), ""))
                        .collect();
                }
                ("units", "units")
            }
            Modal::Search(search) => {
                view.modal_value = search.query.as_str().into();
                view.modal_cursor = index(search.cursor);
                list = search
                    .results
                    .iter()
                    .map(|h| sell_row(h.name.as_str(), h.sku.as_str()))
                    .collect();
                ("search", "search")
            }
            Modal::Help => {
                list = c
                    .home_actions()
                    .iter()
                    .map(|s| sell_row(s.key.label(), s.label))
                    .chain(
                        [
                            ("Ctrl+↑ ↑ ↓", "to_lines"),
                            ("Enter", "edit_qty"),
                            ("Del", "void_line"),
                            ("+ −", "more"),
                            ("U", "unit"),
                            ("P", "price"),
                            ("D", "discount"),
                            ("Ctrl+N / Ctrl+Del", "void_cart"),
                            ("F8", "save_change"),
                            ("F11", "hold"),
                            ("F10", "returns"),
                            ("F6 → R", "reprint"),
                            ("F6 → V", "post_void"),
                            ("F6 → T", "start_return"),
                            ("F10 → F9", "exchange"),
                            ("F3 → F9", "history"),
                            ("Shift+F5 → R", "reverse"),
                            ("Shift+F5 → P", "reprint"),
                        ]
                        .map(|(k, l)| sell_row(k, l)),
                    )
                    .collect();
                ("help", "help")
            }
            Modal::Cash(_) | Modal::XReport(_) => ("cash", "cash"),
            other => after_view(other, c, &mut view, &mut list, &mut receipt),
        };
        view.modal = kind.into();
        view.modal_title = title.into();
    }
    sync(&models.sell_rows, list);
    let mut paid = Vec::new();
    let mut tenders = Vec::new();
    if let Some(pay) = &sell.pay {
        view.pay_open = true;
        view.pay_step = match pay.step {
            PayStep::Cash => "cash",
            PayStep::Pick(_) => "pick",
            PayStep::Amount(_) => "amount",
            PayStep::Reference(..) => "reference",
            PayStep::Ready => "ready",
            PayStep::Save => "save",
        }
        .into();
        view.pay_total = rupiah(pay.payment.total().minor()).into();
        view.pay_rest = rupiah(pay.rest()).into();
        view.pay_change = rupiah(pay.change() - pay.saved()).into();
        view.pay_saved = Some(pay.saved())
            .filter(|s| *s > 0)
            .map(rupiah)
            .unwrap_or_default()
            .into();
        let rounding = pay.payment.cash_rounding().minor();
        view.pay_rounding = if rounding == 0 {
            SharedString::new()
        } else {
            crate::amount::signed_rupiah(rounding).into()
        };
        view.pay_balance = pay.balance.map(rupiah).unwrap_or_default().into();
        if let Some(r) = &sell.exchange {
            let back = pay.pay_back(r.refund(), r.balance);
            if back > 0 {
                view.pay_back = rupiah(back).into();
            }
        }
        view.pay_text = pay.text.as_str().into();
        view.pay_input = if pay.digits.is_empty() {
            SharedString::new()
        } else {
            rupiah(pay.digits.value()).into()
        };
        view.pay_note = note(notice, now);
        let others = pay.others();
        match pay.step {
            PayStep::Cash => view.pay_due = rupiah(pay.cash_due()).into(),
            PayStep::Pick(i) => {
                view.pay_cursor = index(others.iter().position(|x| *x == i).unwrap_or(0));
            }
            PayStep::Amount(i) | PayStep::Reference(i, _) => {
                view.pay_tender = pay.types[i].name.as_str().into();
                view.pay_due = rupiah(pay.default_amount(i)).into();
            }
            _ => {}
        }
        paid = pay
            .rows()
            .into_iter()
            .map(|(name, amount, reference)| {
                let label = if reference.is_empty() {
                    name
                } else {
                    format!("{name} · {}", sultan_core::sale::mask_reference(&reference))
                };
                sell_row(label, rupiah(amount))
            })
            .collect();
        tenders = others
            .iter()
            .map(|&i| {
                let t = &pay.types[i];
                let detail = if t.kind == "customer_balance" {
                    pay.balance.map(rupiah).unwrap_or_default()
                } else {
                    String::new()
                };
                sell_row(t.name.as_str(), detail)
            })
            .collect();
    }
    sync(&models.pay_rows, paid);
    sync(&models.tender_rows, tenders);
    if let Some(done) = &sell.done {
        let s = &done.sale.summary;
        view.done = true;
        view.done_number = s.number.as_str().into();
        view.done_change = rupiah(s.change_given).into();
        view.done_total = rupiah(s.paid).into();
        view.done_drawer = done.drawer;
        view.done_refund = if done.refund > 0 {
            rupiah(done.refund).into()
        } else {
            SharedString::new()
        };
        receipt = done.receipt.iter().map(|l| l.as_str().into()).collect();
    }
    sync(&models.receipt_lines, receipt);
    ui.set_sell(view);
}

/// The S5 dialogs ([`crate::after`]): kind and title codes, the rows, the typed field, the
/// receipt beside the list and the large amount.
#[allow(clippy::too_many_lines)] // one block per dialog
fn after_view(
    modal: &Modal,
    c: &Controller,
    view: &mut SellView,
    list: &mut Vec<SellRow>,
    receipt: &mut Vec<SharedString>,
) -> (&'static str, &'static str) {
    let branch = c.branch.as_ref();
    let branch_code = |id: &str| -> String {
        match branch {
            Some(b) if b.id == id => b.code.clone(),
            _ => id.chars().take(8).collect(),
        }
    };
    let refund_code = |balance: bool| if balance { "balance" } else { "cash" };
    match modal {
        Modal::Note(n) => {
            view.modal_field = true;
            view.modal_value = n.text.as_str().into();
            view.modal_placeholder = "note".into();
            ("note", "hold_note")
        }
        Modal::Held(l) => {
            view.modal_cursor = index(l.cursor);
            *list = l
                .rows
                .iter()
                .map(|r| {
                    let mut detail = format!("{} item · {}", r.items, rupiah(r.total));
                    if !r.customer.is_empty() {
                        detail = format!("{} · {detail}", r.customer);
                    }
                    if !r.note.is_empty() {
                        detail = format!("{detail} · {}", r.note);
                    }
                    sell_row(
                        format!("{} · {} · {}", r.register, r.cashier, time_of_day(r.at)),
                        detail,
                    )
                })
                .collect();
            ("list", "held")
        }
        Modal::Docs(d) => {
            view.modal_cursor = index(d.cursor);
            if let Some((_, name)) = &d.history {
                view.modal_name = name.as_str().into();
            } else {
                view.modal_field = true;
                view.modal_value = d.number.as_str().into();
                view.modal_placeholder = "receipt_number".into();
            }
            *list = d
                .rows
                .iter()
                .map(|doc| {
                    let mut detail = format!("{} · {}", date_time(doc.at), rupiah(doc.total));
                    if doc.voided {
                        detail.push_str(" · VOID");
                    }
                    if let Some((_, name)) = &d.history {
                        detail = format!("{detail} · {}", branch_code(&doc.branch));
                        // A merged customer's documents name who they were posted for.
                        if !doc.customer_name.is_empty() && doc.customer_name != *name {
                            detail = format!("{detail} · {}", doc.customer_name);
                        }
                    } else if !doc.customer_name.is_empty() {
                        detail = format!("{detail} · {}", doc.customer_name);
                    } else if doc.customer {
                        detail.push_str(" · ♦");
                    }
                    let label = if doc.kind == "return" {
                        format!("↩ {}", doc.number)
                    } else {
                        doc.number.clone()
                    };
                    sell_row(label, detail)
                })
                .collect();
            (
                "list",
                if d.history.is_some() {
                    "history"
                } else {
                    "receipts"
                },
            )
        }
        Modal::Receipt(p) => {
            view.modal_receipt = true;
            *receipt = p.lines.iter().map(|l| l.as_str().into()).collect();
            if let Some(sale) = &p.sale {
                let s = &sale.summary;
                let mut name = format!("{} · {}", s.number, rupiah(s.paid));
                if s.status == "voided" {
                    name.push_str(" · VOID");
                }
                view.modal_name = name.into();
            }
            if let Some((cash, balance)) = p.refund {
                view.modal_big = rupiah(cash).into();
                view.modal_big_label = "cash_back".into();
                if balance > 0 {
                    view.modal_refund = "balance".into();
                    view.modal_hint = rupiah(balance).into();
                }
            }
            ("slip", "receipt")
        }
        Modal::PostVoid(v) => {
            view.modal_cursor = index(v.cursor);
            view.modal_text = v.text.as_str().into();
            view.modal_name = format!(
                "{} · {}",
                v.sale.summary.number,
                rupiah(v.sale.summary.paid)
            )
            .into();
            view.modal_refund = refund_code(v.balance).into();
            *list = VOID_REASONS.iter().map(|k| sell_row(*k, "")).collect();
            ("reason", "post_void")
        }
        Modal::Return(r) => {
            view.modal_cursor = index(r.cursor);
            if let Some(sale) = &r.sale {
                view.modal_name = sale.summary.number.as_str().into();
            }
            if let Some((_, reason)) = &r.write_off {
                view.modal_field = true;
                view.modal_value = reason.as_str().into();
                view.modal_placeholder = "write_off".into();
                if let Some(l) = r.lines.get(r.cursor) {
                    view.modal_name = l.name.as_str().into();
                }
                return ("list", "write_off");
            }
            if let Some(typed) = &r.typed_qty {
                view.modal_field = true;
                view.modal_value = typed.as_str().into();
                view.modal_placeholder = "return_qty".into();
                if let Some(l) = r.lines.get(r.cursor) {
                    // Without a receipt nothing caps the quantity.
                    view.modal_name = if r.no_receipt {
                        l.name.as_str().into()
                    } else {
                        format!("{} · ≤ {}", l.name, qty_text(l.left)).into()
                    };
                }
                return ("list", "return_qty");
            }
            match r.step {
                ReturnStep::Find => {
                    view.modal_field = true;
                    view.modal_value = r.text.as_str().into();
                    view.modal_placeholder = "receipt_number".into();
                    *list = r
                        .recent
                        .iter()
                        .map(|s| {
                            sell_row(
                                s.number.as_str(),
                                format!("{} · {}", date_time(s.posted_at), rupiah(s.paid)),
                            )
                        })
                        .collect();
                    ("list", "return_find")
                }
                ReturnStep::Lines => {
                    if r.no_receipt {
                        view.modal_field = true;
                        view.modal_value = r.text.as_str().into();
                        view.modal_placeholder = "item_code".into();
                    }
                    *list = r
                        .lines
                        .iter()
                        .map(|l| {
                            let mut detail = if r.no_receipt {
                                format!("{} {}", qty_text(l.qty), l.unit)
                            } else {
                                format!("{} / {}", qty_text(l.qty), qty_text(l.left))
                            };
                            detail = format!("{detail} · {}", rupiah(l.refund));
                            if let Some(w) = &l.write_off {
                                detail = format!("{detail} · ✎ {w}");
                            }
                            sell_row(l.name.as_str(), detail)
                        })
                        .collect();
                    view.modal_big = rupiah(r.refund()).into();
                    view.modal_big_label = "refund_total".into();
                    if r.refund_customer().is_some() {
                        view.modal_refund = refund_code(r.balance).into();
                    }
                    (
                        "list",
                        if r.no_receipt {
                            "return_no_receipt"
                        } else {
                            "return_lines"
                        },
                    )
                }
                ReturnStep::Done => {
                    let posted = r.result.as_deref();
                    let cash: i64 = posted
                        .map(|p| {
                            p.tenders
                                .iter()
                                .filter(|t| t.kind == "cash")
                                .map(|t| t.amount)
                                .sum()
                        })
                        .unwrap_or_default();
                    let balance: i64 = posted
                        .map(|p| {
                            p.tenders
                                .iter()
                                .filter(|t| t.kind == "customer_balance")
                                .map(|t| t.amount)
                                .sum()
                        })
                        .unwrap_or_default();
                    if let Some(p) = posted {
                        view.modal_name =
                            format!("{} · {}", p.summary.number, rupiah(p.summary.refund)).into();
                    }
                    view.modal_big = rupiah(cash).into();
                    view.modal_big_label = "cash_back".into();
                    if balance > 0 {
                        view.modal_refund = "balance".into();
                        view.modal_hint = rupiah(balance).into();
                    }
                    if !r.slip.is_empty() {
                        view.modal_receipt = true;
                        *receipt = r.slip.iter().map(|l| l.as_str().into()).collect();
                    }
                    *list = r
                        .lines
                        .iter()
                        .filter(|l| l.qty > 0)
                        .map(|l| {
                            let mut detail = format!("{} · {}", qty_text(l.qty), rupiah(l.refund));
                            if let Some(w) = &l.write_off {
                                detail = format!("{detail} · ✎ {w}");
                            }
                            sell_row(l.name.as_str(), detail)
                        })
                        .collect();
                    ("slip", "return_done")
                }
            }
        }
        _ => ("", ""),
    }
}

/// A name-search result: the name and `K7QH-29XM · 0812****789 · Toko Siti · Gold`.
fn customer_row(c: &Customer) -> CustomerRow {
    let tier = c
        .tier_name
        .as_deref()
        .filter(|_| c.tier_applies == Some(true));
    let detail = [
        Some(c.display_code.as_str()),
        c.masked_phone.as_deref(),
        c.company_name.as_deref(),
        tier,
    ]
    .into_iter()
    .flatten()
    .filter(|p| !p.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    CustomerRow {
        name: c.name.as_str().into(),
        detail: detail.into(),
    }
}

/// The customer dialog (spec 2.5 M5): the page, the typed texts and the shown customer.
fn customer_view(d: &Dialog, perms: Perms, attached: Option<&Customer>, now: i64) -> CustomerView {
    let text = |t: Option<&String>| SharedString::from(t.map_or("", String::as_str));
    let mut view = CustomerView {
        open: true,
        page: d.page.name().into(),
        by_name: d.find.by_name,
        can_search: perms.search,
        query: d.find.query.as_str().into(),
        cursor: index(d.find.cursor),
        field: index(d.sign_up.field),
        new_name: d.sign_up.name.as_str().into(),
        new_phone: d.sign_up.phone.as_str().into(),
        amount: rupiah(d.payout.amount.value()).into(),
        confirm: d.payout.confirm,
        note: note(d.notice.as_ref(), now),
        ..CustomerView::default()
    };
    if let Some(c) = &d.shown {
        view.name = c.name.as_str().into();
        view.company = text(c.company_name.as_ref());
        view.code = c.display_code.as_str().into();
        view.phone = text(c.masked_phone.as_ref());
        view.group = text(c.group_name.as_ref());
        view.tier = text(c.tier_name.as_ref());
        view.tier_until = text(c.tier_valid_until.as_ref());
        view.tier_expired = c.tier_applies == Some(false);
        view.balance = rupiah(c.balance).into();
        view.line = receipt_line(c).into();
        view.attached = attached.is_some_and(|a| a.id == c.id);
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

/// The cash dialog or the X report (spec 2.7 K4), and the printable text on screen: the payout
/// slip of the customer dialog, the cash dialog's slip or X, or the close report / Z.
fn render_cash(ui: &MainWindow, c: &Controller, now: i64, models: &Models) {
    let home_notice = match &c.screen {
        Screen::Home(h) => h.notice.as_ref(),
        _ => None,
    };
    let modal = c.sell().and_then(|s| s.modal.as_ref());
    let mut view = CashView::default();
    let mut reasons = Vec::new();
    let mut rows = Vec::new();
    let mut text: Vec<SharedString> = Vec::new();
    match modal {
        Some(Modal::XReport(x)) => {
            view.open = true;
            view.mode = "x".into();
            view.note = note(home_notice, now);
            text = x.lines.iter().map(|l| l.as_str().into()).collect();
        }
        Some(Modal::Cash(d)) => {
            use crate::cash::{Focus, Step};
            view.open = true;
            view.mode = "cash".into();
            view.kind = d.kind.key().into();
            view.step = match d.step {
                Step::Form => "form",
                Step::Confirm => "confirm",
                Step::Reverse => "reverse",
                Step::Slip => "slip",
            }
            .into();
            view.focus = match d.focus {
                Focus::Amount => "amount",
                Focus::Note => "note",
                Focus::List => "list",
            }
            .into();
            view.amount = if d.amount.is_empty() {
                SharedString::new()
            } else {
                rupiah(d.amount.value()).into()
            };
            view.text = d.note.as_str().into();
            view.reason_cursor = index(d.reason);
            view.list_cursor = index(d.cursor);
            view.reverse_note = d.reverse_note.as_str().into();
            view.note = note(home_notice, now);
            let shown = d.shown_reasons();
            reasons = shown
                .iter()
                .map(|r| sell_row(r.name.as_str(), ""))
                .collect();
            view.subject = match d.step {
                Step::Confirm => shown
                    .get(d.reason)
                    .map(|r| r.name.clone())
                    .unwrap_or_default()
                    .into(),
                Step::Reverse => d
                    .movements
                    .get(d.cursor)
                    .map(|m| format!("{} · {}", m.number, rupiah(m.amount)))
                    .unwrap_or_default()
                    .into(),
                Step::Slip => d.slip_number.as_str().into(),
                Step::Form => SharedString::new(),
            };
            rows = d
                .movements
                .iter()
                .map(|m| CashRow {
                    number: m.number.as_str().into(),
                    kind: m.kind.as_str().into(),
                    detail: crate::cash::movement_detail(m).into(),
                })
                .collect();
            if d.step == Step::Slip {
                text = d.slip.iter().map(|l| l.as_str().into()).collect();
            }
        }
        _ => {}
    }
    if let Some(d) = c
        .dialog
        .as_ref()
        .filter(|d| d.page == crate::customer::Page::Slip)
    {
        text = d.slip.iter().map(|l| l.as_str().into()).collect();
    } else if let Screen::Report(r) = &c.screen {
        text = r.text.iter().map(|l| l.as_str().into()).collect();
    }
    ui.set_cash(view);
    sync(&models.cash_reasons, reasons);
    sync(&models.cash_rows, rows);
    sync(&models.text_lines, text);
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
    // The bar shows the keys that are not buttons of the session home.
    let buttons = c.home_actions();
    // The barcode field has its own Enter and Esc buttons.
    let idle = c.dialog.is_none()
        && c.approval.is_none()
        && c.sell().is_some_and(|s| {
            s.modal.is_none() && s.pay.is_none() && s.done.is_none() && s.focus.is_none()
        });
    let bar: Vec<_> = c
        .shortcuts()
        .into_iter()
        .filter(|s| !idle || (!buttons.contains(s) && !matches!(s.key, Key::Enter | Key::Escape)))
        .collect();
    let bar_chips = chips(&bar);
    let (top, bottom) = if bar_chips.len() > 4 {
        let half = bar_chips.len().div_ceil(2);
        (bar_chips[..half].to_vec(), bar_chips[half..].to_vec())
    } else {
        (bar_chips.clone(), Vec::new())
    };
    sync(&models.dialog_chips, top);
    sync(&models.dialog_chips_b, bottom);
    sync(&models.chips, bar_chips);
    let half = buttons.len().div_ceil(2);
    sync(&models.actions, chips(&buttons[..half]));
    sync(&models.actions_b, chips(&buttons[half..]));
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
                proposed: o
                    .proposed
                    .filter(|p| *p > 0)
                    .map(rupiah)
                    .unwrap_or_default()
                    .into(),
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
                customer: c
                    .customer
                    .as_ref()
                    .map(receipt_line)
                    .unwrap_or_default()
                    .into(),
                customer_code: c
                    .customer
                    .as_ref()
                    .map(|x| x.display_code.as_str())
                    .unwrap_or_default()
                    .into(),
                customer_balance: c
                    .customer
                    .as_ref()
                    .map(|x| rupiah(x.balance))
                    .unwrap_or_default()
                    .into(),
            });
            ui.set_scan(scan_view(&h.scan, c.customer.as_ref(), now));
            render_sell(ui, &h.sell, c, h.notice.as_ref(), now, models);
        }
        Screen::Close(close) => {
            sync(&models.denoms, denominations(&close.form));
            ui.set_closing(CloseView {
                cursor: index(close.form.cursor),
                total: rupiah(close.form.counted()).into(),
                total_locked: close.form.uses_denominations(),
                confirm: close.confirm,
                float: rupiah(close.float.value()).into(),
                to_safe: rupiah(close.form.counted() - close.float.value()).into(),
                note: note(close.notice.as_ref(), now),
            });
        }
        Screen::Report(screen) => {
            let report = &screen.report;
            let (lines, tenders) = report_lines(report);
            ui.set_report(ReportView {
                number: report.session.number.as_str().into(),
                over_short: signed_rupiah(report.over_short).into(),
                tone: Tone::of_amount(report.over_short).value(),
                forced: report.forced,
                has_tenders: !tenders.is_empty(),
                z: match &screen.z {
                    ZStep::Hidden => "hidden",
                    ZStep::Offered => "offered",
                    ZStep::Confirm => "confirm",
                    ZStep::Done(_) => "done",
                }
                .into(),
                day: screen.day().into(),
                z_number: match &screen.z {
                    ZStep::Done(z) => z.report.number.as_str().into(),
                    _ => SharedString::new(),
                },
                note: note(screen.notice.as_ref(), now),
            });
            sync(&models.report_lines, lines);
            sync(&models.report_tenders, tenders);
        }
    }
    if !matches!(c.screen, Screen::Report(_)) {
        sync(&models.report_lines, Vec::new());
        sync(&models.report_tenders, Vec::new());
    }
    if !matches!(c.screen, Screen::Home(_)) {
        ui.set_sell(SellView::default());
        sync(&models.cart_rows, Vec::new());
    }

    let dialog = c.dialog.as_ref();
    ui.set_customer(dialog.map_or_else(CustomerView::default, |d| {
        customer_view(d, c.perms, c.customer.as_ref(), now)
    }));
    sync(
        &models.customers,
        dialog
            .map(|d| d.find.results.iter().map(customer_row).collect())
            .unwrap_or_default(),
    );

    render_cash(ui, c, now, models);

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
                "customer" => Msg::PickCustomer(i),
                "field" => Msg::PickField(i),
                "line" => Msg::PickSell("line", i),
                "item" => Msg::PickSell("item", i),
                "tender" => Msg::PickSell("tender", i),
                "reason" => Msg::PickSell("reason", i),
                "movement" => Msg::PickSell("movement", i),
                "kind" => Msg::PickSell("kind", i),
                "cash-field" => Msg::PickSell("field", i),
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
        customers: Rc::default(),
        cart_rows: Rc::default(),
        actions_b: Rc::default(),
        sell_rows: Rc::default(),
        pay_rows: Rc::default(),
        tender_rows: Rc::default(),
        receipt_lines: Rc::default(),
        dialog_chips: Rc::default(),
        dialog_chips_b: Rc::default(),
        cash_reasons: Rc::default(),
        cash_rows: Rc::default(),
        text_lines: Rc::default(),
    };
    ui.set_users(ModelRc::from(models.users.clone()));
    ui.set_managers(ModelRc::from(models.managers.clone()));
    ui.set_chips(ModelRc::from(models.chips.clone()));
    ui.set_actions(ModelRc::from(models.actions.clone()));
    ui.set_denoms(ModelRc::from(models.denoms.clone()));
    ui.set_report_lines(ModelRc::from(models.report_lines.clone()));
    ui.set_report_tenders(ModelRc::from(models.report_tenders.clone()));
    ui.set_customers(ModelRc::from(models.customers.clone()));
    ui.set_cart_rows(ModelRc::from(models.cart_rows.clone()));
    ui.set_actions_b(ModelRc::from(models.actions_b.clone()));
    ui.set_sell_rows(ModelRc::from(models.sell_rows.clone()));
    ui.set_pay_rows(ModelRc::from(models.pay_rows.clone()));
    ui.set_tender_rows(ModelRc::from(models.tender_rows.clone()));
    ui.set_receipt_lines(ModelRc::from(models.receipt_lines.clone()));
    ui.set_dialog_chips(ModelRc::from(models.dialog_chips.clone()));
    ui.set_dialog_chips_b(ModelRc::from(models.dialog_chips_b.clone()));
    ui.set_cash_reasons(ModelRc::from(models.cash_reasons.clone()));
    ui.set_cash_rows(ModelRc::from(models.cash_rows.clone()));
    ui.set_text_lines(ModelRc::from(models.text_lines.clone()));
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
    let (mut controller, effects) = Controller::new();
    controller.start = options.start;
    controller
        .register_label
        .clone_from(&options.register_label);
    let state = Rc::new(State {
        watchers: Watchers::default(),
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
    let result = app.ui.run();
    // The window is closed: its event socket must not outlive it.
    app.state.watchers.stop();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn watchers_are_aborted_when_replaced_or_stopped() {
        let watchers = Watchers::default();
        let pending = || tokio::spawn(std::future::pending::<()>());
        let first = pending();
        watchers.replace(first.abort_handle());
        let second = pending();
        watchers.replace(second.abort_handle());
        assert!(first.await.unwrap_err().is_cancelled(), "replaced");
        watchers.stop();
        assert!(second.await.unwrap_err().is_cancelled(), "stopped");
        let third = pending();
        watchers.replace(third.abort_handle());
        drop(watchers);
        assert!(third.await.unwrap_err().is_cancelled(), "dropped");
    }

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
