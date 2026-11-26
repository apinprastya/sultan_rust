//! The back office's cash screens (spec 2.7 §10, phase K5), part of the [`Controller`] state
//! machine. F9 opens the section; a row of tabs (Ctrl+1 … Ctrl+4) switches between its screens:
//!
//! 1. **Sessions** ([`Screen::CashSessions`]) — filters branch, register, cashier, date from /
//!    until (local `YYYY-MM-DD`, by open time) and status; pages of 50 newest first with
//!    `before_id` ("load more", and moving past the last row loads the next page). Columns:
//!    number, opened, closed, branch / register, cashier, opening cash, expected cash, status.
//!    Enter opens the **session detail** ([`Screen::CashSessionDetail`]): its facts (from the
//!    stored close report once closed: expected, counted, over/short, float kept, to the safe,
//!    cash in / out / drops, drawer opens, X reads), its cash movements, and a text pane: an open
//!    session shows its **live X** (`…/x-report/text`, one audited read per opening or F5; it
//!    needs `report.sales` and `session.view_expected`, else the pane says so), a closed one its
//!    stored **close report** (`…/report/text`); Ctrl+P reprints it as a COPY. A `cash.moved`
//!    event of the open session reloads its movements.
//! 2. **Cash movements** ([`Screen::CashMovements`]) — filters branch, register, session (the
//!    sessions seen in the sessions list), kind, reason, dates and **missing nota**
//!    (`missing_nota=true`: cash outs at or above the company's `nota_flag_above` without a
//!    photo); each row shows its photo count and a "No nota" flag. Enter shows the movement's
//!    **slip** in the pane beside the list, Ctrl+P its COPY (`sale.reprint`). Under the slip are
//!    its **nota photos** (spec 2.7 §2a, phase K7; [`PhotoPane`]): each one is read once
//!    (`GET …/photos/{photo_id}`) and shown as a thumbnail, the selected one larger; Ctrl+G
//!    selects the next, Ctrl+E shows it over the whole screen (Esc closes it). **Ctrl+U** adds a
//!    photo to the highlighted movement from a file (the window's [`crate::photo::PhotoSource`],
//!    then `PUT …/photos/{photo_id}`): refused on screen without `cash.out` or at 3 photos, and
//!    the server's refusals (`photo_limit`, too large, not an image, `permission_denied`) are
//!    messages. After a network failure the picked bytes and the photo ID are kept, and the next
//!    Ctrl+U on that movement sends them again without asking for a file (an idempotent `PUT`,
//!    so no duplicate). A photo that cannot be read (`403` for a user with `cash.out` but without
//!    `report.sales` at the branch, `404` when its file is not on this node) shows a message in
//!    its place. A `cash.moved` event of a listed movement reads it again (its photos and flag).
//! 3. **Day close** ([`Screen::DayClose`]) — the **days still to close** of a branch (the
//!    branch filter, else the user's branch; `GET /z-reports/pending`) with their open and closed
//!    session counts, then the **Z reports** (`GET /z-reports`, newest first, paged) in one list.
//!    Enter on a day asks first, then runs its Z (`POST /z-reports`, `day.close`; the
//!    idempotency key is kept for a retry after a network error); `sessions_open` (naming them),
//!    `nothing_to_close`, a future day, a missing permission and an HQ node are messages. Enter
//!    on a Z shows its text, Ctrl+P a COPY. After a run the new Z is selected and shown. A
//!    `day.closed` event reloads the screen.
//! 4. **Cash reasons** ([`Screen::CashReasons`], HQ, `tender.manage`, spec 2.7 K25) — every
//!    reason with an editor: Ctrl+N new, Ctrl+S save, Ctrl+Shift+A archive (asks first). The
//!    direction is fixed once saved. Without `tender.manage` (the active reasons only) or on a
//!    branch node (`read_only_replica`) it is read-only.
//!
//! Reading needs `report.sales` (spec 2.7 §7); without it the lists say so.

use sultan_client::cash::{CASH_OUT, DAY_CLOSE, SESSION_VIEW_EXPECTED};
use sultan_client::sales::TENDER_MANAGE;
use sultan_client::{
    CashMovement, CashMovementPage, CashQuery, CashReason, CashReasonBody, CashText,
    CashierSession, ClientError, NotaPhoto, PendingDay, ServerEvent, SessionDetail, SessionPage,
    SessionQuery, ZQuery, ZReport, ZReportPage, ZRunRequest, ZSummary,
};
use sultan_core::cash::MAX_PHOTOS;

use super::sales::{DocList, REPORT_SALES, local_midnight, parse_day};
use super::{Confirm, Controller, Effect, Press, PressKey, Screen, Step, notice};
use crate::controller::{Notice, Tone};

/// The screens of the section, in tab order (Ctrl+1 … Ctrl+4).
pub const TABS: [Screen; 4] = [
    Screen::CashSessions,
    Screen::CashMovements,
    Screen::DayClose,
    Screen::CashReasons,
];
/// Status filter choices: all, open, closed.
pub const STATUSES: [&str; 3] = ["", "open", "closed"];
/// Kind filter choices: all, in, out, drop.
pub const KINDS: [&str; 4] = ["", "in", "out", "drop"];
/// Reason directions in picker order.
pub const DIRECTIONS: [&str; 2] = ["in", "out"];
/// Page size of the lists.
pub const PAGE: u32 = 50;
/// Movements shown in a session detail (one page).
pub const SESSION_MOVEMENTS: u32 = 200;
/// The `kind`s of [`super::Msg::Edit`] handled here.
pub const EDIT_KINDS: [&str; 2] = ["cash-filter", "cash-reason"];

/// Whether `screen` belongs to the cash section.
#[must_use]
pub fn in_section(screen: Screen) -> bool {
    TABS.contains(&screen) || screen == Screen::CashSessionDetail
}

/// Which text pane an answer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// The session detail's X or close report.
    Session,
    /// The movements list's slip.
    Slip,
    /// The day close's Z.
    Z,
}

/// A cash call (inside [`Effect::Cash`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `sessions`; appended when `append`.
    Sessions {
        /// Query.
        query: SessionQuery,
        /// Next page.
        append: bool,
        /// The list's request number when sent (stale answers are dropped).
        seq: u64,
    },
    /// `session` (the detail).
    Session(String),
    /// `cash_movements` of one session (the detail).
    SessionMovements(String),
    /// `x_report_text` of an open session.
    XText(String),
    /// `close_report_text` of a closed session.
    CloseText {
        /// Session.
        id: String,
        /// A COPY.
        copy: bool,
    },
    /// `cash_movements`; appended when `append`.
    Movements {
        /// Query.
        query: CashQuery,
        /// Next page.
        append: bool,
        /// The list's request number when sent.
        seq: u64,
    },
    /// `cash_slip`.
    Slip {
        /// Movement.
        id: String,
        /// A COPY.
        copy: bool,
    },
    /// `pending_days` of a branch.
    Pending(String),
    /// `z_reports`; appended when `append`.
    ZList {
        /// Query.
        query: ZQuery,
        /// Next page.
        append: bool,
        /// The list's request number when sent.
        seq: u64,
    },
    /// `z_report_text`.
    ZText {
        /// Z report.
        id: String,
        /// A COPY.
        copy: bool,
    },
    /// `run_z`.
    RunZ {
        /// Day and branch.
        body: ZRunRequest,
        /// Idempotency key.
        key: String,
    },
    /// `all_cash_reasons` (`all`, with `tender.manage`) or `cash_reasons` (the active ones).
    Reasons {
        /// Every reason.
        all: bool,
    },
    /// `put_cash_reason`.
    PutReason {
        /// Reason.
        id: String,
        /// Body.
        body: CashReasonBody,
    },
    /// `delete_cash_reason` (archive).
    ArchiveReason(String),
    /// `cash_movement` (one movement again, after a `cash.moved` event).
    Movement(String),
    /// `cash_photo`: the stored JPEG of one nota photo.
    Photo {
        /// Movement.
        movement_id: String,
        /// Photo.
        photo_id: String,
    },
    /// A file from the window's photo source (the file picker), for this movement.
    PickPhoto(String),
    /// `add_cash_photo`.
    Upload(Upload),
}

/// A nota photo on its way to the server; kept after a network failure so that the next
/// Ctrl+U sends the same bytes under the same ID (an idempotent `PUT`).
#[derive(Clone, PartialEq, Eq)]
pub struct Upload {
    /// Movement.
    pub movement_id: String,
    /// The new photo's ID (`UUIDv7`).
    pub photo_id: String,
    /// The picked file.
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for Upload {
    // The bytes are left out: a photo is up to 10 MB.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Upload")
            .field("movement_id", &self.movement_id)
            .field("photo_id", &self.photo_id)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

/// The answer of a [`Call`] (inside [`super::Msg::Cash`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// [`Call::Sessions`] (append, seq).
    Sessions(bool, u64, Result<SessionPage, ClientError>),
    /// [`Call::Session`].
    Session(String, Box<Result<SessionDetail, ClientError>>),
    /// [`Call::SessionMovements`].
    SessionMovements(String, Result<CashMovementPage, ClientError>),
    /// A text for `pane` of the document with this ID.
    Text(Pane, String, Result<CashText, ClientError>),
    /// [`Call::Movements`] (append, seq).
    Movements(bool, u64, Result<CashMovementPage, ClientError>),
    /// [`Call::Pending`] (branch).
    Pending(String, Result<Vec<PendingDay>, ClientError>),
    /// [`Call::ZList`] (append, seq).
    ZList(bool, u64, Result<ZReportPage, ClientError>),
    /// [`Call::RunZ`] (day).
    ZRun(String, Box<Result<ZReport, ClientError>>),
    /// [`Call::Reasons`].
    Reasons(Result<Vec<CashReason>, ClientError>),
    /// [`Call::PutReason`] or [`Call::ArchiveReason`] (true).
    ReasonSaved(bool, Result<CashReason, ClientError>),
    /// [`Call::Movement`].
    Movement(Result<CashMovement, ClientError>),
    /// [`Call::Photo`] (movement, photo).
    Photo(String, String, Result<Vec<u8>, ClientError>),
    /// [`Call::PickPhoto`] (movement): the bytes, `None` when the picker was cancelled. A picker
    /// failure is [`ClientError::Unexpected`] (its message), a file over 10 MB
    /// `413 payload_too_large`.
    Picked(String, Result<Option<Vec<u8>>, ClientError>),
    /// [`Call::Upload`]: what was sent, and the stored photo.
    Uploaded(Upload, Result<NotaPhoto, ClientError>),
}

/// The list filters (shared by the sessions and the movements).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// 0 = every branch, else 1 + the index into the reference branches.
    pub branch: usize,
    /// 0 = every register, else 1 + the index into [`CashState::registers`].
    pub register: usize,
    /// 0 = every cashier, else 1 + the index into [`CashState::cashiers`].
    pub cashier: usize,
    /// 0 = every session, else 1 + the index into [`CashState::seen_sessions`].
    pub session: usize,
    /// First local day, `YYYY-MM-DD`.
    pub from: String,
    /// Last local day, `YYYY-MM-DD`.
    pub until: String,
    /// Index into [`STATUSES`].
    pub status: usize,
    /// Index into [`KINDS`].
    pub kind: usize,
    /// 0 = every reason, else 1 + the index into [`CashState::reasons`].
    pub reason: usize,
    /// Only cash outs missing a nota (`missing_nota=true`).
    pub missing_nota: bool,
    /// A date that could not be read: `from` or `until`.
    pub bad_date: Option<&'static str>,
}

/// A printable text shown beside a list or a detail.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TextPane {
    /// The document it is of.
    pub id: String,
    /// The lines.
    pub lines: Vec<String>,
    /// A COPY.
    pub copy: bool,
    /// The text is loading.
    pub loading: bool,
    /// Why there is no text.
    pub notice: Option<Notice>,
}

impl TextPane {
    fn asking(id: &str, copy: bool) -> Self {
        Self {
            id: id.to_owned(),
            copy,
            loading: true,
            ..Self::default()
        }
    }
}

/// One nota photo in the pane.
#[derive(Clone, PartialEq, Eq)]
pub enum PhotoImage {
    /// Asked for.
    Loading,
    /// The stored JPEG.
    Loaded(Vec<u8>),
    /// Why it cannot be shown.
    Failed(Notice),
}

impl std::fmt::Debug for PhotoImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Loading => f.write_str("Loading"),
            Self::Loaded(bytes) => write!(f, "Loaded({} bytes)", bytes.len()),
            Self::Failed(n) => f.debug_tuple("Failed").field(n).finish(),
        }
    }
}

/// The nota photos of the movement whose slip is shown (Ctrl+2).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PhotoPane {
    /// The movement (empty: none shown).
    pub movement_id: String,
    /// Its photos, oldest first, with what was read of each.
    pub photos: Vec<(String, PhotoImage)>,
    /// The photo shown larger.
    pub selected: usize,
    /// The selected photo fills the screen.
    pub zoom: bool,
    /// A file is being picked or uploaded.
    pub busy: bool,
    /// Added, refusals.
    pub notice: Option<Notice>,
}

impl PhotoPane {
    /// Adds the photos of `ids` not in the pane yet; the calls that read them.
    fn learn(&mut self, ids: &[String]) -> Vec<String> {
        let mut asked = Vec::new();
        for id in ids {
            if !self.photos.iter().any(|(p, _)| p == id) {
                self.photos.push((id.clone(), PhotoImage::Loading));
                asked.push(id.clone());
            }
        }
        asked
    }
}

/// The open session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionView {
    /// The session asked for.
    pub id: String,
    /// The session and its stored close report, once loaded.
    pub detail: Option<Box<SessionDetail>>,
    /// Its cash movements, newest first.
    pub movements: Vec<CashMovement>,
    /// The highlighted movement.
    pub cursor: usize,
    /// The X or the close report.
    pub text: TextPane,
    /// Errors.
    pub notice: Option<Notice>,
}

impl SessionView {
    /// Whether the session is open (its pane is the live X).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.detail
            .as_ref()
            .is_some_and(|d| d.session.status == "open")
    }
}

/// The day close screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DayClose {
    /// The branch the days are of.
    pub branch: Option<String>,
    /// The days still to close, oldest first.
    pub pending: Vec<PendingDay>,
    /// The pending days are loading.
    pub pending_loading: bool,
    /// The Z reports.
    pub z: DocList<ZSummary>,
    /// The highlighted row: the pending days first, then the Z reports.
    pub cursor: usize,
    /// The day the confirmation asks about.
    pub confirm_day: Option<String>,
    /// The day and idempotency key of the last run that got no answer (a network retry).
    pub run_key: Option<(String, String)>,
    /// A run is going.
    pub running: bool,
    /// The Z to select once the list arrives.
    pub select: Option<String>,
    /// The run's outcome, refusals.
    pub notice: Option<Notice>,
}

/// The cash reason editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the reason's flags and the editor's state, one each
pub struct ReasonEditor {
    /// The highlighted row.
    pub cursor: usize,
    /// The edited reason (`None` while new).
    pub id: Option<String>,
    /// Name.
    pub name: String,
    /// Index into [`DIRECTIONS`].
    pub direction: usize,
    /// A note is required with it.
    pub note_required: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position as typed.
    pub sort: String,
    /// Archived (read-only).
    pub archived: bool,
    /// A save is running.
    pub saving: bool,
    /// The node refused writes (`read_only_replica`): read-only from now on.
    pub replica: bool,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// Everything of the cash section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CashState {
    /// The filters.
    pub filters: Filters,
    /// The registers seen in the lists (ID, code), by code.
    pub registers: Vec<(String, String)>,
    /// The cashiers seen in the sessions list (ID, name), by name.
    pub cashiers: Vec<(String, String)>,
    /// The sessions seen in the sessions list (ID, number), newest first.
    pub seen_sessions: Vec<(String, String)>,
    /// The sessions list.
    pub sessions: DocList<CashierSession>,
    /// The open session.
    pub session: SessionView,
    /// The movements list.
    pub movements: DocList<CashMovement>,
    /// The slip of the movement opened in the list.
    pub slip: TextPane,
    /// The nota photos under the slip.
    pub photos: PhotoPane,
    /// The upload that got no answer (a network failure): Ctrl+U on its movement sends it again.
    pub retry: Option<Upload>,
    /// The day close.
    pub day: DayClose,
    /// The Z shown on the day close.
    pub z_text: TextPane,
    /// The cash reasons (every one with `tender.manage`, else the active ones).
    pub reasons: Vec<CashReason>,
    /// The cash reason editor.
    pub editor: ReasonEditor,
    /// Bumped when texts change under the user: the view rebuilds the text fields.
    pub serial: u64,
}

impl Controller {
    fn cash_call(&self, call: Call) -> Effect {
        Effect::Cash {
            token: self.token(),
            call,
        }
    }

    fn cash_bump(&mut self) {
        self.cash.serial += 1;
    }

    /// Whether cash reasons may be changed (`tender.manage`, spec 2.7 K25).
    #[must_use]
    pub fn can_manage_reasons(&self) -> bool {
        self.may(TENDER_MANAGE)
    }

    /// Whether the cash reasons can be edited here.
    #[must_use]
    pub fn reasons_editable(&self) -> bool {
        self.can_manage_reasons() && !self.cash.editor.replica
    }

    /// Why the cash reasons are read-only, if they are.
    #[must_use]
    pub fn reasons_read_only(&self) -> Option<Notice> {
        if self.cash.editor.replica {
            Some(Notice::bad("read_only_replica"))
        } else if self.session.is_some() && !self.can_manage_reasons() {
            Some(notice(
                "cash_reasons_read_only",
                TENDER_MANAGE,
                Tone::Neutral,
            ))
        } else {
            None
        }
    }

    /// Whether the user may run a Z (`day.close` from any role assignment).
    #[must_use]
    pub fn can_close_day(&self) -> bool {
        self.may(DAY_CLOSE)
    }

    fn denied_cash() -> Notice {
        notice("cash_denied", REPORT_SALES, Tone::Bad)
    }

    /// The branch of the day close: the branch filter, else the user's branch, else the first.
    #[must_use]
    pub fn day_branch(&self) -> Option<String> {
        let picked = self
            .cash
            .filters
            .branch
            .checked_sub(1)
            .and_then(|i| self.reference.branches.get(i))
            .map(|b| b.id.clone());
        let own = self
            .session
            .as_ref()
            .map(|s| s.me.branch_id.clone())
            .filter(|b| !b.is_empty());
        picked
            .or(own)
            .or_else(|| self.reference.branches.first().map(|b| b.id.clone()))
    }

    /// The local-day bounds of the filters (`from`, `until` exclusive), or `None` after marking
    /// a bad date.
    fn cash_window(&mut self) -> Option<(Option<i64>, Option<i64>)> {
        let f = &mut self.cash.filters;
        let from = parse_day(&f.from);
        let until = parse_day(&f.until);
        f.bad_date = match (&from, &until) {
            (Err(()), _) => Some("from"),
            (_, Err(())) => Some("until"),
            _ => None,
        };
        if f.bad_date.is_some() {
            return None;
        }
        let from = from.ok().flatten().and_then(local_midnight);
        let until = until
            .ok()
            .flatten()
            .and_then(|d| d.checked_add_days(chrono::Days::new(1)))
            .and_then(local_midnight);
        Some((from, until))
    }

    fn picked(index: usize, list: &[(String, String)]) -> Option<&String> {
        index.checked_sub(1).and_then(|i| list.get(i)).map(|r| &r.0)
    }

    fn filter_branch(&self) -> Option<String> {
        self.cash
            .filters
            .branch
            .checked_sub(1)
            .and_then(|i| self.reference.branches.get(i))
            .map(|b| b.id.clone())
    }

    fn session_query(&mut self, before: Option<String>) -> Option<SessionQuery> {
        let (from, until) = self.cash_window()?;
        let f = &self.cash.filters;
        Some(SessionQuery {
            branch_id: self.filter_branch(),
            register_id: Self::picked(f.register, &self.cash.registers).cloned(),
            user_id: Self::picked(f.cashier, &self.cash.cashiers).cloned(),
            status: Some(STATUSES[f.status.min(STATUSES.len() - 1)].to_owned())
                .filter(|s| !s.is_empty()),
            from,
            until,
            before_id: before,
            limit: Some(PAGE),
        })
    }

    fn movement_query(&mut self, before: Option<String>) -> Option<CashQuery> {
        let (from, until) = self.cash_window()?;
        let f = &self.cash.filters;
        Some(CashQuery {
            branch_id: self.filter_branch(),
            register_id: Self::picked(f.register, &self.cash.registers).cloned(),
            cashier_session_id: Self::picked(f.session, &self.cash.seen_sessions).cloned(),
            kind: Some(KINDS[f.kind.min(KINDS.len() - 1)].to_owned()).filter(|k| !k.is_empty()),
            reason_id: f
                .reason
                .checked_sub(1)
                .and_then(|i| self.cash.reasons.get(i))
                .map(|r| r.id.clone()),
            from,
            until,
            missing_nota: f.missing_nota.then_some(true),
            before_id: before,
            limit: Some(PAGE),
        })
    }

    /// Loads the first page of the list on view.
    fn reload_cash(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::CashSessions => {
                if !self.can_read_sales() {
                    self.cash.sessions.notice = Some(Self::denied_cash());
                    return Vec::new();
                }
                let Some(query) = self.session_query(None) else {
                    return Vec::new();
                };
                let seq = self.cash.sessions.begin(false);
                vec![self.cash_call(Call::Sessions {
                    query,
                    append: false,
                    seq,
                })]
            }
            Screen::CashMovements => {
                if !self.can_read_sales() {
                    self.cash.movements.notice = Some(Self::denied_cash());
                    return Vec::new();
                }
                let Some(query) = self.movement_query(None) else {
                    return Vec::new();
                };
                let seq = self.cash.movements.begin(false);
                vec![self.cash_call(Call::Movements {
                    query,
                    append: false,
                    seq,
                })]
            }
            Screen::DayClose => self.reload_day(),
            Screen::CashReasons => vec![self.cash_call(Call::Reasons {
                all: self.can_manage_reasons(),
            })],
            Screen::CashSessionDetail => {
                let id = self.cash.session.id.clone();
                self.open_session(id)
            }
            _ => Vec::new(),
        }
    }

    /// Loads the next page of the list on view.
    fn more_cash(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::CashSessions => {
                let l = &self.cash.sessions;
                let Some(next) = l.next.clone().filter(|_| !l.loading) else {
                    return Vec::new();
                };
                let Some(query) = self.session_query(Some(next)) else {
                    return Vec::new();
                };
                let seq = self.cash.sessions.begin(true);
                vec![self.cash_call(Call::Sessions {
                    query,
                    append: true,
                    seq,
                })]
            }
            Screen::CashMovements => {
                let l = &self.cash.movements;
                let Some(next) = l.next.clone().filter(|_| !l.loading) else {
                    return Vec::new();
                };
                let Some(query) = self.movement_query(Some(next)) else {
                    return Vec::new();
                };
                let seq = self.cash.movements.begin(true);
                vec![self.cash_call(Call::Movements {
                    query,
                    append: true,
                    seq,
                })]
            }
            Screen::DayClose => {
                let l = &self.cash.day.z;
                let (Some(next), Some(branch)) = (
                    l.next.clone().filter(|_| !l.loading),
                    self.cash.day.branch.clone(),
                ) else {
                    return Vec::new();
                };
                let seq = self.cash.day.z.begin(true);
                vec![self.cash_call(Call::ZList {
                    query: Self::z_query(branch, Some(next)),
                    append: true,
                    seq,
                })]
            }
            _ => Vec::new(),
        }
    }

    fn z_query(branch: String, before: Option<String>) -> ZQuery {
        ZQuery {
            branch_id: Some(branch),
            before_id: before,
            limit: Some(PAGE),
            ..ZQuery::default()
        }
    }

    /// The day close: the pending days and the first page of Z reports of the branch.
    fn reload_day(&mut self) -> Vec<Effect> {
        if !self.can_read_sales() {
            self.cash.day.z.notice = Some(Self::denied_cash());
            return Vec::new();
        }
        let Some(branch) = self.day_branch() else {
            self.cash.day.notice = Some(Notice::bad("pick_branch"));
            return Vec::new();
        };
        if self.cash.day.branch.as_ref() != Some(&branch) {
            self.cash.day.cursor = 0;
            self.cash.day.run_key = None;
        }
        self.cash.day.branch = Some(branch.clone());
        self.cash.day.pending_loading = true;
        let seq = self.cash.day.z.begin(false);
        vec![
            self.cash_call(Call::Pending(branch.clone())),
            self.cash_call(Call::ZList {
                query: Self::z_query(branch, None),
                append: false,
                seq,
            }),
        ]
    }

    /// Entering a screen of the section (from `enter`).
    pub(super) fn enter_cash(&mut self, target: Screen, from: Screen) -> Vec<Effect> {
        self.focus("list");
        let mut effects = Vec::new();
        if self.reference.branches.is_empty() {
            effects.push(Effect::LoadBranches(self.token()));
        }
        let loaded = match target {
            Screen::CashSessions => self.cash.sessions.loaded,
            Screen::CashMovements => self.cash.movements.loaded,
            _ => false,
        };
        if target == Screen::CashSessions && loaded && from == Screen::CashSessionDetail {
            return effects;
        }
        if target != Screen::CashReasons && self.cash.reasons.is_empty() {
            // The reason filter and the movement rows name the reasons.
            effects.push(self.cash_call(Call::Reasons {
                all: self.can_manage_reasons(),
            }));
        }
        if target == Screen::CashReasons {
            self.cash.editor.notice = None;
        }
        self.cash_bump();
        effects.extend(self.reload_cash());
        effects
    }

    /// Opens the session detail of `id`.
    fn open_session(&mut self, id: String) -> Vec<Effect> {
        self.screen = Some(Screen::CashSessionDetail);
        self.focus("list");
        self.cash.session = SessionView {
            id: id.clone(),
            text: TextPane::asking(&id, false),
            ..SessionView::default()
        };
        vec![
            self.cash_call(Call::Session(id.clone())),
            self.cash_call(Call::SessionMovements(id)),
        ]
    }

    /// The live X of the open session, or why it cannot be shown.
    fn ask_x(&mut self) -> Vec<Effect> {
        let id = self.cash.session.id.clone();
        if !(self.can_read_sales() && self.may(SESSION_VIEW_EXPECTED)) {
            self.cash.session.text = TextPane {
                id,
                notice: Some(notice("x_denied", SESSION_VIEW_EXPECTED, Tone::Neutral)),
                ..TextPane::default()
            };
            return Vec::new();
        }
        self.cash.session.text = TextPane::asking(&id, false);
        vec![self.cash_call(Call::XText(id))]
    }

    /// Ctrl+P: a COPY of the text on view.
    fn reprint(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::CashSessionDetail => {
                let v = &self.cash.session;
                if v.detail.is_none() || v.is_open() {
                    return Vec::new();
                }
                let id = v.id.clone();
                self.cash.session.text = TextPane::asking(&id, true);
                vec![self.cash_call(Call::CloseText { id, copy: true })]
            }
            Screen::CashMovements => self.show_slip(true),
            Screen::DayClose => match self.selected_z() {
                Some(id) => {
                    self.cash.z_text = TextPane::asking(&id, true);
                    vec![self.cash_call(Call::ZText { id, copy: true })]
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    /// The slip of the highlighted movement (Enter), or its COPY (Ctrl+P), with its photos.
    fn show_slip(&mut self, copy: bool) -> Vec<Effect> {
        let l = &self.cash.movements;
        let Some(m) = l.rows.get(l.cursor) else {
            return Vec::new();
        };
        let (id, photo_ids) = (m.id.clone(), m.photo_ids.clone());
        self.cash.slip = TextPane::asking(&id, copy);
        let mut effects = vec![self.cash_call(Call::Slip {
            id: id.clone(),
            copy,
        })];
        effects.extend(self.show_photos(&id, &photo_ids));
        effects
    }

    /// The photo pane of `movement_id` (kept as read when it already shows that movement); the
    /// calls that read the photos not read yet.
    fn show_photos(&mut self, movement_id: &str, photo_ids: &[String]) -> Vec<Effect> {
        let p = &mut self.cash.photos;
        if p.movement_id != movement_id {
            *p = PhotoPane {
                movement_id: movement_id.to_owned(),
                ..PhotoPane::default()
            };
        }
        let asked = p.learn(photo_ids);
        asked
            .into_iter()
            .map(|photo_id| {
                self.cash_call(Call::Photo {
                    movement_id: movement_id.to_owned(),
                    photo_id,
                })
            })
            .collect()
    }

    /// Ctrl+U: a nota photo from a file for the highlighted movement (spec 2.7 §2a, `cash.out`).
    fn add_photo(&mut self) -> Vec<Effect> {
        let l = &self.cash.movements;
        let Some(m) = l.rows.get(l.cursor).cloned() else {
            return Vec::new();
        };
        if self.cash.photos.busy {
            return Vec::new();
        }
        // The pane shows the movement the photo is for.
        let mut effects = if self.cash.photos.movement_id == m.id {
            Vec::new()
        } else {
            self.show_slip(false)
        };
        let refusal = if !self.may(CASH_OUT) {
            Some(notice("photo_denied", CASH_OUT, Tone::Bad))
        } else if m.photo_ids.len() >= MAX_PHOTOS {
            Some(notice("photo_limit", MAX_PHOTOS.to_string(), Tone::Bad))
        } else {
            None
        };
        if refusal.is_some() {
            self.cash.photos.notice = refusal;
            return effects;
        }
        self.cash.photos.busy = true;
        self.cash.photos.notice = None;
        // A retry after a network failure sends the same bytes under the same ID.
        match self.cash.retry.clone() {
            Some(upload) if upload.movement_id == m.id => {
                effects.push(self.cash_call(Call::Upload(upload)));
            }
            _ => effects.push(self.cash_call(Call::PickPhoto(m.id))),
        }
        effects
    }

    /// Ctrl+G: the next photo is shown larger.
    fn next_photo(&mut self) {
        let p = &mut self.cash.photos;
        if !p.photos.is_empty() {
            p.selected = (p.selected + 1) % p.photos.len();
        }
    }

    /// Ctrl+E: the selected photo fills the screen (again: back).
    fn zoom_photo(&mut self) {
        let p = &mut self.cash.photos;
        let loaded = matches!(p.photos.get(p.selected), Some((_, PhotoImage::Loaded(_))));
        p.zoom = !p.zoom && loaded;
    }

    /// The answer of [`Call::Photo`].
    fn photo_read(
        &mut self,
        movement_id: &str,
        photo_id: &str,
        result: Result<Vec<u8>, ClientError>,
    ) {
        let image = match result {
            Ok(bytes) => PhotoImage::Loaded(bytes),
            Err(e) => PhotoImage::Failed(match e.code() {
                Some("permission_denied") => {
                    notice("photo_read_denied", REPORT_SALES, Tone::Neutral)
                }
                Some("not_found") => Notice::new("photo_missing", "", Tone::Neutral),
                _ => match self.failed(&e) {
                    Some(n) => n,
                    None => return,
                },
            }),
        };
        let p = &mut self.cash.photos;
        if p.movement_id != movement_id {
            return;
        }
        if let Some(slot) = p.photos.iter_mut().find(|(id, _)| id == photo_id) {
            slot.1 = image;
        }
    }

    /// The answer of [`Call::PickPhoto`]: the upload, or why there is none.
    fn photo_picked(
        &mut self,
        movement_id: String,
        result: Result<Option<Vec<u8>>, ClientError>,
    ) -> Vec<Effect> {
        match result {
            Ok(Some(bytes)) => {
                let upload = Upload {
                    movement_id,
                    photo_id: super::form::new_id(),
                    bytes,
                };
                return vec![self.cash_call(Call::Upload(upload))];
            }
            Ok(None) => {}
            Err(e) => self.cash.photos.notice = self.photo_failed(&e),
        }
        self.cash.photos.busy = false;
        Vec::new()
    }

    /// The answer of [`Call::Upload`]: the photo joins its movement and the pane; a network
    /// failure keeps the upload for Ctrl+U.
    fn photo_uploaded(
        &mut self,
        upload: Upload,
        result: Result<NotaPhoto, ClientError>,
    ) -> Vec<Effect> {
        self.cash.photos.busy = false;
        // Any answer but a network failure ends this upload's retry (another one's stays).
        if self
            .cash
            .retry
            .as_ref()
            .is_some_and(|r| r.photo_id == upload.photo_id)
        {
            self.cash.retry = None;
        }
        match result {
            Ok(photo) => {
                let mut number = String::new();
                let mut ids = Vec::new();
                for m in &mut self.cash.movements.rows {
                    if m.id == upload.movement_id {
                        if !m.photo_ids.contains(&photo.id) {
                            m.photo_ids.push(photo.id.clone());
                        }
                        m.missing_nota = false;
                        number.clone_from(&m.number);
                        ids.clone_from(&m.photo_ids);
                    }
                }
                if ids.is_empty() {
                    ids.push(photo.id.clone());
                }
                self.cash.photos.notice = Some(notice("photo_added", number, Tone::Good));
                if self.cash.photos.movement_id != upload.movement_id {
                    return Vec::new();
                }
                let effects = self.show_photos(&upload.movement_id, &ids);
                let p = &mut self.cash.photos;
                p.selected = p
                    .photos
                    .iter()
                    .position(|(id, _)| *id == photo.id)
                    .unwrap_or(p.selected);
                effects
            }
            Err(e) => {
                if matches!(e, ClientError::Network(_)) {
                    self.cash.retry = Some(upload);
                    self.cash.photos.notice = Some(Notice::bad("photo_retry"));
                } else {
                    self.cash.photos.notice = self.photo_failed(&e);
                }
                Vec::new()
            }
        }
    }

    /// Why a photo was not added.
    fn photo_failed(&mut self, e: &ClientError) -> Option<Notice> {
        let p = e.problem();
        match e.code() {
            Some("photo_limit") => Some(notice(
                "photo_limit",
                p.and_then(sultan_client::Problem::max)
                    .map_or_else(|| MAX_PHOTOS.to_string(), |v| v.to_string()),
                Tone::Bad,
            )),
            Some("permission_denied") => Some(notice(
                "photo_denied",
                p.and_then(|p| p.permission())
                    .unwrap_or(CASH_OUT)
                    .to_owned(),
                Tone::Bad,
            )),
            _ => match e {
                ClientError::Unexpected(message) => {
                    Some(notice("photo_failed", message.clone(), Tone::Bad))
                }
                _ => self.failed(e),
            },
        }
    }

    /// The Z highlighted on the day close, if the cursor is on one.
    fn selected_z(&self) -> Option<String> {
        let d = &self.cash.day;
        d.cursor
            .checked_sub(d.pending.len())
            .and_then(|i| d.z.rows.get(i))
            .map(|z| z.id.clone())
    }

    /// Enter on the day close: a pending day asks to run its Z, a Z shows its text.
    fn day_enter(&mut self) -> Vec<Effect> {
        let d = &self.cash.day;
        if let Some(p) = d.pending.get(d.cursor) {
            if d.running {
                return Vec::new();
            }
            if !self.can_close_day() {
                self.cash.day.notice = Some(notice("z_denied", DAY_CLOSE, Tone::Bad));
                return Vec::new();
            }
            self.cash.day.confirm_day = Some(p.day.clone());
            self.cash.day.notice = None;
            self.confirm = Some(Confirm::RunZ);
            return Vec::new();
        }
        match self.selected_z() {
            Some(id) => {
                self.cash.z_text = TextPane::asking(&id, false);
                vec![self.cash_call(Call::ZText { id, copy: false })]
            }
            None => Vec::new(),
        }
    }

    /// Opens the highlighted row of the list on view.
    fn open_cash(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::CashSessions => {
                let l = &self.cash.sessions;
                match l.rows.get(l.cursor).map(|s| s.id.clone()) {
                    Some(id) => self.open_session(id),
                    None => Vec::new(),
                }
            }
            Screen::CashMovements => self.show_slip(false),
            Screen::DayClose => self.day_enter(),
            _ => Vec::new(),
        }
    }

    /// Moves the highlight of the list on view; past the last row the next page loads.
    fn move_cash(&mut self, up: bool) -> Vec<Effect> {
        let (cursor, count, next) = match self.screen() {
            Screen::CashSessions => {
                let l = &mut self.cash.sessions;
                (&mut l.cursor, l.rows.len(), l.next.is_some())
            }
            Screen::CashMovements => {
                let l = &mut self.cash.movements;
                (&mut l.cursor, l.rows.len(), l.next.is_some())
            }
            Screen::CashSessionDetail => {
                let v = &mut self.cash.session;
                (&mut v.cursor, v.movements.len(), false)
            }
            Screen::DayClose => {
                let d = &mut self.cash.day;
                (
                    &mut d.cursor,
                    d.pending.len() + d.z.rows.len(),
                    d.z.next.is_some(),
                )
            }
            Screen::CashReasons => {
                let count = self.cash.reasons.len();
                let row = if up {
                    self.cash.editor.cursor.saturating_sub(1)
                } else {
                    (self.cash.editor.cursor + 1).min(count.saturating_sub(1))
                };
                self.select_reason(row);
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        if up {
            *cursor = cursor.saturating_sub(1);
        } else if *cursor + 1 < count {
            *cursor += 1;
        } else if next {
            return self.more_cash();
        }
        Vec::new()
    }

    /// Keys on the cash screens (after the F-keys of the header).
    pub(super) fn cash_capture(&mut self, p: Press, screen: Screen) -> Step {
        for (i, tab) in TABS.iter().enumerate() {
            let digit = char::from_digit(u32::try_from(i + 1).unwrap_or(0), 10).unwrap_or('0');
            if p.ctrl_char(digit) {
                return Step::handled(self.go(*tab));
            }
        }
        if p.f(5) {
            return Step::handled(self.reload_cash());
        }
        if p.plain(PressKey::Up) || p.plain(PressKey::Down) {
            return Step::handled(self.move_cash(p.plain(PressKey::Up)));
        }
        if screen == Screen::CashMovements {
            let photo = if p.ctrl_char('u') {
                Some("cash-photo-add")
            } else if p.ctrl_char('g') {
                Some("cash-photo-next")
            } else if p.ctrl_char('e') {
                Some("cash-photo-zoom")
            } else if p.plain(PressKey::Escape) && self.cash.photos.zoom {
                Some("cash-photo-close")
            } else {
                None
            };
            if let Some(action) = photo {
                return Step::handled(self.cash_action(action).unwrap_or_default());
            }
        }
        let action = match screen {
            Screen::CashSessions | Screen::CashMovements | Screen::DayClose => {
                if p.plain(PressKey::Enter) {
                    "cash-open"
                } else if p.ctrl_char('l') {
                    "cash-more"
                } else if p.ctrl_char('p') && screen != Screen::CashSessions {
                    "cash-reprint"
                } else if p.plain(PressKey::Escape) {
                    self.focus("list");
                    return Step::handled(Vec::new());
                } else {
                    return Step::default();
                }
            }
            Screen::CashSessionDetail => {
                if p.plain(PressKey::Escape) {
                    "cash-session-back"
                } else if p.ctrl_char('p') {
                    "cash-reprint"
                } else {
                    return Step::default();
                }
            }
            Screen::CashReasons => {
                if p.ctrl_char('n') {
                    "cash-reason-new"
                } else if p.ctrl_char('s') {
                    "cash-reason-save"
                } else if p.ctrl_shift_char('a') {
                    "cash-reason-archive"
                } else if p.plain(PressKey::Escape) {
                    return Step::handled(self.go(Screen::CashSessions));
                } else {
                    return Step::default();
                }
            }
            _ => return Step::default(),
        };
        Step::handled(self.cash_action(action).unwrap_or_default())
    }

    /// A button of the section; `None` when not one of them.
    pub(super) fn cash_action(&mut self, id: &str) -> Option<Vec<Effect>> {
        Some(match id {
            "nav-cash" | "tab-cash-sessions" | "cash-session-back" => self.go(Screen::CashSessions),
            "tab-cash-movements" => self.go(Screen::CashMovements),
            "tab-day-close" => self.go(Screen::DayClose),
            "tab-cash-reasons" => self.go(Screen::CashReasons),
            "cash-open" => self.open_cash(),
            "cash-more" => self.more_cash(),
            "cash-refresh" => self.reload_cash(),
            "cash-reprint" => self.reprint(),
            "cash-search" => {
                self.cash_bump();
                self.reload_cash()
            }
            "cash-clear" => {
                self.cash.filters = Filters::default();
                self.cash_bump();
                self.reload_cash()
            }
            "cash-reason-new" => {
                if self.reasons_read_only().is_some() {
                    return Some(Vec::new());
                }
                self.cash.editor = ReasonEditor {
                    cursor: self.cash.editor.cursor,
                    replica: self.cash.editor.replica,
                    active: true,
                    direction: 1,
                    sort: (self.cash.reasons.len() + 1).to_string(),
                    ..ReasonEditor::default()
                };
                self.cash_bump();
                self.focus("cash-reason-name");
                Vec::new()
            }
            "cash-reason-save" => self.save_reason(),
            "cash-photo-add" => self.add_photo(),
            "cash-photo-next" => {
                self.next_photo();
                Vec::new()
            }
            "cash-photo-zoom" => {
                self.zoom_photo();
                Vec::new()
            }
            "cash-photo-close" => {
                self.cash.photos.zoom = false;
                Vec::new()
            }
            "cash-reason-archive" => {
                let e = &self.cash.editor;
                if e.id.is_some() && !e.archived && self.reasons_editable() {
                    self.confirm = Some(Confirm::CashReasonArchive);
                }
                Vec::new()
            }
            _ => return None,
        })
    }

    fn select_reason(&mut self, row: usize) {
        let Some(r) = self.cash.reasons.get(row) else {
            return;
        };
        let replica = self.cash.editor.replica;
        self.cash.editor = ReasonEditor {
            cursor: row,
            id: Some(r.id.clone()),
            name: r.name.clone(),
            direction: DIRECTIONS
                .iter()
                .position(|d| *d == r.direction)
                .unwrap_or(0),
            note_required: r.note_required,
            active: r.active,
            sort: r.sort.to_string(),
            archived: r.archived,
            replica,
            ..ReasonEditor::default()
        };
        self.cash_bump();
    }

    fn save_reason(&mut self) -> Vec<Effect> {
        if !self.reasons_editable() {
            self.cash.editor.notice = self.reasons_read_only();
            return Vec::new();
        }
        let e = &mut self.cash.editor;
        if e.archived || e.saving {
            return Vec::new();
        }
        let name = e.name.trim();
        if name.is_empty() || name.chars().count() > 60 {
            e.notice = Some(Notice::bad("invalid_reason_name"));
            return Vec::new();
        }
        let Ok(sort) = e.sort.trim().parse::<i32>() else {
            e.notice = Some(Notice::bad("invalid_position"));
            return Vec::new();
        };
        let id = e.id.clone().unwrap_or_else(super::form::new_id);
        let body = CashReasonBody {
            name: name.to_owned(),
            direction: DIRECTIONS[e.direction.min(DIRECTIONS.len() - 1)].to_owned(),
            note_required: e.note_required,
            active: e.active,
            sort,
        };
        e.saving = true;
        e.notice = None;
        vec![self.cash_call(Call::PutReason { id, body })]
    }

    /// A confirmation of the section was accepted (the Z run, a reason's archive).
    pub(super) fn cash_confirmed(&mut self, confirm: Confirm) -> Vec<Effect> {
        if confirm == Confirm::CashReasonArchive {
            return match self.cash.editor.id.clone() {
                Some(id) => {
                    self.cash.editor.saving = true;
                    vec![self.cash_call(Call::ArchiveReason(id))]
                }
                None => Vec::new(),
            };
        }
        let (Some(day), Some(branch)) = (
            self.cash.day.confirm_day.take(),
            self.cash.day.branch.clone(),
        ) else {
            return Vec::new();
        };
        // A retry of a run that got no answer keeps its key (the server answers the same Z).
        let key = match &self.cash.day.run_key {
            Some((d, k)) if *d == day => k.clone(),
            _ => super::key(),
        };
        self.cash.day.run_key = Some((day.clone(), key.clone()));
        self.cash.day.running = true;
        vec![self.cash_call(Call::RunZ {
            body: ZRunRequest {
                day,
                branch_id: Some(branch),
            },
            key,
        })]
    }

    /// A text field of the section.
    pub(super) fn cash_edit(&mut self, kind: &str, field: &str, value: String) {
        match (kind, field) {
            ("cash-filter", "from") => self.cash.filters.from = value,
            ("cash-filter", "until") => self.cash.filters.until = value,
            ("cash-reason", "name") => self.cash.editor.name = value,
            ("cash-reason", "sort") => self.cash.editor.sort = value,
            _ => {}
        }
    }

    /// A check box of the section; `None` when not one of them.
    pub(super) fn cash_toggle(&mut self, kind: &str, field: &str, on: bool) -> Option<Vec<Effect>> {
        if (kind, field) == ("cash-filter", "missing_nota") {
            self.cash.filters.missing_nota = on;
            return Some(self.reload_cash());
        }
        if kind != "cash-reason" {
            return None;
        }
        let e = &mut self.cash.editor;
        match field {
            "note_required" => e.note_required = on,
            "active" => e.active = on,
            _ => {}
        }
        Some(Vec::new())
    }

    /// A picker of the section; `None` when not one of them.
    pub(super) fn cash_choose(
        &mut self,
        kind: &str,
        field: &str,
        index: usize,
    ) -> Option<Vec<Effect>> {
        if (kind, field) == ("cash-reason", "direction") {
            // The direction is fixed once saved (spec 2.7 §17 reading 1).
            if self.cash.editor.id.is_none() {
                self.cash.editor.direction = index.min(DIRECTIONS.len() - 1);
            }
            return Some(Vec::new());
        }
        if kind != "cash-filter" {
            return None;
        }
        let f = &mut self.cash.filters;
        match field {
            "branch" => f.branch = index,
            "register" => f.register = index,
            "cashier" => f.cashier = index,
            "session" => f.session = index,
            "status" => f.status = index.min(STATUSES.len() - 1),
            "kind" => f.kind = index.min(KINDS.len() - 1),
            "reason" => f.reason = index,
            _ => return None,
        }
        Some(self.reload_cash())
    }

    /// A click on a row of the section; `None` when not one of them.
    pub(super) fn cash_pick(&mut self, kind: &str, row: usize) -> Option<Vec<Effect>> {
        let c = &mut self.cash;
        Some(match kind {
            "cash-session-row" => {
                if row < c.sessions.rows.len() {
                    c.sessions.cursor = row;
                }
                Vec::new()
            }
            "cash-movement-row" => {
                if row < c.movements.rows.len() {
                    c.movements.cursor = row;
                }
                Vec::new()
            }
            "cash-session-movement" => {
                if row < c.session.movements.len() {
                    c.session.cursor = row;
                }
                Vec::new()
            }
            "cash-day-row" => {
                if row < c.day.pending.len() + c.day.z.rows.len() {
                    c.day.cursor = row;
                }
                Vec::new()
            }
            "cash-reason-row" => {
                self.select_reason(row);
                Vec::new()
            }
            "cash-photo" => {
                if row < c.photos.photos.len() {
                    c.photos.selected = row;
                }
                Vec::new()
            }
            _ => return None,
        })
    }

    /// `cash.moved` reloads the movements of the session on view; `day.closed` the day close.
    pub(super) fn cash_event(&mut self, event: &ServerEvent) -> Vec<Effect> {
        match (event.kind.as_str(), self.screen()) {
            ("cash.moved", Screen::CashSessionDetail)
                if event.data["session_id"].as_str() == Some(self.cash.session.id.as_str()) =>
            {
                let id = self.cash.session.id.clone();
                vec![self.cash_call(Call::SessionMovements(id))]
            }
            ("day.closed", Screen::DayClose) if !self.cash.day.running => self.reload_day(),
            ("cash.moved", Screen::CashMovements) => {
                // A listed movement changed (a photo): read it again.
                let id = event.data["movement_id"].as_str().unwrap_or_default();
                let known = self.cash.movements.rows.iter().any(|m| m.id == id)
                    || self.cash.photos.movement_id == id;
                if known && !id.is_empty() {
                    vec![self.cash_call(Call::Movement(id.to_owned()))]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    /// The registers, cashiers and sessions seen in a page of sessions join the filters.
    fn learn_sessions(&mut self, rows: &[CashierSession]) {
        for s in rows {
            let code = s
                .register_code
                .clone()
                .unwrap_or_else(|| s.number.split('-').nth(1).unwrap_or_default().to_owned());
            learn(&mut self.cash.registers, &s.register_id, code);
            if let Some(name) = &s.cashier_name {
                learn(&mut self.cash.cashiers, &s.user_id, name.clone());
            }
            if !self.cash.seen_sessions.iter().any(|(id, _)| *id == s.id) {
                self.cash
                    .seen_sessions
                    .push((s.id.clone(), s.number.clone()));
            }
        }
        self.cash.registers.sort_by(|a, b| a.1.cmp(&b.1));
        self.cash.cashiers.sort_by(|a, b| a.1.cmp(&b.1));
        self.cash.seen_sessions.sort_by(|a, b| b.1.cmp(&a.1));
    }

    /// The answer to a call of the section.
    #[allow(clippy::too_many_lines)] // one arm per answer
    pub(super) fn cash_reply(&mut self, reply: Reply) -> Vec<Effect> {
        if self.session.is_none() {
            return Vec::new();
        }
        match reply {
            Reply::Sessions(_, seq, _) if seq != self.cash.sessions.seq => {}
            Reply::Sessions(append, _, result) => match result {
                Ok(page) => {
                    self.learn_sessions(&page.items);
                    self.cash
                        .sessions
                        .take(append, page.items, page.next_before_id);
                }
                Err(e) => {
                    self.cash.sessions.loading = false;
                    self.cash.sessions.notice = self.cash_failed(&e);
                }
            },
            Reply::Session(id, _) if id != self.cash.session.id => {}
            Reply::Session(_, result) => match *result {
                Ok(detail) => {
                    let open = detail.session.status == "open";
                    let id = detail.session.id.clone();
                    self.cash.session.detail = Some(Box::new(detail));
                    return if open {
                        self.ask_x()
                    } else {
                        self.cash.session.text = TextPane::asking(&id, false);
                        vec![self.cash_call(Call::CloseText { id, copy: false })]
                    };
                }
                Err(e) => {
                    self.cash.session.text.loading = false;
                    self.cash.session.notice = self.cash_failed(&e);
                }
            },
            Reply::SessionMovements(id, _) if id != self.cash.session.id => {}
            Reply::SessionMovements(_, result) => match result {
                Ok(page) => {
                    self.cash.session.movements = page.items;
                    let last = self.cash.session.movements.len().saturating_sub(1);
                    self.cash.session.cursor = self.cash.session.cursor.min(last);
                }
                Err(e) => self.cash.session.notice = self.cash_failed(&e),
            },
            Reply::Text(pane, id, result) => {
                let failure = result.as_ref().err().map(|e| self.text_failed(pane, e));
                let target = match pane {
                    Pane::Session => &mut self.cash.session.text,
                    Pane::Slip => &mut self.cash.slip,
                    Pane::Z => &mut self.cash.z_text,
                };
                if target.id != id {
                    return Vec::new();
                }
                target.loading = false;
                match result {
                    Ok(text) => {
                        target.lines = text.lines;
                        target.copy = text.copy;
                        target.notice = None;
                    }
                    Err(e) => {
                        target.lines.clear();
                        target.notice = failure.flatten();
                        // Closed since it was opened: its close report instead.
                        if pane == Pane::Session && e.code() == Some("session_closed") {
                            return self.open_session(id);
                        }
                    }
                }
            }
            Reply::Movements(_, seq, _) if seq != self.cash.movements.seq => {}
            Reply::Movements(append, _, result) => match result {
                Ok(page) => {
                    for m in &page.items {
                        let code = m.number.split('-').nth(1).unwrap_or_default().to_owned();
                        learn(&mut self.cash.registers, &m.register_id, code);
                    }
                    self.cash.registers.sort_by(|a, b| a.1.cmp(&b.1));
                    self.cash
                        .movements
                        .take(append, page.items, page.next_before_id);
                }
                Err(e) => {
                    self.cash.movements.loading = false;
                    self.cash.movements.notice = self.cash_failed(&e);
                }
            },
            Reply::Pending(branch, _) if Some(&branch) != self.cash.day.branch.as_ref() => {}
            Reply::Pending(_, result) => {
                self.cash.day.pending_loading = false;
                match result {
                    Ok(days) => {
                        self.cash.day.pending = days;
                        self.clamp_day_cursor();
                    }
                    Err(e) => {
                        self.cash.day.pending.clear();
                        self.cash.day.notice = self.cash_failed(&e);
                    }
                }
            }
            Reply::ZList(_, seq, _) if seq != self.cash.day.z.seq => {}
            Reply::ZList(append, _, result) => match result {
                Ok(page) => {
                    let cursor = self.cash.day.cursor;
                    self.cash
                        .day
                        .z
                        .take(append, page.items, page.next_before_id);
                    self.cash.day.cursor = cursor;
                    if let Some(id) = self.cash.day.select.take()
                        && let Some(i) = self.cash.day.z.rows.iter().position(|z| z.id == id)
                    {
                        self.cash.day.cursor = self.cash.day.pending.len() + i;
                    }
                    self.clamp_day_cursor();
                }
                Err(e) => {
                    self.cash.day.z.loading = false;
                    self.cash.day.z.notice = self.cash_failed(&e);
                }
            },
            Reply::ZRun(day, result) => return self.z_ran(&day, *result),
            Reply::Movement(result) => match result {
                Ok(m) => {
                    let shown = self.cash.photos.movement_id == m.id;
                    let (id, ids) = (m.id.clone(), m.photo_ids.clone());
                    if let Some(row) = self.cash.movements.rows.iter_mut().find(|r| r.id == m.id) {
                        *row = m;
                    }
                    if shown {
                        return self.show_photos(&id, &ids);
                    }
                }
                Err(e) => {
                    // A failed refresh leaves the row as it was; only an ended login matters.
                    let _ = self.failed(&e);
                }
            },
            Reply::Photo(movement_id, photo_id, result) => {
                self.photo_read(&movement_id, &photo_id, result);
            }
            Reply::Picked(movement_id, result) => return self.photo_picked(movement_id, result),
            Reply::Uploaded(upload, result) => return self.photo_uploaded(upload, result),
            Reply::Reasons(result) => match result {
                Ok(mut list) => {
                    list.sort_by(|a, b| {
                        (&a.direction, a.sort, &a.name).cmp(&(&b.direction, b.sort, &b.name))
                    });
                    self.cash.reasons = list;
                    let keep = self.cash.editor.id.clone();
                    let row = keep
                        .and_then(|id| self.cash.reasons.iter().position(|r| r.id == id))
                        .unwrap_or(self.cash.editor.cursor);
                    let notice = self.cash.editor.notice.take();
                    if self.cash.editor.id.is_some() {
                        self.select_reason(row.min(self.cash.reasons.len().saturating_sub(1)));
                    } else if self.cash.editor.name.is_empty() {
                        self.select_reason(0);
                    }
                    // else: a new reason being typed is kept as it is.
                    self.cash.editor.notice = notice;
                }
                Err(e) => self.cash.editor.notice = self.cash_failed(&e),
            },
            Reply::ReasonSaved(archived, result) => {
                self.cash.editor.saving = false;
                match result {
                    Ok(r) => {
                        self.cash.editor.id = Some(r.id);
                        self.cash.editor.notice = Some(notice(
                            if archived {
                                "cash_reason_archived"
                            } else {
                                "saved"
                            },
                            "",
                            Tone::Good,
                        ));
                        return vec![self.cash_call(Call::Reasons {
                            all: self.can_manage_reasons(),
                        })];
                    }
                    Err(e) => {
                        if e.code() == Some("read_only_replica") {
                            self.cash.editor.replica = true;
                        }
                        self.cash.editor.notice = if e.code() == Some("invalid_field")
                            && e.problem().and_then(|p| p.field()) == Some("direction")
                        {
                            Some(Notice::bad("direction_fixed"))
                        } else {
                            self.cash_failed(&e)
                        };
                    }
                }
            }
        }
        Vec::new()
    }

    fn clamp_day_cursor(&mut self) {
        let d = &mut self.cash.day;
        d.cursor = d
            .cursor
            .min((d.pending.len() + d.z.rows.len()).saturating_sub(1));
    }

    /// The Z run answered: the new Z is shown, or the refusal says why.
    fn z_ran(&mut self, day: &str, result: Result<ZReport, ClientError>) -> Vec<Effect> {
        self.cash.day.running = false;
        match result {
            Ok(z) => {
                self.cash.day.run_key = None;
                self.cash.day.notice = Some(notice("z_done", z.report.number.clone(), Tone::Good));
                self.cash.day.select = Some(z.id.clone());
                self.cash.z_text = TextPane::asking(&z.id, false);
                let mut effects = self.reload_day();
                effects.push(self.cash_call(Call::ZText {
                    id: z.id,
                    copy: false,
                }));
                effects
            }
            Err(e) => {
                if !matches!(e, ClientError::Network(_)) {
                    self.cash.day.run_key = None;
                }
                let p = e.problem();
                self.cash.day.notice = match e.code() {
                    Some("sessions_open") => Some(notice(
                        "z_sessions_open",
                        p.map(|p| p.sessions().join(", ")).unwrap_or_default(),
                        Tone::Bad,
                    )),
                    Some("nothing_to_close") => Some(notice("z_nothing_to_close", day, Tone::Bad)),
                    Some("invalid_field") if p.and_then(|p| p.field()) == Some("day") => {
                        Some(notice("z_future_day", day, Tone::Bad))
                    }
                    Some("permission_denied") => Some(notice("z_denied", DAY_CLOSE, Tone::Bad)),
                    Some("read_only_replica") => Some(Notice::bad("z_replica")),
                    _ => self.cash_failed(&e),
                };
                // The days may have changed (a session closed or opened meanwhile).
                if e.code().is_some() {
                    return self.reload_day();
                }
                Vec::new()
            }
        }
    }

    /// Why a text could not be shown.
    fn text_failed(&mut self, pane: Pane, e: &ClientError) -> Option<Notice> {
        let permission = e.problem().and_then(|p| p.permission());
        if pane == Pane::Session
            && e.code() == Some("permission_denied")
            && permission != Some("sale.reprint")
        {
            return Some(notice("x_denied", SESSION_VIEW_EXPECTED, Tone::Neutral));
        }
        if e.code() == Some("permission_denied") && permission == Some("sale.reprint") {
            return Some(notice("cash_reprint_denied", "sale.reprint", Tone::Bad));
        }
        self.cash_failed(e)
    }

    /// A failed call: the login ended, or the message (a missing `report.sales` says so).
    fn cash_failed(&mut self, e: &ClientError) -> Option<Notice> {
        if e.code() == Some("permission_denied")
            && e.problem().and_then(|p| p.permission()) == Some(REPORT_SALES)
        {
            return Some(Self::denied_cash());
        }
        self.failed(e)
    }
}

/// Adds `(id, label)` to `list` when the ID is new.
fn learn(list: &mut Vec<(String, String)>, id: &str, label: String) {
    if !id.is_empty() && !list.iter().any(|(x, _)| x == id) {
        list.push((id.to_owned(), label));
    }
}

#[cfg(test)]
mod tests {
    use sultan_client::{Me, Problem};

    use super::super::{Msg, Session};
    use super::*;

    fn office(permissions: &[&str], branch: &str) -> Controller {
        let mut c = Controller::new("owner");
        c.session = Some(Session {
            token: "t".into(),
            me: Me {
                user_id: "u".into(),
                branch_id: branch.into(),
                permissions_anywhere: permissions.iter().map(|p| (*p).to_owned()).collect(),
                ..Me::default()
            },
            display_name: "Owner".into(),
        });
        c.screen = Some(Screen::Products);
        c
    }

    fn all() -> Controller {
        office(
            &[
                REPORT_SALES,
                SESSION_VIEW_EXPECTED,
                DAY_CLOSE,
                TENDER_MANAGE,
            ],
            "b",
        )
    }

    fn api(status: u16, code: &str) -> ClientError {
        ClientError::Api(Problem::new(status, code, code))
    }

    fn calls(effects: &[Effect]) -> Vec<&Call> {
        effects
            .iter()
            .filter_map(|e| match e {
                Effect::Cash { call, .. } => Some(call),
                _ => None,
            })
            .collect()
    }

    fn reply(c: &mut Controller, r: Reply) -> Vec<Effect> {
        c.update(Msg::Cash(Box::new(r))).effects
    }

    fn key(c: &mut Controller, text: &str, ctrl: bool) -> Vec<Effect> {
        c.update(Msg::Capture(Press::parse(text, ctrl, false)))
            .effects
    }

    const DOWN: &str = "\u{F701}";

    fn session(id: &str) -> CashierSession {
        serde_json::from_value(serde_json::json!({
            "id": id, "number": format!("JKT01-R1-S-270115-{id}"), "user_id": "u",
            "register_id": "r1", "branch_id": "b", "opening_cash": 0, "opened_at": 0,
            "status": "closed",
        }))
        .unwrap()
    }

    fn moved(id: &str) -> CashMovement {
        CashMovement {
            id: id.into(),
            number: format!("JKT01-R1-K-270115-{id}"),
            register_id: "r1".into(),
            ..CashMovement::default()
        }
    }

    #[test]
    fn learning_keeps_the_first_label_and_skips_empty_ids() {
        let mut list = Vec::new();
        learn(&mut list, "r1", "R1".into());
        learn(&mut list, "r1", "other".into());
        learn(&mut list, "", "none".into());
        assert_eq!(list, [("r1".to_owned(), "R1".to_owned())]);
    }

    #[test]
    fn lists_page_on_and_drop_stale_answers() {
        let mut c = all();
        let effects = c
            .update(Msg::Capture(Press::parse("\u{F70C}", false, false)))
            .effects;
        assert_eq!(c.screen(), Screen::CashSessions);
        let Some(Call::Sessions { seq, .. }) = calls(&effects)
            .into_iter()
            .find(|c| matches!(c, Call::Sessions { .. }))
        else {
            panic!("no sessions call: {effects:?}");
        };
        let seq = *seq;
        // An older answer is dropped.
        let page = |ids: &[&str], next: Option<&str>| SessionPage {
            items: ids.iter().map(|i| session(i)).collect(),
            next_before_id: next.map(str::to_owned),
        };
        reply(
            &mut c,
            Reply::Sessions(false, seq - 1, Ok(page(&["x"], None))),
        );
        assert!(c.cash.sessions.rows.is_empty());
        reply(
            &mut c,
            Reply::Sessions(false, seq, Ok(page(&["01", "02"], Some("02")))),
        );
        assert_eq!(c.cash.sessions.rows.len(), 2);
        assert_eq!(c.cash.registers, [("r1".to_owned(), "R1".to_owned())]);
        key(&mut c, DOWN, false);
        let more = key(&mut c, DOWN, false);
        assert!(matches!(
            calls(&more)[..],
            [Call::Sessions { append: true, .. }]
        ));
        assert!(key(&mut c, "l", true).is_empty(), "one page at a time");
        reply(&mut c, Reply::Sessions(true, seq, Ok(page(&["03"], None))));
        assert_eq!(c.cash.sessions.rows.len(), 3);
        reply(
            &mut c,
            Reply::Sessions(false, seq, Err(api(500, "internal"))),
        );
        assert!(c.cash.sessions.notice.is_some());

        // Movements: the same paging (Ctrl+L), and a refused list names report.sales.
        let effects = key(&mut c, "2", true);
        let Some(Call::Movements { seq, .. }) = calls(&effects)
            .into_iter()
            .find(|c| matches!(c, Call::Movements { .. }))
        else {
            panic!("{effects:?}");
        };
        let seq = *seq;
        let page = CashMovementPage {
            items: vec![moved("01")],
            next_before_id: Some("01".into()),
        };
        reply(&mut c, Reply::Movements(false, seq, Ok(page)));
        let more = key(&mut c, "l", true);
        assert!(matches!(
            calls(&more)[..],
            [Call::Movements { append: true, .. }]
        ));
        let denied = ClientError::Api(
            Problem::new(403, "permission_denied", "x").with("permission", REPORT_SALES),
        );
        reply(&mut c, Reply::Movements(true, seq, Err(denied)));
        assert_eq!(
            c.cash.movements.notice.as_ref().map(|n| n.code),
            Some("cash_denied")
        );
        // Clicks on rows out of range are ignored.
        for kind in [
            "cash-session-row",
            "cash-movement-row",
            "cash-session-movement",
            "cash-day-row",
        ] {
            assert_eq!(c.cash_pick(kind, 99), Some(Vec::new()));
        }
        assert_eq!(c.cash_pick("other", 0), None);
        assert_eq!(c.cash.sessions.cursor, 1);
    }

    #[test]
    fn the_day_close_needs_a_branch_and_pages_its_z_reports() {
        let mut c = office(&[REPORT_SALES, DAY_CLOSE], "");
        assert!(
            c.update(Msg::Action("tab-day-close".into()))
                .effects
                .iter()
                .all(|e| {
                    !matches!(
                        e,
                        Effect::Cash {
                            call: Call::Pending(_),
                            ..
                        }
                    )
                })
        );
        assert_eq!(
            c.cash.day.notice.as_ref().map(|n| n.code),
            Some("pick_branch")
        );

        let mut c = all();
        let effects = c.update(Msg::Action("tab-day-close".into())).effects;
        let Some(Call::ZList { seq, .. }) = calls(&effects)
            .into_iter()
            .find(|c| matches!(c, Call::ZList { .. }))
        else {
            panic!("{effects:?}");
        };
        let seq = *seq;
        reply(
            &mut c,
            Reply::Pending("other".into(), Ok(vec![PendingDay::default()])),
        );
        assert!(c.cash.day.pending.is_empty(), "another branch's answer");
        reply(
            &mut c,
            Reply::Pending("b".into(), Err(api(500, "internal"))),
        );
        assert!(c.cash.day.notice.is_some());
        let z = |id: &str| ZSummary {
            id: id.into(),
            ..ZSummary::default()
        };
        reply(
            &mut c,
            Reply::ZList(
                false,
                seq,
                Ok(ZReportPage {
                    items: vec![z("z1")],
                    next_before_id: Some("z1".into()),
                }),
            ),
        );
        let more = key(&mut c, DOWN, false);
        assert!(matches!(
            calls(&more)[..],
            [Call::ZList { append: true, .. }]
        ));
        reply(&mut c, Reply::ZList(true, seq, Err(api(500, "internal"))));
        assert!(c.cash.day.z.notice.is_some());
        // Without `day.close` Enter on a day says so.
        let mut c = office(&[REPORT_SALES], "b");
        c.update(Msg::Action("tab-day-close".into()));
        reply(
            &mut c,
            Reply::Pending(
                "b".into(),
                Ok(vec![PendingDay {
                    day: "2027-01-15".into(),
                    open: 0,
                    closed: 1,
                }]),
            ),
        );
        key(&mut c, "\n", false);
        assert_eq!(c.confirm, None);
        assert_eq!(c.cash.day.notice.as_ref().map(|n| n.code), Some("z_denied"));
    }

    #[test]
    fn a_session_closed_meanwhile_shows_its_close_report() {
        let mut c = all();
        c.update(Msg::Action("nav-cash".into()));
        c.cash.sessions.rows = vec![session("01")];
        let effects = c.update(Msg::Action("cash-open".into())).effects;
        assert_eq!(c.screen(), Screen::CashSessionDetail);
        assert_eq!(calls(&effects).len(), 2);
        // Answers about another session are dropped; a failure says why.
        reply(
            &mut c,
            Reply::SessionMovements("other".into(), Ok(CashMovementPage::default())),
        );
        reply(
            &mut c,
            Reply::Session("01".into(), Box::new(Err(api(404, "not_found")))),
        );
        assert_eq!(
            c.cash.session.notice.as_ref().map(|n| n.code),
            Some("not_found")
        );
        reply(
            &mut c,
            Reply::SessionMovements("01".into(), Err(api(500, "internal"))),
        );
        // The X of a session closed since: the detail is read again.
        c.cash.session.text = TextPane::asking("01", false);
        let again = reply(
            &mut c,
            Reply::Text(Pane::Session, "01".into(), Err(api(409, "session_closed"))),
        );
        assert!(matches!(
            calls(&again)[..],
            [Call::Session(_), Call::SessionMovements(_)]
        ));
        // A text for another document is dropped; events of other sessions are ignored.
        reply(
            &mut c,
            Reply::Text(Pane::Session, "x".into(), Ok(CashText::default())),
        );
        assert!(c.cash.session.text.loading);
        let event = ServerEvent {
            kind: "cash.moved".into(),
            data: serde_json::json!({ "session_id": "other" }),
        };
        assert!(c.cash_event(&event).is_empty());
    }

    #[test]
    fn a_saved_reasons_direction_cannot_change() {
        let mut c = all();
        c.update(Msg::Action("tab-cash-reasons".into()));
        let reason = CashReason {
            id: "r".into(),
            name: "Parkir".into(),
            direction: "out".into(),
            active: true,
            ..CashReason::default()
        };
        reply(&mut c, Reply::Reasons(Ok(vec![reason])));
        assert_eq!(c.cash.editor.id.as_deref(), Some("r"));
        assert_eq!(c.cash.editor.direction, 1);
        let effects = c.update(Msg::Action("cash-reason-save".into())).effects;
        assert!(matches!(calls(&effects)[..], [Call::PutReason { .. }]));
        assert!(
            c.update(Msg::Action("cash-reason-save".into()))
                .effects
                .is_empty(),
            "saving"
        );
        let refused =
            ClientError::Api(Problem::new(422, "invalid_field", "x").with("field", "direction"));
        reply(&mut c, Reply::ReasonSaved(false, Err(refused)));
        assert_eq!(
            c.cash.editor.notice.as_ref().map(|n| n.code),
            Some("direction_fixed")
        );
        reply(&mut c, Reply::Reasons(Err(api(500, "internal"))));
        assert!(c.cash.editor.notice.is_some());
        assert_eq!(c.cash_toggle("other", "active", true), None);
        assert_eq!(c.cash_choose("other", "x", 0), None);
        assert_eq!(c.cash_choose("cash-filter", "x", 0), None);
    }

    fn photo(id: &str, movement: &str) -> NotaPhoto {
        NotaPhoto {
            id: id.into(),
            cash_movement_id: movement.into(),
            ..NotaPhoto::default()
        }
    }

    fn upload(movement: &str, id: &str) -> Upload {
        Upload {
            movement_id: movement.into(),
            photo_id: id.into(),
            bytes: vec![1, 2, 3],
        }
    }

    #[test]
    fn ctrl_u_is_refused_on_screen_without_cash_out_or_at_three_photos() {
        let mut c = office(&[REPORT_SALES], "b");
        c.update(Msg::Action("tab-cash-movements".into()));
        assert!(
            c.update(Msg::Action("cash-photo-add".into()))
                .effects
                .is_empty()
        );
        let mut m = moved("01");
        m.photo_ids = vec!["p1".into()];
        c.cash.movements.rows = vec![m];
        let effects = key(&mut c, "u", true);
        // The slip and the stored photo are read; nothing is picked.
        assert!(matches!(
            calls(&effects)[..],
            [Call::Slip { .. }, Call::Photo { .. }]
        ));
        assert_eq!(
            c.cash
                .photos
                .notice
                .as_ref()
                .map(|n| (n.code, n.arg.as_str())),
            Some(("photo_denied", CASH_OUT))
        );
        let mut c = all();
        c.session
            .as_mut()
            .unwrap()
            .me
            .permissions_anywhere
            .push(CASH_OUT.into());
        c.update(Msg::Action("tab-cash-movements".into()));
        let mut m = moved("01");
        m.photo_ids = vec!["p1".into(), "p2".into(), "p3".into()];
        c.cash.movements.rows = vec![m];
        c.update(Msg::Action("cash-open".into()));
        assert!(key(&mut c, "u", true).is_empty());
        assert_eq!(
            c.cash.photos.notice.as_ref().map(|n| n.code),
            Some("photo_limit")
        );
        // Busy: one at a time.
        c.cash.movements.rows[0].photo_ids.clear();
        assert!(matches!(
            calls(&key(&mut c, "u", true))[..],
            [Call::PickPhoto(_)]
        ));
        assert!(key(&mut c, "u", true).is_empty());
    }

    #[test]
    fn photo_answers_and_refusals() {
        let mut c = all();
        c.update(Msg::Action("tab-cash-movements".into()));
        c.cash.movements.rows = vec![moved("01")];
        c.update(Msg::Action("cash-open".into()));
        c.cash.photos.busy = true;
        // A cancelled picker: nothing.
        assert!(reply(&mut c, Reply::Picked("01".into(), Ok(None))).is_empty());
        assert!(!c.cash.photos.busy && c.cash.photos.notice.is_none());
        // The server's refusals.
        let limit = ClientError::Api(Problem::new(409, "photo_limit", "x").with("max", 3));
        reply(&mut c, Reply::Uploaded(upload("01", "p"), Err(limit)));
        assert_eq!(
            c.cash
                .photos
                .notice
                .as_ref()
                .map(|n| (n.code, n.arg.as_str())),
            Some(("photo_limit", "3"))
        );
        let denied = ClientError::Api(
            Problem::new(403, "permission_denied", "x").with("permission", CASH_OUT),
        );
        reply(&mut c, Reply::Uploaded(upload("01", "p"), Err(denied)));
        assert_eq!(
            c.cash.photos.notice.as_ref().map(|n| n.code),
            Some("photo_denied")
        );
        reply(
            &mut c,
            Reply::Picked("01".into(), Err(api(413, "payload_too_large"))),
        );
        assert_eq!(
            c.cash.photos.notice.as_ref().map(|n| n.code),
            Some("image_too_large")
        );
        assert!(c.cash.retry.is_none());
        // Added to a movement no longer shown (nor listed): only the message; another
        // movement's pending retry stays.
        c.cash.retry = Some(upload("01", "r"));
        assert!(
            reply(
                &mut c,
                Reply::Uploaded(upload("02", "p9"), Ok(photo("p9", "02")))
            )
            .is_empty()
        );
        assert_eq!(
            c.cash.photos.notice.as_ref().map(|n| n.code),
            Some("photo_added")
        );
        assert!(c.cash.photos.photos.is_empty());
        assert!(c.cash.retry.is_some());
        // Reads: a missing file, a failure, an answer for another movement.
        c.cash.photos.photos = vec![
            ("a".into(), PhotoImage::Loading),
            ("b".into(), PhotoImage::Loading),
        ];
        reply(
            &mut c,
            Reply::Photo("01".into(), "a".into(), Err(api(404, "not_found"))),
        );
        reply(
            &mut c,
            Reply::Photo("01".into(), "b".into(), Err(api(500, "internal"))),
        );
        reply(&mut c, Reply::Photo("02".into(), "a".into(), Ok(vec![1])));
        assert!(
            matches!(&c.cash.photos.photos[0].1, PhotoImage::Failed(n) if n.code == "photo_missing")
        );
        assert!(matches!(&c.cash.photos.photos[1].1, PhotoImage::Failed(_)));
        // Zoom needs a loaded photo; next cycles.
        c.update(Msg::Action("cash-photo-zoom".into()));
        assert!(!c.cash.photos.zoom);
        c.update(Msg::Action("cash-photo-next".into()));
        assert_eq!(c.cash.photos.selected, 1);
        c.update(Msg::Action("cash-photo-next".into()));
        assert_eq!(c.cash.photos.selected, 0);
        assert_eq!(c.cash_pick("cash-photo", 9), Some(Vec::new()));
        assert_eq!(c.cash.photos.selected, 0);
        // A refreshed movement: the row is replaced, its new photos read; a failure is ignored.
        let mut m = moved("01");
        m.photo_ids = vec!["a".into(), "c".into()];
        let effects = reply(&mut c, Reply::Movement(Ok(m)));
        assert!(matches!(
            calls(&effects)[..],
            [Call::Photo { photo_id, .. }] if photo_id == "c"
        ));
        assert_eq!(c.cash.movements.rows[0].photo_ids.len(), 2);
        assert!(reply(&mut c, Reply::Movement(Err(api(500, "internal")))).is_empty());
        // A login that ended while reading a photo logs out.
        reply(
            &mut c,
            Reply::Photo("01".into(), "c".into(), Err(api(401, "invalid_session"))),
        );
        assert!(c.session.is_none());
    }

    #[test]
    fn big_bytes_stay_out_of_debug_output() {
        let u = upload("m", "p");
        assert_eq!(
            format!("{u:?}"),
            r#"Upload { movement_id: "m", photo_id: "p", bytes: 3 }"#
        );
        assert_eq!(
            format!("{:?}", PhotoImage::Loaded(vec![0; 5])),
            "Loaded(5 bytes)"
        );
        assert_eq!(format!("{:?}", PhotoImage::Loading), "Loading");
        assert!(format!("{:?}", PhotoImage::Failed(Notice::bad("x"))).starts_with("Failed("));
    }

    #[test]
    fn the_missing_nota_filter_is_sent_and_cleared() {
        let mut c = all();
        c.update(Msg::Action("tab-cash-movements".into()));
        let effects = c
            .update(Msg::Toggle {
                kind: "cash-filter".into(),
                row: 0,
                field: "missing_nota".into(),
                on: true,
            })
            .effects;
        assert!(matches!(
            calls(&effects)[..],
            [Call::Movements {
                query: CashQuery {
                    missing_nota: Some(true),
                    ..
                },
                ..
            }]
        ));
        c.update(Msg::Action("cash-clear".into()));
        assert!(!c.cash.filters.missing_nota);
    }
}
