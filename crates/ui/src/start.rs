//! The Start screen of a single app or a terminal (`docs/plan.md` §3 "UI only, except HQ"): what
//! a bare `sultan` opens once the machine is set up. A plain state machine like
//! [`crate::controller`]; [`crate::start_app`] is its glue to the [`crate::StartWindow`].
//!
//! - **Home** — three big tiles: **F1** the cash register (lock screen), **F2** the back office,
//!   **F3** settings; **F5** checks the connection again. The footer says what the machine is
//!   connected to (single app: its own server; terminal: the paired server) and the register.
//! - **Settings** — **F6** switches the language (Indonesian / English; saved in `sultan.toml`
//!   through [`StartService::set_language`], unless `SULTAN_LANG` fixes it), the connection
//!   details, the version, and **F8** "set up again" (Enter confirms, Esc cancels): the setup
//!   wizard opens, which never overwrites a database that is already set up. Esc goes back.
//! - **Terminals in this shop** (single app only, on Settings) — **F7** (or the switch) allows
//!   terminals on the shop network to connect: the app's server is restarted on the new address
//!   through [`StartService::set_lan`] (all interfaces on the configured port when on, loopback
//!   when off; saved as `listen` in `sultan.toml`). While it restarts, every key waits. When it
//!   is on, the screen lists the addresses terminals type in their setup wizard; when the new
//!   address cannot be bound, the previous setting stays and the screen says why. `SULTAN_LISTEN`
//!   fixes the address, and the switch is then locked ([`Lan::fixed`]).
//!
//! The connection is checked when the screen opens ([`Effect::Check`]); its answer may carry the
//! branch label (`JKT01 · Toko Jakarta`), which then replaces the one shown. The register and the
//! back office open only when the server answers: while the check runs the choice waits for it,
//! after a failure F1/F2 check again first, and if the server still cannot be reached the
//! screen says so instead of opening a window that cannot work. A register the server no longer
//! knows (`invalid_device_token`) can still open the back office, not the register.
//!
//! The screen ends with an [`Choice`] ([`Effect::Close`]); closing the window instead means
//! "quit".

use std::future::Future;
use std::pin::Pin;

use crate::controller::{Notice, Tone};
use crate::input::Key;

/// The language codes the UI is translated into.
pub const LANGUAGES: [&str; 2] = ["id", "en"];

/// What kind of machine this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppKind {
    /// Server and register in one app.
    Single,
    /// A register connected to a server on the shop network.
    Terminal,
}

impl AppKind {
    /// The ID used by the UI.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Terminal => "terminal",
        }
    }
}

/// Whether terminals on the shop network may connect to a single app's server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Lan {
    /// The server listens on every interface (on) or on loopback only (off).
    pub on: bool,
    /// The port terminals connect to when on, e.g. `8080`.
    pub port: u16,
    /// What terminals type in their setup wizard, e.g. `192.168.1.10:8080`: this computer's
    /// network addresses with the port. Empty when none was found.
    pub addresses: Vec<String>,
    /// `SULTAN_LISTEN` sets the listen address on this computer: the switch is locked.
    pub fixed: bool,
    /// Terminals were on, but the server could not listen on that port at start (e.g. another
    /// program uses it): it then listens on loopback only. The port that failed.
    pub failed_port: Option<u16>,
}

/// The answer of [`StartService::set_lan`]: the new setting and where the server now answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanChanged {
    /// The new setting.
    pub lan: Lan,
    /// The server URL for this computer, e.g. `http://127.0.0.1:8080`.
    pub server: String,
}

/// What the Start screen shows about this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartInfo {
    /// Single app or terminal.
    pub kind: AppKind,
    /// The server URL, e.g. `http://192.168.1.10:8080`.
    pub server: String,
    /// Register code, e.g. `R2`.
    pub register_code: String,
    /// Register name, e.g. `Kasir 2`.
    pub register_name: String,
    /// The branch, e.g. `JKT01 · Toko Jakarta`; empty when not known.
    pub branch: String,
    /// The app version.
    pub version: String,
    /// The language in use (`id` or `en`).
    pub language: String,
    /// `SULTAN_LANG` is set: the language cannot be changed here.
    pub language_fixed: bool,
    /// Terminals in this shop (a single app); `None` on a terminal.
    pub lan: Option<Lan>,
}

/// Why the connection check failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartError {
    /// `network` (no answer), `unpaired` (the server does not know this register) or `failed`.
    pub code: &'static str,
    /// Details.
    pub detail: String,
}

impl StartError {
    /// An error.
    #[must_use]
    pub fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// A boxed future of [`StartService::check`]: the branch label (`JKT01 · Toko Jakarta`) when
/// the server says which branch this register belongs to.
pub type StartFuture =
    Pin<Box<dyn Future<Output = Result<Option<String>, StartError>> + Send + 'static>>;

/// A boxed future of [`StartService::set_lan`]: the new setting, or why it was not applied
/// (code `in_use` with the port as the detail, or `failed`).
pub type LanFuture = Pin<Box<dyn Future<Output = Result<LanChanged, StartError>> + Send + 'static>>;

/// The work behind the Start screen.
pub trait StartService: Send + Sync {
    /// Asks the server whether it answers and knows this register.
    fn check(&self) -> StartFuture;
    /// Saves the language (`id` / `en`) for the next start.
    ///
    /// # Errors
    /// Why it could not be saved.
    fn set_language(&self, language: &str) -> Result<(), String>;
    /// A single app: allows terminals to connect (`on`) or not, restarts the server on the new
    /// address and saves it. On an error the previous setting stays in force.
    fn set_lan(&self, on: bool) -> LanFuture;
}

/// What the person chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Open the register (lock screen).
    Register,
    /// Open the back office.
    BackOffice,
    /// Open the setup wizard.
    SetupAgain,
}

/// The connection to the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conn {
    /// Being checked.
    Checking,
    /// The server answers.
    Ok,
    /// The server answers but does not know this register.
    Unpaired,
    /// No answer (or an error); the detail.
    Failed(String),
}

impl Conn {
    /// The ID used by the UI.
    #[must_use]
    pub const fn id(&self) -> &'static str {
        match self {
            Self::Checking => "checking",
            Self::Ok => "ok",
            Self::Unpaired => "unpaired",
            Self::Failed(_) => "failed",
        }
    }
}

/// A page of the Start window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// The tiles.
    Home,
    /// Settings.
    Settings,
}

/// A call or window change for the glue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// [`StartService::check`].
    Check,
    /// [`StartService::set_language`] and switch the window's language.
    SetLanguage(String),
    /// [`StartService::set_lan`].
    SetLan(bool),
    /// Close the window ([`Controller::outcome`] says why).
    Close,
}

/// Input for [`Controller::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// A key.
    Key(Key),
    /// A tile or button: `register`, `backoffice`, `settings`, `refresh`, `language`
    /// (switch), `lang-id`, `lang-en`, `lan` (switch terminals on / off), `setup`, `confirm`,
    /// `back`.
    Action(String),
    /// Result of [`Effect::Check`].
    Checked(Result<Option<String>, StartError>),
    /// Result of [`Effect::SetLanguage`].
    LanguageSaved(Result<(), String>),
    /// Result of [`Effect::SetLan`].
    LanSet(Result<LanChanged, StartError>),
}

/// What [`Controller::update`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Work for the glue.
    pub effects: Vec<Effect>,
    /// For keys: whether the key was used.
    pub handled: bool,
}

/// A key chip: key, label code, action ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    /// The key.
    pub key: Key,
    /// Label code (`StartStrings.chip`).
    pub label: &'static str,
    /// Action ID.
    pub action: &'static str,
}

/// The Start screen's state; see the module docs.
#[derive(Debug)]
pub struct Controller {
    /// About this machine.
    pub info: StartInfo,
    /// The page.
    pub page: Page,
    /// "Set up again?" is being asked.
    pub confirm: bool,
    /// The connection.
    pub conn: Conn,
    /// What to open once the running check succeeds.
    pub pending: Option<Choice>,
    /// A message.
    pub notice: Option<Notice>,
    /// What the person chose, once they have.
    pub outcome: Option<Choice>,
    /// The server is being restarted on a new address ([`Effect::SetLan`]).
    pub lan_busy: bool,
}

impl Controller {
    /// The start state (home, checking the connection) and its first call.
    #[must_use]
    pub fn new(info: StartInfo) -> (Self, Vec<Effect>) {
        let notice = info
            .lan
            .as_ref()
            .and_then(|lan| lan.failed_port)
            .map(|port| Notice::new("lan_start", port.to_string(), Tone::Bad));
        let controller = Self {
            info,
            page: Page::Home,
            confirm: false,
            conn: Conn::Checking,
            pending: None,
            notice,
            outcome: None,
            lan_busy: false,
        };
        (controller, vec![Effect::Check])
    }

    /// The key chips of the current page.
    #[must_use]
    pub fn shortcuts(&self) -> Vec<Shortcut> {
        let s = |key, label, action| Shortcut { key, label, action };
        match self.page {
            Page::Home => vec![
                s(Key::F(1), "register", "register"),
                s(Key::F(2), "backoffice", "backoffice"),
                s(Key::F(3), "settings", "settings"),
                s(Key::F(5), "check_connection", "refresh"),
            ],
            Page::Settings if self.confirm => vec![
                s(Key::Enter, "continue", "confirm"),
                s(Key::Escape, "cancel", "back"),
            ],
            Page::Settings => {
                let mut keys = Vec::new();
                if !self.info.language_fixed {
                    keys.push(s(Key::F(6), "change_language", "language"));
                }
                if self.info.lan.as_ref().is_some_and(|lan| !lan.fixed) {
                    keys.push(s(Key::F(7), "terminals", "lan"));
                }
                keys.push(s(Key::F(8), "set_up_again", "setup"));
                keys.push(s(Key::Escape, "back", "back"));
                keys
            }
        }
    }

    /// Handles one message; see the module docs.
    pub fn update(&mut self, msg: Msg) -> Step {
        match msg {
            Msg::Key(key) => match self.shortcuts().into_iter().find(|s| s.key == key) {
                Some(s) => Step {
                    effects: self.on_action(s.action),
                    handled: true,
                },
                None => Step::default(),
            },
            Msg::Action(id) => Step {
                effects: self.on_action(&id),
                handled: true,
            },
            Msg::Checked(result) => Step {
                effects: self.checked(result),
                handled: false,
            },
            Msg::LanguageSaved(result) => {
                if let Err(detail) = result {
                    self.notice = Some(Notice::new("file", detail, Tone::Bad));
                }
                Step::default()
            }
            Msg::LanSet(result) => Step {
                effects: self.lan_set(result),
                handled: false,
            },
        }
    }

    fn on_action(&mut self, id: &str) -> Vec<Effect> {
        if self.outcome.is_some() || self.lan_busy {
            return Vec::new();
        }
        match (self.page, id) {
            (Page::Home, "register") => self.open(Choice::Register),
            (Page::Home, "backoffice") => self.open(Choice::BackOffice),
            (Page::Home, "settings") => {
                // A register or back office still waiting for the check is not wanted any more.
                self.pending = None;
                self.page = Page::Settings;
                self.notice = None;
                Vec::new()
            }
            (Page::Home, "refresh") => self.recheck(None),
            (Page::Settings, "language") if !self.confirm => {
                let next = if self.info.language == "id" {
                    "en"
                } else {
                    "id"
                };
                self.set_language(next)
            }
            (Page::Settings, "lang-id") if !self.confirm => self.set_language("id"),
            (Page::Settings, "lang-en") if !self.confirm => self.set_language("en"),
            (Page::Settings, "lan") if !self.confirm => self.switch_lan(),
            (Page::Settings, "setup") => {
                self.confirm = true;
                Vec::new()
            }
            (Page::Settings, "confirm") if self.confirm => self.finish(Choice::SetupAgain),
            (Page::Settings, "back") => {
                if self.confirm {
                    self.confirm = false;
                } else {
                    self.page = Page::Home;
                    self.notice = None;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn set_language(&mut self, language: &str) -> Vec<Effect> {
        if self.info.language_fixed || self.info.language == language {
            return Vec::new();
        }
        language.clone_into(&mut self.info.language);
        self.notice = None;
        vec![Effect::SetLanguage(language.to_owned())]
    }

    fn switch_lan(&mut self) -> Vec<Effect> {
        let Some(lan) = self.info.lan.as_ref().filter(|lan| !lan.fixed) else {
            return Vec::new();
        };
        let on = !lan.on;
        self.lan_busy = true;
        self.notice = None;
        vec![Effect::SetLan(on)]
    }

    /// The server was restarted (or not): show the new setting and check it answers.
    fn lan_set(&mut self, result: Result<LanChanged, StartError>) -> Vec<Effect> {
        self.lan_busy = false;
        match result {
            Ok(changed) => {
                self.info.lan = Some(changed.lan);
                self.info.server = changed.server;
                self.notice = None;
                self.recheck(None)
            }
            Err(e) => {
                let code = if e.code == "in_use" {
                    "lan_in_use"
                } else {
                    "lan_failed"
                };
                self.notice = Some(Notice::new(code, e.detail, Tone::Bad));
                Vec::new()
            }
        }
    }

    fn finish(&mut self, choice: Choice) -> Vec<Effect> {
        self.outcome = Some(choice);
        vec![Effect::Close]
    }

    fn recheck(&mut self, pending: Option<Choice>) -> Vec<Effect> {
        self.pending = pending;
        self.notice = None;
        if self.conn == Conn::Checking {
            // The running check will answer.
            return Vec::new();
        }
        self.conn = Conn::Checking;
        vec![Effect::Check]
    }

    /// Opens `choice` when the server answers; otherwise checks (again) first.
    fn open(&mut self, choice: Choice) -> Vec<Effect> {
        match &self.conn {
            Conn::Ok => self.finish(choice),
            Conn::Unpaired if choice == Choice::BackOffice => self.finish(choice),
            Conn::Unpaired => {
                self.notice = Some(Notice::bad("unpaired"));
                Vec::new()
            }
            Conn::Checking | Conn::Failed(_) => self.recheck(Some(choice)),
        }
    }

    fn checked(&mut self, result: Result<Option<String>, StartError>) -> Vec<Effect> {
        self.conn = match result {
            Ok(branch) => {
                if let Some(branch) = branch.filter(|b| !b.is_empty()) {
                    self.info.branch = branch;
                }
                Conn::Ok
            }
            Err(e) if e.code == "unpaired" => Conn::Unpaired,
            Err(e) => Conn::Failed(e.detail),
        };
        let Some(choice) = self.pending.take() else {
            return Vec::new();
        };
        match &self.conn {
            Conn::Failed(detail) => {
                self.notice = Some(Notice::new("network", detail.clone(), Tone::Bad));
                Vec::new()
            }
            _ => self.open(choice),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(fixed: bool) -> StartInfo {
        StartInfo {
            kind: AppKind::Terminal,
            server: "http://10.0.0.1:8080".into(),
            register_code: "R2".into(),
            register_name: "Kasir 2".into(),
            branch: String::new(),
            version: "0.1.0".into(),
            language: "id".into(),
            language_fixed: fixed,
            lan: None,
        }
    }

    fn action(c: &mut Controller, id: &str) -> Vec<Effect> {
        c.update(Msg::Action(id.into())).effects
    }

    #[test]
    fn choices_wait_for_the_check_and_failures_are_shown() {
        let (mut c, effects) = Controller::new(info(false));
        assert_eq!(effects, [Effect::Check]);
        assert_eq!(c.conn.id(), "checking");
        // F1 while checking: waits, no second check.
        assert_eq!(c.update(Msg::Key(Key::F(1))).effects, []);
        assert_eq!(c.pending, Some(Choice::Register));
        let failed = Err(StartError::new("network", "refused"));
        assert_eq!(c.update(Msg::Checked(failed.clone())).effects, []);
        assert_eq!(c.conn, Conn::Failed("refused".into()));
        assert_eq!(c.notice.as_ref().unwrap().code, "network");
        assert_eq!(c.outcome, None);
        // F2 after a failure checks again; this time it answers.
        assert_eq!(action(&mut c, "backoffice"), [Effect::Check]);
        assert_eq!(c.notice, None);
        assert_eq!(c.update(Msg::Checked(Ok(None))).effects, [Effect::Close]);
        assert_eq!(c.outcome, Some(Choice::BackOffice));
        // Nothing else happens once chosen.
        assert_eq!(action(&mut c, "register"), []);
    }

    #[test]
    fn an_unpaired_register_opens_only_the_back_office() {
        let (mut c, _) = Controller::new(info(false));
        assert_eq!(c.update(Msg::Key(Key::F(5))).effects, []);
        let unpaired = Err(StartError::new("unpaired", ""));
        c.update(Msg::Checked(unpaired));
        assert_eq!(c.conn.id(), "unpaired");
        assert_eq!(action(&mut c, "register"), []);
        assert_eq!(c.notice.as_ref().unwrap().code, "unpaired");
        assert_eq!(action(&mut c, "refresh"), [Effect::Check]);
        c.update(Msg::Checked(Err(StartError::new("unpaired", ""))));
        assert_eq!(action(&mut c, "backoffice"), [Effect::Close]);
        // A pending register choice after an unpaired answer.
        let (mut c, _) = Controller::new(info(false));
        action(&mut c, "register");
        c.update(Msg::Checked(Err(StartError::new("unpaired", ""))));
        assert_eq!(c.outcome, None);
        assert_eq!(c.notice.as_ref().unwrap().code, "unpaired");
    }

    #[test]
    fn settings_switch_the_language_and_set_up_again() {
        let (mut c, _) = Controller::new(info(false));
        c.update(Msg::Checked(Ok(None)));
        assert!(c.update(Msg::Key(Key::F(3))).handled);
        assert_eq!(c.page, Page::Settings);
        assert!(!c.update(Msg::Key(Key::F(1))).handled, "F1 is a home key");
        assert_eq!(
            c.update(Msg::Key(Key::F(6))).effects,
            [Effect::SetLanguage("en".into())]
        );
        assert_eq!(action(&mut c, "lang-en"), [], "already English");
        assert_eq!(
            action(&mut c, "lang-id"),
            [Effect::SetLanguage("id".into())]
        );
        c.update(Msg::LanguageSaved(Err("read-only".into())));
        assert_eq!(c.notice.as_ref().unwrap().code, "file");
        c.update(Msg::LanguageSaved(Ok(())));
        // F8 asks first; Esc cancels, Enter goes.
        c.update(Msg::Key(Key::F(8)));
        assert!(c.confirm);
        assert_eq!(c.shortcuts().len(), 2);
        assert_eq!(action(&mut c, "language"), [], "not while asking");
        c.update(Msg::Key(Key::Escape));
        assert!(!c.confirm);
        assert_eq!(c.page, Page::Settings);
        c.update(Msg::Key(Key::F(8)));
        assert_eq!(c.update(Msg::Key(Key::Enter)).effects, [Effect::Close]);
        assert_eq!(c.outcome, Some(Choice::SetupAgain));
        // Esc on settings goes home.
        let (mut c, _) = Controller::new(info(false));
        action(&mut c, "settings");
        c.update(Msg::Key(Key::Escape));
        assert_eq!(c.page, Page::Home);
        assert!(!c.update(Msg::Key(Key::Escape)).handled);
    }

    fn single(on: bool, fixed: bool) -> StartInfo {
        StartInfo {
            kind: AppKind::Single,
            lan: Some(Lan {
                on,
                port: 8080,
                addresses: Vec::new(),
                fixed,
                failed_port: None,
            }),
            ..info(false)
        }
    }

    fn changed(on: bool) -> LanChanged {
        LanChanged {
            lan: Lan {
                on,
                port: 8080,
                addresses: vec!["192.168.1.10:8080".into()],
                fixed: false,
                failed_port: None,
            },
            server: "http://127.0.0.1:8080".into(),
        }
    }

    #[test]
    fn the_check_refreshes_the_branch() {
        let (mut c, _) = Controller::new(info(false));
        c.update(Msg::Checked(Ok(Some("JKT01 · Toko".into()))));
        assert_eq!(c.info.branch, "JKT01 · Toko");
        c.update(Msg::Checked(Ok(Some(String::new()))));
        assert_eq!(
            c.info.branch, "JKT01 · Toko",
            "an empty label changes nothing"
        );
    }

    #[test]
    fn terminals_are_switched_on_and_off_on_a_single_app() {
        let (mut c, _) = Controller::new(single(false, false));
        // Settings cancel a choice that waits for the check.
        action(&mut c, "register");
        action(&mut c, "settings");
        assert_eq!(c.update(Msg::Checked(Ok(None))).effects, []);
        assert_eq!(c.outcome, None);
        action(&mut c, "back");
        action(&mut c, "lan");
        assert!(!c.lan_busy, "not on the home page");
        action(&mut c, "settings");
        assert!(c.shortcuts().iter().any(|s| s.key == Key::F(7)));
        assert_eq!(
            c.update(Msg::Key(Key::F(7))).effects,
            [Effect::SetLan(true)]
        );
        assert!(c.lan_busy);
        // Everything waits for the restart.
        assert_eq!(action(&mut c, "back"), []);
        assert_eq!(c.page, Page::Settings);
        assert_eq!(
            c.update(Msg::LanSet(Ok(changed(true)))).effects,
            [Effect::Check]
        );
        assert!(!c.lan_busy);
        assert_eq!(c.conn, Conn::Checking);
        assert!(c.info.lan.as_ref().unwrap().on);
        assert_eq!(c.info.server, "http://127.0.0.1:8080");
        c.update(Msg::Checked(Ok(None)));
        // Off again, but the port is taken: the old setting stays, the screen says why.
        assert_eq!(action(&mut c, "lan"), [Effect::SetLan(false)]);
        let in_use = Err(StartError::new("in_use", "8080"));
        assert_eq!(c.update(Msg::LanSet(in_use)).effects, []);
        assert!(c.info.lan.as_ref().unwrap().on);
        let notice = c.notice.as_ref().unwrap();
        assert_eq!((notice.code, notice.arg.as_str()), ("lan_in_use", "8080"));
        action(&mut c, "lan");
        c.update(Msg::LanSet(Err(StartError::new("failed", "disk full"))));
        assert_eq!(c.notice.as_ref().unwrap().code, "lan_failed");
        assert_eq!(c.conn, Conn::Ok);
        // Not while "set up again?" is asked.
        action(&mut c, "setup");
        assert_eq!(action(&mut c, "lan"), []);
    }

    #[test]
    fn a_port_that_failed_at_start_is_shown() {
        let mut start = single(false, false);
        start.lan.as_mut().unwrap().failed_port = Some(8080);
        let (c, _) = Controller::new(start);
        let notice = c.notice.as_ref().unwrap();
        assert_eq!((notice.code, notice.arg.as_str()), ("lan_start", "8080"));
    }

    #[test]
    fn sultan_listen_and_terminals_lock_the_switch() {
        let (mut c, _) = Controller::new(single(true, true));
        action(&mut c, "settings");
        assert!(c.shortcuts().iter().all(|s| s.action != "lan"));
        assert_eq!(action(&mut c, "lan"), []);
        let (mut c, _) = Controller::new(info(false));
        action(&mut c, "settings");
        assert!(c.shortcuts().iter().all(|s| s.action != "lan"));
        assert_eq!(action(&mut c, "lan"), []);
    }

    #[test]
    fn a_fixed_language_cannot_change() {
        let (mut c, _) = Controller::new(info(true));
        action(&mut c, "settings");
        assert!(c.shortcuts().iter().all(|s| s.action != "language"));
        assert_eq!(action(&mut c, "lang-en"), []);
        assert_eq!(c.info.language, "id");
        assert_eq!(AppKind::Single.id(), "single");
        assert_eq!(AppKind::Terminal.id(), "terminal");
        assert_eq!(Conn::Ok.id(), "ok");
        assert_eq!(Conn::Failed(String::new()).id(), "failed");
    }
}
