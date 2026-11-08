//! The first-run setup wizard as a plain state machine (no Slint here, every decision is
//! unit-tested). The glue to the window is [`crate::setup_app`].
//!
//! A bare `sultan` on an unconfigured machine opens the wizard. It offers four app types:
//! - **Single app** ([`AppType::Single`]): shop → owner → the database is set up; the app then
//!   starts its own server and opens the register.
//! - **HQ server** ([`AppType::Hq`]): database (`SQLite` file or `MySQL` URL, with a connection
//!   test) → listen address → shop → owner. The wizard exits; `sultan serve` starts the server.
//! - **Branch server** ([`AppType::Branch`]): shown but not selectable. A branch joins HQ and copies
//!   its master data, which needs the HQ↔branch sync that does not exist yet.
//! - **Terminal** ([`AppType::Terminal`]): server address (tested) → a manager logs in → branch,
//!   register code and name; the wizard creates the register and pairs this machine. Fallback: a
//!   pairing code issued elsewhere.
//!
//! The work itself (database, config file, HTTP) is behind [`SetupService`], implemented by the
//! `sultan` binary (`crates/app/src/setup.rs`); tests use a fake.

use std::future::Future;
use std::pin::Pin;

use crate::controller::Tone;
use crate::input::Key;

/// The four app types on the first page, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AppType {
    /// Server and register in one app (one shop).
    Single,
    /// Head-office server.
    Hq,
    /// Branch server (needs HQ sync; not available yet).
    Branch,
    /// A register connecting to a server.
    Terminal,
}

impl AppType {
    /// The cards, in order.
    pub const ALL: [Self; 4] = [Self::Single, Self::Hq, Self::Branch, Self::Terminal];

    /// The ID the window uses.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Hq => "hq",
            Self::Branch => "branch",
            Self::Terminal => "terminal",
        }
    }

    /// Whether the card can be picked.
    #[must_use]
    pub const fn available(self) -> bool {
        !matches!(self, Self::Branch)
    }
}

/// A wizard page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Pick the app type.
    Welcome,
    /// HQ: where the database lives.
    Database,
    /// HQ: the listen address.
    Network,
    /// Company, first branch, time zone, stock location.
    Shop,
    /// The Owner account.
    Owner,
    /// Terminal: the server address.
    Server,
    /// Terminal: a manager logs in.
    Manager,
    /// Terminal: branch, register code and name.
    Terminal,
    /// Terminal: pair with a code instead.
    Code,
    /// Finished.
    Done,
}

impl Page {
    /// The name the window uses.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Welcome => "welcome",
            Self::Database => "database",
            Self::Network => "network",
            Self::Shop => "shop",
            Self::Owner => "owner",
            Self::Server => "server",
            Self::Manager => "manager",
            Self::Terminal => "terminal",
            Self::Code => "code",
            Self::Done => "done",
        }
    }
}

/// Where an HQ server keeps its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DbKind {
    /// A `SQLite` file.
    #[default]
    Sqlite,
    /// A `MySQL` server (the database must exist).
    Mysql,
}

/// The time zones offered for the first branch (WIB, WITA, WIT).
pub const TIME_ZONES: [&str; 3] = ["Asia/Jakarta", "Asia/Makassar", "Asia/Jayapura"];

/// Default listen address of an HQ server: the shop LAN.
pub const DEFAULT_LISTEN: &str = "0.0.0.0:8080";

/// Default stock location name.
pub const DEFAULT_LOCATION: &str = "Store floor";

/// Default port added to a server address without one.
pub const DEFAULT_PORT: u16 = 8080;

/// A text field of the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// `SQLite` file path.
    SqlitePath,
    /// `MySQL` URL.
    MysqlUrl,
    /// Listen address.
    Listen,
    /// Company name.
    Company,
    /// Branch code.
    BranchCode,
    /// Branch name.
    BranchName,
    /// Stock location name.
    Location,
    /// Owner username.
    OwnerUsername,
    /// Owner display name.
    OwnerName,
    /// Owner password.
    Password,
    /// Owner password again.
    Password2,
    /// Owner PIN.
    Pin,
    /// Owner PIN again.
    Pin2,
    /// Server address.
    Server,
    /// Manager username.
    Manager,
    /// Manager password.
    ManagerPassword,
    /// New register code.
    RegisterCode,
    /// New register name.
    RegisterName,
    /// Pairing code.
    PairingCode,
}

impl Field {
    const ALL: [Self; 19] = [
        Self::SqlitePath,
        Self::MysqlUrl,
        Self::Listen,
        Self::Company,
        Self::BranchCode,
        Self::BranchName,
        Self::Location,
        Self::OwnerUsername,
        Self::OwnerName,
        Self::Password,
        Self::Password2,
        Self::Pin,
        Self::Pin2,
        Self::Server,
        Self::Manager,
        Self::ManagerPassword,
        Self::RegisterCode,
        Self::RegisterName,
        Self::PairingCode,
    ];

    /// The ID the window uses.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::SqlitePath => "sqlite_path",
            Self::MysqlUrl => "mysql_url",
            Self::Listen => "listen",
            Self::Company => "company",
            Self::BranchCode => "branch_code",
            Self::BranchName => "branch_name",
            Self::Location => "location",
            Self::OwnerUsername => "owner_username",
            Self::OwnerName => "owner_name",
            Self::Password => "password",
            Self::Password2 => "password2",
            Self::Pin => "pin",
            Self::Pin2 => "pin2",
            Self::Server => "server",
            Self::Manager => "manager",
            Self::ManagerPassword => "manager_password",
            Self::RegisterCode => "register_code",
            Self::RegisterName => "register_name",
            Self::PairingCode => "pairing_code",
        }
    }

    /// The field with this ID.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.id() == id)
    }
}

/// Everything typed into the wizard.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Form {
    /// `SQLite` file path (HQ).
    pub sqlite_path: String,
    /// `MySQL` URL (HQ).
    pub mysql_url: String,
    /// Listen address (HQ).
    pub listen: String,
    /// Company name.
    pub company: String,
    /// First branch code.
    pub branch_code: String,
    /// First branch name.
    pub branch_name: String,
    /// Stock location name.
    pub location: String,
    /// Owner username.
    pub owner_username: String,
    /// Owner display name.
    pub owner_name: String,
    /// Owner password.
    pub password: String,
    /// Owner password again.
    pub password2: String,
    /// Owner PIN.
    pub pin: String,
    /// Owner PIN again.
    pub pin2: String,
    /// Server address (terminal).
    pub server: String,
    /// Manager username (terminal).
    pub manager: String,
    /// Manager password (terminal).
    pub manager_password: String,
    /// New register code (terminal).
    pub register_code: String,
    /// New register name (terminal).
    pub register_name: String,
    /// Pairing code (terminal).
    pub pairing_code: String,
}

impl Form {
    /// The form with the defaults filled in.
    #[must_use]
    pub fn with_defaults(sqlite_path: &str) -> Self {
        Self {
            sqlite_path: sqlite_path.to_owned(),
            listen: DEFAULT_LISTEN.to_owned(),
            location: DEFAULT_LOCATION.to_owned(),
            owner_username: "owner".to_owned(),
            owner_name: "Owner".to_owned(),
            ..Self::default()
        }
    }

    /// The value of `field`.
    #[must_use]
    pub fn get(&self, field: Field) -> &str {
        match field {
            Field::SqlitePath => &self.sqlite_path,
            Field::MysqlUrl => &self.mysql_url,
            Field::Listen => &self.listen,
            Field::Company => &self.company,
            Field::BranchCode => &self.branch_code,
            Field::BranchName => &self.branch_name,
            Field::Location => &self.location,
            Field::OwnerUsername => &self.owner_username,
            Field::OwnerName => &self.owner_name,
            Field::Password => &self.password,
            Field::Password2 => &self.password2,
            Field::Pin => &self.pin,
            Field::Pin2 => &self.pin2,
            Field::Server => &self.server,
            Field::Manager => &self.manager,
            Field::ManagerPassword => &self.manager_password,
            Field::RegisterCode => &self.register_code,
            Field::RegisterName => &self.register_name,
            Field::PairingCode => &self.pairing_code,
        }
    }

    fn slot(&mut self, field: Field) -> &mut String {
        match field {
            Field::SqlitePath => &mut self.sqlite_path,
            Field::MysqlUrl => &mut self.mysql_url,
            Field::Listen => &mut self.listen,
            Field::Company => &mut self.company,
            Field::BranchCode => &mut self.branch_code,
            Field::BranchName => &mut self.branch_name,
            Field::Location => &mut self.location,
            Field::OwnerUsername => &mut self.owner_username,
            Field::OwnerName => &mut self.owner_name,
            Field::Password => &mut self.password,
            Field::Password2 => &mut self.password2,
            Field::Pin => &mut self.pin,
            Field::Pin2 => &mut self.pin2,
            Field::Server => &mut self.server,
            Field::Manager => &mut self.manager,
            Field::ManagerPassword => &mut self.manager_password,
            Field::RegisterCode => &mut self.register_code,
            Field::RegisterName => &mut self.register_name,
            Field::PairingCode => &mut self.pairing_code,
        }
    }

    /// Sets `field`.
    pub fn set(&mut self, field: Field, value: &str) {
        value.clone_into(self.slot(field));
    }
}

/// A failed setup call: a message code the window translates and one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupError {
    /// Message code, e.g. `network`, `invalid_credentials`, `invalid`.
    pub code: String,
    /// Its argument (a detail, a company name, …).
    pub arg: String,
}

impl SetupError {
    /// A new error.
    #[must_use]
    pub fn new(code: impl Into<String>, arg: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            arg: arg.into(),
        }
    }
}

/// A boxed, sendable future of a setup call.
pub type SetupFuture<T> = Pin<Box<dyn Future<Output = Result<T, SetupError>> + Send + 'static>>;

/// Whether a database is free to set up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DbCheck {
    /// Reachable and not set up yet.
    Empty,
    /// Already set up, for this company.
    Initialised(String),
}

/// What `init_shop` creates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShopRequest {
    /// Single or HQ.
    pub app_type: AppType,
    /// Database URL; `None` = the default `SQLite` file next to the config.
    pub db_url: Option<String>,
    /// Listen address (HQ only).
    pub listen: Option<String>,
    /// Company name.
    pub company: String,
    /// First branch code.
    pub branch_code: String,
    /// First branch name.
    pub branch_name: String,
    /// First branch time zone.
    pub timezone: String,
    /// Stock location name.
    pub location_name: String,
    /// Owner username.
    pub owner_username: String,
    /// Owner display name.
    pub owner_display_name: String,
    /// Owner password.
    pub owner_password: String,
    /// Owner PIN.
    pub owner_pin: String,
}

/// What `init_shop` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShopReport {
    /// Company name.
    pub company: String,
    /// First branch code.
    pub branch_code: String,
    /// The database URL written to `sultan.toml`.
    pub db_url: String,
}

/// A branch the manager can pick for the new register.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchChoice {
    /// Branch ID.
    pub id: String,
    /// Branch code.
    pub code: String,
    /// Branch name.
    pub name: String,
}

/// A manager's back-office session (terminal setup only).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerSession {
    /// Bearer token.
    pub token: String,
    /// The branch the session acts at.
    pub branch_id: String,
    /// The branches the manager can see.
    pub branches: Vec<BranchChoice>,
}

/// The register to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalRequest {
    /// Branch ID.
    pub branch_id: String,
    /// Register code, e.g. `R2`.
    pub code: String,
    /// Register name, e.g. `Kasir 2`.
    pub name: String,
}

/// This machine is paired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paired {
    /// Register code.
    pub register_code: String,
    /// Register name.
    pub register_name: String,
    /// Server URL.
    pub server: String,
}

/// The work behind the wizard.
pub trait SetupService: Send + Sync {
    /// Connects to `url` and tells whether it is already set up.
    fn check_db(&self, url: &str) -> SetupFuture<DbCheck>;
    /// Sets up the database and writes `sultan.toml` (single: also this machine's register).
    fn init_shop(&self, request: &ShopRequest) -> SetupFuture<ShopReport>;
    /// Checks that a Sultan server answers at `server`.
    fn check_server(&self, server: &str) -> SetupFuture<()>;
    /// Back-office login of a manager.
    fn manager_login(
        &self,
        server: &str,
        username: &str,
        password: &str,
    ) -> SetupFuture<ManagerSession>;
    /// Creates a register and pairs this machine with it; ends the manager's session.
    fn register_terminal(
        &self,
        server: &str,
        token: &str,
        request: &TerminalRequest,
    ) -> SetupFuture<Paired>;
    /// Pairs this machine with a one-time code.
    fn pair_with_code(&self, server: &str, code: &str) -> SetupFuture<Paired>;
}

/// A button of the wizard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Next / finish the page.
    Next,
    /// Previous page.
    Back,
    /// Test the database connection.
    Test,
    /// Terminal: use a pairing code instead of a manager login.
    UseCode,
    /// Terminal: back to the manager login.
    UseLogin,
}

impl Action {
    /// The action with this ID.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "next" => Self::Next,
            "back" => Self::Back,
            "test" => Self::Test,
            "use_code" => Self::UseCode,
            "use_login" => Self::UseLogin,
            _ => return None,
        })
    }
}

/// A list the window picks from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// An app-type card.
    AppType,
    /// `SQLite` (0) or `MySQL` (1).
    DbKind,
    /// A time zone of [`TIME_ZONES`].
    TimeZone,
    /// A branch of the manager session.
    Branch,
}

impl Pick {
    /// The list with this ID.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Some(match id {
            "type" => Self::AppType,
            "db" => Self::DbKind,
            "timezone" => Self::TimeZone,
            "branch" => Self::Branch,
            _ => return None,
        })
    }
}

/// An input to [`SetupController::update`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// A key that reached the window (not taken by a text field).
    Key(Key),
    /// A button.
    Action(Action),
    /// A text field changed.
    Field(Field, String),
    /// An item of a list was picked.
    Pick(Pick, usize),
    /// Answer of [`Effect::CheckDb`].
    DbChecked(Result<DbCheck, SetupError>),
    /// Answer of [`Effect::InitShop`].
    ShopDone(Result<ShopReport, SetupError>),
    /// Answer of [`Effect::CheckServer`].
    ServerChecked(Result<(), SetupError>),
    /// Answer of [`Effect::Login`].
    LoggedIn(Result<ManagerSession, SetupError>),
    /// Answer of [`Effect::Register`] and [`Effect::Pair`].
    Paired(Result<Paired, SetupError>),
}

/// A call to make (or the window to close).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// [`SetupService::check_db`].
    CheckDb {
        /// Database URL.
        url: String,
    },
    /// [`SetupService::init_shop`].
    InitShop(Box<ShopRequest>),
    /// [`SetupService::check_server`].
    CheckServer {
        /// Server URL.
        server: String,
    },
    /// [`SetupService::manager_login`].
    Login {
        /// Server URL.
        server: String,
        /// Username.
        username: String,
        /// Password.
        password: String,
    },
    /// [`SetupService::register_terminal`].
    Register {
        /// Server URL.
        server: String,
        /// Manager token.
        token: String,
        /// The register.
        request: TerminalRequest,
    },
    /// [`SetupService::pair_with_code`].
    Pair {
        /// Server URL.
        server: String,
        /// Pairing code.
        code: String,
    },
    /// The wizard is finished: close the window.
    Close,
}

/// A message on the page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Message code (translated by the window).
    pub code: String,
    /// Its argument.
    pub arg: String,
    /// Colour.
    pub tone: Tone,
}

impl Note {
    fn bad(code: &str, arg: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            arg: arg.into(),
            tone: Tone::Bad,
        }
    }

    fn good(code: &str, arg: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            arg: arg.into(),
            tone: Tone::Good,
        }
    }

    fn from_error(e: SetupError) -> Self {
        Self::bad(&e.code, e.arg)
    }
}

/// What the wizard finished with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Summary {
    /// The database is set up.
    Shop(ShopReport),
    /// This machine is paired.
    Paired(Paired),
}

/// What [`SetupController::update`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Calls to make.
    pub effects: Vec<Effect>,
    /// For keys: whether the key was used.
    pub handled: bool,
}

impl Step {
    fn effect(effect: Effect) -> Self {
        Self {
            effects: vec![effect],
            handled: true,
        }
    }

    const fn done() -> Self {
        Self {
            effects: Vec::new(),
            handled: true,
        }
    }
}

/// The wizard state; see the module docs.
#[derive(Debug)]
pub struct SetupController {
    /// The current page.
    pub page: Page,
    /// The highlighted card on the first page.
    pub cursor: usize,
    /// The chosen app type.
    pub app_type: Option<AppType>,
    /// The typed values.
    pub form: Form,
    /// HQ database kind.
    pub db_kind: DbKind,
    /// Index into [`TIME_ZONES`].
    pub timezone: usize,
    /// Terminal: the manager's session.
    pub session: Option<ManagerSession>,
    /// Terminal: index into the session's branches.
    pub branch: usize,
    /// A call is running.
    pub busy: bool,
    /// The message on the page.
    pub note: Option<Note>,
    /// The result, on the last page.
    pub summary: Option<Summary>,
    /// Set when the person finished the wizard (the app type to continue with).
    pub outcome: Option<AppType>,
    history: Vec<Page>,
    /// The database check was started by Next (go on when it is free).
    check_then_next: bool,
}

impl SetupController {
    /// A wizard on its first page. `sqlite_path` is the default database file.
    #[must_use]
    pub fn new(sqlite_path: &str) -> Self {
        Self {
            page: Page::Welcome,
            cursor: 0,
            app_type: None,
            form: Form::with_defaults(sqlite_path),
            db_kind: DbKind::Sqlite,
            timezone: 0,
            session: None,
            branch: 0,
            busy: false,
            note: None,
            summary: None,
            outcome: None,
            history: Vec::new(),
            check_then_next: false,
        }
    }

    /// The database URL the HQ pages describe.
    #[must_use]
    pub fn db_url(&self) -> String {
        match self.db_kind {
            DbKind::Sqlite => {
                let path = self.form.sqlite_path.trim();
                if path.starts_with("sqlite:") {
                    path.to_owned()
                } else {
                    format!("sqlite://{path}")
                }
            }
            DbKind::Mysql => self.form.mysql_url.trim().to_owned(),
        }
    }

    /// The server URL typed on the terminal pages: `http://` and the default port are added
    /// when missing.
    #[must_use]
    pub fn server_url(&self) -> String {
        server_url(&self.form.server)
    }

    /// Handles one input.
    pub fn update(&mut self, msg: Msg) -> Step {
        match msg {
            Msg::Key(key) => self.key(key),
            Msg::Action(action) => self.action(action),
            Msg::Field(field, value) => {
                self.form.set(field, &value);
                Step::done()
            }
            Msg::Pick(pick, i) => self.pick(pick, i),
            Msg::DbChecked(result) => self.db_checked(result),
            Msg::ShopDone(result) => {
                self.busy = false;
                match result {
                    Ok(report) => self.finish_with(Summary::Shop(report)),
                    Err(e) => self.note = Some(Note::from_error(e)),
                }
                Step::done()
            }
            Msg::ServerChecked(result) => {
                self.busy = false;
                match result {
                    Ok(()) => self.go(Page::Manager),
                    Err(e) => self.note = Some(Note::from_error(e)),
                }
                Step::done()
            }
            Msg::LoggedIn(result) => {
                self.busy = false;
                match result {
                    Ok(session) => {
                        self.branch = session
                            .branches
                            .iter()
                            .position(|b| b.id == session.branch_id)
                            .unwrap_or(0);
                        self.session = Some(session);
                        self.form.manager_password.clear();
                        self.go(Page::Terminal);
                    }
                    Err(e) => self.note = Some(Note::from_error(e)),
                }
                Step::done()
            }
            Msg::Paired(result) => {
                self.busy = false;
                match result {
                    Ok(paired) => {
                        self.session = None;
                        self.finish_with(Summary::Paired(paired));
                    }
                    Err(e) => self.note = Some(Note::from_error(e)),
                }
                Step::done()
            }
        }
    }

    fn key(&mut self, key: Key) -> Step {
        if self.page == Page::Welcome {
            let last = AppType::ALL.len() - 1;
            match key {
                Key::Up | Key::Left | Key::BackTab => self.cursor = self.cursor.saturating_sub(1),
                Key::Down | Key::Right | Key::Tab => self.cursor = (self.cursor + 1).min(last),
                Key::Digit(d @ 1..=4) => return self.pick(Pick::AppType, usize::from(d - 1)),
                Key::Enter => return self.action(Action::Next),
                _ => return Step::default(),
            }
            return Step::done();
        }
        match key {
            Key::Enter => self.action(Action::Next),
            Key::Escape => self.action(Action::Back),
            _ => Step::default(),
        }
    }

    fn pick(&mut self, pick: Pick, i: usize) -> Step {
        if self.busy {
            return Step::done();
        }
        match pick {
            Pick::AppType if self.page == Page::Welcome && i < AppType::ALL.len() => {
                self.cursor = i;
                return self.choose();
            }
            Pick::DbKind => {
                self.db_kind = if i == 1 {
                    DbKind::Mysql
                } else {
                    DbKind::Sqlite
                };
                self.note = None;
            }
            Pick::TimeZone if i < TIME_ZONES.len() => self.timezone = i,
            Pick::Branch if self.session.as_ref().is_some_and(|s| i < s.branches.len()) => {
                self.branch = i;
            }
            _ => {}
        }
        Step::done()
    }

    fn choose(&mut self) -> Step {
        let app_type = AppType::ALL[self.cursor];
        if !app_type.available() {
            self.note = Some(Note {
                code: "branch_needs_sync".into(),
                arg: String::new(),
                tone: Tone::Neutral,
            });
            return Step::done();
        }
        self.app_type = Some(app_type);
        self.go(match app_type {
            AppType::Single => Page::Shop,
            AppType::Hq => Page::Database,
            AppType::Branch | AppType::Terminal => Page::Server,
        });
        Step::done()
    }

    fn go(&mut self, page: Page) {
        self.history.push(self.page);
        self.page = page;
        self.note = None;
    }

    fn finish_with(&mut self, summary: Summary) {
        self.summary = Some(summary);
        // No way back: the work is done.
        self.history.clear();
        self.page = Page::Done;
        self.note = None;
    }

    fn action(&mut self, action: Action) -> Step {
        if self.busy {
            return Step::done();
        }
        match action {
            Action::Back => {
                if let Some(page) = self.history.pop() {
                    if self.page == Page::Terminal {
                        // The manager logs in again: the session is not kept around.
                        self.session = None;
                    }
                    self.page = page;
                    self.note = None;
                    return Step::done();
                }
                Step::default()
            }
            Action::Next => self.next(),
            Action::Test if self.page == Page::Database => self.check_db(false),
            Action::UseCode if self.page == Page::Manager => {
                self.go(Page::Code);
                Step::done()
            }
            Action::UseLogin if self.page == Page::Code => self.action(Action::Back),
            _ => Step::done(),
        }
    }

    fn next(&mut self) -> Step {
        let result = match self.page {
            Page::Welcome => return self.choose(),
            Page::Database => return self.check_db(true),
            Page::Network => self.network_next(),
            Page::Shop => self.shop_next(),
            Page::Owner => self.owner_next(),
            Page::Server => self.server_next(),
            Page::Manager => self.manager_next(),
            Page::Terminal => self.terminal_next(),
            Page::Code => self.code_next(),
            Page::Done => {
                self.outcome = self.app_type;
                return Step::effect(Effect::Close);
            }
        };
        match result {
            Ok(Some(effect)) => {
                self.busy = true;
                self.note = None;
                Step::effect(effect)
            }
            Ok(None) => Step::done(),
            Err(note) => {
                self.note = Some(note);
                Step::done()
            }
        }
    }

    fn check_db(&mut self, then_next: bool) -> Step {
        let url = self.db_url();
        let valid = match self.db_kind {
            DbKind::Sqlite => !self.form.sqlite_path.trim().is_empty(),
            DbKind::Mysql => url.starts_with("mysql://") && url.len() > "mysql://".len(),
        };
        if !valid {
            self.note = Some(match self.db_kind {
                DbKind::Sqlite => required(Field::SqlitePath),
                DbKind::Mysql => Note::bad("mysql_url", ""),
            });
            return Step::done();
        }
        self.check_then_next = then_next;
        self.busy = true;
        self.note = None;
        Step::effect(Effect::CheckDb { url })
    }

    fn db_checked(&mut self, result: Result<DbCheck, SetupError>) -> Step {
        self.busy = false;
        match result {
            Ok(DbCheck::Empty) if self.check_then_next => self.go(Page::Network),
            Ok(DbCheck::Empty) => self.note = Some(Note::good("db_ok", "")),
            Ok(DbCheck::Initialised(company)) => {
                self.note = Some(Note::bad("db_initialised", company));
            }
            Err(e) => self.note = Some(Note::from_error(e)),
        }
        Step::done()
    }

    fn network_next(&mut self) -> Result<Option<Effect>, Note> {
        let listen = self.form.listen.trim();
        if listen.is_empty() {
            return Err(required(Field::Listen));
        }
        if !listen.contains(':') {
            return Err(Note::bad("listen_format", ""));
        }
        self.go(Page::Shop);
        Ok(None)
    }

    fn shop_next(&mut self) -> Result<Option<Effect>, Note> {
        filled(
            &self.form,
            &[Field::Company, Field::BranchCode, Field::Location],
        )?;
        if !is_code(&self.form.branch_code) {
            return Err(Note::bad("code_format", ""));
        }
        self.go(Page::Owner);
        Ok(None)
    }

    fn owner_next(&mut self) -> Result<Option<Effect>, Note> {
        let f = &self.form;
        filled(
            f,
            &[Field::OwnerUsername, Field::OwnerName, Field::Password],
        )?;
        if f.password.chars().count() < 8 {
            return Err(Note::bad("password_short", ""));
        }
        if f.password != f.password2 {
            return Err(Note::bad("password_mismatch", ""));
        }
        if !(4..=6).contains(&f.pin.len()) || !f.pin.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Note::bad("pin_format", ""));
        }
        if f.pin != f.pin2 {
            return Err(Note::bad("pin_mismatch", ""));
        }
        let hq = self.app_type == Some(AppType::Hq);
        let branch_code = f.branch_code.trim().to_owned();
        let branch_name = match f.branch_name.trim() {
            "" => branch_code.clone(),
            name => name.to_owned(),
        };
        Ok(Some(Effect::InitShop(Box::new(ShopRequest {
            app_type: self.app_type.unwrap_or(AppType::Single),
            db_url: hq.then(|| self.db_url()),
            listen: hq.then(|| f.listen.trim().to_owned()),
            company: f.company.trim().to_owned(),
            branch_code,
            branch_name,
            timezone: TIME_ZONES[self.timezone].to_owned(),
            location_name: f.location.trim().to_owned(),
            owner_username: f.owner_username.trim().to_owned(),
            owner_display_name: f.owner_name.trim().to_owned(),
            owner_password: f.password.clone(),
            owner_pin: f.pin.clone(),
        }))))
    }

    fn server_next(&self) -> Result<Option<Effect>, Note> {
        filled(&self.form, &[Field::Server])?;
        Ok(Some(Effect::CheckServer {
            server: self.server_url(),
        }))
    }

    fn manager_next(&self) -> Result<Option<Effect>, Note> {
        filled(&self.form, &[Field::Manager, Field::ManagerPassword])?;
        Ok(Some(Effect::Login {
            server: self.server_url(),
            username: self.form.manager.trim().to_owned(),
            password: self.form.manager_password.clone(),
        }))
    }

    fn terminal_next(&self) -> Result<Option<Effect>, Note> {
        filled(&self.form, &[Field::RegisterCode, Field::RegisterName])?;
        if !is_code(&self.form.register_code) {
            return Err(Note::bad("code_format", ""));
        }
        let Some(session) = &self.session else {
            return Err(Note::bad("session_ended", ""));
        };
        let Some(branch) = session.branches.get(self.branch) else {
            return Err(Note::bad("no_branch", ""));
        };
        Ok(Some(Effect::Register {
            server: self.server_url(),
            token: session.token.clone(),
            request: TerminalRequest {
                branch_id: branch.id.clone(),
                code: self.form.register_code.trim().to_owned(),
                name: self.form.register_name.trim().to_owned(),
            },
        }))
    }

    fn code_next(&self) -> Result<Option<Effect>, Note> {
        filled(&self.form, &[Field::PairingCode])?;
        Ok(Some(Effect::Pair {
            server: self.server_url(),
            code: self.form.pairing_code.trim().to_uppercase(),
        }))
    }
}

fn required(field: Field) -> Note {
    Note::bad("required", field.id())
}

fn filled(form: &Form, fields: &[Field]) -> Result<(), Note> {
    match fields.iter().find(|f| form.get(**f).trim().is_empty()) {
        Some(f) => Err(required(*f)),
        None => Ok(()),
    }
}

/// 1–10 letters or digits (branch and register codes).
fn is_code(code: &str) -> bool {
    let code = code.trim();
    (1..=10).contains(&code.len()) && code.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// `192.168.1.10` → `http://192.168.1.10:8080`; a scheme or port given is kept.
#[must_use]
pub fn server_url(typed: &str) -> String {
    let typed = typed.trim().trim_end_matches('/');
    let (scheme, rest) = typed
        .split_once("://")
        .map_or(("http", typed), |(s, r)| (s, r));
    let host = rest.split('/').next().unwrap_or_default();
    let has_port = host
        .rsplit_once(':')
        .is_some_and(|(_, p)| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    if has_port {
        format!("{scheme}://{rest}")
    } else {
        let path = &rest[host.len()..];
        format!("{scheme}://{host}:{DEFAULT_PORT}{path}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wizard() -> SetupController {
        SetupController::new("/shop/sultan.db")
    }

    fn type_into(c: &mut SetupController, values: &[(Field, &str)]) {
        for (field, value) in values {
            c.update(Msg::Field(*field, (*value).to_owned()));
        }
    }

    fn next(c: &mut SetupController) -> Vec<Effect> {
        c.update(Msg::Action(Action::Next)).effects
    }

    fn note(c: &SetupController) -> (&str, &str) {
        c.note
            .as_ref()
            .map_or(("", ""), |n| (n.code.as_str(), n.arg.as_str()))
    }

    fn fill_shop(c: &mut SetupController) {
        type_into(c, &[(Field::Company, "PT A"), (Field::BranchCode, "JKT01")]);
    }

    fn fill_owner(c: &mut SetupController) {
        type_into(
            c,
            &[
                (Field::Password, "owner-pass-1"),
                (Field::Password2, "owner-pass-1"),
                (Field::Pin, "1234"),
                (Field::Pin2, "1234"),
            ],
        );
    }

    #[test]
    fn ids_round_trip() {
        for field in Field::ALL {
            assert_eq!(Field::from_id(field.id()), Some(field));
        }
        assert_eq!(Field::from_id("nope"), None);
        for id in ["next", "back", "test", "use_code", "use_login"] {
            assert!(Action::from_id(id).is_some(), "{id}");
        }
        assert_eq!(Action::from_id("x"), None);
        for id in ["type", "db", "timezone", "branch"] {
            assert!(Pick::from_id(id).is_some(), "{id}");
        }
        assert_eq!(Pick::from_id("x"), None);
        let names: Vec<_> = AppType::ALL.iter().map(|t| t.id()).collect();
        assert_eq!(names, ["single", "hq", "branch", "terminal"]);
        let pages = [
            Page::Welcome,
            Page::Database,
            Page::Network,
            Page::Shop,
            Page::Owner,
            Page::Server,
            Page::Manager,
            Page::Terminal,
            Page::Code,
            Page::Done,
        ];
        let mut names: Vec<_> = pages.iter().map(|p| p.name()).collect();
        names.dedup();
        assert_eq!(names.len(), pages.len());
    }

    #[test]
    fn welcome_cards_by_keyboard_and_the_branch_card_is_disabled() {
        let mut c = wizard();
        assert!(c.update(Msg::Key(Key::Down)).handled);
        assert!(c.update(Msg::Key(Key::Right)).handled);
        assert_eq!(c.cursor, 2);
        // Enter on the branch card: a note, no page change.
        assert!(next(&mut c).is_empty());
        assert_eq!(c.page, Page::Welcome);
        assert_eq!(note(&c).0, "branch_needs_sync");
        assert_eq!(c.app_type, None);
        // Digit 3 and a click on it: still disabled.
        c.update(Msg::Key(Key::Digit(3)));
        c.update(Msg::Pick(Pick::AppType, 2));
        assert_eq!(c.page, Page::Welcome);
        // Clamped at both ends; other keys are not used.
        for _ in 0..5 {
            c.update(Msg::Key(Key::Tab));
        }
        assert_eq!(c.cursor, 3);
        for _ in 0..5 {
            c.update(Msg::Key(Key::BackTab));
        }
        assert_eq!(c.cursor, 0);
        assert!(!c.update(Msg::Key(Key::F(5))).handled);
        assert!(
            !c.update(Msg::Key(Key::Escape)).handled,
            "nothing to go back to"
        );
        c.update(Msg::Pick(Pick::AppType, 9));
        assert_eq!(c.page, Page::Welcome);
        // Digit 4 picks the terminal.
        c.update(Msg::Key(Key::Digit(4)));
        assert_eq!(
            (c.page, c.app_type),
            (Page::Server, Some(AppType::Terminal))
        );
        assert!(c.update(Msg::Key(Key::Escape)).handled);
        assert_eq!(c.page, Page::Welcome);
        assert_eq!(c.cursor, 3, "the card stays highlighted");
        c.update(Msg::Key(Key::Digit(1)));
        assert_eq!((c.page, c.app_type), (Page::Shop, Some(AppType::Single)));
        assert!(!c.update(Msg::Key(Key::F(1))).handled);
    }

    #[test]
    fn single_app_sets_up_the_default_database() {
        let mut c = wizard();
        c.update(Msg::Pick(Pick::AppType, 0));
        assert_eq!(c.page, Page::Shop);
        assert!(next(&mut c).is_empty());
        assert_eq!(note(&c), ("required", "company"));
        type_into(
            &mut c,
            &[(Field::Company, "PT A"), (Field::BranchCode, "JKT-1")],
        );
        next(&mut c);
        assert_eq!(note(&c).0, "code_format");
        type_into(&mut c, &[(Field::BranchCode, "JKT01")]);
        c.update(Msg::Pick(Pick::TimeZone, 2));
        c.update(Msg::Pick(Pick::TimeZone, 7));
        assert_eq!(c.timezone, 2);
        next(&mut c);
        assert_eq!(c.page, Page::Owner);
        assert_eq!(c.note, None);

        let cases: [(&[(Field, &str)], &str); 5] = [
            (&[(Field::Password, "short")], "password_short"),
            (&[(Field::Password, "owner-pass-1")], "password_mismatch"),
            (
                &[(Field::Password2, "owner-pass-1"), (Field::Pin, "12a4")],
                "pin_format",
            ),
            (&[(Field::Pin, "1234567")], "pin_format"),
            (
                &[(Field::Pin, "1234"), (Field::Pin2, "9999")],
                "pin_mismatch",
            ),
        ];
        for (values, code) in cases {
            type_into(&mut c, values);
            assert!(next(&mut c).is_empty());
            assert_eq!(note(&c).0, code);
        }
        type_into(&mut c, &[(Field::Pin2, "1234")]);
        let effects = next(&mut c);
        assert!(c.busy);
        let Effect::InitShop(request) = &effects[0] else {
            panic!("{effects:?}")
        };
        assert_eq!(request.app_type, AppType::Single);
        assert_eq!(request.db_url, None, "single: the default file");
        assert_eq!(request.listen, None);
        assert_eq!(request.branch_name, "JKT01", "defaults to the code");
        assert_eq!(request.timezone, "Asia/Jayapura");
        assert_eq!(request.location_name, "Store floor");
        assert_eq!(request.owner_username, "owner");
        // Busy: buttons and picks wait.
        assert!(next(&mut c).is_empty());
        c.update(Msg::Pick(Pick::TimeZone, 0));
        assert_eq!(c.timezone, 2);

        c.update(Msg::ShopDone(Err(SetupError::new("invalid", "bad PIN"))));
        assert_eq!(note(&c), ("invalid", "bad PIN"));
        assert!(!c.busy);
        next(&mut c);
        let report = ShopReport {
            company: "PT A".into(),
            branch_code: "JKT01".into(),
            db_url: "sqlite:///shop/sultan.db".into(),
        };
        c.update(Msg::ShopDone(Ok(report.clone())));
        assert_eq!(c.page, Page::Done);
        assert_eq!(c.summary, Some(Summary::Shop(report)));
        assert!(!c.update(Msg::Action(Action::Back)).handled, "no way back");
        assert_eq!(c.outcome, None);
        assert_eq!(c.update(Msg::Key(Key::Enter)).effects, [Effect::Close]);
        assert_eq!(c.outcome, Some(AppType::Single));
    }

    #[test]
    fn hq_server_checks_the_database_then_the_listen_address() {
        let mut c = wizard();
        c.update(Msg::Pick(Pick::AppType, 1));
        assert_eq!(c.page, Page::Database);
        assert_eq!(c.db_url(), "sqlite:///shop/sultan.db");
        // Test only: a note, the page stays.
        let effects = c.update(Msg::Action(Action::Test)).effects;
        assert_eq!(
            effects,
            [Effect::CheckDb {
                url: "sqlite:///shop/sultan.db".into()
            }]
        );
        c.update(Msg::DbChecked(Ok(DbCheck::Empty)));
        assert_eq!((c.page, note(&c).0), (Page::Database, "db_ok"));
        c.update(Msg::Action(Action::Test));
        c.update(Msg::DbChecked(Err(SetupError::new("db", "refused"))));
        assert_eq!(note(&c), ("db", "refused"));

        // MySQL: the URL must be a mysql:// URL.
        c.update(Msg::Pick(Pick::DbKind, 1));
        assert_eq!(c.db_kind, DbKind::Mysql);
        assert!(next(&mut c).is_empty());
        assert_eq!(note(&c).0, "mysql_url");
        type_into(&mut c, &[(Field::MysqlUrl, " mysql://u:p@h/sultan ")]);
        assert_eq!(
            next(&mut c),
            [Effect::CheckDb {
                url: "mysql://u:p@h/sultan".into()
            }]
        );
        c.update(Msg::DbChecked(Ok(DbCheck::Initialised("PT Old".into()))));
        assert_eq!(
            (c.page, note(&c)),
            (Page::Database, ("db_initialised", "PT Old"))
        );
        // Back to SQLite with an empty path.
        c.update(Msg::Pick(Pick::DbKind, 0));
        type_into(&mut c, &[(Field::SqlitePath, " ")]);
        next(&mut c);
        assert_eq!(note(&c), ("required", "sqlite_path"));
        type_into(&mut c, &[(Field::SqlitePath, "sqlite:///data/hq.db")]);
        assert_eq!(c.db_url(), "sqlite:///data/hq.db");
        next(&mut c);
        c.update(Msg::DbChecked(Ok(DbCheck::Empty)));
        assert_eq!(c.page, Page::Network);

        assert_eq!(c.form.listen, "0.0.0.0:8080");
        type_into(&mut c, &[(Field::Listen, "")]);
        next(&mut c);
        assert_eq!(note(&c), ("required", "listen"));
        type_into(&mut c, &[(Field::Listen, "localhost")]);
        next(&mut c);
        assert_eq!(note(&c).0, "listen_format");
        type_into(&mut c, &[(Field::Listen, "0.0.0.0:9000")]);
        next(&mut c);
        assert_eq!(c.page, Page::Shop);
        fill_shop(&mut c);
        type_into(&mut c, &[(Field::BranchName, "Toko Jakarta")]);
        next(&mut c);
        fill_owner(&mut c);
        let effects = next(&mut c);
        let Effect::InitShop(request) = &effects[0] else {
            panic!("{effects:?}")
        };
        assert_eq!(request.app_type, AppType::Hq);
        assert_eq!(request.db_url.as_deref(), Some("sqlite:///data/hq.db"));
        assert_eq!(request.listen.as_deref(), Some("0.0.0.0:9000"));
        assert_eq!(request.branch_name, "Toko Jakarta");
        assert_eq!(request.timezone, "Asia/Jakarta");

        // Back walks the pages in reverse.
        c.busy = false;
        for page in [Page::Shop, Page::Network, Page::Database, Page::Welcome] {
            c.update(Msg::Key(Key::Escape));
            assert_eq!(c.page, page);
        }
    }

    #[test]
    fn terminal_logs_in_a_manager_and_registers() {
        let mut c = wizard();
        c.update(Msg::Pick(Pick::AppType, 3));
        assert_eq!(c.page, Page::Server);
        next(&mut c);
        assert_eq!(note(&c), ("required", "server"));
        type_into(&mut c, &[(Field::Server, "192.168.1.10")]);
        assert_eq!(
            next(&mut c),
            [Effect::CheckServer {
                server: "http://192.168.1.10:8080".into()
            }]
        );
        c.update(Msg::ServerChecked(Err(SetupError::new(
            "network", "refused",
        ))));
        assert_eq!((c.page, note(&c).0), (Page::Server, "network"));
        next(&mut c);
        c.update(Msg::ServerChecked(Ok(())));
        assert_eq!(c.page, Page::Manager);

        next(&mut c);
        assert_eq!(note(&c), ("required", "manager"));
        type_into(
            &mut c,
            &[(Field::Manager, " owner "), (Field::ManagerPassword, "pw")],
        );
        assert_eq!(
            next(&mut c),
            [Effect::Login {
                server: "http://192.168.1.10:8080".into(),
                username: "owner".into(),
                password: "pw".into(),
            }]
        );
        c.update(Msg::LoggedIn(Err(SetupError::new(
            "invalid_credentials",
            "",
        ))));
        assert_eq!(note(&c).0, "invalid_credentials");
        next(&mut c);
        let branches = vec![
            BranchChoice {
                id: "b1".into(),
                code: "JKT01".into(),
                name: "Jakarta".into(),
            },
            BranchChoice {
                id: "b2".into(),
                code: "BDG01".into(),
                name: "Bandung".into(),
            },
        ];
        c.update(Msg::LoggedIn(Ok(ManagerSession {
            token: "t".into(),
            branch_id: "b2".into(),
            branches,
        })));
        assert_eq!((c.page, c.branch), (Page::Terminal, 1));
        assert!(c.form.manager_password.is_empty(), "not kept");
        c.update(Msg::Pick(Pick::Branch, 0));
        c.update(Msg::Pick(Pick::Branch, 5));
        assert_eq!(c.branch, 0);

        next(&mut c);
        assert_eq!(note(&c), ("required", "register_code"));
        type_into(
            &mut c,
            &[
                (Field::RegisterCode, "R 2"),
                (Field::RegisterName, "Kasir 2"),
            ],
        );
        next(&mut c);
        assert_eq!(note(&c).0, "code_format");
        type_into(&mut c, &[(Field::RegisterCode, "R2")]);
        let effects = next(&mut c);
        assert_eq!(
            effects,
            [Effect::Register {
                server: "http://192.168.1.10:8080".into(),
                token: "t".into(),
                request: TerminalRequest {
                    branch_id: "b1".into(),
                    code: "R2".into(),
                    name: "Kasir 2".into(),
                },
            }]
        );
        c.update(Msg::Paired(Err(SetupError::new("conflict", ""))));
        assert_eq!(note(&c).0, "conflict");
        let paired = Paired {
            register_code: "R2".into(),
            register_name: "Kasir 2".into(),
            server: "http://192.168.1.10:8080".into(),
        };
        next(&mut c);
        c.update(Msg::Paired(Ok(paired.clone())));
        assert_eq!(c.page, Page::Done);
        assert_eq!(c.summary, Some(Summary::Paired(paired)));
        assert_eq!(c.session, None);
        assert_eq!(next(&mut c), [Effect::Close]);
        assert_eq!(c.outcome, Some(AppType::Terminal));
    }

    #[test]
    fn terminal_without_a_session_or_branch_and_back_drops_the_session() {
        let mut c = wizard();
        c.update(Msg::Pick(Pick::AppType, 3));
        type_into(
            &mut c,
            &[
                (Field::Server, "h:1"),
                (Field::Manager, "m"),
                (Field::ManagerPassword, "p"),
                (Field::RegisterCode, "R2"),
                (Field::RegisterName, "Kasir 2"),
            ],
        );
        next(&mut c);
        c.update(Msg::ServerChecked(Ok(())));
        next(&mut c);
        c.update(Msg::LoggedIn(Ok(ManagerSession {
            token: "t".into(),
            branch_id: "x".into(),
            branches: Vec::new(),
        })));
        assert_eq!(c.branch, 0, "unknown branch: the first");
        next(&mut c);
        assert_eq!(note(&c).0, "no_branch");
        c.update(Msg::Action(Action::Back));
        assert_eq!((c.page, c.session.is_none()), (Page::Manager, true));
        // Forced onto the terminal page without a session (cannot happen from the window).
        c.page = Page::Terminal;
        next(&mut c);
        assert_eq!(note(&c).0, "session_ended");
    }

    #[test]
    fn terminal_pairing_code_fallback() {
        let mut c = wizard();
        c.update(Msg::Pick(Pick::AppType, 3));
        // Use-code only works on the manager page.
        c.update(Msg::Action(Action::UseCode));
        assert_eq!(c.page, Page::Server);
        type_into(&mut c, &[(Field::Server, "https://pos.example:8443/")]);
        next(&mut c);
        c.update(Msg::ServerChecked(Ok(())));
        c.update(Msg::Action(Action::UseCode));
        assert_eq!(c.page, Page::Code);
        next(&mut c);
        assert_eq!(note(&c), ("required", "pairing_code"));
        type_into(&mut c, &[(Field::PairingCode, " k7q2-m9xp ")]);
        assert_eq!(
            next(&mut c),
            [Effect::Pair {
                server: "https://pos.example:8443".into(),
                code: "K7Q2-M9XP".into(),
            }]
        );
        c.update(Msg::Paired(Err(SetupError::new(
            "invalid_pairing_code",
            "",
        ))));
        assert_eq!(note(&c).0, "invalid_pairing_code");
        c.update(Msg::Action(Action::UseLogin));
        assert_eq!(c.page, Page::Manager);
        // Test is for the database page only.
        assert!(c.update(Msg::Action(Action::Test)).effects.is_empty());
    }

    #[test]
    fn server_urls() {
        let cases = [
            ("192.168.1.10", "http://192.168.1.10:8080"),
            (" 192.168.1.10:9000/ ", "http://192.168.1.10:9000"),
            ("http://h", "http://h:8080"),
            ("http://h:81/api/v1", "http://h:81/api/v1"),
            ("h/api/v1", "http://h:8080/api/v1"),
            ("h:", "http://h::8080"),
        ];
        for (typed, url) in cases {
            assert_eq!(server_url(typed), url, "{typed}");
        }
        assert!(is_code("R1") && !is_code("") && !is_code("ABCDEFGHIJK"));
    }
}
