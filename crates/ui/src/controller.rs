//! The register's screen flow as a plain state machine (plan Phase 8; spec 2.1 §4–§6). No Slint
//! and no IO here: [`Controller::update`] takes a [`Msg`] (a key, a click, or the result of a
//! service call) and returns the [`Effect`]s (service calls) to run; the glue (`crate::app`) runs
//! them through [`sultan_client::Service`] and feeds the results back as messages.
//!
//! Screens ([`Screen`]):
//! 1. **Lock** — tiles from `GET /registers/me/users`; arrows (or a click) pick a user, a digit or
//!    Enter opens the PIN pad; Enter logs in, Esc closes the pad. `401 invalid_credentials` shows
//!    "wrong PIN"; `423 locked_out` shows a countdown to `locked_until`. Opened from the Start
//!    screen ([`Controller::start`]), F9 closes the window and goes back there
//!    ([`Effect::Exit`]).
//! 2. **Open session** — after login when this register has no open session: the opening cash
//!    is typed as digits and shown as Rupiah; Enter opens (`Idempotency-Key`, reused only when
//!    the answer was lost). If another user's session is open here, the screen says whose and
//!    only offers "back to lock" (spec 2.1 §5 handover: no shared sessions).
//! 3. **Home** — the cashier's screen: user, session start, opening cash, the **barcode field**
//!    (phase C5, spec 2.2 §6) and the actions F2 test override, F10 close session, F12 lock
//!    (logout; spec 2.1 §4: locking keeps the session open). Typed or scanned characters go into
//!    the field ([`crate::scan::ScanField`]); Enter resolves the code through
//!    `GET /catalog/lookup` and shows the item (or why there is none: not found, not sold at this
//!    branch, archived, not for sale, a scale label of 0); Esc clears the field and the item. The
//!    lookup does not block input, so the next scan can follow at once; only the newest answer is
//!    shown. The item shows its store price (spec 2.3 R5); with a customer attached, the
//!    customer's price is quoted too (`POST /pricing/quote`, [`Effect::Quote`]). No cart yet
//!    (spec 2.6).
//! 4. **Close** — blind close (spec 2.1 §5): a counted total or per-denomination counts; expected
//!    cash is never shown before the close. Enter asks for confirmation, Enter again closes.
//! 5. **Report** — expected, counted, over/short and tender totals; Enter logs out to Lock.
//!
//! **Customer** (spec 2.5 §6–§11, phase M5; [`crate::customer`]): F3 on the session home opens
//! the customer dialog (find by phone, member code or, with `customer.search`, name; sign up;
//! pay a balance out). Enter on a found customer attaches them: the home shows their receipt
//! line and balance until F4 detaches them, the user locks or the session closes (the cart and
//! its customer come with 2.6). After login the register asks `GET /auth/me` for the user's
//! permissions, so the dialog shows only what the user may do.
//!
//! **Manager override** ([`Approval`], spec 2.1 §6): an action answered with `403 needs_override`
//! opens a dialog over the current screen with the action, the value and the cashier's limit; the
//! manager is picked with ↑/↓ (or a click) and types their PIN; Enter asks `POST /overrides` and
//! the original action is retried with the token as `X-Override`. Esc cancels. Self-approval, a
//! wrong PIN, a lockout and `override_limit_exceeded` are shown in the dialog.
//!
//! **Events** (spec 2.1 §4): after login the glue watches `GET /events`; `session.revoked` for
//! this login session returns to Lock. Events carry the login generation they were watched for,
//! so a late event of an earlier login is ignored. `catalog.changed` for the shown item's
//! product (or for the catalog settings), `price.changed` for its variant (or for any price:
//! `variant_ids` null) and `events.lagged` look the shown code up again, so the panel follows
//! the back office (spec 2.2 §11, 2.3 §9); a customer price is then quoted again. The socket
//! needs the PIN session, which the home screen always has.
//!
//! While a call is running (`busy`), input is swallowed, so a double Enter cannot log in or open
//! twice.

use sultan_client::{
    Branch, CashierSession, ClientError, CloseReport, CloseRequest, Customer, LockUser, Login,
    Lookup, OverrideGrant, OverrideRequest, ProtectedAction, ProtectedResult, Quote, QuoteRequest,
    ServerEvent,
};

use crate::amount::{Digits, percent, rupiah};
use crate::count::{CountForm, wrap};
use crate::customer::{Call, Dialog, Outcome, Perms, Reply};
use crate::input::{Key, Pin};
use crate::scan::ScanField;

/// The action the "test override" button asks for: a 15% line discount, above the seeded
/// Cashier limit (10%) and within the Manager limit (20%).
pub const DEMO_ACTION: &str = "sale.discount";
/// The requested value of [`DEMO_ACTION`] (basis points).
pub const DEMO_VALUE: i64 = 1_500;
/// Lock-screen tiles per row (the arrow keys move by this much vertically).
pub const TILE_COLUMNS: usize = 4;

/// The colour of a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Information.
    Neutral,
    /// Success / over.
    Good,
    /// Error / short.
    Bad,
}

impl Tone {
    /// `-1`, `0`, `1` for Slint.
    #[must_use]
    pub const fn value(self) -> i32 {
        match self {
            Self::Neutral => 0,
            Self::Good => 1,
            Self::Bad => -1,
        }
    }

    /// The tone of an over/short amount: over = good, short = bad, exact = neutral.
    #[must_use]
    pub const fn of_amount(amount: i64) -> Self {
        if amount > 0 {
            Self::Good
        } else if amount < 0 {
            Self::Bad
        } else {
            Self::Neutral
        }
    }
}

/// A message on screen: a code the UI translates (ID/EN, `ui/strings.slint`) and one argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    /// Message code, e.g. `wrong_pin`.
    pub code: &'static str,
    /// The `{}` argument.
    pub arg: String,
    /// Colour.
    pub tone: Tone,
    /// For a lockout: its end (unix ms); the argument is then the remaining time.
    pub until: Option<i64>,
}

impl Notice {
    /// A message with an argument.
    #[must_use]
    pub fn new(code: &'static str, arg: impl Into<String>, tone: Tone) -> Self {
        Self {
            code,
            arg: arg.into(),
            tone,
            until: None,
        }
    }

    /// An error message.
    #[must_use]
    pub fn bad(code: &'static str) -> Self {
        Self::new(code, "", Tone::Bad)
    }

    /// A lockout until `until` (unix ms), shown as a countdown.
    #[must_use]
    pub fn locked(until: i64) -> Self {
        Self {
            until: Some(until),
            ..Self::bad("locked_out")
        }
    }

    /// What to show at `now`: the countdown for a lockout, and nothing once it is over.
    #[must_use]
    pub fn shown(&self, now: i64) -> Option<(&'static str, String, Tone)> {
        match self.until {
            Some(until) if until <= now => None,
            Some(until) => Some((self.code, crate::input::remaining(until - now), self.tone)),
            None => Some((self.code, self.arg.clone(), self.tone)),
        }
    }
}

/// Whether an action's value is a percentage (basis points) rather than an amount.
fn is_percentage(action: &str) -> bool {
    matches!(action, "sale.discount" | "sale.cart_discount")
}

/// A requested value or limit of `action` as text: `15%` or `Rp 50.000`.
#[must_use]
pub fn value_text(action: &str, value: Option<i64>) -> String {
    match value {
        None => String::new(),
        Some(v) if is_percentage(action) => percent(v),
        Some(v) => rupiah(v),
    }
}

/// The message for a failed call (codes of `crates/server/src/error.rs`).
#[must_use]
pub fn notice_for(error: &ClientError) -> Notice {
    let Some(problem) = error.problem() else {
        return match error {
            ClientError::Network(_) => Notice::bad("network"),
            _ => Notice::new("error", "unexpected", Tone::Bad),
        };
    };
    match problem.code.as_str() {
        "invalid_credentials" => Notice::bad("wrong_pin"),
        "locked_out" => Notice::locked(problem.locked_until().unwrap_or(0)),
        "invalid_device_token" => Notice::bad("device_unpaired"),
        "invalid_session" => Notice::bad("session_ended"),
        "permission_denied" | "not_session_owner" => Notice::bad("permission_denied"),
        "self_approval" => Notice::bad("self_approval"),
        "override_limit_exceeded" => Notice::new(
            "override_limit_exceeded",
            value_text(problem.permission().unwrap_or_default(), problem.limit()),
            Tone::Bad,
        ),
        "invalid_override" => Notice::bad("invalid_override"),
        "session_already_open" if problem.conflict() == Some("user") => {
            Notice::bad("already_open_user")
        }
        "session_already_open" => Notice::bad("already_open_register"),
        "session_closed" => Notice::bad("session_closed"),
        "no_open_session" => Notice::bad("no_open_session"),
        "invalid_field" | "amount_overflow" => Notice::bad("invalid_amount"),
        other => Notice::new("error", other.to_owned(), Tone::Bad),
    }
}

/// The message for a failed lookup of `code` (spec 2.2 §6 errors); other errors as
/// [`notice_for`].
#[must_use]
pub fn lookup_notice(error: &ClientError, code: &str) -> Notice {
    let item = match error.code() {
        Some("not_found") => "item_not_found",
        Some("not_sold_here") => "item_not_sold_here",
        Some("archived") => "item_archived",
        Some("not_sellable") => "item_not_sellable",
        Some("scale_value_zero") => "scale_value_zero",
        _ => return notice_for(error),
    };
    Notice::new(item, code, Tone::Bad)
}

/// The message for a failed customer price: the error code of a quoted line (`no_price`,
/// `invalid_reference`, …), translated like the API's.
#[must_use]
pub fn price_notice(code: &str) -> Notice {
    match code {
        "no_price" => Notice::bad("no_price"),
        "invalid_reference" => Notice::bad("price_reference"),
        "invalid_field" | "amount_overflow" => Notice::bad("invalid_amount"),
        other => Notice::new("error", other.to_owned(), Tone::Bad),
    }
}

/// The message for a quote that failed as a whole (spec 2.3 §15.12: an unknown, archived,
/// merged or erased customer is `422 invalid_reference`).
#[must_use]
pub fn quote_notice(error: &ClientError) -> Notice {
    match error.code() {
        Some(code @ ("no_price" | "invalid_reference")) => price_notice(code),
        _ => notice_for(error),
    }
}

/// Whether the error means the login (or the device) is no longer valid.
fn ends_login(error: &ClientError) -> bool {
    matches!(
        error.code(),
        Some("invalid_session" | "invalid_device_token")
    )
}

/// A fresh idempotency key / context reference (also the id of a new customer).
pub(crate) fn new_key() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// What a key chip or button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Enter: log in, open, confirm, approve, done.
    Confirm,
    /// Esc: close the pad, cancel the dialog, back.
    Back,
    /// Reload the lock-screen users.
    Retry,
    /// Log out to the lock screen (the cashier session stays open).
    Lock,
    /// Start the blind close.
    CloseSession,
    /// Run the demo action protected by an override.
    TestOverride,
    /// Leave the register for the Start screen (lock screen only, when opened from it).
    Start,
    /// Open the customer dialog.
    Customer,
    /// Detach the attached customer.
    Detach,
    /// Customer dialog: phone or code ↔ name search.
    SwitchMode,
    /// Customer dialog: sign up a new customer.
    NewCustomer,
    /// Customer dialog: pay the balance out.
    PayOut,
    /// Customer dialog: show the customer whose phone the sign-up wanted.
    ShowTaken,
    /// Customer dialog: the sign-up's next field.
    NextField,
}

impl Action {
    const ALL: [Self; 14] = [
        Self::Confirm,
        Self::Back,
        Self::Retry,
        Self::Lock,
        Self::CloseSession,
        Self::TestOverride,
        Self::Start,
        Self::Customer,
        Self::Detach,
        Self::SwitchMode,
        Self::NewCustomer,
        Self::PayOut,
        Self::ShowTaken,
        Self::NextField,
    ];

    /// The ID used by the UI.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Back => "back",
            Self::Retry => "retry",
            Self::Lock => "lock",
            Self::CloseSession => "close-session",
            Self::TestOverride => "test-override",
            Self::Start => "start",
            Self::Customer => "customer",
            Self::Detach => "detach-customer",
            Self::SwitchMode => "switch-mode",
            Self::NewCustomer => "new-customer",
            Self::PayOut => "pay-out",
            Self::ShowTaken => "show-taken",
            Self::NextField => "next-field",
        }
    }

    /// The action for a UI ID.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.id() == id)
    }
}

/// A key chip: the key, what it does and its label code (`ui/strings.slint`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shortcut {
    /// The key.
    pub key: Key,
    /// The action.
    pub action: Action,
    /// Label code.
    pub label: &'static str,
}

/// The PIN pad of the lock screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pad {
    /// Index into the users.
    pub user: usize,
    /// The PIN being typed.
    pub pin: Pin,
    /// Wrong PIN / lockout.
    pub notice: Option<Notice>,
}

/// Lock screen state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockScreen {
    /// The highlighted tile.
    pub cursor: usize,
    /// The PIN pad, when a user was picked.
    pub pad: Option<Pad>,
    /// Loading the users failed, the session ended, …
    pub notice: Option<Notice>,
}

/// Open-session screen state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenScreen {
    /// The opening cash typed.
    pub amount: Digits,
    /// Someone else's session open on this register.
    pub blocked: Option<CashierSession>,
    /// Errors.
    pub notice: Option<Notice>,
    key: String,
}

impl OpenScreen {
    fn new(blocked: Option<CashierSession>, notice: Option<Notice>) -> Self {
        Self {
            amount: Digits::amount(),
            blocked,
            notice,
            key: new_key(),
        }
    }
}

/// Home screen state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomeScreen {
    /// Result of the last action.
    pub notice: Option<Notice>,
    /// The barcode field and the item it found.
    pub scan: ScanField,
}

/// Blind-close screen state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseScreen {
    /// The count.
    pub form: CountForm,
    /// Asking "close with this count?".
    pub confirm: bool,
    /// Errors.
    pub notice: Option<Notice>,
    key: String,
}

/// The screens; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Who is at the register.
    Lock(LockScreen),
    /// Opening cash.
    Open(OpenScreen),
    /// Session home.
    Home(HomeScreen),
    /// Blind close.
    Close(CloseScreen),
    /// The close report.
    Report(Box<CloseReport>),
}

impl Screen {
    /// The name the UI switches on.
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Lock(_) => "lock",
            Self::Open(_) => "open",
            Self::Home(_) => "home",
            Self::Close(_) => "close",
            Self::Report(_) => "report",
        }
    }
}

/// The manager-override dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    /// The action being approved.
    pub action: ProtectedAction,
    /// The cashier's own limit, if the server said.
    pub limit: Option<i64>,
    /// The highlighted manager (index into [`Controller::approvers`]).
    pub cursor: usize,
    /// The manager's PIN.
    pub pin: Pin,
    /// Errors.
    pub notice: Option<Notice>,
}

/// A service call to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// [`sultan_client::Service::lock_users`].
    LoadUsers,
    /// [`sultan_client::Service::pin_login`].
    Login {
        /// User.
        user_id: String,
        /// PIN.
        pin: String,
    },
    /// [`sultan_client::Service::branch`].
    LoadBranch {
        /// Session token.
        token: String,
        /// Branch.
        branch_id: String,
    },
    /// [`sultan_client::Service::current_session`].
    LoadCurrent {
        /// Session token.
        token: String,
    },
    /// [`sultan_client::Service::open_session`].
    Open {
        /// Session token.
        token: String,
        /// Opening cash.
        amount: i64,
        /// Idempotency key.
        key: String,
    },
    /// [`sultan_client::Service::close_session`].
    Close {
        /// Session token.
        token: String,
        /// The cashier session.
        session_id: String,
        /// The count.
        body: CloseRequest,
        /// Idempotency key.
        key: String,
    },
    /// [`sultan_client::Service::request_override`].
    RequestOverride {
        /// Session token.
        token: String,
        /// The request.
        body: OverrideRequest,
    },
    /// [`sultan_client::Service::demo_protected`].
    Protected {
        /// Session token.
        token: String,
        /// The action.
        body: ProtectedAction,
        /// `X-Override`.
        override_token: Option<String>,
    },
    /// [`sultan_client::Service::logout`].
    Logout {
        /// Session token.
        token: String,
    },
    /// [`sultan_client::Service::lookup`]; the answer comes back as [`Msg::Looked`] with `seq`.
    Lookup {
        /// Session token.
        token: String,
        /// The scanned or typed code.
        code: String,
        /// The lookup's sequence number.
        seq: u64,
        /// The login generation the lookup was started for.
        generation: u64,
    },
    /// [`sultan_client::Service::quote`] of the shown item for the attached customer; the answer
    /// comes back as [`Msg::Quoted`] with `seq`.
    Quote {
        /// Session token.
        token: String,
        /// One line, with the customer.
        request: QuoteRequest,
        /// The quote's sequence number.
        seq: u64,
        /// The login generation the quote was asked for.
        generation: u64,
    },
    /// A customer call ([`Call`]); the answer comes back as [`Msg::Customer`] with
    /// `generation`.
    Customer {
        /// Session token.
        token: String,
        /// The call.
        call: Call,
        /// The login generation the call was made for.
        generation: u64,
    },
    /// [`sultan_client::Service::watch_events`]; events come back as [`Msg::Event`] with
    /// `generation`.
    WatchEvents {
        /// Session token.
        token: String,
        /// The login they belong to.
        generation: u64,
    },
    /// Close the window: back to the Start screen ([`Action::Start`]).
    Exit,
}

/// Input for [`Controller::update`].
#[derive(Debug, Clone, PartialEq)]
pub enum Msg {
    /// A key press.
    Key(Key),
    /// A chip or button.
    Action(Action),
    /// A click on a lock-screen tile.
    PickUser(usize),
    /// A click on a manager in the override dialog.
    PickApprover(usize),
    /// A click on a row of the count form.
    PickRow(usize),
    /// Result of [`Effect::LoadUsers`].
    Users(Result<Vec<LockUser>, ClientError>),
    /// Result of [`Effect::Login`].
    LoggedIn(Result<Login, ClientError>),
    /// Result of [`Effect::LoadBranch`].
    Branch(Result<Branch, ClientError>),
    /// Result of [`Effect::LoadCurrent`].
    Current(Result<Option<CashierSession>, ClientError>),
    /// Result of [`Effect::Open`].
    Opened(Result<CashierSession, ClientError>),
    /// Result of [`Effect::Close`] (boxed: the report is by far the largest message).
    Closed(Result<Box<CloseReport>, ClientError>),
    /// Result of [`Effect::RequestOverride`].
    Approved(Result<OverrideGrant, ClientError>),
    /// Result of [`Effect::Protected`].
    Protected(Result<ProtectedResult, ClientError>),
    /// Result of [`Effect::Logout`].
    LoggedOut(Result<(), ClientError>),
    /// An event of the login `generation`.
    Event(u64, ServerEvent),
    /// Result of [`Effect::Lookup`] number `seq` (second field) of the login `generation`
    /// (first field).
    Looked(u64, u64, Result<Lookup, ClientError>),
    /// Result of [`Effect::Quote`] number `seq` (second field) of the login `generation` (first
    /// field).
    Quoted(u64, u64, Result<Quote, ClientError>),
    /// Result of [`Effect::Customer`] of the login `generation`.
    Customer(u64, Box<Reply>),
    /// A click on a name-search result in the customer dialog.
    PickCustomer(usize),
    /// A click on a sign-up field in the customer dialog.
    PickField(usize),
}

/// What [`Controller::update`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Calls to make.
    pub effects: Vec<Effect>,
    /// For keys: whether the key was used (otherwise Slint may handle it).
    pub handled: bool,
}

impl Step {
    const fn handled(effects: Vec<Effect>) -> Self {
        Self {
            effects,
            handled: true,
        }
    }
}

/// The register state; see the module docs.
#[derive(Debug)]
pub struct Controller {
    /// Lock-screen users.
    pub users: Vec<LockUser>,
    /// The register's branch, once known.
    pub branch: Option<Branch>,
    /// The logged-in user.
    pub login: Option<Login>,
    /// The user's open cashier session.
    pub session: Option<CashierSession>,
    /// The current screen.
    pub screen: Screen,
    /// The override dialog, when open.
    pub approval: Option<Approval>,
    /// A call is running; input is ignored.
    pub busy: bool,
    /// The protected action waiting for a result.
    pending: Option<ProtectedAction>,
    /// The lock-screen message is from a failed user list (cleared once it loads).
    users_failed: bool,
    generation: u64,
    /// Sequence number of the last lookup.
    lookups: u64,
    /// Sequence number of the last quote.
    quotes: u64,
    /// The window was opened from the Start screen: the lock screen offers F9 to go back.
    pub start: bool,
    /// The customer attached to the session home (spec 2.5 M5).
    pub customer: Option<Customer>,
    /// The customer dialog, when open.
    pub dialog: Option<Dialog>,
    /// The logged-in user's customer permissions.
    pub perms: Perms,
}

impl Controller {
    /// The start state (Lock) and its first call.
    #[must_use]
    pub fn new() -> (Self, Vec<Effect>) {
        let controller = Self {
            users: Vec::new(),
            branch: None,
            login: None,
            session: None,
            screen: Screen::Lock(LockScreen::default()),
            approval: None,
            busy: false,
            pending: None,
            users_failed: false,
            generation: 0,
            lookups: 0,
            quotes: 0,
            start: false,
            customer: None,
            dialog: None,
            perms: Perms::default(),
        };
        (controller, vec![Effect::LoadUsers])
    }

    /// The display name of a user ID, if it is on the lock screen.
    #[must_use]
    pub fn user_name(&self, id: &str) -> Option<&str> {
        self.users
            .iter()
            .find(|u| u.id == id)
            .map(|u| u.display_name.as_str())
    }

    /// Who may approve an override: everyone on the lock screen except the logged-in user (the
    /// server checks permissions and limits).
    #[must_use]
    pub fn approvers(&self) -> Vec<&LockUser> {
        let me = self.login.as_ref().map(|l| l.user_id.as_str());
        self.users
            .iter()
            .filter(|u| Some(u.id.as_str()) != me)
            .collect()
    }

    /// The actions of the session home (also shown as large buttons there). F4 detaches the
    /// customer, when one is attached.
    #[must_use]
    pub fn home_actions(&self) -> Vec<Shortcut> {
        let s = |key, action, label| Shortcut { key, action, label };
        let mut keys = vec![
            s(Key::F(2), Action::TestOverride, "test_override"),
            s(Key::F(3), Action::Customer, "customer"),
        ];
        if self.customer.is_some() {
            keys.push(s(Key::F(4), Action::Detach, "detach_customer"));
        }
        keys.push(s(Key::F(10), Action::CloseSession, "close_session"));
        keys.push(s(Key::F(12), Action::Lock, "lock"));
        keys
    }

    /// The keys of the barcode field (shown next to it): Enter looks up, Esc clears.
    #[must_use]
    pub fn scan_actions() -> Vec<Shortcut> {
        let s = |key, action, label| Shortcut { key, action, label };
        vec![
            s(Key::Enter, Action::Confirm, "look_up"),
            s(Key::Escape, Action::Back, "clear"),
        ]
    }

    /// The key chips of the current screen or dialog.
    #[must_use]
    pub fn shortcuts(&self) -> Vec<Shortcut> {
        use Action::{Back, Confirm, Lock, Retry};
        let s = |key, action, label| Shortcut { key, action, label };
        if self.approval.is_some() {
            return vec![
                s(Key::Enter, Confirm, "approve"),
                s(Key::Escape, Back, "cancel"),
            ];
        }
        if let Some(d) = &self.dialog {
            return d.shortcuts(self.perms);
        }
        match &self.screen {
            Screen::Lock(l) if l.pad.is_some() => {
                vec![
                    s(Key::Enter, Confirm, "log_in"),
                    s(Key::Escape, Back, "back"),
                ]
            }
            Screen::Lock(_) => {
                let mut keys = vec![
                    s(Key::Enter, Confirm, "enter_pin"),
                    s(Key::F(5), Retry, "refresh"),
                ];
                if self.start {
                    keys.push(s(Key::F(9), Action::Start, "start"));
                }
                keys
            }
            Screen::Open(o) if o.blocked.is_some() => vec![s(Key::Escape, Lock, "back_to_lock")],
            Screen::Open(_) => vec![
                s(Key::Enter, Confirm, "open_session"),
                s(Key::Escape, Lock, "lock"),
            ],
            Screen::Home(_) => {
                let mut keys = self.home_actions();
                keys.extend(Self::scan_actions());
                keys
            }
            Screen::Close(c) if c.confirm => vec![
                s(Key::Enter, Confirm, "confirm_close"),
                s(Key::Escape, Back, "edit_count"),
            ],
            Screen::Close(_) => vec![
                s(Key::Enter, Confirm, "close_session"),
                s(Key::Escape, Back, "back"),
            ],
            Screen::Report(_) => vec![s(Key::Enter, Confirm, "done")],
        }
    }

    /// Handles one message; see the module docs.
    pub fn update(&mut self, msg: Msg) -> Step {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::Action(action) => Step::handled(self.on_action(action)),
            Msg::PickUser(i) => {
                self.pick_user(i);
                Step::default()
            }
            Msg::PickApprover(i) => {
                let count = self.approvers().len();
                if let Some(a) = self.approval.as_mut().filter(|_| i < count) {
                    a.cursor = i;
                }
                Step::default()
            }
            Msg::PickCustomer(i) => {
                if let Some(d) = self.dialog.as_mut().filter(|_| !self.busy) {
                    d.pick(i);
                }
                Step::default()
            }
            Msg::PickField(i) => {
                if let Some(d) = self.dialog.as_mut().filter(|_| !self.busy) {
                    d.pick_field(i);
                }
                Step::default()
            }
            Msg::PickRow(i) => {
                if let Screen::Close(c) = &mut self.screen
                    && !c.confirm
                {
                    c.form.select(i);
                }
                Step::default()
            }
            result => Step {
                effects: self.on_result(result),
                handled: false,
            },
        }
    }

    fn on_key(&mut self, key: Key) -> Step {
        if self.busy {
            return Step {
                effects: Vec::new(),
                handled: key != Key::Other,
            };
        }
        if self.edit(key) {
            return Step::handled(Vec::new());
        }
        match self.shortcuts().into_iter().find(|s| s.key == key) {
            Some(s) => Step::handled(self.on_action(s.action)),
            None => Step::default(),
        }
    }

    /// Typing and moving within the screen or dialog. Returns whether the key was used.
    fn edit(&mut self, key: Key) -> bool {
        let count = self.approvers().len();
        if let Some(a) = &mut self.approval {
            return match key {
                Key::Up | Key::BackTab => {
                    a.cursor = wrap(a.cursor, -1, count);
                    true
                }
                Key::Down | Key::Tab => {
                    a.cursor = wrap(a.cursor, 1, count);
                    true
                }
                Key::Digit(d) => {
                    a.pin.push(d);
                    true
                }
                Key::Backspace => {
                    a.pin.pop();
                    true
                }
                _ => false,
            };
        }
        if let Some(d) = &mut self.dialog {
            return d.edit(key);
        }
        let users = self.users.len();
        match &mut self.screen {
            Screen::Lock(l) => lock_edit(l, key, users),
            Screen::Open(o) if o.blocked.is_none() => match key {
                Key::Digit(d) => {
                    o.amount.push(d);
                    o.notice = None;
                    true
                }
                Key::Backspace => {
                    o.amount.pop();
                    true
                }
                _ => false,
            },
            Screen::Home(h) => match key {
                Key::Digit(d) => h.scan.push(char::from(b'0' + d)),
                Key::Char(c) => h.scan.push(c),
                Key::Backspace => {
                    h.scan.pop();
                    true
                }
                _ => false,
            },
            Screen::Close(c) if !c.confirm => {
                let used = match key {
                    Key::Up | Key::BackTab => {
                        c.form.step(-1);
                        true
                    }
                    Key::Down | Key::Tab => {
                        c.form.step(1);
                        true
                    }
                    Key::Digit(d) => {
                        c.form.push(d);
                        true
                    }
                    Key::Backspace => {
                        c.form.pop();
                        true
                    }
                    _ => false,
                };
                if used {
                    c.notice = None;
                }
                used
            }
            _ => false,
        }
    }

    fn pick_user(&mut self, index: usize) {
        if self.busy || index >= self.users.len() {
            return;
        }
        if let Screen::Lock(l) = &mut self.screen {
            l.cursor = index;
            l.pad = Some(Pad {
                user: index,
                pin: Pin::default(),
                notice: None,
            });
        }
    }

    fn on_action(&mut self, action: Action) -> Vec<Effect> {
        if self.busy || !self.shortcuts().iter().any(|s| s.action == action) {
            return Vec::new();
        }
        if let Some(d) = &mut self.dialog {
            let outcome = d.act(action);
            return self.dialog_outcome(outcome);
        }
        match action {
            Action::Confirm => self.confirm(),
            Action::Back => {
                self.back();
                Vec::new()
            }
            Action::Retry => vec![Effect::LoadUsers],
            Action::Start => vec![Effect::Exit],
            Action::Lock => self.logout(None),
            Action::CloseSession => {
                self.screen = Screen::Close(CloseScreen {
                    form: CountForm::default(),
                    confirm: false,
                    notice: None,
                    key: new_key(),
                });
                Vec::new()
            }
            Action::TestOverride => {
                let body = ProtectedAction {
                    action: DEMO_ACTION.to_owned(),
                    value: Some(DEMO_VALUE),
                    context_ref: format!("demo-{}", new_key()),
                };
                self.protected(body, None)
            }
            Action::Customer => {
                self.dialog = Some(Dialog::new(self.customer.as_ref()));
                Vec::new()
            }
            Action::Detach => {
                self.customer = None;
                if let Screen::Home(h) = &mut self.screen {
                    h.scan.forget_quote();
                }
                self.set_notice(Notice::new("customer_detached", "", Tone::Neutral));
                Vec::new()
            }
            // Dialog actions; never on a screen's shortcuts.
            Action::SwitchMode
            | Action::NewCustomer
            | Action::PayOut
            | Action::ShowTaken
            | Action::NextField => Vec::new(),
        }
    }

    /// What the customer dialog's action leads to.
    fn dialog_outcome(&mut self, outcome: Outcome) -> Vec<Effect> {
        match outcome {
            Outcome::Stay => Vec::new(),
            Outcome::Close => {
                self.dialog = None;
                Vec::new()
            }
            Outcome::Call(call) => {
                self.busy = true;
                vec![Effect::Customer {
                    token: self.token(),
                    call,
                    generation: self.generation,
                }]
            }
            Outcome::Attach(customer) => {
                let line = crate::customer::receipt_line(&customer);
                self.customer = Some(*customer);
                self.dialog = None;
                self.set_notice(Notice::new("customer_attached", line, Tone::Good));
                self.quote_shown()
            }
        }
    }

    /// The answer to a customer call: the permissions after login, or the dialog's call.
    fn customer_reply(&mut self, generation: u64, reply: Reply) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        if let Some(e) = reply.error().filter(|e| ends_login(e)).cloned() {
            return self.fail(&e);
        }
        if let Reply::Permissions(result) = reply {
            // Still busy until the current session is known; without the keys, the dialog
            // offers only the lookup.
            self.perms = result.map(|keys| Perms::of(&keys)).unwrap_or_default();
            return Vec::new();
        }
        self.busy = false;
        let Some(d) = &mut self.dialog else {
            return Vec::new();
        };
        if let Some(after) = d.reply(reply)
            && self.customer.as_ref().is_some_and(|c| c.id == after.id)
        {
            self.customer = Some(after);
        }
        Vec::new()
    }

    fn token(&self) -> String {
        self.login
            .as_ref()
            .map(|l| l.token.clone())
            .unwrap_or_default()
    }

    fn protected(&mut self, body: ProtectedAction, override_token: Option<String>) -> Vec<Effect> {
        self.busy = true;
        self.pending = Some(body.clone());
        vec![Effect::Protected {
            token: self.token(),
            body,
            override_token,
        }]
    }

    fn confirm(&mut self) -> Vec<Effect> {
        let token = self.token();
        if self.approval.is_some() {
            let approver = self
                .approvers()
                .get(self.approval.as_ref().map_or(0, |a| a.cursor))
                .map(|u| u.id.clone());
            let Some(a) = self.approval.as_mut() else {
                return Vec::new();
            };
            let Some(approver_id) = approver else {
                a.notice = Some(Notice::bad("pick_manager"));
                return Vec::new();
            };
            if !a.pin.is_complete() {
                a.notice = Some(Notice::bad("pin_too_short"));
                return Vec::new();
            }
            let body = OverrideRequest {
                action: a.action.action.clone(),
                value: a.action.value,
                context_ref: a.action.context_ref.clone(),
                approver_id,
                approver_pin: a.pin.take(),
            };
            a.notice = None;
            self.busy = true;
            return vec![Effect::RequestOverride { token, body }];
        }
        let session_id = self.session.as_ref().map(|s| s.id.clone());
        match &mut self.screen {
            Screen::Lock(l) => {
                let Some(pad) = &mut l.pad else {
                    let cursor = l.cursor;
                    self.pick_user(cursor);
                    return Vec::new();
                };
                if !pad.pin.is_complete() {
                    pad.notice = Some(Notice::bad("pin_too_short"));
                    return Vec::new();
                }
                let pin = pad.pin.take();
                let user_id = self.users.get(pad.user).map(|u| u.id.clone());
                let Some(user_id) = user_id else {
                    return Vec::new();
                };
                self.busy = true;
                vec![Effect::Login { user_id, pin }]
            }
            Screen::Open(o) => {
                let effect = Effect::Open {
                    token,
                    amount: o.amount.value(),
                    key: o.key.clone(),
                };
                self.busy = true;
                vec![effect]
            }
            Screen::Close(c) if !c.confirm => {
                c.confirm = true;
                Vec::new()
            }
            Screen::Close(c) => {
                let effect = Effect::Close {
                    token,
                    session_id: session_id.unwrap_or_default(),
                    body: c.form.request(),
                    key: c.key.clone(),
                };
                self.busy = true;
                vec![effect]
            }
            Screen::Report(_) => self.logout(None),
            Screen::Home(h) => {
                let seq = self.lookups + 1;
                let Some(code) = h.scan.take(seq) else {
                    return Vec::new();
                };
                // A scan is the next action: the previous action's message goes.
                h.notice = None;
                self.lookups = seq;
                vec![Effect::Lookup {
                    token,
                    code,
                    seq,
                    generation: self.generation,
                }]
            }
        }
    }

    fn back(&mut self) {
        if self.approval.take().is_some() {
            self.pending = None;
            self.set_notice(Notice::new("override_cancelled", "", Tone::Neutral));
            return;
        }
        match &mut self.screen {
            Screen::Lock(l) => l.pad = None,
            Screen::Close(c) if c.confirm => c.confirm = false,
            Screen::Close(_) => self.screen = Screen::Home(HomeScreen::default()),
            Screen::Home(h) => {
                h.scan.clear();
                h.notice = None;
            }
            _ => {}
        }
    }

    /// Back to Lock: forgets the login (after asking the server to revoke it, when `token` is
    /// still valid) and reloads the users.
    fn logout(&mut self, notice: Option<Notice>) -> Vec<Effect> {
        let token = self.login.take().map(|l| l.token);
        self.session = None;
        self.approval = None;
        self.customer = None;
        self.dialog = None;
        self.perms = Perms::default();
        self.pending = None;
        self.busy = false;
        self.generation += 1;
        self.screen = Screen::Lock(LockScreen {
            notice,
            ..LockScreen::default()
        });
        let mut effects: Vec<Effect> = token
            .map(|token| Effect::Logout { token })
            .into_iter()
            .collect();
        effects.push(Effect::LoadUsers);
        effects
    }

    /// Shows `notice` where the user is looking.
    fn set_notice(&mut self, notice: Notice) {
        let slot = match &mut self.screen {
            Screen::Lock(LockScreen { pad: Some(p), .. }) => &mut p.notice,
            Screen::Lock(l) => &mut l.notice,
            Screen::Open(o) => &mut o.notice,
            Screen::Home(h) => &mut h.notice,
            Screen::Close(c) => &mut c.notice,
            Screen::Report(_) => return,
        };
        *slot = Some(notice);
    }

    /// A failed call: back to Lock if the login or device is gone, else a message.
    fn fail(&mut self, error: &ClientError) -> Vec<Effect> {
        self.busy = false;
        if ends_login(error) {
            self.login = None;
            return self.logout(Some(notice_for(error)));
        }
        self.set_notice(notice_for(error));
        Vec::new()
    }

    fn on_result(&mut self, msg: Msg) -> Vec<Effect> {
        match msg {
            Msg::Users(Ok(users)) => {
                self.users = users;
                if let Screen::Lock(l) = &mut self.screen {
                    if std::mem::take(&mut self.users_failed) {
                        l.notice = None;
                    }
                    l.cursor = l.cursor.min(self.users.len().saturating_sub(1));
                    l.pad = l.pad.take().filter(|p| p.user < self.users.len());
                }
                Vec::new()
            }
            Msg::Users(Err(e)) => {
                if let Screen::Lock(l) = &mut self.screen {
                    l.notice = Some(notice_for(&e));
                    self.users_failed = true;
                }
                Vec::new()
            }
            Msg::LoggedIn(result) => self.logged_in(result),
            Msg::Branch(Ok(branch)) => {
                self.branch = Some(branch);
                Vec::new()
            }
            Msg::Current(result) => self.current(result),
            Msg::Opened(result) => self.opened(result),
            Msg::Closed(result) => self.closed(result.map(|report| *report)),
            Msg::Approved(result) => self.approved(result),
            Msg::Protected(result) => self.protected_done(result),
            Msg::Event(generation, event) => {
                if generation != self.generation || self.login.is_none() {
                    return Vec::new();
                }
                if event.revokes_this_session() {
                    // The server revoked it already; nothing to log out.
                    self.login = None;
                    return self.logout(Some(Notice::bad("session_ended")));
                }
                self.catalog_event(&event)
            }
            Msg::Looked(generation, seq, result) => self.looked(generation, seq, result),
            Msg::Quoted(generation, seq, result) => self.quoted(generation, seq, result),
            Msg::Customer(generation, reply) => self.customer_reply(generation, *reply),
            // Logout and branch failures change nothing on screen.
            _ => Vec::new(),
        }
    }

    /// `catalog.changed` touching the shown item (its product, or the settings with the scale
    /// layouts), `price.changed` touching its variant (or every price) or `events.lagged`: look
    /// the shown code up again.
    fn catalog_event(&mut self, event: &ServerEvent) -> Vec<Effect> {
        let token = self.token();
        let Screen::Home(h) = &mut self.screen else {
            return Vec::new();
        };
        let Some(shown) = &h.scan.shown else {
            return Vec::new();
        };
        let touches = match event.kind.as_str() {
            "events.lagged" => true,
            "catalog.changed" => {
                let data = &event.data;
                data["scope"] == "settings"
                    || shown.item().is_none_or(|item| {
                        data["product_id"].as_str() == Some(item.product_id.as_str())
                    })
            }
            "price.changed" => shown.item().is_some_and(|item| {
                event.data["variant_ids"]
                    .as_array()
                    .is_none_or(|ids| ids.iter().any(|id| id == item.variant_id.as_str()))
            }),
            _ => false,
        };
        if !touches {
            return Vec::new();
        }
        let seq = self.lookups + 1;
        let Some(code) = h.scan.again(seq) else {
            return Vec::new();
        };
        self.lookups = seq;
        vec![Effect::Lookup {
            token,
            code,
            seq,
            generation: self.generation,
        }]
    }

    fn looked(
        &mut self,
        generation: u64,
        seq: u64,
        result: Result<Lookup, ClientError>,
    ) -> Vec<Effect> {
        // An answer of an earlier login must not end (or change) the current one.
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        if let Err(e) = &result
            && ends_login(e)
        {
            return self.fail(e);
        }
        if let Screen::Home(h) = &mut self.screen {
            let code = h.scan.looking().unwrap_or_default().to_owned();
            if h.scan
                .answer(seq, result.map_err(|e| lookup_notice(&e, &code)))
            {
                return self.quote_shown();
            }
        }
        Vec::new()
    }

    /// Quotes the shown item for the attached customer, if both are there and the item is
    /// quoted ([`crate::scan::quote_line`]).
    fn quote_shown(&mut self) -> Vec<Effect> {
        let token = self.token();
        let Some(customer) = &self.customer else {
            return Vec::new();
        };
        let Screen::Home(h) = &mut self.screen else {
            return Vec::new();
        };
        let seq = self.quotes + 1;
        let Some(line) = h.scan.start_quote(seq) else {
            return Vec::new();
        };
        self.quotes = seq;
        vec![Effect::Quote {
            token,
            request: QuoteRequest {
                customer_id: Some(customer.id.clone()),
                lines: vec![line],
            },
            seq,
            generation: self.generation,
        }]
    }

    fn quoted(
        &mut self,
        generation: u64,
        seq: u64,
        result: Result<Quote, ClientError>,
    ) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        if let Err(e) = &result
            && ends_login(e)
        {
            return self.fail(e);
        }
        let line = match result {
            Ok(quote) => quote
                .lines
                .into_iter()
                .next()
                .ok_or_else(|| Notice::new("error", "unexpected", Tone::Bad))
                .and_then(|line| match &line.error {
                    Some(error) => Err(price_notice(&error.code)),
                    None => Ok(line),
                }),
            Err(e) => Err(quote_notice(&e)),
        };
        if let Screen::Home(h) = &mut self.screen {
            h.scan.quoted(seq, line);
        }
        Vec::new()
    }

    fn logged_in(&mut self, result: Result<Login, ClientError>) -> Vec<Effect> {
        match result {
            Ok(login) => {
                self.generation += 1;
                let token = login.token.clone();
                let mut effects = vec![
                    Effect::LoadCurrent {
                        token: token.clone(),
                    },
                    Effect::WatchEvents {
                        token: token.clone(),
                        generation: self.generation,
                    },
                ];
                if self.branch.is_none() {
                    effects.push(Effect::LoadBranch {
                        token: token.clone(),
                        branch_id: login.branch_id.clone(),
                    });
                }
                effects.push(Effect::Customer {
                    token,
                    call: Call::Permissions,
                    generation: self.generation,
                });
                self.login = Some(login);
                // Still busy until the current session is known.
                effects
            }
            Err(e) => {
                self.busy = false;
                if let Screen::Lock(LockScreen { pad: Some(p), .. }) = &mut self.screen {
                    p.pin.clear();
                    p.notice = Some(notice_for(&e));
                }
                Vec::new()
            }
        }
    }

    fn current(&mut self, result: Result<Option<CashierSession>, ClientError>) -> Vec<Effect> {
        let Some(login) = &self.login else {
            return Vec::new();
        };
        match result {
            Ok(Some(session)) if session.user_id == login.user_id => {
                self.busy = false;
                self.session = Some(session);
                self.screen = Screen::Home(HomeScreen::default());
                Vec::new()
            }
            Ok(other) => {
                self.busy = false;
                let notice = match &self.screen {
                    Screen::Open(o) => o.notice.clone(),
                    _ => None,
                };
                self.session = None;
                self.screen = Screen::Open(OpenScreen::new(other, notice));
                Vec::new()
            }
            // Right after login: without the session the register cannot go on; log out again.
            Err(e) if matches!(self.screen, Screen::Lock(_)) && !ends_login(&e) => {
                self.logout(Some(notice_for(&e)))
            }
            Err(e) => self.fail(&e),
        }
    }

    fn opened(&mut self, result: Result<CashierSession, ClientError>) -> Vec<Effect> {
        self.busy = false;
        match result {
            Ok(session) => {
                self.session = Some(session);
                self.screen = Screen::Home(HomeScreen {
                    notice: Some(Notice::new("session_opened", "", Tone::Good)),
                    scan: ScanField::default(),
                });
                Vec::new()
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(e) => {
                let Screen::Open(o) = &mut self.screen else {
                    return Vec::new();
                };
                if !matches!(e, ClientError::Network(_)) {
                    o.key = new_key();
                }
                o.notice = Some(notice_for(&e));
                // Someone else opened here meanwhile: show whose session it is.
                if e.code() == Some("session_already_open")
                    && e.problem().and_then(|p| p.conflict()) != Some("user")
                {
                    self.busy = true;
                    return vec![Effect::LoadCurrent {
                        token: self.token(),
                    }];
                }
                Vec::new()
            }
        }
    }

    fn closed(&mut self, result: Result<CloseReport, ClientError>) -> Vec<Effect> {
        self.busy = false;
        match result {
            Ok(report) => {
                self.session = None;
                self.customer = None;
                self.screen = Screen::Report(Box::new(report));
                Vec::new()
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(e) if e.code() == Some("session_closed") => {
                // Force-closed by a manager meanwhile.
                self.session = None;
                self.screen = Screen::Open(OpenScreen::new(None, Some(notice_for(&e))));
                Vec::new()
            }
            Err(e) => {
                if let Screen::Close(c) = &mut self.screen {
                    c.confirm = false;
                    if !matches!(e, ClientError::Network(_)) {
                        c.key = new_key();
                    }
                    c.notice = Some(notice_for(&e));
                }
                Vec::new()
            }
        }
    }

    fn approved(&mut self, result: Result<OverrideGrant, ClientError>) -> Vec<Effect> {
        match result {
            Ok(grant) => {
                // Retry the original action with the token.
                if let Some(action) = self.approval.as_ref().map(|a| a.action.clone()) {
                    return self.protected(action, Some(grant.token));
                }
                self.busy = false;
                Vec::new()
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(e) => {
                self.busy = false;
                if let Some(a) = &mut self.approval {
                    a.pin.clear();
                    a.notice = Some(notice_for(&e));
                }
                Vec::new()
            }
        }
    }

    fn protected_done(&mut self, result: Result<ProtectedResult, ClientError>) -> Vec<Effect> {
        self.busy = false;
        let pending = self.pending.take();
        match result {
            Ok(done) => {
                self.approval = None;
                let notice = match done.approved_by {
                    Some(id) => {
                        let name = self.user_name(&id).unwrap_or(&id).to_owned();
                        Notice::new("override_ok", name, Tone::Good)
                    }
                    None => Notice::new("override_not_needed", "", Tone::Good),
                };
                self.set_notice(notice);
                Vec::new()
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(e) => match (e.code(), pending, &mut self.approval) {
                (Some("needs_override"), Some(action), None) => {
                    self.approval = Some(Approval {
                        action,
                        limit: e.problem().and_then(sultan_client::Problem::limit),
                        cursor: 0,
                        pin: Pin::default(),
                        notice: None,
                    });
                    Vec::new()
                }
                (_, _, Some(a)) => {
                    // The retry with the token failed (expired, already used, …).
                    a.notice = Some(notice_for(&e));
                    Vec::new()
                }
                _ => {
                    self.set_notice(notice_for(&e));
                    Vec::new()
                }
            },
        }
    }
}

/// Keys on the lock screen: arrows/Tab move between tiles; a digit opens the PIN pad of the
/// highlighted user and starts the PIN; in the pad, digits and Backspace edit the PIN.
fn lock_edit(l: &mut LockScreen, key: Key, users: usize) -> bool {
    if let Some(pad) = &mut l.pad {
        let used = match key {
            Key::Digit(d) => {
                pad.pin.push(d);
                true
            }
            Key::Backspace => {
                pad.pin.pop();
                true
            }
            _ => false,
        };
        // A typed digit hides "wrong PIN", but not a running lockout.
        if used && pad.notice.as_ref().is_some_and(|n| n.until.is_none()) {
            pad.notice = None;
        }
        return used;
    }
    if users == 0 {
        return false;
    }
    let column = TILE_COLUMNS.cast_signed();
    let delta = match key {
        Key::Left | Key::BackTab => -1,
        Key::Right | Key::Tab => 1,
        Key::Up => -column,
        Key::Down => column,
        Key::Digit(d) => {
            let mut pin = Pin::default();
            pin.push(d);
            l.pad = Some(Pad {
                user: l.cursor.min(users - 1),
                pin,
                notice: None,
            });
            return true;
        }
        _ => return false,
    };
    let target = l.cursor.cast_signed() + delta;
    if (0..users.cast_signed()).contains(&target) {
        l.cursor = target.cast_unsigned();
    }
    true
}

#[cfg(test)]
mod tests {
    use sultan_client::Problem;

    use super::*;

    fn api(problem: Problem) -> ClientError {
        ClientError::Api(problem)
    }

    #[test]
    fn errors_become_messages() {
        let p = |status, code| Problem::new(status, code, "x");
        #[rustfmt::skip]
        let cases = [
            (ClientError::Network("x".into()), "network", ""),
            (ClientError::Unexpected("x".into()), "error", "unexpected"),
            (api(p(401, "invalid_credentials")), "wrong_pin", ""),
            (api(p(401, "invalid_device_token")), "device_unpaired", ""),
            (api(p(401, "invalid_session")), "session_ended", ""),
            (api(p(403, "permission_denied")), "permission_denied", ""),
            (api(p(403, "self_approval")), "self_approval", ""),
            (api(p(403, "override_limit_exceeded").with("permission", "sale.discount").with("limit", 2_000)),
                "override_limit_exceeded", "20%"),
            (api(p(403, "override_limit_exceeded").with("permission", "cash.out").with("limit", 50_000)),
                "override_limit_exceeded", "Rp 50.000"),
            (api(p(403, "invalid_override")), "invalid_override", ""),
            (api(p(409, "session_already_open").with("conflict", "user")), "already_open_user", ""),
            (api(p(409, "session_already_open").with("conflict", "register")), "already_open_register", ""),
            (api(p(409, "session_closed")), "session_closed", ""),
            (api(p(409, "no_open_session")), "no_open_session", ""),
            (api(p(422, "invalid_field")), "invalid_amount", ""),
            (api(p(500, "internal")), "error", "internal"),
        ];
        for (error, code, arg) in cases {
            let notice = notice_for(&error);
            assert_eq!((notice.code, notice.arg.as_str()), (code, arg), "{error:?}");
            assert_eq!(notice.tone, Tone::Bad);
        }
        let locked = notice_for(&api(p(423, "locked_out").with("locked_until", 10_000)));
        assert_eq!(
            locked.shown(0),
            Some(("locked_out", "0:10".into(), Tone::Bad))
        );
        assert_eq!(locked.shown(10_000), None, "over");
        assert_eq!(value_text("sale.discount", None), "");
    }

    #[test]
    fn lookup_errors_name_the_code() {
        let p = |status, code| api(Problem::new(status, code, "x"));
        #[rustfmt::skip]
        let cases = [
            (p(404, "not_found"), "item_not_found", "C1"),
            (p(422, "not_sold_here"), "item_not_sold_here", "C1"),
            (p(422, "archived"), "item_archived", "C1"),
            (p(422, "not_sellable"), "item_not_sellable", "C1"),
            (p(422, "scale_value_zero"), "scale_value_zero", "C1"),
            (ClientError::Network("x".into()), "network", ""),
            (p(500, "internal"), "error", "internal"),
        ];
        for (error, code, arg) in cases {
            let notice = lookup_notice(&error, "C1");
            assert_eq!((notice.code, notice.arg.as_str()), (code, arg), "{error:?}");
        }
    }

    /// Ani at home (the fake's flow without the glue).
    fn at_home() -> Controller {
        let (mut c, _) = Controller::new();
        c.update(Msg::Users(Ok(users(1))));
        c.login = Some(Login {
            token: "t".into(),
            user_id: "u0".into(),
            display_name: "User 0".into(),
            branch_id: "b".into(),
            register_id: None,
            expires_at: 0,
        });
        c.screen = Screen::Home(HomeScreen::default());
        c
    }

    fn found(product: &str) -> Lookup {
        serde_json::from_value(serde_json::json!({
            "product_id": product, "variant_id": "v", "unit_id": "u", "name": "X", "sku": "X",
            "type": "stock", "source": "sku", "qty_thousandths": 1000
        }))
        .unwrap()
    }

    fn event(kind: &str, data: serde_json::Value) -> Msg {
        Msg::Event(
            0,
            ServerEvent {
                kind: kind.into(),
                data,
            },
        )
    }

    #[test]
    fn scans_resolve_and_catalog_events_refresh_them() {
        let mut c = at_home();
        for key in [Key::Char('x'), Key::Digit(1)] {
            assert!(c.update(Msg::Key(key)).handled);
        }
        let step = c.update(Msg::Key(Key::Enter));
        assert_eq!(
            step.effects,
            [Effect::Lookup {
                token: "t".into(),
                code: "x1".into(),
                seq: 1,
                generation: 0
            }]
        );
        assert!(!c.busy, "scanning goes on while the lookup runs");
        // An answer of an earlier login, even a 401, leaves the current login alone.
        let stale = Msg::Looked(9, 1, Err(api(Problem::new(401, "invalid_session", "x"))));
        assert!(c.update(stale).effects.is_empty());
        assert!(c.login.is_some());
        c.update(Msg::Looked(0, 1, Ok(found("p1"))));
        let Screen::Home(h) = &c.screen else {
            panic!("home")
        };
        assert_eq!(h.scan.shown.as_ref().unwrap().code, "x1");

        // Events that do not touch the shown item; one of another login.
        for msg in [
            event(
                "catalog.changed",
                serde_json::json!({"scope": "product", "product_id": "p2"}),
            ),
            event("user.changed", serde_json::Value::Null),
            Msg::Event(
                7,
                ServerEvent {
                    kind: "events.lagged".into(),
                    data: serde_json::Value::Null,
                },
            ),
        ] {
            assert!(c.update(msg).effects.is_empty());
        }
        // The shown product, the settings, a lagged socket: look it up again (one at a time).
        let step = c.update(event(
            "catalog.changed",
            serde_json::json!({"scope": "product", "product_id": "p1"}),
        ));
        assert_eq!(step.effects.len(), 1);
        assert!(
            c.update(event(
                "catalog.changed",
                serde_json::json!({"scope": "settings"})
            ))
            .effects
            .is_empty(),
            "one is already waiting"
        );
        c.update(Msg::Looked(
            0,
            2,
            Err(api(Problem::new(422, "archived", "x"))),
        ));
        let step = c.update(event("events.lagged", serde_json::Value::Null));
        assert_eq!(step.effects.len(), 1);
        // A late answer of an older lookup is ignored.
        c.update(Msg::Looked(0, 2, Ok(found("p1"))));
        let Screen::Home(h) = &c.screen else {
            panic!("home")
        };
        assert_eq!(
            h.scan
                .shown
                .as_ref()
                .unwrap()
                .result
                .as_ref()
                .unwrap_err()
                .code,
            "item_archived"
        );

        // Not on the home screen: events and answers change nothing.
        c.screen = Screen::Lock(LockScreen::default());
        assert!(
            c.update(event("events.lagged", serde_json::Value::Null))
                .effects
                .is_empty()
        );
        assert!(
            c.update(Msg::Looked(0, 3, Ok(found("p1"))))
                .effects
                .is_empty()
        );
        // Nothing shown at home: nothing to refresh.
        c.screen = Screen::Home(HomeScreen::default());
        assert!(
            c.update(event("events.lagged", serde_json::Value::Null))
                .effects
                .is_empty()
        );
    }

    #[test]
    fn customer_prices_are_quoted_for_the_shown_item() {
        let mut c = at_home();
        c.customer = Some(Customer {
            id: "cu".into(),
            ..Customer::default()
        });
        c.update(Msg::Key(Key::Char('x')));
        c.update(Msg::Key(Key::Enter));
        let step = c.update(Msg::Looked(0, 1, Ok(found("p1"))));
        let [Effect::Quote { request, seq, .. }] = step.effects.as_slice() else {
            panic!("{:?}", step.effects)
        };
        assert_eq!((request.customer_id.as_deref(), *seq), (Some("cu"), 1));
        // A quote of another login is ignored; an empty answer is an unexpected error.
        let quote = |lines: Vec<sultan_client::QuotedLine>| Quote {
            branch_id: "b".into(),
            date: "2027-01-15".into(),
            customer_id: Some("cu".into()),
            tax_inclusive: true,
            tax_registered: true,
            cash_rounding: "none".into(),
            lines,
            totals: sultan_client::QuoteTotals::default(),
        };
        assert!(
            c.update(Msg::Quoted(5, 1, Ok(quote(Vec::new()))))
                .effects
                .is_empty()
        );
        c.update(Msg::Quoted(0, 1, Ok(quote(Vec::new()))));
        let Screen::Home(h) = &c.screen else {
            panic!("home")
        };
        assert_eq!(
            h.scan.shown.as_ref().unwrap().customer,
            crate::scan::CustomerPrice::Failed(Notice::new("error", "unexpected", Tone::Bad))
        );
        // `price.changed` for the shown variant (or every price) looks it up again; another
        // variant's does not.
        let other = event(
            "price.changed",
            serde_json::json!({"scope": "entry", "variant_ids": ["w"]}),
        );
        assert!(c.update(other).effects.is_empty());
        let mine = event(
            "price.changed",
            serde_json::json!({"scope": "entry", "variant_ids": ["v"]}),
        );
        assert_eq!(c.update(mine).effects.len(), 1);
        // Not at home: nothing to quote.
        c.screen = Screen::Lock(LockScreen::default());
        assert!(c.quote_shown().is_empty());
        assert!(
            c.update(Msg::Quoted(0, 1, Ok(quote(Vec::new()))))
                .effects
                .is_empty()
        );
    }

    #[test]
    fn customer_answers_of_another_login_or_an_ended_login() {
        let mut c = at_home();
        let keys = vec!["customer.search".to_owned()];
        c.update(Msg::Customer(0, Box::new(Reply::Permissions(Ok(keys)))));
        assert!(c.perms.search);
        // An answer of an earlier login is ignored.
        c.update(Msg::Customer(
            9,
            Box::new(Reply::Permissions(Ok(Vec::new()))),
        ));
        assert!(c.perms.search);
        // Without the keys, only the lookup is offered.
        let lost = ClientError::Network("x".into());
        c.update(Msg::Customer(0, Box::new(Reply::Permissions(Err(lost)))));
        assert_eq!(c.perms, Perms::default());
        // The dialog was closed meanwhile: the answer only ends the wait.
        c.busy = true;
        c.update(Msg::Customer(
            0,
            Box::new(Reply::Found(Ok(Customer::default()))),
        ));
        assert!(!c.busy && c.dialog.is_none());
        for msg in [Msg::PickCustomer(0), Msg::PickField(1)] {
            assert_eq!(c.update(msg), Step::default());
        }
        // A payout of a customer who is not attached leaves the attached one alone.
        c.customer = Some(Customer {
            id: "other".into(),
            balance: 5,
            ..Customer::default()
        });
        let mut d = Dialog::new(Some(&Customer {
            id: "shown".into(),
            ..Customer::default()
        }));
        d.page = crate::customer::Page::Payout;
        c.dialog = Some(d);
        let paid = sultan_client::Payout::default();
        c.update(Msg::Customer(0, Box::new(Reply::PaidOut(Ok(paid)))));
        assert_eq!(c.customer.as_ref().map(|x| x.balance), Some(5));
        // The login ended.
        let ended = api(Problem::new(401, "invalid_session", "x"));
        c.update(Msg::Customer(0, Box::new(Reply::Found(Err(ended)))));
        assert_eq!(c.screen.name(), "lock");
        assert!(c.customer.is_none() && c.dialog.is_none());
    }

    #[test]
    fn tones_and_ids() {
        let tones = [
            (5, Tone::Good, 1),
            (-5, Tone::Bad, -1),
            (0, Tone::Neutral, 0),
        ];
        for (amount, tone, value) in tones {
            assert_eq!(Tone::of_amount(amount), tone);
            assert_eq!(tone.value(), value);
        }
        for action in Action::ALL {
            assert_eq!(Action::from_id(action.id()), Some(action));
        }
        assert_eq!(Action::from_id("nope"), None);
    }

    fn users(n: usize) -> Vec<LockUser> {
        (0..n)
            .map(|i| LockUser {
                id: format!("u{i}"),
                display_name: format!("User {i}"),
            })
            .collect()
    }

    #[test]
    fn lock_screen_arrows_stay_on_the_grid() {
        let mut l = LockScreen::default();
        assert!(
            !lock_edit(&mut l, Key::Right, 0),
            "no users, nothing to move"
        );
        #[rustfmt::skip]
        let cases = [
            (Key::Right, 1), (Key::Down, 5), (Key::Down, 5), (Key::Right, 6), (Key::Right, 6),
            (Key::Up, 2), (Key::Up, 2), (Key::Left, 1), (Key::BackTab, 0), (Key::Left, 0),
            (Key::Tab, 1),
        ];
        for (key, cursor) in cases {
            assert!(lock_edit(&mut l, key, 7));
            assert_eq!(l.cursor, cursor, "{key:?}");
        }
        assert!(!lock_edit(&mut l, Key::Escape, 7));
        assert!(lock_edit(&mut l, Key::Digit(4), 7));
        let pad = l.pad.as_ref().unwrap();
        assert_eq!((pad.user, pad.pin.len()), (1, 1));
    }

    #[test]
    fn stale_results_change_nothing() {
        let (mut c, _) = Controller::new();
        c.update(Msg::Users(Ok(users(2))));
        // A current-session answer without a login (it logged out meanwhile).
        assert!(c.update(Msg::Current(Ok(None))).effects.is_empty());
        assert_eq!(c.screen.name(), "lock");
        // Approval answers without a dialog.
        c.busy = true;
        let grant = OverrideGrant {
            token: "t".into(),
            action: "a".into(),
            value: None,
            context_ref: "c".into(),
            requester_id: "r".into(),
            approver_id: "u1".into(),
            expires_at: 0,
        };
        assert!(c.update(Msg::Approved(Ok(grant))).effects.is_empty());
        assert!(!c.busy);
        c.update(Msg::Approved(Err(ClientError::Network("x".into()))));
        // Results for screens that are gone.
        for msg in [
            Msg::Opened(Err(api(Problem::new(409, "session_already_open", "x")))),
            Msg::Closed(Err(ClientError::Network("x".into()))),
            Msg::Branch(Err(ClientError::Network("x".into()))),
            Msg::LoggedOut(Ok(())),
            Msg::PickApprover(0),
            Msg::PickRow(3),
            Msg::PickUser(9),
        ] {
            assert!(c.update(msg).effects.is_empty());
        }
        assert_eq!(c.screen.name(), "lock");
        // A login error while the pad is closed.
        c.update(Msg::LoggedIn(Err(ClientError::Network("x".into()))));
        assert_eq!(c.screen, Screen::Lock(LockScreen::default()));
        // Keys nobody uses.
        assert!(!c.update(Msg::Key(Key::F(7))).handled);
        // While a call runs, input is swallowed.
        c.busy = true;
        assert_eq!(c.update(Msg::Key(Key::Digit(1))), Step::handled(Vec::new()));
        assert!(!c.update(Msg::Key(Key::Other)).handled);
        assert!(c.update(Msg::Action(Action::Retry)).effects.is_empty());
        c.update(Msg::PickUser(0));
        assert_eq!(c.screen, Screen::Lock(LockScreen::default()));
        c.busy = false;
        // Actions that are not on this screen.
        assert!(
            c.update(Msg::Action(Action::CloseSession))
                .effects
                .is_empty()
        );
        assert_eq!(c.screen.name(), "lock");
    }
}
