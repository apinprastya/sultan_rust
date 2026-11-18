//! The state screen's flow (spec 2.6 §2–§5, §13, phase S4): the session home's cart, line
//! actions, payment panel, posting, receipt preview and crash copy, as more of
//! [`Controller`]'s state machine. The cart and payment rules are in [`crate::sell`].
//!
//! - **Adding:** a scan (or a typed code, or a pick from the F2 search) is looked up; `3*` before
//!   the code sets the quantity. An open-price item asks for its price first. In price-check mode
//!   (F7) a scan only shows the item and its prices (the C5/R5 panel).
//! - **Quote:** every change quotes the whole cart again (`POST /pricing/quote`), as does
//!   attaching or detaching a customer and `price.changed` / `events.lagged`. Only the newest
//!   quote is applied.
//! - **Lines** (Ctrl+↑): ↑/↓ pick a line; Enter qty, Del void (reason, `POST /cart-voids`), `+`/`−`
//!   qty ±1, U unit, P price override, D line discount; Ctrl+D the cart discount; Ctrl+N or
//!   Ctrl+Del voids the whole cart (the reason picker asks first). Voids that need a manager go
//!   through the override dialog (`context_ref` = the cart ID).
//! - **Pay** (F4 cash, F9 other tenders): split tenders until the rest is 0, F8 saves change,
//!   Enter posts (`PUT /sales/{id}`). The checks a cashier's limits do not cover are approved one
//!   at a time in the override dialog and sent together (`X-Override`, comma separated);
//!   `cart_stale` closes the panel with the new total and quotes the cart again.
//! - **After posting:** the change due in large digits and the receipt text, then (Enter) a new
//!   empty cart.
//! - **After the sale** (phase S5): F5 drawer, F6 last receipts, F10 return / exchange, F11 hold
//!   and Shift+F11 resume are in [`crate::after`]; an exchange pays its new cart with the
//!   refund first ([`Pay::with_exchange`]) and posts it with the return.
//! - **Crash copy** (D2): every change writes the cart ([`crate::sell::Saved`]); the login of the
//!   same cashier session restores it, another session's copy is ignored; posting and a cart void
//!   delete it.

use sultan_client::{
    CartVoid, CartVoidRequest, ClientError, Lookup, ProtectedAction, Quote, Receipt, Sale,
    SearchHit, SearchPage, TenderType, VoidedLine,
};
use sultan_core::catalog::Qty;
use sultan_core::sale::{Discount, SaleError};

use crate::amount::rupiah;
use crate::controller::{
    Action, Approval, Controller, Effect, Notice, Screen, Shortcut, Tone, ends_login, new_key,
    notice_for,
};
use crate::input::{Key, Pin};
use crate::sell::{Cart, Line, Pay, PayStep, Saved};

/// The void reasons of §3, in the picker's order.
pub const VOID_REASONS: [&str; 4] = [
    "customer_changed_mind",
    "wrong_item",
    "price_dispute",
    "other",
];

/// What an entry field asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntryKind {
    /// A line's quantity.
    Qty(usize),
    /// A line's manual price.
    Price(usize),
    /// The price of an open-price item before it is added.
    OpenPrice(Box<Lookup>, i64),
    /// A line discount (% or Rp).
    LineDiscount(usize),
    /// The cart discount (% or Rp).
    CartDiscount,
}

/// A typed value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What it is for.
    pub kind: EntryKind,
    /// The text typed (digits and one decimal comma).
    pub text: String,
    /// A discount in percent (else Rupiah).
    pub pct: bool,
}

impl Entry {
    fn new(kind: EntryKind) -> Self {
        let pct = matches!(kind, EntryKind::LineDiscount(_) | EntryKind::CartDiscount);
        Self {
            kind,
            text: String::new(),
            pct,
        }
    }

    /// Whether a decimal comma may be typed: quantities and percentages.
    const fn decimals(&self) -> bool {
        match self.kind {
            EntryKind::Qty(_) => true,
            EntryKind::LineDiscount(_) | EntryKind::CartDiscount => self.pct,
            _ => false,
        }
    }

    fn push(&mut self, c: char) -> bool {
        if self.text.len() >= 16 {
            return false;
        }
        match c {
            '0'..='9' => self.text.push(c),
            ',' | '.' if self.decimals() && !self.text.contains(',') => self.text.push(','),
            _ => return false,
        }
        true
    }

    /// The typed amount in Rupiah.
    fn amount(&self) -> Option<i64> {
        self.text.parse().ok()
    }

    /// The typed quantity.
    fn qty(&self) -> Option<Qty> {
        Qty::parse(&self.text.replace(',', ".")).ok()
    }

    /// The typed discount, `None` when nothing was typed (clears the discount).
    fn discount(&self) -> Result<Option<Discount>, ()> {
        if self.text.is_empty() {
            return Ok(None);
        }
        if self.pct {
            let q = self.qty().ok_or(())?;
            // Percent with up to 2 decimals → basis points.
            let bp = q.thousandths() / 10;
            u16::try_from(bp)
                .map(|b| Some(Discount::Pct(b)))
                .map_err(|_| ())
        } else {
            self.amount()
                .map(|a| Some(Discount::Amount(sultan_core::money::Money::from_minor(a))))
                .ok_or(())
        }
    }
}

/// Which void the reason picker is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoidTarget {
    /// One line.
    Line(usize),
    /// The whole cart.
    Cart,
}

/// The void reason picker (§3: a reason is required).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReasonPick {
    /// What is voided.
    pub target: VoidTarget,
    /// The highlighted reason (index into [`VOID_REASONS`]).
    pub cursor: usize,
    /// The text of `other`.
    pub text: String,
}

/// The F2 product search.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    /// The typed text.
    pub query: String,
    /// The hits of the last search.
    pub results: Vec<SearchHit>,
    /// The highlighted hit.
    pub cursor: usize,
    /// The hits are for the typed text.
    pub current: bool,
}

/// A dialog over the state screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// A typed value.
    Entry(Entry),
    /// The void reason.
    Reason(ReasonPick),
    /// The unit of a line.
    Units {
        /// The line.
        line: usize,
        /// The highlighted unit.
        cursor: usize,
    },
    /// The product search.
    Search(Search),
    /// The key list (F1).
    Help,
    /// The hold note (F11).
    Note(crate::after::HoldNote),
    /// The resume list (Shift+F11).
    Held(crate::after::HeldList),
    /// The last receipts (F6) or a customer's purchase history.
    Docs(crate::after::Docs),
    /// A receipt on screen (reprint, post-void, return).
    Receipt(Box<crate::after::Preview>),
    /// The post-void (§7).
    PostVoid(Box<crate::after::PostVoid>),
    /// A return or exchange (F10).
    Return(Box<crate::after::Return>),
}

/// The posted sale, shown until Enter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Done {
    /// The sale.
    pub sale: Box<Sale>,
    /// The receipt text, once loaded.
    pub receipt: Vec<String>,
    /// A tender opened the drawer.
    pub drawer: bool,
    /// An exchange: the rest of the refund paid back in cash.
    pub refund: i64,
}

/// A void waiting for the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VoidCall {
    /// What is voided.
    pub target: VoidTarget,
    /// The body.
    pub body: CartVoidRequest,
    /// The idempotency key.
    pub key: String,
}

/// The state screen's state, part of the session home.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sell {
    /// The cart.
    pub cart: Cart,
    /// The highlighted line when the lines have the keyboard (Ctrl+↑); `None` = the barcode field.
    pub focus: Option<usize>,
    /// The line added or changed last (highlighted).
    pub last: Option<usize>,
    /// A dialog.
    pub modal: Option<Modal>,
    /// The payment panel.
    pub pay: Option<Pay>,
    /// The posted sale.
    pub done: Option<Done>,
    /// Price-check mode (F7): scans only show the item.
    pub check: bool,
    /// The `3*` quantity of the lookup with this number.
    pub qty_next: Option<(u64, Qty)>,
    /// A void waiting for its answer.
    pub voiding: Option<VoidCall>,
    /// A call of [`crate::after`] an override would retry.
    pub waiting: Option<crate::after::Waiting>,
    /// An exchange in progress: the return whose refund pays this cart (D12).
    pub exchange: Option<Box<crate::after::Return>>,
}

/// What an approved override retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    /// The waiting void.
    Void,
    /// The sale.
    Sale,
    /// The waiting call of [`crate::after`].
    After,
}

/// The message for a cart or payment rule (`core::sale::SaleError`).
#[must_use]
pub fn sale_notice(error: &SaleError) -> Notice {
    match error {
        SaleError::TenderOverRest { rest } => {
            Notice::new("tender_over_rest", rupiah(rest.minor()), Tone::Bad)
        }
        SaleError::CartTooLarge { max } => {
            Notice::new("cart_too_large", max.to_string(), Tone::Bad)
        }
        other => match (other.code(), other.field()) {
            ("invalid_field", Some("line")) => Notice::bad("line_fixed"),
            ("invalid_field", Some("tax_invoice")) => Notice::bad("tax_invoice_needs_tax_id"),
            ("invalid_field", Some("tender_type")) => Notice::bad("no_cash_tender"),
            ("invalid_field", Some("customer")) => Notice::bad("needs_customer"),
            ("invalid_field", Some("discount")) => Notice::bad("invalid_discount"),
            ("invalid_field", _) => Notice::bad("invalid_amount"),
            (
                code @ ("invalid_qty"
                | "tender_reference_required"
                | "insufficient_balance"
                | "balance_limit"),
                _,
            ) => Notice::bad(code),
            (code, _) => Notice::new("error", code, Tone::Bad),
        },
    }
}

fn home(screen: &mut Screen) -> Option<&mut crate::controller::HomeScreen> {
    match screen {
        Screen::Home(h) => Some(h),
        _ => None,
    }
}

impl Controller {
    pub(crate) fn sell(&self) -> Option<&Sell> {
        match &self.screen {
            Screen::Home(h) => Some(&h.sell),
            _ => None,
        }
    }

    fn sell_mut(&mut self) -> Option<&mut Sell> {
        home(&mut self.screen).map(|h| &mut h.sell)
    }

    /// The buttons of the session home (spec 2.6 §13, D14).
    #[must_use]
    pub fn home_actions(&self) -> Vec<Shortcut> {
        let s = |key, action, label| Shortcut { key, action, label };
        let empty = self.sell().is_none_or(|s| s.cart.is_empty());
        let mut keys = vec![
            s(Key::F(1), Action::Help, "help"),
            s(Key::F(2), Action::Search, "search"),
            s(Key::F(3), Action::Customer, "customer"),
        ];
        if self.perms.search {
            keys.push(s(Key::ShiftF(3), Action::NameSearch, "name_search"));
        }
        if self.customer.is_some() {
            keys.push(s(Key::CtrlF(3), Action::Detach, "detach_customer"));
        }
        if self.customer.as_ref().is_some_and(|c| c.tax_id.is_some()) {
            keys.push(s(Key::Ctrl('t'), Action::TaxInvoice, "tax_invoice"));
        }
        if !empty {
            keys.push(s(Key::F(4), Action::PayCash, "pay_cash"));
            keys.push(s(Key::F(9), Action::PayOther, "pay_other"));
            keys.push(s(Key::CtrlUp, Action::Lines, "to_lines"));
            keys.push(s(Key::Ctrl('d'), Action::CartDiscount, "cart_discount"));
        }
        keys.push(s(Key::F(5), Action::Drawer, "drawer"));
        keys.push(s(Key::F(6), Action::Receipts, "receipts"));
        keys.push(s(Key::F(7), Action::PriceCheck, "price_check"));
        let exchange = self.sell().is_some_and(|s| s.exchange.is_some());
        if exchange {
            keys.push(s(Key::F(10), Action::Return, "cancel_exchange"));
        } else if empty {
            keys.push(s(Key::F(10), Action::Return, "returns"));
        }
        if !empty && !exchange {
            keys.push(s(Key::F(11), Action::Hold, "hold"));
        }
        if !exchange {
            keys.push(s(Key::ShiftF(11), Action::Resume, "resume"));
        }
        keys.push(s(Key::F(12), Action::Lock, "lock"));
        if empty {
            keys.push(s(Key::CtrlF(10), Action::CloseSession, "close_session"));
        }
        keys
    }

    /// The keys of the state screen: the dialog's, the payment panel's, the lines' or the barcode
    /// field's and the buttons.
    #[allow(clippy::too_many_lines)] // one list per dialog, panel and focus
    pub(crate) fn sell_shortcuts(&self) -> Vec<Shortcut> {
        use Action::{Back, Confirm};
        let s = |key, action, label| Shortcut { key, action, label };
        let Some(state) = self.sell() else {
            return Vec::new();
        };
        if state.done.is_some() {
            return vec![s(Key::Enter, Confirm, "new_sale")];
        }
        if let Some(keys) = self.after_shortcuts() {
            return keys;
        }
        if let Some(pay) = &state.pay {
            return match pay.step {
                PayStep::Cash => vec![
                    s(Key::Enter, Confirm, "take_cash"),
                    s(Key::F(9), Action::PayOther, "pay_other"),
                    s(Key::Escape, Back, "back"),
                ],
                PayStep::Pick(_) => vec![
                    s(Key::Enter, Confirm, "choose"),
                    s(Key::F(4), Action::PayCash, "pay_cash"),
                    s(Key::Escape, Back, "back"),
                ],
                PayStep::Amount(_) | PayStep::Reference(..) => vec![
                    s(Key::Enter, Confirm, "add_tender"),
                    s(Key::Escape, Back, "back"),
                ],
                PayStep::Ready => {
                    let mut keys = vec![s(Key::Enter, Confirm, "confirm_sale")];
                    if self.customer.is_some() && pay.change() + pay.saved() > 0 {
                        keys.push(s(Key::F(8), Action::SaveChange, "save_change"));
                    }
                    keys.push(s(Key::Escape, Back, "undo_tender"));
                    keys
                }
                PayStep::Save => vec![s(Key::Enter, Confirm, "save"), s(Key::Escape, Back, "back")],
            };
        }
        if let Some(modal) = &state.modal {
            return match modal {
                Modal::Entry(e) => {
                    let mut keys = vec![s(Key::Enter, Confirm, "ok")];
                    if matches!(e.kind, EntryKind::LineDiscount(_) | EntryKind::CartDiscount) {
                        keys.push(s(Key::Char('%'), Action::Percent, "pct_or_rp"));
                    }
                    keys.push(s(Key::Escape, Back, "cancel"));
                    keys
                }
                Modal::Reason(_) => vec![
                    s(Key::Enter, Confirm, "void"),
                    s(Key::Escape, Back, "cancel"),
                ],
                Modal::Units { .. } => vec![
                    s(Key::Enter, Confirm, "choose"),
                    s(Key::Escape, Back, "cancel"),
                ],
                Modal::Search(search) => vec![
                    s(
                        Key::Enter,
                        Confirm,
                        if search.current && !search.results.is_empty() {
                            "add_item"
                        } else {
                            "find"
                        },
                    ),
                    s(Key::Escape, Back, "close"),
                ],
                Modal::Help => vec![s(Key::Escape, Back, "close"), s(Key::F(1), Back, "close")],
                _ => Vec::new(),
            };
        }
        if state.focus.is_some() {
            return vec![
                s(Key::Enter, Action::EditQty, "edit_qty"),
                s(Key::Delete, Action::VoidLine, "void_line"),
                s(Key::Char('+'), Action::More, "more"),
                s(Key::Char('-'), Action::Less, "less"),
                s(Key::Char('u'), Action::Unit, "unit"),
                s(Key::Char('p'), Action::Price, "price"),
                s(Key::Char('d'), Action::Discount, "discount"),
                s(Key::Ctrl('d'), Action::CartDiscount, "cart_discount"),
                s(Key::Ctrl('n'), Action::VoidCart, "void_cart"),
                s(Key::CtrlDown, Action::Scan, "to_scan"),
                s(Key::Escape, Action::Scan, "to_scan"),
            ];
        }
        let mut keys = self.home_actions();
        keys.push(s(
            Key::Enter,
            Confirm,
            if state.check { "look_up" } else { "add_item" },
        ));
        keys.push(s(Key::Escape, Back, "clear"));
        if state.cart.lines.is_empty() {
            return keys;
        }
        // The cart void is one button in the bar for both keys (the window merges them).
        keys.push(s(Key::Ctrl('n'), Action::VoidCart, "void_cart"));
        keys.push(s(Key::CtrlDelete, Action::VoidCart, "void_cart"));
        keys
    }

    /// Typing on the state screen. Returns whether the key was used.
    #[allow(clippy::too_many_lines)] // one block per dialog and panel
    pub(crate) fn sell_edit(&mut self, key: Key) -> bool {
        if self.sell().is_some_and(|s| s.done.is_none())
            && let Some(used) = self.after_edit(key)
        {
            return used;
        }
        let Some(h) = home(&mut self.screen) else {
            return false;
        };
        let state = &mut h.sell;
        if state.done.is_some() {
            return matches!(key, Key::Escape) && {
                state.done = None;
                true
            };
        }
        if let Some(pay) = &mut state.pay {
            let used = match (&pay.step, key) {
                (PayStep::Pick(i), Key::Up | Key::Down) => {
                    let others = pay.others();
                    let pos = others.iter().position(|x| x == i).unwrap_or(0);
                    let next = if key == Key::Up {
                        pos.checked_sub(1).unwrap_or(others.len().saturating_sub(1))
                    } else {
                        (pos + 1) % others.len().max(1)
                    };
                    if let Some(&n) = others.get(next) {
                        pay.step = PayStep::Pick(n);
                    }
                    true
                }
                (PayStep::Reference(..), Key::Char(_) | Key::Digit(_)) => {
                    let ch = match key {
                        Key::Digit(d) => char::from(b'0' + d),
                        Key::Char(c) => c,
                        _ => ' ',
                    };
                    if pay.text.chars().count() < crate::sell::MAX_REFERENCE {
                        pay.text.push(ch);
                    }
                    true
                }
                (PayStep::Reference(..), Key::Backspace) => {
                    pay.text.pop();
                    true
                }
                (PayStep::Cash | PayStep::Amount(_) | PayStep::Save, Key::Digit(d)) => {
                    pay.digits.push(d);
                    true
                }
                (PayStep::Cash | PayStep::Amount(_) | PayStep::Save, Key::Backspace) => {
                    pay.digits.pop();
                    true
                }
                _ => false,
            };
            return used;
        }
        if let Some(modal) = &mut state.modal {
            return match modal {
                Modal::Entry(e) => match key {
                    Key::Digit(d) => e.push(char::from(b'0' + d)),
                    Key::Char(c @ (',' | '.')) => e.push(c),
                    Key::Backspace => {
                        e.text.pop();
                        true
                    }
                    _ => false,
                },
                Modal::Reason(r) => match key {
                    Key::Up => {
                        r.cursor = crate::count::wrap(r.cursor, -1, VOID_REASONS.len());
                        true
                    }
                    Key::Down | Key::Tab => {
                        r.cursor = crate::count::wrap(r.cursor, 1, VOID_REASONS.len());
                        true
                    }
                    Key::Char(c) if r.cursor == 3 && r.text.chars().count() < 200 => {
                        r.text.push(c);
                        true
                    }
                    Key::Digit(d) if r.cursor == 3 => {
                        r.text.push(char::from(b'0' + d));
                        true
                    }
                    Key::Backspace if r.cursor == 3 => {
                        r.text.pop();
                        true
                    }
                    _ => false,
                },
                Modal::Units { line, cursor } => {
                    let count = state
                        .cart
                        .lines
                        .get(*line)
                        .map_or(0, |l| l.item.units.len());
                    match key {
                        Key::Up => {
                            *cursor = crate::count::wrap(*cursor, -1, count);
                            true
                        }
                        Key::Down | Key::Tab => {
                            *cursor = crate::count::wrap(*cursor, 1, count);
                            true
                        }
                        _ => false,
                    }
                }
                Modal::Search(search) => match key {
                    Key::Up => {
                        search.cursor = crate::count::wrap(search.cursor, -1, search.results.len());
                        true
                    }
                    Key::Down => {
                        search.cursor = crate::count::wrap(search.cursor, 1, search.results.len());
                        true
                    }
                    Key::Char(c) if search.query.chars().count() < 60 => {
                        search.query.push(c);
                        search.current = false;
                        true
                    }
                    Key::Digit(d) => {
                        search.query.push(char::from(b'0' + d));
                        search.current = false;
                        true
                    }
                    Key::Backspace => {
                        search.query.pop();
                        search.current = false;
                        true
                    }
                    _ => false,
                },
                _ => false,
            };
        }
        if let Some(cursor) = &mut state.focus {
            let count = state.cart.lines.len();
            return match key {
                Key::Up => {
                    *cursor = cursor.saturating_sub(1);
                    true
                }
                Key::Down => {
                    *cursor = (*cursor + 1).min(count.saturating_sub(1));
                    true
                }
                _ => false,
            };
        }
        match key {
            Key::Digit(d) => h.scan.push(char::from(b'0' + d)),
            Key::Char(c) => h.scan.push(c),
            Key::Backspace => {
                h.scan.pop();
                true
            }
            _ => false,
        }
    }

    /// The line shortcuts take letters in either case.
    pub(crate) fn sell_key(&self, key: Key) -> Key {
        match key {
            Key::Char(c)
                if self.sell().is_some_and(|s| {
                    (s.focus.is_some() && s.modal.is_none())
                        || matches!(s.modal, Some(Modal::Receipt(_)))
                        || matches!(&s.modal, Some(Modal::Return(r))
                            if r.step == crate::after::ReturnStep::Lines
                                && (!r.no_receipt || r.text.is_empty())
                                && r.write_off.is_none())
                }) =>
            {
                Key::Char(c.to_ascii_lowercase())
            }
            other => other,
        }
    }

    /// A click on a row: a cart line, or an item of the open list.
    pub(crate) fn pick_sell(&mut self, kind: &str, index: usize) {
        if self.busy {
            return;
        }
        if kind == "item" && self.in_after() {
            self.after_pick(index);
            return;
        }
        let Some(state) = self.sell_mut() else {
            return;
        };
        match (kind, &mut state.modal, &mut state.pay) {
            ("tender", _, Some(pay)) if pay.others().contains(&index) => {
                pay.step = PayStep::Pick(index);
            }
            ("line", None, None) if index < state.cart.lines.len() => state.focus = Some(index),
            ("item", Some(Modal::Reason(r)), None) if index < VOID_REASONS.len() => {
                r.cursor = index;
            }
            ("item", Some(Modal::Units { cursor, .. }), None) => *cursor = index,
            ("item", Some(Modal::Search(s)), None) if index < s.results.len() => {
                s.cursor = index;
            }
            _ => {}
        }
    }

    /// Handles a state action (the caller checked that it is on screen).
    #[allow(clippy::too_many_lines)] // one arm per key of §13
    pub(crate) fn sell_action(&mut self, action: Action) -> Vec<Effect> {
        let in_modal = self.sell().is_some_and(|s| s.modal.is_some());
        let in_pay = self.sell().is_some_and(|s| s.pay.is_some());
        let in_done = self.sell().is_some_and(|s| s.done.is_some());
        if self.in_after() && !in_done {
            return self.after_action(action);
        }
        match action {
            Action::Confirm if self.sell().is_some_and(|s| s.done.is_some()) => {
                if let Some(s) = self.sell_mut() {
                    s.done = None;
                }
                Vec::new()
            }
            Action::Confirm if in_pay => self.pay_confirm(),
            Action::Confirm if in_modal => self.modal_confirm(),
            Action::Back if in_pay => {
                self.pay_back();
                Vec::new()
            }
            Action::Back if in_modal => {
                if let Some(s) = self.sell_mut() {
                    s.modal = None;
                }
                Vec::new()
            }
            Action::Confirm => self.scan_confirm(),
            Action::Back => {
                if let Some(h) = home(&mut self.screen) {
                    h.scan.clear();
                    h.notice = None;
                }
                Vec::new()
            }
            Action::Help => self.open_modal(Modal::Help),
            Action::Search => self.open_modal(Modal::Search(Search::default())),
            Action::PriceCheck => {
                if let Some(h) = home(&mut self.screen) {
                    h.sell.check = !h.sell.check;
                    h.scan.clear();
                    let code = if h.sell.check {
                        "price_check_on"
                    } else {
                        "price_check_off"
                    };
                    h.notice = Some(Notice::new(code, "", Tone::Neutral));
                }
                Vec::new()
            }
            Action::Drawer | Action::Hold | Action::Resume | Action::Receipts | Action::Return => {
                self.after_action(action)
            }
            Action::Lines => {
                if let Some(s) = self.sell_mut() {
                    let last = s.cart.lines.len().saturating_sub(1);
                    s.focus = Some(s.last.unwrap_or(last).min(last));
                }
                Vec::new()
            }
            Action::Scan => {
                if let Some(s) = self.sell_mut() {
                    s.focus = None;
                }
                Vec::new()
            }
            Action::EditQty => self.line_entry(EntryKind::Qty),
            Action::Price => self.line_entry(EntryKind::Price),
            Action::Discount => self.line_entry(EntryKind::LineDiscount),
            Action::CartDiscount => {
                self.open_modal(Modal::Entry(Entry::new(EntryKind::CartDiscount)))
            }
            Action::Percent => {
                if let Some(Modal::Entry(e)) = self.sell_mut().and_then(|s| s.modal.as_mut()) {
                    e.pct = !e.pct;
                    e.text.clear();
                }
                Vec::new()
            }
            Action::More | Action::Less => {
                let up = action == Action::More;
                let result = self.sell_mut().and_then(|s| {
                    let i = s.focus?;
                    Some(s.cart.bump(i, up).map(|()| i))
                });
                self.changed(result)
            }
            Action::Unit => {
                let Some(s) = self.sell_mut() else {
                    return Vec::new();
                };
                let Some(i) = s.focus else {
                    return Vec::new();
                };
                let line = &s.cart.lines[i];
                if line.item.units.len() < 2 || line.core.scale_label.is_some() {
                    self.set_notice(Notice::bad("no_other_unit"));
                    return Vec::new();
                }
                let cursor = line
                    .item
                    .units
                    .iter()
                    .position(|u| u.id == line.item.unit_id)
                    .unwrap_or(0);
                s.modal = Some(Modal::Units { line: i, cursor });
                Vec::new()
            }
            Action::VoidLine => {
                if let Some(s) = self.sell_mut()
                    && let Some(i) = s.focus
                {
                    s.modal = Some(Modal::Reason(ReasonPick {
                        target: VoidTarget::Line(i),
                        cursor: 0,
                        text: String::new(),
                    }));
                }
                Vec::new()
            }
            Action::VoidCart => {
                if let Some(s) = self.sell_mut()
                    && !s.cart.is_empty()
                {
                    s.modal = Some(Modal::Reason(ReasonPick {
                        target: VoidTarget::Cart,
                        cursor: 0,
                        text: String::new(),
                    }));
                }
                Vec::new()
            }
            Action::TaxInvoice => {
                let has_tax_id = self.customer.as_ref().is_some_and(|c| c.tax_id.is_some());
                let Some(s) = self.sell_mut() else {
                    return Vec::new();
                };
                if !has_tax_id {
                    return Vec::new();
                }
                s.cart.tax_invoice = !s.cart.tax_invoice;
                let code = if s.cart.tax_invoice {
                    "tax_invoice_on"
                } else {
                    "tax_invoice_off"
                };
                self.set_notice(Notice::new(code, "", Tone::Neutral));
                self.persist().into_iter().collect()
            }
            Action::PayCash | Action::PayOther => self.open_pay(action == Action::PayOther),
            Action::SaveChange => {
                if let Some(pay) = self.sell_mut().and_then(|s| s.pay.as_mut()) {
                    pay.digits.clear();
                    pay.step = PayStep::Save;
                }
                Vec::new()
            }
            Action::NameSearch => {
                let mut dialog = crate::customer::Dialog::new(None);
                if !dialog.find.by_name {
                    dialog.act(Action::SwitchMode);
                }
                self.dialog = Some(dialog);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn open_modal(&mut self, modal: Modal) -> Vec<Effect> {
        if let Some(s) = self.sell_mut() {
            s.modal = Some(modal);
        }
        Vec::new()
    }

    fn line_entry(&mut self, kind: fn(usize) -> EntryKind) -> Vec<Effect> {
        if let Some(s) = self.sell_mut()
            && let Some(i) = s.focus
        {
            s.modal = Some(Modal::Entry(Entry::new(kind(i))));
        }
        Vec::new()
    }

    /// After a change of the cart: the message, a new quote and the crash copy.
    fn changed(&mut self, result: Option<Result<usize, SaleError>>) -> Vec<Effect> {
        match result {
            None => Vec::new(),
            Some(Err(e)) => {
                self.set_notice(sale_notice(&e));
                Vec::new()
            }
            Some(Ok(i)) => {
                if let Some(h) = home(&mut self.screen) {
                    h.sell.last = Some(i);
                    h.notice = None;
                }
                self.cart_changed()
            }
        }
    }

    /// Quotes the cart again and writes the crash copy.
    pub(crate) fn cart_changed(&mut self) -> Vec<Effect> {
        let mut effects = self.requote();
        effects.extend(self.persist());
        effects
    }

    /// Quotes the whole cart (spec 2.3 §10); an empty cart needs none.
    pub(crate) fn requote(&mut self) -> Vec<Effect> {
        let token = self.token();
        let generation = self.generation;
        let seq = self.cart_quotes + 1;
        let customer = self.customer.as_ref().map(|c| c.id.clone());
        let Some(s) = self.sell_mut() else {
            return Vec::new();
        };
        if s.cart.is_empty() {
            s.cart.quoting = None;
            return Vec::new();
        }
        let (request, map) = s.cart.quote_request(customer.as_deref());
        s.cart.quoting = Some((seq, map));
        self.cart_quotes = seq;
        vec![Effect::QuoteCart {
            token,
            request,
            seq,
            generation,
        }]
    }

    /// The crash copy of the cart: written while it has lines, deleted once it is empty.
    pub(crate) fn persist(&self) -> Option<Effect> {
        let session = self.session.as_ref()?;
        let state = self.sell()?;
        if state.cart.is_empty() {
            return Some(Effect::DeleteCart);
        }
        let saved = Saved::of(&state.cart, &session.id, self.customer.as_ref());
        serde_json::to_string(&saved).ok().map(Effect::SaveCart)
    }

    /// The answer of quote `seq` of the cart.
    pub(crate) fn cart_quoted(
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
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        let map = match h.sell.cart.quoting.take() {
            Some((waiting, map)) if waiting == seq => map,
            other => {
                h.sell.cart.quoting = other;
                return Vec::new();
            }
        };
        match result {
            Ok(quote) => {
                h.sell.cart.apply_quote(&quote, &map);
                let dropped = h.sell.cart.drop_unpriced();
                let count = h.sell.cart.lines.len();
                h.sell.focus = h.sell.focus.filter(|_| count > 0).map(|f| f.min(count - 1));
                h.sell.last = h.sell.last.filter(|l| *l < count);
                if let Some(name) = dropped.first() {
                    h.notice = Some(Notice::new("no_price_item", name.clone(), Tone::Bad));
                    return self.persist().into_iter().collect();
                }
                Vec::new()
            }
            Err(e) => {
                h.notice = Some(crate::controller::quote_notice(&e));
                Vec::new()
            }
        }
    }

    /// Enter on the barcode field: looks the typed code up (`3*` sets the quantity).
    fn scan_confirm(&mut self) -> Vec<Effect> {
        let token = self.token();
        let generation = self.generation;
        let seq = self.lookups + 1;
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        let typed = h.scan.code.clone();
        let mut qty = None;
        if !h.sell.check
            && let Some((prefix, code)) = typed.split_once('*')
        {
            match Qty::parse(&prefix.replace(',', ".")) {
                Ok(q) if q.is_positive() && !code.is_empty() => {
                    qty = Some(q);
                    code.clone_into(&mut h.scan.code);
                }
                _ => {
                    h.scan.code.clear();
                    h.notice = Some(Notice::bad("invalid_qty"));
                    return Vec::new();
                }
            }
        }
        let Some(code) = h.scan.take(seq) else {
            return Vec::new();
        };
        h.notice = None;
        h.sell.qty_next = qty.map(|q| (seq, q));
        self.lookups = seq;
        vec![Effect::Lookup {
            token,
            code,
            seq,
            generation,
        }]
    }

    /// A lookup answer in selling mode: the item goes into the cart (or asks for its price).
    /// Returns `None` in price-check mode (the panel shows it).
    pub(crate) fn sell_looked(
        &mut self,
        seq: u64,
        result: Result<Lookup, Notice>,
    ) -> Option<Vec<Effect>> {
        let h = home(&mut self.screen)?;
        if h.sell.check {
            return None;
        }
        if h.scan.waiting.as_ref().is_none_or(|(w, _)| *w != seq) {
            return Some(Vec::new());
        }
        h.scan.waiting = None;
        h.scan.shown = None;
        let qty = h
            .sell
            .qty_next
            .take()
            .filter(|(s, _)| *s == seq)
            .map(|(_, q)| q);
        let item = match result {
            Ok(item) => item,
            Err(notice) => {
                h.notice = Some(notice);
                return Some(Vec::new());
            }
        };
        let qty = qty
            .or_else(|| item.qty_thousandths.map(Qty::from_thousandths))
            .unwrap_or(Qty::ONE);
        if item.open_price && item.amount.is_none() {
            h.sell.modal = Some(Modal::Entry(Entry::new(EntryKind::OpenPrice(
                Box::new(item),
                qty.thousandths(),
            ))));
            return Some(Vec::new());
        }
        let result = h.sell.cart.add(Line::new(item, qty, None));
        Some(self.changed(Some(result)))
    }

    /// Enter in a dialog.
    #[allow(clippy::too_many_lines)] // one arm per dialog
    fn modal_confirm(&mut self) -> Vec<Effect> {
        let token = self.token();
        let generation = self.generation;
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        let Some(modal) = h.sell.modal.clone() else {
            return Vec::new();
        };
        let cart = &mut h.sell.cart;
        match modal {
            Modal::Help => {
                h.sell.modal = None;
                Vec::new()
            }
            Modal::Note(_)
            | Modal::Held(_)
            | Modal::Docs(_)
            | Modal::Receipt(_)
            | Modal::PostVoid(_)
            | Modal::Return(_) => Vec::new(),
            Modal::Entry(e) => {
                let result: Result<usize, SaleError> = match &e.kind {
                    EntryKind::Qty(i) => match e.qty() {
                        Some(q) => cart.set_qty(*i, q).map(|()| *i),
                        None => Err(SaleError::InvalidQty { reason: "typed" }),
                    },
                    EntryKind::Price(i) => match e.amount() {
                        Some(p) => cart.override_price(*i, p).map(|()| *i),
                        None => Err(SaleError::InvalidField {
                            field: "price",
                            reason: "typed",
                        }),
                    },
                    EntryKind::OpenPrice(item, qty) => match e.amount() {
                        Some(p) => cart.add(Line::new(
                            (**item).clone(),
                            Qty::from_thousandths(*qty),
                            Some(p),
                        )),
                        None => Err(SaleError::InvalidField {
                            field: "price",
                            reason: "typed",
                        }),
                    },
                    EntryKind::LineDiscount(i) => match e.discount() {
                        Ok(d) => cart.set_line_discount(*i, d).map(|()| *i),
                        Err(()) => Err(SaleError::InvalidField {
                            field: "discount",
                            reason: "typed",
                        }),
                    },
                    EntryKind::CartDiscount => match e.discount() {
                        Ok(d) => cart.set_cart_discount(d).map(|()| h.sell.last.unwrap_or(0)),
                        Err(()) => Err(SaleError::InvalidField {
                            field: "discount",
                            reason: "typed",
                        }),
                    },
                };
                if result.is_ok() {
                    h.sell.modal = None;
                }
                self.changed(Some(result))
            }
            Modal::Units { line, cursor } => {
                let unit = cart
                    .lines
                    .get(line)
                    .and_then(|l| l.item.units.get(cursor))
                    .cloned();
                h.sell.modal = None;
                let result = unit.map(|u| cart.set_unit(line, &u).map(|()| line));
                self.changed(result)
            }
            Modal::Search(search) => {
                if !search.current || search.results.is_empty() {
                    let q = search.query.trim().to_owned();
                    if q.is_empty() {
                        return Vec::new();
                    }
                    self.busy = true;
                    return vec![Effect::Search {
                        token,
                        q,
                        generation,
                    }];
                }
                let Some(hit) = search.results.get(search.cursor) else {
                    return Vec::new();
                };
                h.sell.modal = None;
                h.scan.code.clone_from(&hit.sku);
                self.scan_confirm()
            }
            Modal::Reason(r) => {
                let reason = VOID_REASONS[r.cursor.min(VOID_REASONS.len() - 1)];
                let text = r.text.trim().to_owned();
                if reason == "other" && text.is_empty() {
                    h.notice = Some(Notice::bad("reason_text_required"));
                    return Vec::new();
                }
                let voided = |l: &Line| VoidedLine {
                    variant_id: l.item.variant_id.clone(),
                    name: l.item.name.clone(),
                    qty_thousandths: l.core.qty.thousandths(),
                    amount: l.core.gross().map_or(0, sultan_core::money::Money::minor),
                };
                let (kind, lines) = match r.target {
                    VoidTarget::Line(i) => {
                        ("line", cart.lines.get(i).map(voided).into_iter().collect())
                    }
                    VoidTarget::Cart => ("cart", cart.lines.iter().map(voided).collect()),
                };
                let body = CartVoidRequest {
                    kind: kind.to_owned(),
                    cart_id: cart.id.clone(),
                    reason: reason.to_owned(),
                    reason_text: (reason == "other").then_some(text),
                    lines,
                };
                let call = VoidCall {
                    target: r.target,
                    body,
                    key: new_key(),
                };
                h.sell.modal = None;
                h.sell.voiding = Some(call.clone());
                self.busy = true;
                vec![Effect::CartVoid {
                    token,
                    body: call.body,
                    key: call.key,
                    override_token: None,
                    generation,
                }]
            }
        }
    }

    /// The answer of the F2 search.
    pub(crate) fn searched(
        &mut self,
        generation: u64,
        result: Result<SearchPage, ClientError>,
    ) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        self.busy = false;
        match result {
            Err(e) => self.fail(&e),
            Ok(page) => {
                if let Some(Modal::Search(s)) = self.sell_mut().and_then(|s| s.modal.as_mut()) {
                    s.results = page.items;
                    s.cursor = 0;
                    s.current = true;
                    if s.results.is_empty() {
                        self.set_notice(Notice::bad("nothing_found"));
                    }
                }
                Vec::new()
            }
        }
    }

    /// The answer of a void (or its retry with a manager's token).
    pub(crate) fn cart_voided(
        &mut self,
        generation: u64,
        result: Result<CartVoid, ClientError>,
    ) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        self.busy = false;
        let Some(call) = self.sell().and_then(|s| s.voiding.clone()) else {
            return Vec::new();
        };
        match result {
            Ok(_) => {
                self.approval = None;
                self.retry = None;
                let Some(h) = home(&mut self.screen) else {
                    return Vec::new();
                };
                h.sell.voiding = None;
                match call.target {
                    VoidTarget::Line(i) if i < h.sell.cart.lines.len() => {
                        let name = h.sell.cart.lines.remove(i).item.name;
                        let count = h.sell.cart.lines.len();
                        h.sell.focus = h.sell.focus.filter(|_| count > 0).map(|f| f.min(count - 1));
                        h.sell.last = None;
                        h.notice = Some(Notice::new("line_voided", name, Tone::Neutral));
                        self.cart_changed()
                    }
                    VoidTarget::Line(_) => Vec::new(),
                    VoidTarget::Cart => {
                        h.sell = Sell::default();
                        h.notice = Some(Notice::new("cart_voided", "", Tone::Neutral));
                        self.customer = None;
                        vec![Effect::DeleteCart]
                    }
                }
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(e) if e.code() == Some("needs_override") && self.approval.is_none() => {
                let permission = e
                    .problem()
                    .and_then(sultan_client::Problem::permission)
                    .unwrap_or(if call.body.kind == "cart" {
                        "sale.void"
                    } else {
                        "sale.void_line"
                    })
                    .to_owned();
                self.ask_approval(
                    ProtectedAction {
                        action: permission,
                        value: None,
                        context_ref: call.body.cart_id.clone(),
                    },
                    e.problem().and_then(sultan_client::Problem::limit),
                    Retry::Void,
                );
                Vec::new()
            }
            Err(e) => {
                if let Some(a) = &mut self.approval {
                    a.notice = Some(notice_for(&e));
                } else {
                    if let Some(s) = self.sell_mut() {
                        s.voiding = None;
                    }
                    self.set_notice(notice_for(&e));
                }
                Vec::new()
            }
        }
    }

    fn ask_approval(&mut self, action: ProtectedAction, limit: Option<i64>, retry: Retry) {
        self.approval = Some(Approval {
            action,
            limit,
            cursor: 0,
            pin: Pin::default(),
            notice: None,
        });
        self.retry = Some(retry);
    }

    /// A manager approved: the void or the sale again, with the token(s).
    pub(crate) fn retry_with(&mut self, token: String) -> Vec<Effect> {
        let session_token = self.token();
        let generation = self.generation;
        match self.retry {
            Some(Retry::Void) => {
                let Some(call) = self.sell().and_then(|s| s.voiding.clone()) else {
                    return Vec::new();
                };
                self.busy = true;
                vec![Effect::CartVoid {
                    token: session_token,
                    body: call.body,
                    key: call.key,
                    override_token: Some(token),
                    generation,
                }]
            }
            Some(Retry::Sale) => {
                let action = self.approval.as_ref().map(|a| a.action.clone());
                if let (Some(pay), Some(a)) = (self.sell_mut().and_then(|s| s.pay.as_mut()), action)
                {
                    pay.tokens.push(token);
                    pay.approved.push((a.action, a.value));
                }
                self.post()
            }
            Some(Retry::After) => self.after_retry(token),
            None => {
                self.busy = false;
                Vec::new()
            }
        }
    }

    /// The override dialog was closed without approval.
    pub(crate) fn approval_cancelled(&mut self) {
        let retry = self.retry.take();
        if let Some(s) = self.sell_mut() {
            match retry {
                Some(Retry::Void) => s.voiding = None,
                Some(Retry::Sale) => {
                    if let Some(pay) = &mut s.pay {
                        pay.posting = false;
                    }
                }
                Some(Retry::After) => s.waiting = None,
                None => {}
            }
        }
    }

    /// F4 / F9: opens the payment panel (or switches its step).
    fn open_pay(&mut self, other: bool) -> Vec<Effect> {
        let balance = self.customer.as_ref().map(|c| c.balance);
        let types = self.tenders.clone();
        let token = self.token();
        let generation = self.generation;
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        if h.sell.pay.is_none() {
            if !h.sell.cart.priced() {
                h.notice = Some(Notice::bad("cart_not_priced"));
                return Vec::new();
            }
            if types.is_empty() {
                h.notice = Some(Notice::bad("no_tender_types"));
                return vec![Effect::TenderTypes { token, generation }];
            }
            let total = match h.sell.cart.totals() {
                Ok(t) => t.total.minor(),
                Err(e) => {
                    h.notice = Some(sale_notice(&e));
                    return Vec::new();
                }
            };
            let credit = h.sell.exchange.as_ref().map_or(0, |r| r.refund());
            match Pay::new(total, h.sell.cart.settings.rounding, types, balance)
                .and_then(|p| p.with_exchange(credit))
            {
                Ok(pay) => h.sell.pay = Some(pay),
                Err(e) => {
                    h.notice = Some(sale_notice(&e));
                    return Vec::new();
                }
            }
            h.sell.focus = None;
            h.notice = None;
        }
        let Some(pay) = &mut h.sell.pay else {
            return Vec::new();
        };
        if pay.complete() {
            return Vec::new();
        }
        pay.digits.clear();
        pay.step = if other {
            match pay.others().first() {
                Some(&i) => PayStep::Pick(i),
                None => PayStep::Cash,
            }
        } else {
            PayStep::Cash
        };
        Vec::new()
    }

    /// Esc in the payment panel: one step back, the last tender back, or the panel closed.
    fn pay_back(&mut self) {
        let Some(h) = home(&mut self.screen) else {
            return;
        };
        let Some(pay) = &mut h.sell.pay else {
            return;
        };
        pay.digits.clear();
        match pay.step {
            PayStep::Amount(i) | PayStep::Reference(i, _) => pay.step = PayStep::Pick(i),
            PayStep::Save => pay.step = PayStep::Ready,
            PayStep::Pick(_) => pay.step = PayStep::Cash,
            PayStep::Cash | PayStep::Ready => {
                if !pay.undo() {
                    h.sell.pay = None;
                }
            }
        }
        h.notice = None;
    }

    /// Enter in the payment panel.
    fn pay_confirm(&mut self) -> Vec<Effect> {
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        let Some(pay) = &mut h.sell.pay else {
            return Vec::new();
        };
        let result = match pay.step.clone() {
            PayStep::Cash => pay.add_cash(),
            PayStep::Pick(i) => {
                pay.digits.clear();
                pay.step = PayStep::Amount(i);
                Ok(())
            }
            PayStep::Amount(i) => pay.confirm_amount(i),
            PayStep::Reference(i, amount) => {
                let reference = pay.text.clone();
                pay.add_other(i, amount, Some(&reference))
            }
            PayStep::Save => pay.save_change(),
            PayStep::Ready => return self.post(),
        };
        h.notice = match result {
            Ok(()) => None,
            Err(e) => Some(sale_notice(&e)),
        };
        Vec::new()
    }

    /// Posts the paid cart (`PUT /sales/{id}`) with the approvals collected so far.
    fn post(&mut self) -> Vec<Effect> {
        let token = self.token();
        let generation = self.generation;
        let customer = self.customer.as_ref().map(|c| c.id.clone());
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        let Some(pay) = &mut h.sell.pay else {
            return Vec::new();
        };
        let body =
            match h
                .sell
                .cart
                .sale_request(customer.as_deref(), pay.requests(), pay.save_request())
            {
                Ok(body) => body,
                Err(e) => {
                    h.notice = Some(sale_notice(&e));
                    return Vec::new();
                }
            };
        pay.posting = true;
        let override_token = Some(pay.tokens.join(",")).filter(|t| !t.is_empty());
        self.busy = true;
        if let Some(r) = &h.sell.exchange {
            let mut request = r.request();
            request.exchange = Some(sultan_client::ExchangeRequest {
                sale_id: h.sell.cart.id.clone(),
                sale: body,
            });
            return vec![Effect::After {
                token,
                call: crate::after::Call::Exchange {
                    id: r.id.clone(),
                    body: Box::new(request),
                    override_token,
                },
                generation,
            }];
        }
        vec![Effect::PostSale {
            token,
            id: h.sell.cart.id.clone(),
            body: Box::new(body),
            override_token,
            generation,
        }]
    }

    /// The answer of a posting.
    pub(crate) fn posted(
        &mut self,
        generation: u64,
        result: Result<Sale, ClientError>,
    ) -> Vec<Effect> {
        self.posted_with(generation, result, 0)
    }

    /// The answer of a posting; `refund` is the cash an exchange pays back besides.
    pub(crate) fn posted_with(
        &mut self,
        generation: u64,
        result: Result<Sale, ClientError>,
        refund: i64,
    ) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        self.busy = false;
        let token = self.token();
        match result {
            Ok(sale) => {
                self.approval = None;
                self.retry = None;
                self.customer = None;
                let Some(h) = home(&mut self.screen) else {
                    return Vec::new();
                };
                let drawer = h.sell.pay.as_ref().is_some_and(Pay::opens_drawer) || refund > 0;
                let id = sale.summary.id.clone();
                h.sell = Sell {
                    done: Some(Done {
                        sale: Box::new(sale),
                        receipt: Vec::new(),
                        drawer,
                        refund,
                    }),
                    ..Sell::default()
                };
                h.notice = None;
                vec![
                    Effect::DeleteCart,
                    Effect::Receipt {
                        token,
                        id,
                        generation,
                    },
                ]
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(e) if matches!(e.code(), Some("needs_override" | "return_window")) => {
                self.sale_needs_override(&e)
            }
            Err(e) if e.code() == Some("cart_stale") => {
                self.approval = None;
                self.retry = None;
                let total = e
                    .problem()
                    .and_then(|p| p.extra.get("quote"))
                    .and_then(|q| q["totals"]["total"].as_i64());
                if let Some(h) = home(&mut self.screen) {
                    h.sell.pay = None;
                    h.notice = Some(Notice::new(
                        "cart_stale",
                        total.map(rupiah).unwrap_or_default(),
                        Tone::Bad,
                    ));
                }
                self.requote()
            }
            Err(e) => {
                let invalid = e.code() == Some("invalid_override");
                if let Some(pay) = self.sell_mut().and_then(|s| s.pay.as_mut()) {
                    pay.posting = false;
                    if invalid {
                        pay.tokens.clear();
                        pay.approved.clear();
                    }
                }
                if let Some(a) = &mut self.approval {
                    a.notice = Some(notice_for(&e));
                } else {
                    self.set_notice(notice_for(&e));
                }
                Vec::new()
            }
        }
    }

    /// `403 needs_override` at posting: the next check to approve, with the value the register
    /// computes for it ([`Cart::approvals`]).
    fn sale_needs_override(&mut self, e: &ClientError) -> Vec<Effect> {
        let problem = e.problem();
        let permission = problem
            .and_then(sultan_client::Problem::permission)
            .unwrap_or_default()
            .to_owned();
        let limit = problem.and_then(sultan_client::Problem::limit);
        let Some(state) = self.sell() else {
            return Vec::new();
        };
        if permission.starts_with("refund.")
            && let Some(r) = &state.exchange
        {
            // The return's own checks (window, no-receipt limit) take the return's ID (§19 item 36).
            let value = (permission == "refund.no_receipt").then(|| r.refund());
            let id = r.id.clone();
            self.ask_approval(
                ProtectedAction {
                    action: permission,
                    value,
                    context_ref: id,
                },
                limit,
                Retry::Sale,
            );
            return Vec::new();
        }
        let id = state.cart.id.clone();
        let approved = state
            .pay
            .as_ref()
            .map(|p| p.approved.clone())
            .unwrap_or_default();
        let mut used = approved;
        let value = state
            .cart
            .approvals()
            .unwrap_or_default()
            .into_iter()
            .filter(|(p, _)| *p == permission)
            .find(|(p, v)| {
                if let Some(i) = used.iter().position(|(ap, av)| ap == p && av == v) {
                    used.remove(i);
                    return false;
                }
                v.zip(limit).is_none_or(|(v, l)| v > l)
            })
            .and_then(|(_, v)| v);
        self.ask_approval(
            ProtectedAction {
                action: permission,
                value,
                context_ref: id,
            },
            limit,
            Retry::Sale,
        );
        Vec::new()
    }

    /// The receipt text of the posted sale.
    pub(crate) fn receipt_loaded(
        &mut self,
        generation: u64,
        result: Result<Receipt, ClientError>,
    ) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        match (result, &mut h.sell.done) {
            (Ok(r), Some(done)) if r.sale_id == done.sale.summary.id => done.receipt = r.lines,
            (Err(e), Some(_)) => h.notice = Some(notice_for(&e)),
            _ => {}
        }
        Vec::new()
    }

    /// The tender types (loaded after login).
    pub(crate) fn tenders_loaded(
        &mut self,
        generation: u64,
        result: Result<Vec<TenderType>, ClientError>,
    ) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        match result {
            Ok(types) => {
                self.tenders = types;
                Vec::new()
            }
            Err(e) if ends_login(&e) => self.fail(&e),
            Err(_) => Vec::new(),
        }
    }

    /// The crash copy read after login: restored for the same cashier session only (D2).
    pub(crate) fn cart_file(&mut self, generation: u64, contents: Option<&str>) -> Vec<Effect> {
        if generation != self.generation || self.login.is_none() {
            return Vec::new();
        }
        let Some(saved) = contents.and_then(|c| serde_json::from_str::<Saved>(c).ok()) else {
            return Vec::new();
        };
        let session = self.session.as_ref().map(|s| s.id.clone());
        if session.as_deref() != Some(saved.session_id.as_str()) || saved.lines.is_empty() {
            return Vec::new();
        }
        let Some(h) = home(&mut self.screen) else {
            return Vec::new();
        };
        if !h.sell.cart.is_empty() {
            return Vec::new();
        }
        h.sell.cart = saved.cart();
        h.notice = Some(Notice::new(
            "cart_restored",
            saved.lines.len().to_string(),
            Tone::Neutral,
        ));
        if saved.customer.is_some() {
            self.customer = saved.customer;
        }
        self.requote()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries() {
        let mut e = Entry::new(EntryKind::Qty(0));
        for c in "1,5,2".chars() {
            e.push(c);
        }
        assert_eq!(e.text, "1,52");
        assert_eq!(e.qty(), Some(Qty::from_thousandths(1_520)));
        assert!(!e.push('x'));
        let mut d = Entry::new(EntryKind::CartDiscount);
        assert!(d.pct);
        d.text = "12,5".to_owned();
        assert_eq!(d.discount(), Ok(Some(Discount::Pct(1_250))));
        d.text.clear();
        assert_eq!(d.discount(), Ok(None));
        d.pct = false;
        assert!(!d.push(','), "Rupiah have no decimals");
        d.text = "5000".to_owned();
        assert_eq!(
            d.discount(),
            Ok(Some(Discount::Amount(
                sultan_core::money::Money::from_minor(5_000)
            )))
        );
        d.pct = true;
        d.text = "100".to_owned();
        assert_eq!(d.discount(), Ok(Some(Discount::Pct(10_000))));
        d.text = "999999999".to_owned();
        assert_eq!(d.discount(), Err(()));
        let mut long = Entry::new(EntryKind::Price(0));
        for _ in 0..20 {
            long.push('9');
        }
        assert_eq!(long.text.len(), 16);
        assert_eq!(long.amount(), Some(9_999_999_999_999_999));
    }

    #[test]
    fn sale_notices() {
        use sultan_core::money::Money;
        let cases = [
            (
                SaleError::TenderOverRest {
                    rest: Money::from_minor(500),
                },
                "tender_over_rest",
            ),
            (SaleError::CartTooLarge { max: 300 }, "cart_too_large"),
            (
                SaleError::TenderReferenceRequired,
                "tender_reference_required",
            ),
            (SaleError::InvalidQty { reason: "x" }, "invalid_qty"),
            (
                SaleError::InvalidField {
                    field: "line",
                    reason: "x",
                },
                "line_fixed",
            ),
            (
                SaleError::InvalidField {
                    field: "tax_invoice",
                    reason: "x",
                },
                "tax_invoice_needs_tax_id",
            ),
            (
                SaleError::InvalidField {
                    field: "tender_type",
                    reason: "x",
                },
                "no_cash_tender",
            ),
            (
                SaleError::InvalidField {
                    field: "customer",
                    reason: "x",
                },
                "needs_customer",
            ),
            (
                SaleError::InvalidField {
                    field: "discount",
                    reason: "x",
                },
                "invalid_discount",
            ),
            (
                SaleError::InvalidField {
                    field: "amount",
                    reason: "x",
                },
                "invalid_amount",
            ),
            (SaleError::CartStale, "error"),
        ];
        for (error, code) in cases {
            assert_eq!(sale_notice(&error).code, code, "{error:?}");
        }
    }
}
