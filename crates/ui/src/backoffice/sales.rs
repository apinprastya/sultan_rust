//! The back office's sales screens (spec 2.6, phase S6), part of the [`Controller`] state
//! machine. F8 opens the section; a row of tabs (Ctrl+1 … Ctrl+4) switches between its screens:
//!
//! 1. **Sales** ([`Screen::Sales`]) — filters branch, register, date from / until (local
//!    `YYYY-MM-DD`), customer (member code or phone, looked up once), number and status; pages
//!    of 50 newest first with `before_id` ("load more", and moving past the last row loads the
//!    next page). Columns: number, local time, branch / register, cashier, customer, total,
//!    status. Enter opens the **sale detail** ([`Screen::SaleDetail`]): lines, discounts, tax,
//!    tenders (references masked), saved change, cash rounding, the VOID, the returns made against
//!    it (↑/↓ + Enter opens one), and the receipt text in the window language.
//! 2. **Returns** ([`Screen::Returns`]) — the same filters (status does not apply); Enter opens
//!    the **return detail** ([`Screen::ReturnDetail`]) with its lines and refund tenders, links to
//!    the original sale and the exchange sale, and the return receipt.
//! 3. **Tender types** ([`Screen::TenderTypes`], HQ, `tender.manage`) — every tender type with an
//!    editor: Ctrl+N new, Ctrl+S save, Ctrl+Shift+A archive (asks first). The kind is fixed once
//!    saved. Without `tender.manage` (or on a branch node, `read_only_replica`) it is read-only.
//! 4. **Sales settings** ([`Screen::SalesSettings`]) — `return_days` (0 = no limit) and the
//!    receipt width of each branch (32 / 42 / 48); Ctrl+S saves (HQ, `tender.manage`).
//!
//! The customer form's **purchase history** ([`Screen::CustomerHistory`], Ctrl+H on the form,
//! spec 2.5 §9): the customer's sales and returns across branches (merged customers included,
//! with their name), newest first, paged, VOID marked; Enter opens the sale or return detail and
//! Esc goes back.
//!
//! Reading needs `report.sales` (spec 2.6 §19 item 45); without it the lists say so.

use chrono::{Days, Local, NaiveDate, TimeZone};
use sultan_client::sales::TENDER_MANAGE;
use sultan_client::{
    BranchWidth, ClientError, Customer, DocQuery, HistoryItem, HistoryPage, Receipt, ReturnPage,
    ReturnSummary, Sale, SalePage, SaleReturn, SaleSummary, SalesSettings, TenderType,
    TenderTypeBody,
};

use super::{Confirm, Controller, Effect, Press, PressKey, Screen, Step, notice};
use crate::controller::{Notice, Tone};

/// `report.sales`: reading sales and returns in the back office.
pub const REPORT_SALES: &str = "report.sales";
/// The screens of the section, in tab order (Ctrl+1 … Ctrl+4).
pub const TABS: [Screen; 4] = [
    Screen::Sales,
    Screen::Returns,
    Screen::TenderTypes,
    Screen::SalesSettings,
];
/// Tender kinds in picker order (spec 2.6 D4).
pub const KINDS: [&str; 7] = [
    "cash",
    "card",
    "qris",
    "transfer",
    "ewallet",
    "customer_balance",
    "other",
];
/// Receipt widths in picker order.
pub const WIDTHS: [u16; 3] = [32, 42, 48];
/// Status filter choices: all, posted, voided.
pub const STATUSES: [&str; 3] = ["", "posted", "voided"];
/// Page size of the lists.
pub const PAGE: u32 = 50;
/// The `kind`s of [`super::Msg::Edit`] handled here.
pub const EDIT_KINDS: [&str; 3] = ["sales-filter", "tender", "sales-settings"];

/// Whether `screen` belongs to the sales section (the history belongs to the customer form).
#[must_use]
pub fn in_section(screen: Screen) -> bool {
    TABS.contains(&screen) || matches!(screen, Screen::SaleDetail | Screen::ReturnDetail)
}

/// A sales call (inside [`Effect::Sales`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `sales`; appended when `append`.
    Sales {
        /// Query.
        query: DocQuery,
        /// Next page.
        append: bool,
        /// The list's request number when sent (stale answers are dropped).
        seq: u64,
    },
    /// `returns`; appended when `append`.
    Returns {
        /// Query.
        query: DocQuery,
        /// Next page.
        append: bool,
        /// The list's request number when sent (stale answers are dropped).
        seq: u64,
    },
    /// `customer_history` (customer, cursor).
    History(String, Option<String>, u64),
    /// `sale`.
    Sale(String),
    /// `sale_return`.
    Return(String),
    /// `returns` after a sale (its branch, from its posting): the sale detail's returns.
    SaleReturns(DocQuery),
    /// `receipt` of a sale in the window language.
    SaleReceipt(String),
    /// `return_receipt` in the window language.
    ReturnReceipt(String),
    /// `all_tender_types`.
    TenderTypes,
    /// `put_tender_type`.
    PutTender {
        /// Tender type.
        id: String,
        /// Body.
        body: TenderTypeBody,
    },
    /// `delete_tender_type`.
    ArchiveTender(String),
    /// `sales_settings`.
    Settings,
    /// `put_sales_settings`.
    PutSettings(SalesSettings),
    /// `customer_by_code`, or `customer_by_phone` for digits (the customer filter).
    FindCustomer(String),
}

/// The answer of a [`Call`] (inside [`super::Msg::Sales`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// [`Call::Sales`] (append).
    Sales(bool, u64, Result<SalePage, ClientError>),
    /// [`Call::Returns`] (append).
    Returns(bool, u64, Result<ReturnPage, ClientError>),
    /// [`Call::History`] (append).
    History(bool, u64, Result<HistoryPage, ClientError>),
    /// [`Call::Sale`].
    Sale(Result<Sale, ClientError>),
    /// [`Call::Return`].
    Return(Box<Result<SaleReturn, ClientError>>),
    /// [`Call::SaleReturns`].
    SaleReturns(Result<ReturnPage, ClientError>),
    /// [`Call::SaleReceipt`] or [`Call::ReturnReceipt`].
    Receipt(Result<Receipt, ClientError>),
    /// [`Call::TenderTypes`].
    TenderTypes(Result<Vec<TenderType>, ClientError>),
    /// [`Call::PutTender`] or [`Call::ArchiveTender`] (true).
    TenderSaved(bool, Result<TenderType, ClientError>),
    /// [`Call::Settings`] or [`Call::PutSettings`] (true).
    Settings(bool, Result<SalesSettings, ClientError>),
    /// [`Call::FindCustomer`].
    Found(Result<Customer, ClientError>),
}

/// The list filters (shared by sales and returns).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    /// 0 = every branch, else 1 + the index into the reference branches.
    pub branch: usize,
    /// 0 = every register, else 1 + the index into [`SalesState::registers`].
    pub register: usize,
    /// First local day, `YYYY-MM-DD`.
    pub from: String,
    /// Last local day, `YYYY-MM-DD`.
    pub until: String,
    /// Member code or phone as typed.
    pub customer: String,
    /// The customer found for it.
    pub customer_id: Option<String>,
    /// Number as typed.
    pub number: String,
    /// Index into [`STATUSES`].
    pub status: usize,
    /// A date that could not be read: `from` or `until`.
    pub bad_date: Option<&'static str>,
}

/// A list with keyset paging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocList<T> {
    /// Newest first.
    pub rows: Vec<T>,
    /// The next page's cursor.
    pub next: Option<String>,
    /// The highlighted row.
    pub cursor: usize,
    /// A page is loading.
    pub loading: bool,
    /// Loaded at least once.
    pub loaded: bool,
    /// Errors.
    pub notice: Option<Notice>,
    /// The number of the latest fresh request (an answer of an older one is stale).
    pub seq: u64,
}

impl<T> Default for DocList<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            next: None,
            cursor: 0,
            loading: false,
            loaded: false,
            notice: None,
            seq: 0,
        }
    }
}

impl<T> DocList<T> {
    /// Marks a request as started and returns its number: a fresh one (not `append`)
    /// supersedes every request in flight.
    fn begin(&mut self, append: bool) -> u64 {
        if !append {
            self.seq += 1;
        }
        self.loading = true;
        self.seq
    }

    fn take(&mut self, append: bool, rows: Vec<T>, next: Option<String>) {
        if append {
            self.rows.extend(rows);
        } else {
            self.rows = rows;
            self.cursor = 0;
        }
        self.next = next;
        self.loading = false;
        self.loaded = true;
        self.notice = None;
    }
}

/// A document linked from a detail: the kind (`sale`, `return`), ID and number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// `sale` or `return`.
    pub kind: &'static str,
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// What it is to this document: `original`, `exchange`, `return`.
    pub role: &'static str,
}

/// The open sale or return.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detail {
    /// The sale, once loaded.
    pub sale: Option<Box<Sale>>,
    /// The return, once loaded.
    pub ret: Option<Box<SaleReturn>>,
    /// The ID asked for.
    pub id: String,
    /// The receipt text.
    pub receipt: Vec<String>,
    /// Linked documents (Enter opens the highlighted one).
    pub links: Vec<Link>,
    /// The highlighted link.
    pub cursor: usize,
    /// Where Esc goes.
    pub back: Option<Screen>,
    /// The details opened before this one (Esc goes back through them).
    pub trail: Vec<(&'static str, String)>,
    /// Errors.
    pub notice: Option<Notice>,
}

/// The tender type editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // the tender type's flags, one field each
pub struct TenderEditor {
    /// The highlighted row.
    pub cursor: usize,
    /// The edited tender type (`None` while new).
    pub id: Option<String>,
    /// Name.
    pub name: String,
    /// Index into [`KINDS`].
    pub kind: usize,
    /// A reference must be typed.
    pub needs_reference: bool,
    /// Opens the drawer.
    pub opens_drawer: bool,
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

/// The sales settings form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsEditor {
    /// As stored.
    pub stored: Option<SalesSettings>,
    /// `return_days` as typed.
    pub return_days: String,
    /// The width index per branch (into [`WIDTHS`]), by branch ID.
    pub widths: Vec<(String, usize)>,
    /// The node refused writes.
    pub replica: bool,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// Everything of the sales section and the customer history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SalesState {
    /// The filters.
    pub filters: Filters,
    /// The registers seen in the lists (ID, code), by code.
    pub registers: Vec<(String, String)>,
    /// The sales list.
    pub sales: DocList<SaleSummary>,
    /// The returns list.
    pub returns: DocList<ReturnSummary>,
    /// The customer history (customer ID and name, then its list).
    pub history_of: Option<(String, String)>,
    /// The history list.
    pub history: DocList<HistoryItem>,
    /// The open detail.
    pub detail: Detail,
    /// Every tender type, by sort.
    pub tenders: Vec<TenderType>,
    /// The tender type editor.
    pub editor: TenderEditor,
    /// The sales settings.
    pub settings: SettingsEditor,
    /// Bumped when texts change under the user: the view rebuilds the text fields.
    pub serial: u64,
}

/// Midnight of local day `date` (unix ms).
fn local_midnight(date: NaiveDate) -> Option<i64> {
    Local
        .from_local_datetime(&date.and_hms_opt(0, 0, 0)?)
        .earliest()
        .map(|t| t.timestamp_millis())
}

fn parse_day(text: &str) -> Result<Option<NaiveDate>, ()> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    NaiveDate::parse_from_str(t, "%Y-%m-%d")
        .map(Some)
        .map_err(|_| ())
}

/// `JKT01-R1-261116-0001` → `JKT01 · R1` (the branch and register codes of a number).
#[must_use]
pub fn place_of(number: &str) -> String {
    let mut parts = number.split('-');
    match (parts.next(), parts.next()) {
        (Some(b), Some(r)) => format!("{b} · {r}"),
        _ => String::new(),
    }
}

impl Controller {
    /// Whether sales and returns may be read (`report.sales` from any role assignment).
    #[must_use]
    pub fn can_read_sales(&self) -> bool {
        self.may(REPORT_SALES)
    }

    /// Whether tender types and the sales settings may be changed (`tender.manage`).
    #[must_use]
    pub fn can_manage_tenders(&self) -> bool {
        self.may(TENDER_MANAGE)
    }

    fn sales_call(&self, call: Call) -> Effect {
        Effect::Sales {
            token: self.token(),
            call,
        }
    }

    fn sales_bump(&mut self) {
        self.sales.serial += 1;
    }

    /// The query of the filters (`before` = the next page), or the field of a bad date.
    fn doc_query(&mut self, before: Option<String>, sales: bool) -> Option<DocQuery> {
        let f = &mut self.sales.filters;
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
            .and_then(|d| d.checked_add_days(Days::new(1)))
            .and_then(local_midnight);
        let branch = f
            .branch
            .checked_sub(1)
            .and_then(|i| self.reference.branches.get(i))
            .map(|b| b.id.clone());
        let register = f
            .register
            .checked_sub(1)
            .and_then(|i| self.sales.registers.get(i))
            .map(|r| r.0.clone());
        let number = Some(f.number.trim().to_uppercase()).filter(|n| !n.is_empty());
        Some(DocQuery {
            branch_id: branch,
            register_id: register,
            customer_id: f.customer_id.clone(),
            from,
            until,
            number,
            status: if sales {
                Some(STATUSES[f.status.min(2)].to_owned()).filter(|s| !s.is_empty())
            } else {
                None
            },
            before_id: before,
            limit: Some(PAGE),
            ..DocQuery::default()
        })
    }

    fn denied_sales() -> Notice {
        notice("sales_denied", REPORT_SALES, Tone::Bad)
    }

    /// Loads the first page of the list on view.
    fn reload_docs(&mut self) -> Vec<Effect> {
        let sales = self.screen() == Screen::Sales;
        if !self.can_read_sales() {
            let n = Some(Self::denied_sales());
            if sales {
                self.sales.sales.notice = n;
            } else {
                self.sales.returns.notice = n;
            }
            return Vec::new();
        }
        let Some(query) = self.doc_query(None, sales) else {
            return Vec::new();
        };
        if sales {
            let seq = self.sales.sales.begin(false);
            vec![self.sales_call(Call::Sales {
                query,
                append: false,
                seq,
            })]
        } else {
            let seq = self.sales.returns.begin(false);
            vec![self.sales_call(Call::Returns {
                query,
                append: false,
                seq,
            })]
        }
    }

    /// Loads the next page of the list on view (or of the history).
    fn more_docs(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::Sales => {
                let l = &self.sales.sales;
                let Some(next) = l.next.clone().filter(|_| !l.loading) else {
                    return Vec::new();
                };
                let Some(query) = self.doc_query(Some(next), true) else {
                    return Vec::new();
                };
                let seq = self.sales.sales.begin(true);
                vec![self.sales_call(Call::Sales {
                    query,
                    append: true,
                    seq,
                })]
            }
            Screen::Returns => {
                let l = &self.sales.returns;
                let Some(next) = l.next.clone().filter(|_| !l.loading) else {
                    return Vec::new();
                };
                let Some(query) = self.doc_query(Some(next), false) else {
                    return Vec::new();
                };
                let seq = self.sales.returns.begin(true);
                vec![self.sales_call(Call::Returns {
                    query,
                    append: true,
                    seq,
                })]
            }
            Screen::CustomerHistory => {
                let l = &self.sales.history;
                let (Some(next), Some((id, _))) = (
                    l.next.clone().filter(|_| !l.loading),
                    self.sales.history_of.clone(),
                ) else {
                    return Vec::new();
                };
                let seq = self.sales.history.begin(true);
                vec![self.sales_call(Call::History(id, Some(next), seq))]
            }
            _ => Vec::new(),
        }
    }

    /// Entering a screen of the section (from `enter`).
    pub(super) fn enter_sales(&mut self, target: Screen, from: Screen) -> Vec<Effect> {
        self.focus("list");
        match target {
            Screen::Sales | Screen::Returns => {
                let loaded = if target == Screen::Sales {
                    self.sales.sales.loaded
                } else {
                    self.sales.returns.loaded
                };
                if loaded && matches!(from, Screen::SaleDetail | Screen::ReturnDetail) {
                    return Vec::new();
                }
                self.sales_bump();
                let mut effects = self.reload_docs();
                if self.reference.branches.is_empty() {
                    effects.push(Effect::LoadBranches(self.token()));
                }
                // The details name the tenders.
                if self.sales.tenders.is_empty() {
                    effects.push(self.sales_call(Call::TenderTypes));
                }
                effects
            }
            Screen::TenderTypes => {
                self.sales.editor.notice = None;
                vec![self.sales_call(Call::TenderTypes)]
            }
            Screen::SalesSettings => {
                self.sales.settings.notice = None;
                let mut effects = vec![self.sales_call(Call::Settings)];
                if self.reference.branches.is_empty() {
                    effects.push(Effect::LoadBranches(self.token()));
                }
                effects
            }
            _ => Vec::new(),
        }
    }

    /// Opens the purchase history of the customer in the form (Ctrl+H, spec 2.5 §9).
    pub(super) fn open_history(&mut self) -> Vec<Effect> {
        let Some((id, name)) = self
            .customers
            .form
            .as_ref()
            .and_then(|f| f.stored.as_ref())
            .map(|c| (c.id.clone(), c.name.clone()))
        else {
            return Vec::new();
        };
        self.screen = Some(Screen::CustomerHistory);
        self.focus("list");
        if self
            .sales
            .history_of
            .as_ref()
            .is_some_and(|(h, _)| *h == id)
            && self.sales.history.loaded
        {
            return Vec::new();
        }
        self.sales.history_of = Some((id.clone(), name));
        let seq = self.sales.history.seq + 1;
        self.sales.history = DocList {
            loading: true,
            seq,
            ..DocList::default()
        };
        vec![self.sales_call(Call::History(id, None, seq))]
    }

    /// Opens a sale or return detail; Esc goes back to `back`.
    fn open_detail(&mut self, kind: &'static str, id: String, back: Screen) -> Vec<Effect> {
        let trail = if matches!(self.screen(), Screen::SaleDetail | Screen::ReturnDetail) {
            let mut t = std::mem::take(&mut self.sales.detail.trail);
            let current = if self.screen() == Screen::SaleDetail {
                "sale"
            } else {
                "return"
            };
            t.push((current, self.sales.detail.id.clone()));
            t
        } else {
            Vec::new()
        };
        let back = self
            .sales
            .detail
            .back
            .filter(|_| !trail.is_empty())
            .or(Some(back));
        self.sales.detail = Detail {
            id: id.clone(),
            back,
            trail,
            ..Detail::default()
        };
        self.focus("list");
        if kind == "sale" {
            self.screen = Some(Screen::SaleDetail);
            vec![
                self.sales_call(Call::Sale(id.clone())),
                self.sales_call(Call::SaleReceipt(id)),
            ]
        } else {
            self.screen = Some(Screen::ReturnDetail);
            vec![
                self.sales_call(Call::Return(id.clone())),
                self.sales_call(Call::ReturnReceipt(id)),
            ]
        }
    }

    /// Esc on a detail: the previous detail, else the list (or history) it came from.
    fn close_detail(&mut self) -> Vec<Effect> {
        if let Some((kind, id)) = self.sales.detail.trail.pop() {
            let trail = std::mem::take(&mut self.sales.detail.trail);
            let back = self.sales.detail.back;
            let effects = self.open_detail(kind, id, back.unwrap_or(Screen::Sales));
            self.sales.detail.trail = trail;
            self.sales.detail.back = back;
            return effects;
        }
        let back = self.sales.detail.back.unwrap_or(Screen::Sales);
        if back == Screen::CustomerHistory {
            self.screen = Some(Screen::CustomerHistory);
            self.focus("list");
            return Vec::new();
        }
        self.enter(back)
    }

    fn open_doc(&mut self, screen: Screen) -> Vec<Effect> {
        match screen {
            Screen::Sales => match self.sales.sales.rows.get(self.sales.sales.cursor) {
                Some(s) => {
                    let id = s.id.clone();
                    self.open_detail("sale", id, Screen::Sales)
                }
                None => Vec::new(),
            },
            Screen::Returns => match self.sales.returns.rows.get(self.sales.returns.cursor) {
                Some(r) => {
                    let id = r.id.clone();
                    self.open_detail("return", id, Screen::Returns)
                }
                None => Vec::new(),
            },
            Screen::CustomerHistory => {
                match self.sales.history.rows.get(self.sales.history.cursor) {
                    Some(h) => {
                        let (kind, id) = (
                            if h.kind == "return" { "return" } else { "sale" },
                            h.document_id.clone(),
                        );
                        self.open_detail(kind, id, Screen::CustomerHistory)
                    }
                    None => Vec::new(),
                }
            }
            Screen::SaleDetail | Screen::ReturnDetail => {
                match self.sales.detail.links.get(self.sales.detail.cursor) {
                    Some(l) => {
                        let (kind, id) = (l.kind, l.id.clone());
                        let back = self.sales.detail.back.unwrap_or(Screen::Sales);
                        self.open_detail(kind, id, back)
                    }
                    None => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    /// Moves the highlight of the list on view; past the last row the next page loads.
    fn move_cursor(&mut self, up: bool) -> Vec<Effect> {
        let screen = self.screen();
        let (cursor, count, next) = match screen {
            Screen::Sales => {
                let l = &mut self.sales.sales;
                (&mut l.cursor, l.rows.len(), l.next.is_some())
            }
            Screen::Returns => {
                let l = &mut self.sales.returns;
                (&mut l.cursor, l.rows.len(), l.next.is_some())
            }
            Screen::CustomerHistory => {
                let l = &mut self.sales.history;
                (&mut l.cursor, l.rows.len(), l.next.is_some())
            }
            Screen::SaleDetail | Screen::ReturnDetail => {
                let d = &mut self.sales.detail;
                (&mut d.cursor, d.links.len(), false)
            }
            Screen::TenderTypes => {
                let count = self.sales.tenders.len();
                let row = if up {
                    self.sales.editor.cursor.saturating_sub(1)
                } else {
                    (self.sales.editor.cursor + 1).min(count.saturating_sub(1))
                };
                self.select_tender(row);
                return Vec::new();
            }
            _ => return Vec::new(),
        };
        if up {
            *cursor = cursor.saturating_sub(1);
        } else if *cursor + 1 < count {
            *cursor += 1;
        } else if next {
            return self.more_docs();
        }
        Vec::new()
    }

    /// Keys on the sales screens and the history (after the F-keys of the header).
    pub(super) fn sales_capture(&mut self, p: Press, screen: Screen) -> Step {
        if screen != Screen::CustomerHistory {
            for (i, tab) in TABS.iter().enumerate() {
                let digit = char::from_digit(u32::try_from(i + 1).unwrap_or(0), 10).unwrap_or('0');
                if p.ctrl_char(digit) {
                    return Step::handled(self.go(*tab));
                }
            }
        }
        if p.f(5) {
            return Step::handled(self.sales_action("sales-refresh").unwrap_or_default());
        }
        if p.plain(PressKey::Up) || p.plain(PressKey::Down) {
            return Step::handled(self.move_cursor(p.plain(PressKey::Up)));
        }
        let action = match screen {
            Screen::Sales | Screen::Returns | Screen::CustomerHistory => {
                if p.plain(PressKey::Enter) {
                    "sales-open"
                } else if p.ctrl_char('l') {
                    "sales-more"
                } else if p.plain(PressKey::Escape) {
                    if screen == Screen::CustomerHistory {
                        "history-back"
                    } else {
                        self.focus("list");
                        return Step::handled(Vec::new());
                    }
                } else if p.ctrl_char('f') && screen != Screen::CustomerHistory {
                    self.focus("sales-number");
                    return Step::handled(Vec::new());
                } else {
                    return Step::default();
                }
            }
            Screen::SaleDetail | Screen::ReturnDetail => {
                if p.plain(PressKey::Enter) {
                    "sales-open"
                } else if p.plain(PressKey::Escape) {
                    "detail-back"
                } else {
                    return Step::default();
                }
            }
            Screen::TenderTypes => {
                if p.ctrl_char('n') {
                    "tender-new"
                } else if p.ctrl_char('s') {
                    "tender-save"
                } else if p.ctrl_shift_char('a') {
                    "tender-archive"
                } else if p.plain(PressKey::Escape) {
                    return Step::handled(self.go(Screen::Sales));
                } else {
                    return Step::default();
                }
            }
            Screen::SalesSettings => {
                if p.ctrl_char('s') {
                    "sales-settings-save"
                } else if p.plain(PressKey::Escape) {
                    return Step::handled(self.go(Screen::Sales));
                } else {
                    return Step::default();
                }
            }
            _ => return Step::default(),
        };
        Step::handled(self.sales_action(action).unwrap_or_default())
    }

    /// A button of the section; `None` when not one of them.
    pub(super) fn sales_action(&mut self, id: &str) -> Option<Vec<Effect>> {
        Some(match id {
            "nav-sales" | "tab-sales" => self.go(Screen::Sales),
            "tab-returns" => self.go(Screen::Returns),
            "tab-tender-types" => self.go(Screen::TenderTypes),
            "tab-sales-settings" => self.go(Screen::SalesSettings),
            "customer-history" => self.open_history(),
            "history-back" => {
                self.screen = Some(Screen::CustomerForm);
                self.focus("list");
                Vec::new()
            }
            "detail-back" => self.close_detail(),
            "sales-open" => self.open_doc(self.screen()),
            "sales-more" => self.more_docs(),
            "sales-refresh" => match self.screen() {
                Screen::Sales | Screen::Returns => self.reload_docs(),
                Screen::CustomerHistory => {
                    self.sales.history.loaded = false;
                    self.open_history()
                }
                Screen::TenderTypes => vec![self.sales_call(Call::TenderTypes)],
                Screen::SalesSettings => vec![self.sales_call(Call::Settings)],
                _ => Vec::new(),
            },
            "sales-search" => self.search_docs(),
            "sales-clear" => {
                self.sales.filters = Filters::default();
                self.sales_bump();
                self.reload_docs()
            }
            "tender-new" => {
                self.sales.editor = TenderEditor {
                    cursor: self.sales.editor.cursor,
                    replica: self.sales.editor.replica,
                    active: true,
                    sort: (self.sales.tenders.len() + 1).to_string(),
                    ..TenderEditor::default()
                };
                self.sales_bump();
                self.focus("tender-name");
                Vec::new()
            }
            "tender-save" => self.save_tender(),
            "tender-archive" => {
                let e = &self.sales.editor;
                if e.id.is_some() && !e.archived && self.tenders_editable() {
                    self.confirm = Some(Confirm::TenderArchive);
                }
                Vec::new()
            }
            "sales-settings-save" => self.save_sales_settings(),
            _ => return None,
        })
    }

    /// Whether the tender types and settings can be edited here.
    #[must_use]
    pub fn tenders_editable(&self) -> bool {
        self.can_manage_tenders() && !self.sales.editor.replica
    }

    /// Why the tender types and settings are read-only, if they are.
    #[must_use]
    pub fn tenders_read_only(&self) -> Option<Notice> {
        if self.sales.editor.replica || self.sales.settings.replica {
            Some(Notice::bad("read_only_replica"))
        } else if self.session.is_some() && !self.can_manage_tenders() {
            Some(notice("tender_read_only", TENDER_MANAGE, Tone::Neutral))
        } else {
            None
        }
    }

    /// Runs the filters: a typed customer is looked up first.
    fn search_docs(&mut self) -> Vec<Effect> {
        let f = &mut self.sales.filters;
        let typed = f.customer.trim().to_owned();
        if typed.is_empty() {
            f.customer_id = None;
            return self.reload_docs();
        }
        vec![self.sales_call(Call::FindCustomer(typed))]
    }

    fn select_tender(&mut self, row: usize) {
        let Some(t) = self.sales.tenders.get(row) else {
            return;
        };
        let replica = self.sales.editor.replica;
        self.sales.editor = TenderEditor {
            cursor: row,
            id: Some(t.id.clone()),
            name: t.name.clone(),
            kind: KINDS.iter().position(|k| *k == t.kind).unwrap_or(0),
            needs_reference: t.needs_reference,
            opens_drawer: t.opens_drawer,
            active: t.active,
            sort: t.sort.to_string(),
            archived: t.archived,
            replica,
            ..TenderEditor::default()
        };
        self.sales_bump();
    }

    fn save_tender(&mut self) -> Vec<Effect> {
        if !self.tenders_editable() {
            self.sales.editor.notice = self.tenders_read_only();
            return Vec::new();
        }
        let e = &mut self.sales.editor;
        if e.archived || e.saving {
            return Vec::new();
        }
        let name = e.name.trim();
        if name.is_empty() || name.chars().count() > 40 {
            e.notice = Some(Notice::bad("invalid_tender_name"));
            return Vec::new();
        }
        let Ok(sort) = e.sort.trim().parse::<u32>() else {
            e.notice = Some(Notice::bad("invalid_sort"));
            return Vec::new();
        };
        let id = e.id.clone().unwrap_or_else(super::form::new_id);
        let body = TenderTypeBody {
            name: name.to_owned(),
            kind: KINDS[e.kind.min(KINDS.len() - 1)].to_owned(),
            needs_reference: e.needs_reference,
            opens_drawer: e.opens_drawer,
            active: e.active,
            sort,
        };
        e.saving = true;
        e.notice = None;
        vec![self.sales_call(Call::PutTender { id, body })]
    }

    fn save_sales_settings(&mut self) -> Vec<Effect> {
        if !self.can_manage_tenders() || self.sales.settings.replica {
            self.sales.settings.notice = self.tenders_read_only();
            return Vec::new();
        }
        let s = &mut self.sales.settings;
        let Ok(return_days) = s.return_days.trim().parse::<u32>() else {
            s.notice = Some(Notice::bad("invalid_return_days"));
            return Vec::new();
        };
        if return_days > 3_650 {
            s.notice = Some(Notice::bad("invalid_return_days"));
            return Vec::new();
        }
        let body = SalesSettings {
            return_days,
            receipt_widths: s
                .widths
                .iter()
                .map(|(b, i)| BranchWidth {
                    branch_id: b.clone(),
                    receipt_width: WIDTHS[(*i).min(WIDTHS.len() - 1)],
                })
                .collect(),
        };
        s.notice = None;
        vec![self.sales_call(Call::PutSettings(body))]
    }

    /// The tender archive was confirmed.
    pub(super) fn sales_confirmed(&mut self) -> Vec<Effect> {
        match self.sales.editor.id.clone() {
            Some(id) => {
                self.sales.editor.saving = true;
                vec![self.sales_call(Call::ArchiveTender(id))]
            }
            None => Vec::new(),
        }
    }

    /// A text field of the section; `None` when not one of them.
    pub(super) fn sales_edit(&mut self, kind: &str, row: usize, field: &str, value: String) {
        match (kind, field) {
            ("sales-filter", "from") => self.sales.filters.from = value,
            ("sales-filter", "until") => self.sales.filters.until = value,
            ("sales-filter", "customer") => {
                self.sales.filters.customer = value;
                self.sales.filters.customer_id = None;
            }
            ("sales-filter", "number") => self.sales.filters.number = value,
            ("tender", "name") => self.sales.editor.name = value,
            ("tender", "sort") => self.sales.editor.sort = value,
            ("sales-settings", "return_days") => self.sales.settings.return_days = value,
            _ => {
                let _ = row;
            }
        }
    }

    /// A check box of the section; `None` when not one of them.
    pub(super) fn sales_toggle(&mut self, kind: &str, field: &str, on: bool) -> Option<()> {
        if kind != "tender" {
            return None;
        }
        let e = &mut self.sales.editor;
        match field {
            "needs_reference" => e.needs_reference = on,
            "opens_drawer" => e.opens_drawer = on,
            "active" => e.active = on,
            _ => {}
        }
        Some(())
    }

    /// A picker of the section; `None` when not one of them.
    pub(super) fn sales_choose(
        &mut self,
        kind: &str,
        row: usize,
        field: &str,
        index: usize,
    ) -> Option<Vec<Effect>> {
        Some(match (kind, field) {
            ("sales-filter", "branch") => {
                self.sales.filters.branch = index;
                self.reload_docs()
            }
            ("sales-filter", "register") => {
                self.sales.filters.register = index;
                self.reload_docs()
            }
            ("sales-filter", "status") => {
                self.sales.filters.status = index.min(STATUSES.len() - 1);
                self.reload_docs()
            }
            ("tender", "kind") => {
                // The kind is fixed once saved (spec 2.6 §19 item 21).
                if self.sales.editor.id.is_none() {
                    self.sales.editor.kind = index.min(KINDS.len() - 1);
                }
                Vec::new()
            }
            ("sales-settings", "width") => {
                if let Some(w) = self.sales.settings.widths.get_mut(row) {
                    w.1 = index.min(WIDTHS.len() - 1);
                }
                Vec::new()
            }
            _ => return None,
        })
    }

    /// A click on a row of the section; `None` when not one of them.
    pub(super) fn sales_pick(&mut self, kind: &str, row: usize) -> Option<Vec<Effect>> {
        Some(match kind {
            "sale-row" => {
                if row < self.sales.sales.rows.len() {
                    self.sales.sales.cursor = row;
                }
                Vec::new()
            }
            "return-row" => {
                if row < self.sales.returns.rows.len() {
                    self.sales.returns.cursor = row;
                }
                Vec::new()
            }
            "history-row" => {
                if row < self.sales.history.rows.len() {
                    self.sales.history.cursor = row;
                }
                Vec::new()
            }
            "doc-link" => {
                if row < self.sales.detail.links.len() {
                    self.sales.detail.cursor = row;
                    return Some(self.open_doc(self.screen()));
                }
                Vec::new()
            }
            "tender-row" => {
                self.select_tender(row);
                Vec::new()
            }
            _ => return None,
        })
    }

    /// The registers seen in `rows` join the register filter.
    fn learn_registers<'a>(&mut self, rows: impl Iterator<Item = (Option<&'a str>, &'a str)>) {
        for (id, number) in rows {
            let Some(id) = id else { continue };
            if self.sales.registers.iter().any(|(r, _)| r == id) {
                continue;
            }
            let code = number.split('-').nth(1).unwrap_or(id).to_owned();
            self.sales.registers.push((id.to_owned(), code));
        }
        self.sales.registers.sort_by(|a, b| a.1.cmp(&b.1));
    }

    fn detail_links_of_sale(sale: &Sale, returns: &[ReturnSummary]) -> Vec<Link> {
        returns
            .iter()
            .filter(|r| r.sale_id.as_deref() == Some(sale.summary.id.as_str()))
            .map(|r| Link {
                kind: "return",
                id: r.id.clone(),
                number: r.number.clone(),
                role: "return",
            })
            .collect()
    }

    /// The answer to a call of the section.
    #[allow(clippy::too_many_lines)] // one arm per answer
    pub(super) fn sales_reply(&mut self, reply: Reply) -> Vec<Effect> {
        if self.session.is_none() {
            return Vec::new();
        }
        match reply {
            Reply::Sales(_, seq, _) if seq != self.sales.sales.seq => {}
            Reply::Sales(append, _, result) => match result {
                Ok(page) => {
                    let rows: Vec<(Option<String>, String)> = page
                        .items
                        .iter()
                        .map(|s| (s.register_id.clone(), s.number.clone()))
                        .collect();
                    self.learn_registers(rows.iter().map(|(r, n)| (r.as_deref(), n.as_str())));
                    self.sales
                        .sales
                        .take(append, page.items, page.next_before_id);
                }
                Err(e) => {
                    self.sales.sales.loading = false;
                    self.sales.sales.notice = self.sales_failed(&e);
                }
            },
            Reply::Returns(_, seq, _) if seq != self.sales.returns.seq => {}
            Reply::Returns(append, _, result) => match result {
                Ok(page) => {
                    let rows: Vec<(String, String)> = page
                        .items
                        .iter()
                        .map(|r| (r.register_id.clone(), r.number.clone()))
                        .collect();
                    self.learn_registers(rows.iter().map(|(r, n)| (Some(r.as_str()), n.as_str())));
                    self.sales
                        .returns
                        .take(append, page.items, page.next_before_id);
                }
                Err(e) => {
                    self.sales.returns.loading = false;
                    self.sales.returns.notice = self.sales_failed(&e);
                }
            },
            Reply::History(_, seq, _) if seq != self.sales.history.seq => {}
            Reply::History(append, _, result) => match result {
                Ok(page) => self
                    .sales
                    .history
                    .take(append, page.items, page.next_before_id),
                Err(e) => {
                    self.sales.history.loading = false;
                    self.sales.history.notice = self.sales_failed(&e);
                }
            },
            Reply::Sale(result) => match result {
                Ok(sale) if sale.summary.id == self.sales.detail.id => {
                    let mut links = Vec::new();
                    if sale.summary.return_count > 0 {
                        // The returns of this sale: the loaded list, else ask for them.
                        links = Self::detail_links_of_sale(&sale, &self.sales.returns.rows);
                        if links.len() < sale.summary.return_count as usize {
                            let query = DocQuery {
                                branch_id: Some(sale.summary.branch_id.clone()),
                                from: Some(sale.summary.posted_at),
                                limit: Some(200),
                                ..DocQuery::default()
                            };
                            self.sales.detail.sale = Some(Box::new(sale));
                            return vec![self.sales_call(Call::SaleReturns(query))];
                        }
                    }
                    self.sales.detail.links = links;
                    self.sales.detail.sale = Some(Box::new(sale));
                }
                Ok(sale) => {
                    // The original sale of the open return: its number for the link.
                    if let Some(l) = self
                        .sales
                        .detail
                        .links
                        .iter_mut()
                        .find(|l| l.id == sale.summary.id)
                    {
                        l.number = sale.summary.number;
                    }
                }
                Err(e) => self.sales.detail.notice = self.sales_failed(&e),
            },
            Reply::SaleReturns(result) => match result {
                Ok(page) => {
                    if let Some(sale) = &self.sales.detail.sale {
                        self.sales.detail.links = Self::detail_links_of_sale(sale, &page.items);
                    }
                }
                Err(e) => self.sales.detail.notice = self.sales_failed(&e),
            },
            Reply::Return(result) => match *result {
                Ok(ret) if ret.summary.id == self.sales.detail.id => {
                    let mut links = Vec::new();
                    if let Some(id) = &ret.summary.sale_id {
                        links.push(Link {
                            kind: "sale",
                            id: id.clone(),
                            number: String::new(),
                            role: "original",
                        });
                    }
                    if let Some(x) = &ret.exchange_sale {
                        links.push(Link {
                            kind: "sale",
                            id: x.summary.id.clone(),
                            number: x.summary.number.clone(),
                            role: "exchange",
                        });
                    }
                    self.sales.detail.links = links;
                    let original = ret.summary.sale_id.clone();
                    self.sales.detail.ret = Some(Box::new(ret));
                    // The original sale's number comes with its header.
                    if let Some(id) = original {
                        return vec![self.sales_call(Call::Sale(id))];
                    }
                }
                Ok(_) => {}
                Err(e) => self.sales.detail.notice = self.sales_failed(&e),
            },
            Reply::Receipt(result) => match result {
                Ok(r)
                    if r.sale_id == self.sales.detail.id || r.return_id == self.sales.detail.id =>
                {
                    self.sales.detail.receipt = r.lines;
                }
                Ok(_) => {}
                Err(e) => self.sales.detail.notice = self.sales_failed(&e),
            },
            Reply::TenderTypes(result) => match result {
                Ok(mut list) => {
                    list.sort_by(|a, b| a.sort.cmp(&b.sort).then(a.name.cmp(&b.name)));
                    self.sales.tenders = list;
                    let keep = self.sales.editor.id.clone();
                    let row = keep
                        .and_then(|id| self.sales.tenders.iter().position(|t| t.id == id))
                        .unwrap_or(self.sales.editor.cursor);
                    let notice = self.sales.editor.notice.take();
                    if self.sales.editor.id.is_some() || !self.sales.editor.name.is_empty() {
                        self.select_tender(row.min(self.sales.tenders.len().saturating_sub(1)));
                    } else {
                        self.select_tender(0);
                    }
                    self.sales.editor.notice = notice;
                }
                Err(e) => self.sales.editor.notice = self.sales_failed(&e),
            },
            Reply::TenderSaved(archived, result) => {
                self.sales.editor.saving = false;
                match result {
                    Ok(t) => {
                        self.sales.editor.id = Some(t.id);
                        self.sales.editor.notice = Some(notice(
                            if archived { "tender_archived" } else { "saved" },
                            "",
                            Tone::Good,
                        ));
                        return vec![self.sales_call(Call::TenderTypes)];
                    }
                    Err(e) => {
                        if e.code() == Some("read_only_replica") {
                            self.sales.editor.replica = true;
                        }
                        self.sales.editor.notice = self.sales_failed(&e);
                    }
                }
            }
            Reply::Settings(saved, result) => match result {
                Ok(s) => {
                    let ed = &mut self.sales.settings;
                    ed.return_days = s.return_days.to_string();
                    ed.widths = s
                        .receipt_widths
                        .iter()
                        .map(|w| {
                            (
                                w.branch_id.clone(),
                                WIDTHS
                                    .iter()
                                    .position(|x| *x == w.receipt_width)
                                    .unwrap_or(0),
                            )
                        })
                        .collect();
                    ed.stored = Some(s);
                    if saved {
                        ed.notice = Some(notice("saved", "", Tone::Good));
                    }
                    self.sales_bump();
                }
                Err(e) => {
                    if e.code() == Some("read_only_replica") {
                        self.sales.settings.replica = true;
                    }
                    self.sales.settings.notice = self.sales_failed(&e);
                }
            },
            Reply::Found(result) => match result {
                Ok(c) => {
                    self.sales.filters.customer_id = Some(c.id);
                    return self.reload_docs();
                }
                Err(e) => {
                    let n = if e.code() == Some("not_found") {
                        Some(Notice::bad("customer_not_found"))
                    } else {
                        self.sales_failed(&e)
                    };
                    if self.screen() == Screen::Returns {
                        self.sales.returns.notice = n;
                    } else {
                        self.sales.sales.notice = n;
                    }
                }
            },
        }
        Vec::new()
    }

    /// A failed call: the login ended, or the message (a missing `report.sales` says so).
    fn sales_failed(&mut self, e: &ClientError) -> Option<Notice> {
        if e.code() == Some("permission_denied")
            && e.problem().and_then(|p| p.permission()) == Some(REPORT_SALES)
        {
            return Some(Self::denied_sales());
        }
        self.failed(e)
    }
}

#[cfg(test)]
mod tests {
    use super::DocList;

    #[test]
    fn a_fresh_request_supersedes_the_ones_in_flight() {
        let mut list = DocList::<u8>::default();
        let first = list.begin(false);
        let second = list.begin(false);
        assert_ne!(first, second, "an older answer no longer matches");
        assert_eq!(list.begin(true), second, "a next page keeps the number");
        assert!(list.loading);
    }
}
