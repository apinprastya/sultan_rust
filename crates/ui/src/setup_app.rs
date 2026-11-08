//! Glue between the [`SetupWindow`] and the [`SetupController`], the same way [`crate::app`]
//! drives the register window: callbacks become [`Msg`]s, [`Effect`]s become [`SetupService`]
//! calls whose answers come back as messages, and the window is rendered from the controller
//! after each batch. [`Runner::Block`] awaits every call on the spot (tests);
//! [`Runner::Spawn`] runs them on the runtime and wakes the window through `pump`.
//!
//! [`Effect::Close`] hides the window; [`run`] then returns the app type the wizard finished
//! with, or `None` when the window was closed early.

use std::cell::{Cell, Ref, RefCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use tokio::runtime::Handle;

use crate::app::Runner;
use crate::input::Key;
use crate::setup::{
    Action, AppType, DbKind, Effect, Field, Msg, Pick, SetupController, SetupService, Summary,
};
use crate::{Note, SetupForm, SetupWindow};

type Inbox = Arc<Mutex<VecDeque<Msg>>>;
type Wake = Arc<dyn Fn() + Send + Sync>;
type MsgFuture = Pin<Box<dyn Future<Output = Msg> + Send>>;

fn push(inbox: &Inbox, msg: Msg) {
    inbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(msg);
}

struct State {
    ui: slint::Weak<SetupWindow>,
    controller: RefCell<SetupController>,
    inbox: Inbox,
    wake: Wake,
    service: Arc<dyn SetupService>,
    runner: Runner,
    branches: Rc<VecModel<SharedString>>,
    draining: Cell<bool>,
}

/// The setup window and its state.
pub struct Setup {
    /// The window.
    pub ui: SetupWindow,
    state: Rc<State>,
}

impl Setup {
    /// The controller (read-only), for tests.
    #[must_use]
    pub fn controller(&self) -> Ref<'_, SetupController> {
        self.state.controller.borrow()
    }

    /// Sends a message as if the window had.
    #[must_use = "whether a key was used"]
    pub fn dispatch(&self, msg: Msg) -> bool {
        self.state.dispatch(msg)
    }

    /// The app type the wizard finished with, once it has.
    #[must_use]
    pub fn outcome(&self) -> Option<AppType> {
        self.state.controller.borrow().outcome
    }
}

/// Starts the call of `effect`; `None` for [`Effect::Close`].
fn start(service: &dyn SetupService, effect: &Effect) -> Option<MsgFuture> {
    macro_rules! call {
        ($future:expr, $msg:path) => {{
            let future = $future;
            Some(Box::pin(async move { $msg(future.await) }) as MsgFuture)
        }};
    }
    match effect {
        Effect::CheckDb { url } => call!(service.check_db(url), Msg::DbChecked),
        Effect::InitShop(request) => call!(service.init_shop(request), Msg::ShopDone),
        Effect::CheckServer { server } => call!(service.check_server(server), Msg::ServerChecked),
        Effect::Login {
            server,
            username,
            password,
        } => call!(
            service.manager_login(server, username, password),
            Msg::LoggedIn
        ),
        Effect::Register {
            server,
            token,
            request,
        } => call!(
            service.register_terminal(server, token, request),
            Msg::Paired
        ),
        Effect::Pair { server, code } => call!(service.pair_with_code(server, code), Msg::Paired),
        Effect::Close => None,
    }
}

impl State {
    fn dispatch(&self, msg: Msg) -> bool {
        let step = self.controller.borrow_mut().update(msg);
        self.run(step.effects);
        self.drain();
        self.render();
        step.handled
    }

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
            let Some(future) = start(self.service.as_ref(), &effect) else {
                if let Some(ui) = self.ui.upgrade() {
                    // Ends `run_event_loop` when this is the last window.
                    let _ = ui.hide();
                }
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
        render(&ui, &c, &self.branches);
    }
}

fn form(c: &SetupController) -> SetupForm {
    let f = &c.form;
    SetupForm {
        sqlite_path: f.sqlite_path.as_str().into(),
        mysql_url: f.mysql_url.as_str().into(),
        listen: f.listen.as_str().into(),
        company: f.company.as_str().into(),
        branch_code: f.branch_code.as_str().into(),
        branch_name: f.branch_name.as_str().into(),
        location: f.location.as_str().into(),
        owner_username: f.owner_username.as_str().into(),
        owner_name: f.owner_name.as_str().into(),
        server: f.server.as_str().into(),
        manager: f.manager.as_str().into(),
        register_code: f.register_code.as_str().into(),
        register_name: f.register_name.as_str().into(),
        pairing_code: f.pairing_code.as_str().into(),
    }
}

fn index(i: usize) -> i32 {
    i32::try_from(i).unwrap_or(i32::MAX)
}

fn render(ui: &SetupWindow, c: &SetupController, branches: &VecModel<SharedString>) {
    ui.set_page(c.page.name().into());
    ui.set_cursor(index(c.cursor));
    ui.set_busy(c.busy);
    ui.set_note(c.note.as_ref().map_or_else(Note::default, |n| Note {
        code: n.code.as_str().into(),
        arg: n.arg.as_str().into(),
        tone: n.tone.value(),
    }));
    let current = form(c);
    if ui.get_form() != current {
        ui.set_form(current);
    }
    ui.set_db_kind(i32::from(c.db_kind == DbKind::Mysql));
    ui.set_timezone(index(c.timezone));
    let rows: Vec<SharedString> = c
        .session
        .iter()
        .flat_map(|s| &s.branches)
        .map(|b| format!("{} · {}", b.code, b.name).into())
        .collect();
    if slint::Model::iter(branches).ne(rows.iter().cloned()) {
        branches.set_vec(rows);
    }
    ui.set_branch(index(c.branch));
    ui.set_app_type(c.app_type.map_or("", AppType::id).into());
    let (a, b, cc) = match &c.summary {
        Some(Summary::Shop(r)) => (
            r.company.as_str(),
            r.branch_code.as_str(),
            r.db_url.as_str(),
        ),
        Some(Summary::Paired(p)) => (
            p.register_code.as_str(),
            p.register_name.as_str(),
            p.server.as_str(),
        ),
        None => ("", "", ""),
    };
    ui.set_summary_a(a.into());
    ui.set_summary_b(b.into());
    ui.set_summary_c(cc.into());
}

fn connect(ui: &SetupWindow, state: &Rc<State>) {
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
            if let (Some(s), Some(action)) = (state.upgrade(), Action::from_id(&id)) {
                s.dispatch(Msg::Action(action));
            }
        }
    });
    ui.on_field({
        let state = Rc::downgrade(state);
        move |id, value| {
            if let (Some(s), Some(field)) = (state.upgrade(), Field::from_id(&id)) {
                // Typing must not re-render the field it came from: only the model changes.
                s.controller
                    .borrow_mut()
                    .update(Msg::Field(field, value.into()));
            }
        }
    });
    ui.on_pick({
        let state = Rc::downgrade(state);
        move |kind, i| {
            if let (Some(s), Some(pick), Ok(i)) =
                (state.upgrade(), Pick::from_id(&kind), usize::try_from(i))
            {
                s.dispatch(Msg::Pick(pick, i));
            }
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

/// Builds the setup window. `sqlite_path` is the default database file; `language` as in
/// [`crate::Options::language`].
///
/// # Errors
/// The Slint platform error (no display, no backend).
pub fn build_setup(
    service: Arc<dyn SetupService>,
    runner: Runner,
    sqlite_path: &str,
    language: Option<&str>,
) -> Result<Setup, slint::PlatformError> {
    let ui = SetupWindow::new()?;
    if let Some(language) = language
        && slint::select_bundled_translation(language).is_err()
    {
        let _ = slint::select_bundled_translation("en");
    }
    let branches: Rc<VecModel<SharedString>> = Rc::default();
    ui.set_branches(ModelRc::from(branches.clone()));
    let wake: Wake = match &runner {
        Runner::Spawn(_) => {
            let weak = ui.as_weak();
            Arc::new(move || {
                let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_pump());
            })
        }
        Runner::Block(_) => Arc::new(|| {}),
    };
    let state = Rc::new(State {
        ui: ui.as_weak(),
        controller: RefCell::new(SetupController::new(sqlite_path)),
        inbox: Inbox::default(),
        wake,
        service,
        runner,
        branches,
        draining: Cell::new(false),
    });
    connect(&ui, &state);
    state.render();
    Ok(Setup { ui, state })
}

/// Opens the setup wizard and runs the event loop until it is finished or closed. Returns the
/// app type it finished with, or `None` when the window was closed first.
///
/// # Errors
/// The Slint platform error.
pub fn run(
    service: Arc<dyn SetupService>,
    handle: Handle,
    sqlite_path: &str,
    language: Option<&str>,
) -> Result<Option<AppType>, slint::PlatformError> {
    let setup = build_setup(service, Runner::Spawn(handle), sqlite_path, language)?;
    setup.ui.run()?;
    Ok(setup.outcome())
}
