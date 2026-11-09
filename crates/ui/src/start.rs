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
//!
//! The connection is checked when the screen opens ([`Effect::Check`]). The register and the
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

/// A boxed future of [`StartService::check`].
pub type StartFuture = Pin<Box<dyn Future<Output = Result<(), StartError>> + Send + 'static>>;

/// The work behind the Start screen.
pub trait StartService: Send + Sync {
    /// Asks the server whether it answers and knows this register.
    fn check(&self) -> StartFuture;
    /// Saves the language (`id` / `en`) for the next start.
    ///
    /// # Errors
    /// Why it could not be saved.
    fn set_language(&self, language: &str) -> Result<(), String>;
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
    /// Close the window ([`Controller::outcome`] says why).
    Close,
}

/// Input for [`Controller::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// A key.
    Key(Key),
    /// A tile or button: `register`, `backoffice`, `settings`, `refresh`, `language`
    /// (switch), `lang-id`, `lang-en`, `setup`, `confirm`, `back`.
    Action(String),
    /// Result of [`Effect::Check`].
    Checked(Result<(), StartError>),
    /// Result of [`Effect::SetLanguage`].
    LanguageSaved(Result<(), String>),
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
}

impl Controller {
    /// The start state (home, checking the connection) and its first call.
    #[must_use]
    pub fn new(info: StartInfo) -> (Self, Vec<Effect>) {
        let controller = Self {
            info,
            page: Page::Home,
            confirm: false,
            conn: Conn::Checking,
            pending: None,
            notice: None,
            outcome: None,
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
        }
    }

    fn on_action(&mut self, id: &str) -> Vec<Effect> {
        if self.outcome.is_some() {
            return Vec::new();
        }
        match (self.page, id) {
            (Page::Home, "register") => self.open(Choice::Register),
            (Page::Home, "backoffice") => self.open(Choice::BackOffice),
            (Page::Home, "settings") => {
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

    fn checked(&mut self, result: Result<(), StartError>) -> Vec<Effect> {
        self.conn = match result {
            Ok(()) => Conn::Ok,
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
        assert_eq!(c.update(Msg::Checked(Ok(()))).effects, [Effect::Close]);
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
        c.update(Msg::Checked(Ok(())));
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
