//! Glue between the [`StartWindow`] and the Start [`Controller`], the same way
//! [`crate::setup_app`] drives the wizard: callbacks become [`Msg`]s, [`Effect`]s become
//! [`StartService`] calls whose answers come back as messages, and the window is rendered from
//! the controller after each batch. [`Runner::Block`] awaits the check on the spot (tests);
//! [`Runner::Spawn`] runs it on the runtime and wakes the window through `pump`.
//!
//! [`Effect::SetLanguage`] saves the choice and switches the bundled translation at once, so the
//! window re-renders in the new language. [`Effect::SetLan`] restarts a single app's server like
//! a check: awaited on the spot or spawned, its answer comes back as [`Msg::LanSet`]. [`Effect::Close`] hides the window; [`run`] then
//! returns the choice, or `None` when the window was closed (quit).

use std::cell::{Cell, Ref, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

use slint::{ComponentHandle, ModelRc, VecModel};
use tokio::runtime::Handle;

use crate::app::Runner;
use crate::input::Key;
use crate::start::{Choice, Controller, Effect, Msg, Page, StartInfo, StartService};
use crate::{Chip, Note, StartWindow};

type Inbox = Arc<Mutex<VecDeque<Msg>>>;
type Wake = Arc<dyn Fn() + Send + Sync>;

fn push(inbox: &Inbox, msg: Msg) {
    inbox
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push_back(msg);
}

/// Switches the bundled translation; an unknown language falls back to English (the source).
pub(crate) fn select_language(language: &str) {
    if slint::select_bundled_translation(language).is_err() {
        let _ = slint::select_bundled_translation("en");
    }
}

struct State {
    ui: slint::Weak<StartWindow>,
    controller: RefCell<Controller>,
    inbox: Inbox,
    wake: Wake,
    service: Arc<dyn StartService>,
    runner: Runner,
    chips: Rc<VecModel<Chip>>,
    draining: Cell<bool>,
}

/// The Start window and its state.
pub struct Start {
    /// The window.
    pub ui: StartWindow,
    state: Rc<State>,
}

impl Start {
    /// The controller (read-only), for tests.
    #[must_use]
    pub fn controller(&self) -> Ref<'_, Controller> {
        self.state.controller.borrow()
    }

    /// Handles queued results (what `pump` does on the event loop).
    pub fn pump(&self) {
        self.state.drain();
        self.state.render();
    }

    /// What the person chose, once they have.
    #[must_use]
    pub fn outcome(&self) -> Option<Choice> {
        self.state.controller.borrow().outcome
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
            match effect {
                Effect::Check => self.call(self.service.check(), Msg::Checked),
                Effect::SetLan(on) => self.call(self.service.set_lan(on), Msg::LanSet),
                Effect::SetLanguage(language) => {
                    select_language(&language);
                    push(
                        &self.inbox,
                        Msg::LanguageSaved(self.service.set_language(&language)),
                    );
                }
                Effect::Close => {
                    if let Some(ui) = self.ui.upgrade() {
                        // Ends `run_event_loop` when this is the last window.
                        let _ = ui.hide();
                    }
                }
            }
        }
    }

    /// Runs a service call: awaited on the spot ([`Runner::Block`]) or spawned, its answer
    /// queued as `msg`.
    fn call<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = T> + Send + 'static,
        msg: fn(T) -> Msg,
    ) {
        match &self.runner {
            Runner::Block(handle) => push(&self.inbox, msg(handle.block_on(future))),
            Runner::Spawn(handle) => {
                let inbox = self.inbox.clone();
                let wake = self.wake.clone();
                handle.spawn(async move {
                    push(&inbox, msg(future.await));
                    wake();
                });
            }
        }
    }

    fn render(&self) {
        let Some(ui) = self.ui.upgrade() else { return };
        let c = self.controller.borrow();
        render(&ui, &c, &self.chips);
    }
}

/// The server as shown: without the scheme and a trailing `/api/v1`.
#[must_use]
pub fn server_text(server: &str) -> String {
    let s = server.trim();
    let s = s
        .strip_prefix("http://")
        .or_else(|| s.strip_prefix("https://"))
        .unwrap_or(s);
    let s = s.trim_end_matches('/');
    s.strip_suffix("/api/v1").unwrap_or(s).to_owned()
}

fn render(ui: &StartWindow, c: &Controller, chips: &VecModel<Chip>) {
    let info = &c.info;
    ui.set_page(
        match c.page {
            Page::Home => "home",
            Page::Settings => "settings",
        }
        .into(),
    );
    ui.set_confirm(c.confirm);
    ui.set_kind(info.kind.id().into());
    ui.set_conn(c.conn.id().into());
    ui.set_server(server_text(&info.server).into());
    ui.set_register_code(info.register_code.as_str().into());
    ui.set_register_name(info.register_name.as_str().into());
    ui.set_branch(info.branch.as_str().into());
    ui.set_version(info.version.as_str().into());
    ui.set_language(info.language.as_str().into());
    ui.set_language_fixed(info.language_fixed);
    let lan = info.lan.clone().unwrap_or_default();
    ui.set_lan_shown(info.lan.is_some());
    ui.set_lan_on(lan.on);
    ui.set_lan_fixed(lan.fixed);
    ui.set_lan_busy(c.lan_busy);
    ui.set_lan_port(lan.port.to_string().into());
    ui.set_lan_addresses(lan.addresses.join("\n").into());
    ui.set_note(c.note_view());
    // The home tiles carry F1–F3 and the dialog its own buttons.
    let rows: Vec<Chip> = c
        .shortcuts()
        .into_iter()
        .filter(|s| !(c.confirm || c.page == Page::Home && s.action != "refresh"))
        .map(|s| Chip {
            keys: s.key.label().into(),
            label: s.label.into(),
            id: s.action.into(),
        })
        .collect();
    if slint::Model::iter(chips).ne(rows.iter().cloned()) {
        chips.set_vec(rows);
    }
}

impl Controller {
    fn note_view(&self) -> Note {
        self.notice.as_ref().map_or_else(Note::default, |n| Note {
            code: n.code.into(),
            arg: n.arg.as_str().into(),
            tone: n.tone.value(),
        })
    }
}

fn connect(ui: &StartWindow, state: &Rc<State>) {
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
            if let Some(s) = state.upgrade() {
                s.dispatch(Msg::Action(id.into()));
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

/// Builds the Start window (in `info.language`) and starts the connection check.
///
/// # Errors
/// The Slint platform error (no display, no backend).
pub fn build_start(
    service: Arc<dyn StartService>,
    runner: Runner,
    info: StartInfo,
) -> Result<Start, slint::PlatformError> {
    let ui = StartWindow::new()?;
    select_language(&info.language);
    let chips: Rc<VecModel<Chip>> = Rc::default();
    ui.set_chips(ModelRc::from(chips.clone()));
    let wake: Wake = match &runner {
        Runner::Spawn(_) => {
            let weak = ui.as_weak();
            Arc::new(move || {
                let _ = weak.upgrade_in_event_loop(|ui| ui.invoke_pump());
            })
        }
        Runner::Block(_) => Arc::new(|| {}),
    };
    let (controller, effects) = Controller::new(info);
    let state = Rc::new(State {
        ui: ui.as_weak(),
        controller: RefCell::new(controller),
        inbox: Inbox::default(),
        wake,
        service,
        runner,
        chips,
        draining: Cell::new(false),
    });
    connect(&ui, &state);
    state.run(effects);
    state.drain();
    state.render();
    Ok(Start { ui, state })
}

/// Opens the Start window and runs the event loop until a choice is made or the window is
/// closed (`None`).
///
/// # Errors
/// The Slint platform error.
pub fn run(
    service: Arc<dyn StartService>,
    handle: Handle,
    info: StartInfo,
) -> Result<Option<Choice>, slint::PlatformError> {
    let start = build_start(service, Runner::Spawn(handle), info)?;
    start.ui.run()?;
    Ok(start.outcome())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servers_are_shown_without_scheme_and_api_path() {
        let cases = [
            ("http://192.168.1.10:8080", "192.168.1.10:8080"),
            ("https://hq.example/api/v1/", "hq.example"),
            (" 10.0.0.1:81/ ", "10.0.0.1:81"),
        ];
        for (url, shown) in cases {
            assert_eq!(server_text(url), shown, "{url}");
        }
    }
}
