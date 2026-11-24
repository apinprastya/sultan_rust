//! Cash management at the register (spec 2.7 §9, phase K4), as more of [`Controller`]'s state
//! machine.
//!
//! - **Cash dialog** (Shift+F5, the sell screen with or without a cart): ←/→ or I / O / D pick
//!   cash in / out / a safe drop, ↑/↓ the reason (of that direction; a drop has none), digits type
//!   the amount, Tab moves to the note and on to the list of this session's movements. Enter
//!   asks "post …?", Enter again posts it (`PUT /cash-movements/{id}`, a client `UUIDv7` kept for a
//!   retry). `403 needs_override` (a cash out above the cashier's limit, a drop by a cashier)
//!   opens the manager-override dialog (`context_ref` = the movement ID, the value the amount for
//!   `cash.out`). After posting, the slip text is shown in monospace (`GET …/slip`, until 2.11
//!   prints it); Enter or Esc goes back to a fresh form. On the list, R reverses the highlighted
//!   movement (the note is asked for; the reversal takes the original's kind, amount and reason)
//!   and P reprints its slip (COPY). The slip panel is where the nota-photo keys of phase K6 go.
//! - **X report** (Shift+F12): `GET /cashier-sessions/{id}/x-report/text` for the caller's open
//!   session; a cashier without `session.view_expected` gets the manager PIN dialog
//!   (`context_ref` = the session ID). Esc goes back to the sell screen with the cart kept.
//! - **Float** (§3): the open screen starts the opening cash at `GET /registers/me/float` (the
//!   first digit typed replaces it); the blind close asks for the cash kept as float once the
//!   count is confirmed (default the opening cash, at most the counted cash) and sends
//!   `float_kept`.
//! - **Close report** ([`ReportScreen`]): the figures, the printable close report
//!   (`GET /cashier-sessions/{id}/report/text`) and **End of day (Z)**: offered when
//!   `GET /z-reports/pending` lists no other open session of the session's business day (or
//!   when the user may not read that list, a cashier: then the run's `409 sessions_open` says
//!   which sessions are open). Z asks first, then runs `POST /z-reports` for the business day
//!   (`day.close`, a manager PIN for a cashier, `context_ref` = `<branch_id>:<day>`) and shows the
//!   Z text. `nothing_to_close` and `sessions_open` are shown as messages.
//! - **Payout slip**: after a customer balance payout (F3 → F7) the customer dialog shows the
//!   payout slip (`GET /customers/{id}/balance/payouts/{entry_id}/slip`).

use chrono::TimeZone;
use sultan_client::{
    CashMovement, CashMovementPage, CashMovementRequest, CashQuery, CashReason, CashText,
    ClientError, CloseReport, PendingDay, ProposedFloat, ProtectedAction, ZReport, ZRunRequest,
};

use crate::amount::{Digits, rupiah};
use crate::controller::{
    Action, Approval, Controller, Effect, Notice, Screen, Shortcut, Tone, ends_login, new_key,
    notice_for,
};
use crate::input::{Key, Pin};
use crate::till::{Modal, Retry};

/// Longest note of a cash movement (spec 2.7 §2).
pub const MAX_NOTE: usize = 200;

/// The kind of a cash movement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Kind {
    /// Cash into the drawer.
    #[default]
    In,
    /// Cash out of the drawer (an expense).
    Out,
    /// A safe drop.
    Drop,
}

impl Kind {
    /// In / out / drop, in the order ←/→ cycle through them.
    pub const ALL: [Self; 3] = [Self::In, Self::Out, Self::Drop];

    /// The API key (`in`, `out`, `drop`), also the UI code.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::In => "in",
            Self::Out => "out",
            Self::Drop => "drop",
        }
    }

    /// The kind of an API key.
    #[must_use]
    pub fn of(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.key() == key)
    }

    fn step(self, delta: isize) -> Self {
        let at = Self::ALL.iter().position(|k| *k == self).unwrap_or(0);
        Self::ALL[crate::count::wrap(at, delta, Self::ALL.len())]
    }
}

/// Where the keyboard is in the cash dialog's form (Tab cycles).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Focus {
    /// The amount (digits; I / O / D pick the kind).
    #[default]
    Amount,
    /// The note (any text).
    Note,
    /// The list of this session's movements (R reverse, P reprint).
    List,
}

/// The cash dialog's step.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Step {
    /// Kind, reason, amount, note and the list.
    #[default]
    Form,
    /// "Post …?" (Enter again posts).
    Confirm,
    /// The note of a reversal of the highlighted movement.
    Reverse,
    /// The slip of the movement just posted, or a reprint.
    Slip,
}

/// The cash dialog (Shift+F5); see the module docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CashDialog {
    /// In / out / drop.
    pub kind: Kind,
    /// The keyboard's place.
    pub focus: Focus,
    /// The step.
    pub step: Step,
    /// The active reasons, both directions (`GET /cash-reasons`).
    pub reasons: Vec<CashReason>,
    /// The highlighted reason among [`CashDialog::shown_reasons`].
    pub reason: usize,
    /// The amount.
    pub amount: Digits,
    /// The note.
    pub note: String,
    /// This session's movements, newest first.
    pub movements: Vec<CashMovement>,
    /// The highlighted movement.
    pub cursor: usize,
    /// The reversal's note.
    pub reverse_note: String,
    /// The slip on screen.
    pub slip: Vec<String>,
    /// The number of the movement whose slip is shown.
    pub slip_number: String,
    /// The ID of the next movement (kept until it is posted, so a retry is a replay).
    pub id: String,
}

impl CashDialog {
    fn new() -> Self {
        Self {
            amount: Digits::amount(),
            id: new_key(),
            ..Self::default()
        }
    }

    /// The reasons of the kind's direction, in the server's order (none for a drop).
    #[must_use]
    pub fn shown_reasons(&self) -> Vec<&CashReason> {
        if self.kind == Kind::Drop {
            return Vec::new();
        }
        self.reasons
            .iter()
            .filter(|r| r.direction == self.kind.key())
            .collect()
    }

    fn selected_reason(&self) -> Option<&CashReason> {
        self.shown_reasons().get(self.reason).copied()
    }

    fn set_kind(&mut self, kind: Kind) {
        if self.kind != kind {
            self.kind = kind;
            self.reason = 0;
        }
    }

    /// The movement to post, or why it cannot be posted yet.
    ///
    /// # Errors
    /// `invalid_amount` (nothing typed), `pick_reason` (in / out without a reason) or
    /// `note_required` (the reason asks for one).
    pub fn request(&self) -> Result<CashMovementRequest, Notice> {
        if self.amount.value() <= 0 {
            return Err(Notice::bad("invalid_amount"));
        }
        let reason = self.selected_reason();
        if self.kind != Kind::Drop && reason.is_none() {
            return Err(Notice::bad("pick_reason"));
        }
        let note = self.note.trim();
        if note.is_empty() && reason.is_some_and(|r| r.note_required) {
            return Err(Notice::bad("note_required"));
        }
        Ok(CashMovementRequest {
            kind: Some(self.kind.key().to_owned()),
            amount: Some(self.amount.value()),
            reason_id: reason.map(|r| r.id.clone()),
            note: (!note.is_empty()).then(|| note.to_owned()),
            reverses: None,
        })
    }

    /// The highlighted movement's reversal, or why it cannot be reversed.
    ///
    /// # Errors
    /// `cannot_reverse` (a reversal itself), `already_reversed` or `note_required`.
    pub fn reversal(&self) -> Result<(&CashMovement, CashMovementRequest), Notice> {
        let Some(target) = self.movements.get(self.cursor) else {
            return Err(Notice::bad("cannot_reverse"));
        };
        if target.reverses.is_some() {
            return Err(Notice::bad("cannot_reverse"));
        }
        if target.reversed_by.is_some() {
            return Err(Notice::bad("already_reversed"));
        }
        let note = self.reverse_note.trim();
        if note.is_empty() {
            return Err(Notice::bad("note_required"));
        }
        Ok((
            target,
            CashMovementRequest {
                note: Some(note.to_owned()),
                reverses: Some(target.id.clone()),
                ..CashMovementRequest::default()
            },
        ))
    }

    /// Back to an empty form for the next movement (the kind stays).
    fn fresh(&mut self) {
        self.step = Step::Form;
        self.focus = Focus::Amount;
        self.amount.clear();
        self.note.clear();
        self.reverse_note.clear();
        self.slip.clear();
        self.slip_number.clear();
        self.id = new_key();
    }

    /// Types `key` into the dialog. Returns whether it was used.
    fn edit(&mut self, key: Key) -> bool {
        let typed = match key {
            Key::Digit(d) => Some(char::from(b'0' + d)),
            Key::Char(c) => Some(c),
            _ => None,
        };
        let text = |t: &mut String| match (key, typed) {
            (_, Some(c)) if t.chars().count() < MAX_NOTE => {
                t.push(c);
                true
            }
            (_, Some(_)) => true,
            (Key::Backspace, _) => {
                t.pop();
                true
            }
            _ => false,
        };
        match self.step {
            Step::Reverse => text(&mut self.reverse_note),
            Step::Confirm | Step::Slip => false,
            Step::Form => {
                let reasons = self.shown_reasons().len();
                match (self.focus, key) {
                    (Focus::List, Key::Up) => {
                        self.cursor = crate::count::wrap(self.cursor, -1, self.movements.len());
                        true
                    }
                    (Focus::List, Key::Down) => {
                        self.cursor = crate::count::wrap(self.cursor, 1, self.movements.len());
                        true
                    }
                    (Focus::List, _) => false,
                    (_, Key::Up) => {
                        self.reason = crate::count::wrap(self.reason, -1, reasons);
                        true
                    }
                    (_, Key::Down) => {
                        self.reason = crate::count::wrap(self.reason, 1, reasons);
                        true
                    }
                    (_, Key::Left) => {
                        self.set_kind(self.kind.step(-1));
                        true
                    }
                    (_, Key::Right) => {
                        self.set_kind(self.kind.step(1));
                        true
                    }
                    (Focus::Amount, Key::Digit(d)) => {
                        self.amount.push(d);
                        true
                    }
                    (Focus::Amount, Key::Backspace) => {
                        self.amount.pop();
                        true
                    }
                    (Focus::Note, _) => text(&mut self.note),
                    _ => false,
                }
            }
        }
    }
}

/// The X report on screen (Shift+F12).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct XView {
    /// The text.
    pub lines: Vec<String>,
}

/// Where End of day (Z) is on the close report.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum ZStep {
    /// Not offered (yet, or another session of the day is open).
    #[default]
    Hidden,
    /// The Z key is offered.
    Offered,
    /// "Run the Z of …?".
    Confirm,
    /// Run: the Z (its text replaces the close report's).
    Done(Box<ZReport>),
}

/// The close report screen: the figures, the printable report and End of day (Z).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportScreen {
    /// The stored close report.
    pub report: CloseReport,
    /// The close report's text, then the Z's.
    pub text: Vec<String>,
    /// End of day (Z).
    pub z: ZStep,
    /// Messages (`sessions_open`, `nothing_to_close`, …).
    pub notice: Option<Notice>,
    key: String,
}

impl ReportScreen {
    /// The screen of a close report just received.
    #[must_use]
    pub fn new(report: CloseReport) -> Self {
        Self {
            report,
            text: Vec::new(),
            z: ZStep::Hidden,
            notice: None,
            key: new_key(),
        }
    }

    /// The business day of the closed session (`YYYY-MM-DD`).
    #[must_use]
    pub fn day(&self) -> String {
        business_day(&self.report.session.number, self.report.session.opened_at)
    }
}

/// The business day of a session (spec 2.7 K14: the date in its number,
/// `JKT01-R1-S-270115-01` → `2027-01-15`), or the local date of its open time.
#[must_use]
pub fn business_day(number: &str, opened_at: i64) -> String {
    let date = number
        .split('-')
        .rev()
        .nth(1)
        .filter(|p| p.len() == 6 && p.bytes().all(|b| b.is_ascii_digit()));
    if let Some(d) = date {
        return format!("20{}-{}-{}", &d[..2], &d[2..4], &d[4..]);
    }
    chrono::Local
        .timestamp_millis_opt(opened_at)
        .single()
        .map(|t| t.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

/// A service call of this module; the answer comes back as [`Reply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `GET /cash-reasons`.
    Reasons,
    /// `GET /cash-movements?cashier_session_id=`.
    Movements(String),
    /// `PUT /cash-movements/{id}`.
    Post {
        /// The movement ID.
        id: String,
        /// The body.
        body: CashMovementRequest,
        /// `X-Override`.
        override_token: Option<String>,
    },
    /// `GET /cash-movements/{id}/slip` in the window language.
    Slip {
        /// The movement.
        id: String,
        /// A reprint (COPY).
        copy: bool,
        /// `X-Override` (`sale.reprint`).
        override_token: Option<String>,
    },
    /// `GET /cashier-sessions/{id}/x-report/text`.
    XText {
        /// The session.
        session_id: String,
        /// `X-Override` (`session.view_expected`).
        override_token: Option<String>,
    },
    /// `GET /cashier-sessions/{id}/report/text`.
    CloseText(String),
    /// `GET /z-reports/pending` (the register's branch).
    Pending,
    /// `POST /z-reports`.
    RunZ {
        /// The business day.
        day: String,
        /// Idempotency key.
        key: String,
        /// `X-Override` (`day.close`).
        override_token: Option<String>,
    },
    /// `GET /z-reports/{id}/text`.
    ZText(String),
    /// `GET /registers/me/float`.
    Float,
    /// `GET /customers/{id}/balance/payouts/{entry_id}/slip`.
    PayoutSlip {
        /// The customer.
        customer_id: String,
        /// The payout's balance entry.
        entry_id: String,
    },
}

/// The answer to a [`Call`].
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// [`Call::Reasons`].
    Reasons(Result<Vec<CashReason>, ClientError>),
    /// [`Call::Movements`].
    Movements(Result<CashMovementPage, ClientError>),
    /// [`Call::Post`].
    Posted(Result<Box<CashMovement>, ClientError>),
    /// [`Call::Slip`], with `copy`.
    Slip(bool, Result<CashText, ClientError>),
    /// [`Call::XText`].
    X(Result<CashText, ClientError>),
    /// [`Call::CloseText`].
    CloseText(Result<CashText, ClientError>),
    /// [`Call::Pending`].
    Pending(Result<Vec<PendingDay>, ClientError>),
    /// [`Call::RunZ`].
    Z(Result<Box<ZReport>, ClientError>),
    /// [`Call::ZText`].
    ZText(Result<CashText, ClientError>),
    /// [`Call::Float`].
    Float(Result<ProposedFloat, ClientError>),
    /// [`Call::PayoutSlip`], with the id of the customer who was paid: the slip is only shown
    /// while the dialog still shows that customer.
    PayoutSlip(String, Result<CashText, ClientError>),
}

impl Reply {
    fn error(&self) -> Option<&ClientError> {
        match self {
            Self::Reasons(r) => r.as_ref().err(),
            Self::Movements(r) => r.as_ref().err(),
            Self::Posted(r) => r.as_ref().err(),
            Self::Slip(_, r)
            | Self::PayoutSlip(_, r)
            | Self::X(r)
            | Self::CloseText(r)
            | Self::ZText(r) => r.as_ref().err(),
            Self::Pending(r) => r.as_ref().err(),
            Self::Z(r) => r.as_ref().err(),
            Self::Float(r) => r.as_ref().err(),
        }
    }
}

/// What an approved override retries in this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Waiting {
    /// Posting a movement of this amount.
    Post {
        /// The movement ID.
        id: String,
        /// The body.
        body: CashMovementRequest,
        /// The amount (the value of a `cash.out` override).
        amount: i64,
    },
    /// A reprint of this movement's slip.
    Slip(String),
    /// The X report.
    X,
    /// The Z of this day.
    Z {
        /// The business day.
        day: String,
        /// Idempotency key.
        key: String,
    },
}

/// The message for a failed call of this module (spec 2.7 error codes).
#[must_use]
pub fn cash_notice(error: &ClientError) -> Notice {
    match (error.code(), error.problem().and_then(|p| p.field())) {
        (Some("invalid_field"), Some("note")) => Notice::bad("note_required"),
        (Some("invalid_field"), Some("reason")) => Notice::bad("pick_reason"),
        (Some("invalid_field"), Some("reverses")) => Notice::bad("cannot_reverse"),
        (Some("invalid_field"), Some("float_kept")) => Notice::bad("float_above_count"),
        _ => notice_for(error),
    }
}

impl Controller {
    fn cash_modal(&self) -> Option<&Modal> {
        self.sell().and_then(|s| s.modal.as_ref())
    }

    fn cash_modal_mut(&mut self) -> Option<&mut Modal> {
        match &mut self.screen {
            Screen::Home(h) => h.sell.modal.as_mut(),
            _ => None,
        }
    }

    fn dialog_mut(&mut self) -> Option<&mut CashDialog> {
        match self.cash_modal_mut() {
            Some(Modal::Cash(d)) => Some(d),
            _ => None,
        }
    }

    fn report_mut(&mut self) -> Option<&mut ReportScreen> {
        match &mut self.screen {
            Screen::Report(r) => Some(r),
            _ => None,
        }
    }

    pub(crate) fn cash_call(&mut self, call: Call) -> Vec<Effect> {
        self.busy = true;
        self.cash_quiet(call)
    }

    /// A call that does not block input (lists, texts).
    pub(crate) fn cash_quiet(&self, call: Call) -> Vec<Effect> {
        vec![Effect::Cash {
            token: self.token(),
            call,
            generation: self.generation,
        }]
    }

    /// Whether the cash dialog or the X report is open.
    pub(crate) fn in_cash(&self) -> bool {
        matches!(self.cash_modal(), Some(Modal::Cash(_) | Modal::XReport(_)))
    }

    /// The keys of the cash dialog and the X report.
    pub(crate) fn cash_shortcuts(&self) -> Option<Vec<Shortcut>> {
        use Action::{Back, Confirm};
        let s = |key, action, label| Shortcut { key, action, label };
        let d = match self.cash_modal()? {
            Modal::XReport(_) => return Some(vec![s(Key::Escape, Back, "close")]),
            Modal::Cash(d) => d,
            _ => return None,
        };
        Some(match d.step {
            Step::Confirm => vec![
                s(Key::Enter, Confirm, "post_cash"),
                s(Key::Escape, Back, "edit_amount"),
            ],
            Step::Reverse => vec![
                s(Key::Enter, Confirm, "reverse"),
                s(Key::Escape, Back, "back"),
            ],
            Step::Slip => vec![
                s(Key::Enter, Confirm, "done"),
                s(Key::Escape, Confirm, "done"),
                s(Key::Char('p'), Action::Reprint, "reprint"),
            ],
            Step::Form if d.focus == Focus::List => {
                let mut keys = Vec::new();
                if !d.movements.is_empty() {
                    keys.push(s(Key::Char('r'), Action::Reverse, "reverse"));
                    keys.push(s(Key::Char('p'), Action::Reprint, "reprint"));
                }
                keys.push(s(Key::Tab, Action::CashTab, "to_form"));
                keys.push(s(Key::Escape, Back, "close"));
                keys
            }
            Step::Form => {
                let mut keys = vec![s(Key::Enter, Confirm, "ok")];
                if d.focus == Focus::Amount {
                    keys.push(s(Key::Char('i'), Action::CashIn, "cash_in"));
                    keys.push(s(Key::Char('o'), Action::CashOut, "cash_out"));
                    keys.push(s(Key::Char('d'), Action::CashDrop, "cash_drop"));
                }
                keys.push(s(
                    Key::Tab,
                    Action::CashTab,
                    if d.focus == Focus::Amount {
                        "to_note"
                    } else {
                        "to_list"
                    },
                ));
                keys.push(s(Key::Escape, Back, "close"));
                keys
            }
        })
    }

    /// Typing in the cash dialog. `None` when it is not open.
    pub(crate) fn cash_edit(&mut self, key: Key) -> Option<bool> {
        match self.cash_modal_mut()? {
            Modal::Cash(d) => {
                let used = d.edit(key);
                if used {
                    self.clear_home_notice();
                }
                Some(used)
            }
            Modal::XReport(_) => Some(false),
            _ => None,
        }
    }

    fn clear_home_notice(&mut self) {
        if let Screen::Home(h) = &mut self.screen {
            h.notice = None;
        }
    }

    /// A click on a row of the cash dialog: `reason` or `movement`.
    pub(crate) fn cash_pick(&mut self, kind: &str, index: usize) {
        let Some(d) = self.dialog_mut().filter(|d| d.step == Step::Form) else {
            return;
        };
        match kind {
            "reason" if index < d.shown_reasons().len() => d.reason = index,
            "movement" if index < d.movements.len() => {
                d.cursor = index;
                d.focus = Focus::List;
            }
            "kind" => {
                if let Some(k) = Kind::ALL.get(index) {
                    d.set_kind(*k);
                }
            }
            "field" => {
                d.focus = if index == 0 {
                    Focus::Amount
                } else {
                    Focus::Note
                };
            }
            _ => {}
        }
    }

    /// Shift+F5: the cash dialog, with the reasons and this session's movements.
    pub(crate) fn open_cash(&mut self) -> Vec<Effect> {
        let Some(session) = self.session.as_ref().map(|s| s.id.clone()) else {
            return Vec::new();
        };
        if let Screen::Home(h) = &mut self.screen {
            h.sell.modal = Some(Modal::Cash(Box::new(CashDialog::new())));
            h.sell.focus = None;
            h.notice = None;
        }
        let mut effects = self.cash_quiet(Call::Reasons);
        effects.extend(self.cash_quiet(Call::Movements(session)));
        effects
    }

    /// Shift+F12: the X report of the caller's open session.
    pub(crate) fn open_x(&mut self) -> Vec<Effect> {
        let Some(session_id) = self.session.as_ref().map(|s| s.id.clone()) else {
            return Vec::new();
        };
        self.cash_wait = Some(Waiting::X);
        self.cash_call(Call::XText {
            session_id,
            override_token: None,
        })
    }

    /// An action of the cash dialog or the X report (the caller checked it is on screen).
    pub(crate) fn cash_action(&mut self, action: Action) -> Vec<Effect> {
        if matches!(self.cash_modal(), Some(Modal::XReport(_))) {
            self.close_cash_modal();
            return Vec::new();
        }
        let Some(d) = self.dialog_mut() else {
            return Vec::new();
        };
        match action {
            Action::CashIn => d.set_kind(Kind::In),
            Action::CashOut => d.set_kind(Kind::Out),
            Action::CashDrop => d.set_kind(Kind::Drop),
            Action::CashTab => {
                d.focus = match d.focus {
                    Focus::Amount => Focus::Note,
                    Focus::Note => Focus::List,
                    Focus::List => Focus::Amount,
                };
            }
            Action::Reverse => {
                // The note is asked next; only the other refusals stop here.
                d.reverse_note.clear();
                let refused = d.reversal().err().filter(|n| n.code != "note_required");
                if let Some(n) = refused {
                    self.set_notice(n);
                    return Vec::new();
                }
                if let Some(d) = self.dialog_mut() {
                    d.step = Step::Reverse;
                }
            }
            Action::Reprint => {
                let id = match d.step {
                    Step::Slip => d
                        .movements
                        .iter()
                        .find(|m| m.number == d.slip_number)
                        .map(|m| m.id.clone()),
                    _ => d.movements.get(d.cursor).map(|m| m.id.clone()),
                };
                let Some(id) = id else {
                    return Vec::new();
                };
                self.cash_wait = Some(Waiting::Slip(id.clone()));
                return self.cash_call(Call::Slip {
                    id,
                    copy: true,
                    override_token: None,
                });
            }
            Action::Confirm => return self.cash_confirm(),
            Action::Back => self.cash_back(),
            _ => {}
        }
        Vec::new()
    }

    fn close_cash_modal(&mut self) {
        if let Screen::Home(h) = &mut self.screen {
            h.sell.modal = None;
        }
    }

    fn cash_back(&mut self) {
        let Some(d) = self.dialog_mut() else {
            return;
        };
        match d.step {
            Step::Confirm => d.step = Step::Form,
            Step::Reverse => {
                d.step = Step::Form;
                d.reverse_note.clear();
            }
            Step::Slip => d.fresh(),
            Step::Form => self.close_cash_modal(),
        }
    }

    fn cash_confirm(&mut self) -> Vec<Effect> {
        let Some(d) = self.dialog_mut() else {
            return Vec::new();
        };
        match d.step {
            Step::Form => match d.request() {
                Ok(_) => {
                    d.step = Step::Confirm;
                    Vec::new()
                }
                Err(n) => {
                    self.set_notice(n);
                    Vec::new()
                }
            },
            Step::Confirm => match d.request() {
                Ok(body) => {
                    let id = d.id.clone();
                    let amount = d.amount.value();
                    self.post_movement(id, body, amount)
                }
                Err(n) => {
                    d.step = Step::Form;
                    self.set_notice(n);
                    Vec::new()
                }
            },
            Step::Reverse => match d.reversal() {
                Ok((target, body)) => {
                    let amount = target.amount;
                    let id = d.id.clone();
                    self.post_movement(id, body, amount)
                }
                Err(n) => {
                    self.set_notice(n);
                    Vec::new()
                }
            },
            Step::Slip => {
                d.fresh();
                Vec::new()
            }
        }
    }

    fn post_movement(&mut self, id: String, body: CashMovementRequest, amount: i64) -> Vec<Effect> {
        self.cash_wait = Some(Waiting::Post {
            id: id.clone(),
            body: body.clone(),
            amount,
        });
        self.cash_call(Call::Post {
            id,
            body,
            override_token: None,
        })
    }

    /// A manager approved: the waiting call again with the token.
    pub(crate) fn cash_retry(&mut self, token: String) -> Vec<Effect> {
        let Some(waiting) = self.cash_wait.clone() else {
            self.busy = false;
            return Vec::new();
        };
        let t = Some(token);
        match waiting {
            Waiting::Post { id, body, .. } => self.cash_call(Call::Post {
                id,
                body,
                override_token: t,
            }),
            Waiting::Slip(id) => self.cash_call(Call::Slip {
                id,
                copy: true,
                override_token: t,
            }),
            Waiting::X => {
                let session_id = self
                    .session
                    .as_ref()
                    .map(|s| s.id.clone())
                    .unwrap_or_default();
                self.cash_call(Call::XText {
                    session_id,
                    override_token: t,
                })
            }
            Waiting::Z { day, key } => self.cash_call(Call::RunZ {
                day,
                key,
                override_token: t,
            }),
        }
    }

    /// `403 needs_override` for the waiting call: the override dialog.
    fn cash_needs_override(&mut self, e: &ClientError) {
        let problem = e.problem();
        let permission = problem
            .and_then(sultan_client::Problem::permission)
            .unwrap_or_default()
            .to_owned();
        let limit = problem.and_then(sultan_client::Problem::limit);
        let session = self
            .session
            .as_ref()
            .map(|s| s.id.clone())
            .unwrap_or_default();
        let (context_ref, value) = match self.cash_wait.clone() {
            Some(Waiting::Post { id, amount, .. }) => (
                id,
                (permission == sultan_client::cash::CASH_OUT).then_some(amount),
            ),
            Some(Waiting::Slip(id)) => (id, None),
            Some(Waiting::X) => (session, None),
            Some(Waiting::Z { day, .. }) => {
                let branch = match &self.screen {
                    Screen::Report(r) => r.report.session.branch_id.clone(),
                    _ => self
                        .login
                        .as_ref()
                        .map(|l| l.branch_id.clone())
                        .unwrap_or_default(),
                };
                (format!("{branch}:{day}"), None)
            }
            None => return,
        };
        self.approval = Some(Approval {
            action: ProtectedAction {
                action: permission,
                value,
                context_ref,
            },
            limit,
            cursor: 0,
            pin: Pin::default(),
            notice: None,
        });
        self.retry = Some(Retry::Cash);
    }

    /// The waiting call is done (or refused): the override dialog closes.
    fn cash_settled(&mut self) {
        self.cash_wait = None;
        if self.retry == Some(Retry::Cash) {
            self.approval = None;
            self.retry = None;
        }
    }

    /// The close report screen's message slot.
    fn report_notice(&mut self, notice: Notice) {
        if let Some(r) = self.report_mut() {
            r.notice = Some(notice);
        }
    }

    /// The answer to a call of this module.
    #[allow(clippy::too_many_lines)] // one arm per answer
    pub(crate) fn cash_reply(&mut self, generation: u64, reply: Reply) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        if let Some(e) = reply.error().filter(|e| ends_login(e)).cloned() {
            return self.fail(&e);
        }
        let waits = matches!(
            reply,
            Reply::Posted(_) | Reply::X(_) | Reply::Z(_) | Reply::Slip(true, _)
        );
        if waits {
            self.busy = false;
        }
        if waits
            && let Some(e) = reply.error()
            && e.code() == Some("needs_override")
            && self.approval.is_none()
            && self.cash_wait.is_some()
        {
            let e = e.clone();
            self.cash_needs_override(&e);
            return Vec::new();
        }
        if waits
            && let Some(e) = reply.error()
            && e.code() == Some("invalid_override")
            && let Some(a) = self.approval.as_mut()
        {
            // A refused token stays in the dialog.
            a.pin.clear();
            a.notice = Some(notice_for(e));
            return Vec::new();
        }
        if waits {
            self.cash_settled();
        }
        match reply {
            Reply::Reasons(Ok(reasons)) => {
                if let Some(d) = self.dialog_mut() {
                    d.reasons = reasons
                        .into_iter()
                        .filter(|r| r.active && !r.archived)
                        .collect();
                    d.reason = d.reason.min(d.shown_reasons().len().saturating_sub(1));
                }
                Vec::new()
            }
            Reply::Movements(Ok(page)) => {
                if let Some(d) = self.dialog_mut() {
                    d.movements = page.items;
                    d.cursor = d.cursor.min(d.movements.len().saturating_sub(1));
                }
                Vec::new()
            }
            Reply::Posted(Ok(movement)) => {
                let session = movement.cashier_session_id.clone();
                let id = movement.id.clone();
                let reversal = movement.reverses.is_some();
                if let Some(d) = self.dialog_mut() {
                    d.step = Step::Slip;
                    d.slip.clear();
                    d.slip_number.clone_from(&movement.number);
                    d.movements.insert(0, (*movement).clone());
                    d.cursor = 0;
                }
                let code = if reversal {
                    "cash_reversed"
                } else {
                    "cash_posted"
                };
                self.set_notice(Notice::new(code, movement.number, Tone::Good));
                let mut effects = self.cash_quiet(Call::Slip {
                    id,
                    copy: false,
                    override_token: None,
                });
                effects.extend(self.cash_quiet(Call::Movements(session)));
                effects
            }
            Reply::Slip(copy, Ok(text)) => {
                if let Some(d) = self.dialog_mut() {
                    d.step = Step::Slip;
                    d.slip = text.lines;
                    if copy && let Some(m) = d.movements.get(d.cursor) {
                        d.slip_number.clone_from(&m.number);
                    }
                }
                if copy {
                    let number = self
                        .dialog_mut()
                        .map(|d| d.slip_number.clone())
                        .unwrap_or_default();
                    self.set_notice(Notice::new("reprinted", number, Tone::Good));
                }
                Vec::new()
            }
            Reply::X(Ok(text)) => {
                if let Screen::Home(h) = &mut self.screen {
                    h.sell.modal = Some(Modal::XReport(XView { lines: text.lines }));
                    h.sell.focus = None;
                    h.notice = None;
                }
                Vec::new()
            }
            Reply::CloseText(Ok(text)) => {
                if let Some(r) = self.report_mut()
                    && !matches!(r.z, ZStep::Done(_))
                {
                    r.text = text.lines;
                }
                Vec::new()
            }
            Reply::Pending(result) => {
                if let Some(r) = self.report_mut()
                    && r.z == ZStep::Hidden
                {
                    let day = r.day();
                    let blocked =
                        result.is_ok_and(|days| days.iter().any(|p| p.day == day && p.open > 0));
                    if !blocked {
                        r.z = ZStep::Offered;
                    }
                }
                Vec::new()
            }
            Reply::Z(Ok(z)) => {
                let id = z.id.clone();
                let number = z.report.number.clone();
                if let Some(r) = self.report_mut() {
                    r.z = ZStep::Done(z);
                    r.text.clear();
                    r.notice = Some(Notice::new("z_done", number, Tone::Good));
                }
                self.cash_quiet(Call::ZText(id))
            }
            Reply::ZText(Ok(text)) => {
                if let Some(r) = self.report_mut() {
                    r.text = text.lines;
                }
                Vec::new()
            }
            Reply::Z(Err(e)) => {
                if let Some(r) = self.report_mut() {
                    r.z = ZStep::Offered;
                    if !matches!(e, ClientError::Network(_)) {
                        r.key = new_key();
                    }
                    if e.code() == Some("sessions_open") {
                        r.z = ZStep::Hidden;
                    }
                }
                self.report_notice(cash_notice(&e));
                Vec::new()
            }
            Reply::Float(Ok(p)) => {
                if let Screen::Open(o) = &mut self.screen {
                    o.propose(p.proposed_float);
                }
                Vec::new()
            }
            Reply::PayoutSlip(customer_id, Ok(text)) => {
                // A slow answer may land after the cashier opened another customer.
                if let Some(d) = &mut self.dialog {
                    d.show_slip(&customer_id, text.lines);
                }
                Vec::new()
            }
            // Lists, texts and the proposal that failed leave the screen as it is.
            Reply::Reasons(Err(e))
            | Reply::Movements(Err(e))
            | Reply::CloseText(Err(e))
            | Reply::ZText(Err(e))
            | Reply::PayoutSlip(_, Err(e)) => {
                if self.in_cash() {
                    self.set_notice(cash_notice(&e));
                }
                Vec::new()
            }
            Reply::Float(Err(_)) => Vec::new(),
            Reply::Posted(Err(e)) => {
                if let Some(d) = self.dialog_mut() {
                    if d.step == Step::Confirm {
                        d.step = Step::Form;
                    }
                    if !matches!(e, ClientError::Network(_)) {
                        d.id = new_key();
                    }
                }
                self.set_notice(cash_notice(&e));
                Vec::new()
            }
            Reply::Slip(_, Err(e)) | Reply::X(Err(e)) => {
                self.set_notice(cash_notice(&e));
                Vec::new()
            }
        }
    }

    /// The keys of the close report: done, and End of day (Z) when offered.
    pub(crate) fn report_shortcuts(r: &ReportScreen) -> Vec<Shortcut> {
        use Action::{Back, Confirm};
        let s = |key, action, label| Shortcut { key, action, label };
        match &r.z {
            ZStep::Confirm => vec![
                s(Key::Enter, Confirm, "run_z"),
                s(Key::Escape, Back, "back"),
            ],
            ZStep::Offered => vec![
                s(Key::Enter, Confirm, "done"),
                s(Key::Char('z'), Action::RunZ, "end_of_day"),
            ],
            ZStep::Hidden | ZStep::Done(_) => vec![s(Key::Enter, Confirm, "done")],
        }
    }

    /// An action of the close report; `None` when the caller handles it (done: log out).
    pub(crate) fn report_action(&mut self, action: Action) -> Option<Vec<Effect>> {
        let r = self.report_mut()?;
        match (action, &r.z) {
            (Action::RunZ, ZStep::Offered) => {
                r.z = ZStep::Confirm;
                r.notice = None;
                Some(Vec::new())
            }
            (Action::Back, ZStep::Confirm) => {
                r.z = ZStep::Offered;
                Some(Vec::new())
            }
            (Action::Confirm, ZStep::Confirm) => {
                let day = r.day();
                let key = r.key.clone();
                self.cash_wait = Some(Waiting::Z {
                    day: day.clone(),
                    key: key.clone(),
                });
                Some(self.cash_call(Call::RunZ {
                    day,
                    key,
                    override_token: None,
                }))
            }
            _ => None,
        }
    }

    /// After the close: the printable report and whether a Z can be offered.
    pub(crate) fn after_close(&self, session_id: String) -> Vec<Effect> {
        let mut effects = self.cash_quiet(Call::CloseText(session_id));
        effects.extend(self.cash_quiet(Call::Pending));
        effects
    }
}

/// The body of a Z run at the till (the register's own branch).
#[must_use]
pub fn run_z_request(day: &str) -> ZRunRequest {
    ZRunRequest {
        day: day.to_owned(),
        branch_id: None,
    }
}

/// The query of this session's movements.
#[must_use]
pub fn session_query(session: &str) -> CashQuery {
    CashQuery {
        cashier_session_id: Some(session.to_owned()),
        limit: Some(100),
        ..CashQuery::default()
    }
}

/// A movement's row in the dialog's list: `JKT01-R1-K-270115-01`, and
/// `Cash out · Parkir · Rp 75.000` (kind code, reason, amount, "↩ …" for a reversal).
#[must_use]
pub fn movement_detail(m: &CashMovement) -> String {
    let mut parts = Vec::new();
    if let Some(r) = &m.reason_name {
        parts.push(r.clone());
    }
    parts.push(rupiah(m.amount));
    if let Some(n) = &m.reverses_number {
        parts.push(format!("↩ {n}"));
    }
    if m.reversed_by.is_some() {
        parts.push("↩".to_owned());
    }
    parts.join(" · ")
}

#[cfg(test)]
mod tests {
    use sultan_client::Problem;

    use super::*;

    fn reason(id: &str, direction: &str, note_required: bool) -> CashReason {
        CashReason {
            id: id.into(),
            name: id.to_uppercase(),
            direction: direction.into(),
            note_required,
            active: true,
            sort: 0,
            archived: false,
        }
    }

    fn dialog() -> CashDialog {
        let mut d = CashDialog::new();
        d.reasons = vec![
            reason("topup", "in", false),
            reason("other-in", "in", true),
            reason("parkir", "out", false),
        ];
        d
    }

    #[test]
    fn kinds_cycle_and_filter_the_reasons() {
        assert_eq!(Kind::In.step(-1), Kind::Drop);
        assert_eq!(Kind::Drop.step(1), Kind::In);
        assert_eq!(Kind::of("out"), Some(Kind::Out));
        assert_eq!(Kind::of("x"), None);
        let mut d = dialog();
        assert_eq!(d.shown_reasons().len(), 2);
        d.reason = 1;
        assert!(d.edit(Key::Right));
        assert_eq!(
            (d.kind, d.reason),
            (Kind::Out, 0),
            "a new kind starts at its first reason"
        );
        assert_eq!(d.shown_reasons()[0].id, "parkir");
        d.set_kind(Kind::Drop);
        assert!(d.shown_reasons().is_empty());
        assert!(
            d.edit(Key::Down),
            "nothing to move, but the key is the dialog's"
        );
    }

    #[test]
    fn requests_follow_the_rules() {
        let mut d = dialog();
        assert_eq!(d.request().unwrap_err().code, "invalid_amount");
        for k in [7, 5, 0, 0, 0] {
            d.edit(Key::Digit(k));
        }
        let r = d.request().unwrap();
        assert_eq!(
            (r.kind.as_deref(), r.amount, r.reason_id.as_deref(), r.note),
            (Some("in"), Some(75_000), Some("topup"), None)
        );
        // "Other" needs a note.
        d.edit(Key::Down);
        assert_eq!(d.request().unwrap_err().code, "note_required");
        d.focus = Focus::Note;
        for c in "  modal ".chars() {
            d.edit(Key::Char(c));
        }
        d.edit(Key::Backspace);
        assert_eq!(
            d.request().unwrap().note.as_deref(),
            Some("modal"),
            "trimmed"
        );
        // The note is capped.
        for _ in 0..300 {
            d.edit(Key::Char('x'));
        }
        assert_eq!(d.note.chars().count(), MAX_NOTE);
        // A drop has no reason; in / out without one cannot post.
        d.set_kind(Kind::Drop);
        assert_eq!(d.request().unwrap().reason_id, None);
        d.reasons.clear();
        d.set_kind(Kind::Out);
        assert_eq!(d.request().unwrap_err().code, "pick_reason");
    }

    #[test]
    fn reversals_check_the_target() {
        let mut d = dialog();
        assert_eq!(d.reversal().unwrap_err().code, "cannot_reverse");
        d.movements = vec![
            CashMovement {
                id: "m2".into(),
                reverses: Some("m1".into()),
                ..CashMovement::default()
            },
            CashMovement {
                id: "m1".into(),
                reversed_by: Some("m2".into()),
                ..CashMovement::default()
            },
            CashMovement {
                id: "m0".into(),
                amount: 5,
                ..CashMovement::default()
            },
        ];
        assert_eq!(d.reversal().unwrap_err().code, "cannot_reverse");
        d.cursor = 1;
        assert_eq!(d.reversal().unwrap_err().code, "already_reversed");
        d.cursor = 2;
        assert_eq!(d.reversal().unwrap_err().code, "note_required");
        d.step = Step::Reverse;
        for c in "salah".chars() {
            assert!(d.edit(Key::Char(c)));
        }
        let (target, body) = d.reversal().unwrap();
        assert_eq!(target.amount, 5);
        assert_eq!(
            (body.reverses.as_deref(), body.note.as_deref(), body.kind),
            (Some("m0"), Some("salah"), None)
        );
        // The list moves with the arrows; the slip and confirm steps take no typing.
        d.step = Step::Form;
        d.focus = Focus::List;
        assert!(d.edit(Key::Down));
        assert_eq!(d.cursor, 0);
        assert!(d.edit(Key::Up));
        assert_eq!(d.cursor, 2);
        assert!(!d.edit(Key::Digit(1)));
        d.step = Step::Slip;
        assert!(!d.edit(Key::Digit(1)));
    }

    #[test]
    fn business_days_come_from_the_number() {
        assert_eq!(business_day("JKT01-R1-S-270115-01", 0), "2027-01-15");
        assert_eq!(business_day("JKT01-R1-S-270115-123", 0), "2027-01-15");
        assert_eq!(business_day("odd", 1_800_000_000_000).len(), 10);
        assert_eq!(business_day("odd", i64::MAX), "");
    }

    #[test]
    fn errors_become_messages() {
        let p = |field: &str| {
            ClientError::Api(Problem::new(422, "invalid_field", "x").with("field", field))
        };
        for (field, code) in [
            ("note", "note_required"),
            ("reason", "pick_reason"),
            ("reverses", "cannot_reverse"),
            ("float_kept", "float_above_count"),
            ("amount", "invalid_amount"),
        ] {
            assert_eq!(cash_notice(&p(field)).code, code, "{field}");
        }
        let m = CashMovement {
            amount: 75_000,
            reason_name: Some("Parkir".into()),
            reverses_number: Some("K-01".into()),
            reversed_by: Some("x".into()),
            ..CashMovement::default()
        };
        assert_eq!(movement_detail(&m), "Parkir · Rp 75.000 · ↩ K-01 · ↩");
        assert_eq!(session_query("s").cashier_session_id.as_deref(), Some("s"));
        assert_eq!(run_z_request("2027-01-15").day, "2027-01-15");
    }
}
