//! The register's customer step (spec 2.5 §6, §7, §10, §11; phase M5): the dialog that finds a
//! customer, signs one up and pays a balance out, and the line the session home shows for the
//! attached customer. Pure state like [`crate::controller`], which opens the dialog (F3), routes
//! keys and actions to it and runs the service calls it asks for ([`Call`]).
//!
//! Pages ([`Page`]):
//! 1. **Find** — one field. By default it takes a **phone or a member code**, typed or scanned
//!    (the scanner types into this field, so a scanned code here is always a member code, never a
//!    product barcode: spec 2.5 §6). [`classify`] decides which lookup runs. With
//!    `customer.search`, Tab switches to a **name** search (at most 20 results, ↑/↓ and Enter or
//!    a click open one); without it the option is hidden. F6 opens the sign-up (`customer.create`).
//! 2. **Shown** — name, member code, masked phone, group, tier (a warning when it no longer
//!    applies), balance, the receipt line (§10) and the recent receipts (empty until 2.6). Enter
//!    attaches the customer; F7 pays the balance out (`customer.balance_payout`, balance above 0).
//! 3. **Sign-up** — name and phone (optional); the client draws the `UUIDv7` once per dialog, so
//!    a retried sign-up sends the same id. `phone_taken` names the other customer and F8 shows
//!    them, so the cashier can attach them instead.
//! 4. **Payout** — an amount typed as digits, Enter asks "pay out …?", Enter again pays it out
//!    with an `Idempotency-Key` that is kept for a retry after a network error and renewed after
//!    any other answer. The server checks the balance (`insufficient_balance`) and the open
//!    session (`no_open_session`).
//!
//! Esc goes back one page, and closes the dialog from Find.

use sultan_client::customers::{CUSTOMER_BALANCE_PAYOUT, CUSTOMER_CREATE, CUSTOMER_SEARCH};
use sultan_client::{ClientError, Customer, CustomerBody, CustomerPage, Payout};
use sultan_core::customer::{CustomerCode, MAX_CUSTOMER_NAME_LEN, Phone, ReceiptCustomer};

use crate::amount::{Digits, rupiah};
use crate::controller::{Action, Notice, Shortcut, Tone, new_key, notice_for};
use crate::input::Key;

/// Longest text of a field (the longest customer name).
pub const MAX_TEXT: usize = MAX_CUSTOMER_NAME_LEN;
/// Longest typed phone (with separators).
pub const MAX_PHONE: usize = 24;

/// The customer permissions the register shows or hides actions by (`GET /auth/me`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Perms {
    /// `customer.search`: name search.
    pub search: bool,
    /// `customer.create`: sign-up.
    pub create: bool,
    /// `customer.balance_payout`: pay a balance out.
    pub payout: bool,
}

impl Perms {
    /// The permissions among `keys`.
    #[must_use]
    pub fn of(keys: &[String]) -> Self {
        let holds = |key: &str| keys.iter().any(|k| k == key);
        Self {
            search: holds(CUSTOMER_SEARCH),
            create: holds(CUSTOMER_CREATE),
            payout: holds(CUSTOMER_BALANCE_PAYOUT),
        }
    }
}

/// What a typed or scanned phone-or-code looks up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// `GET /customers/lookup?phone=`.
    Phone(String),
    /// `GET /customers/lookup?code=`.
    Code(String),
}

/// The lookup for `text`: a member code when it has a letter or is a valid code (`23456789`
/// has neither `0` nor `1`, which every phone has), else a phone. `None` when blank. The server
/// checks either form (`invalid_code`, `invalid_phone`).
#[must_use]
pub fn classify(text: &str) -> Option<Lookup> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.chars().any(|c| c.is_ascii_alphabetic()) || CustomerCode::parse(text).is_ok() {
        Some(Lookup::Code(text.to_owned()))
    } else {
        Some(Lookup::Phone(text.to_owned()))
    }
}

/// The receipt line of spec 2.5 §10 (`Budi S. · 0812****789 · Gold`), with the tier only when
/// it applies today.
#[must_use]
pub fn receipt_line(c: &Customer) -> String {
    let phone = c.phone.as_deref().and_then(|p| Phone::parse(p).ok());
    ReceiptCustomer {
        name: &c.name,
        company_name: c.company_name.as_deref(),
        phone: phone.as_ref(),
        tier_name: c
            .tier_name
            .as_deref()
            .filter(|_| c.tier_applies == Some(true)),
    }
    .line()
}

/// A service call the dialog asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// [`sultan_client::Service::permissions`] (after login).
    Permissions,
    /// [`sultan_client::Service::customer_by_phone`].
    ByPhone(String),
    /// [`sultan_client::Service::customer_by_code`].
    ByCode(String),
    /// [`sultan_client::Service::search_customers`].
    Search(String),
    /// [`sultan_client::Service::sign_up_customer`].
    SignUp {
        /// The new customer's id (the same on a retry).
        id: String,
        /// Name and phone.
        body: Box<CustomerBody>,
    },
    /// [`sultan_client::Service::balance_payout`].
    Payout {
        /// The customer.
        id: String,
        /// Amount paid out.
        amount: i64,
        /// Idempotency key.
        key: String,
    },
}

/// The answer to a [`Call`].
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// To [`Call::Permissions`].
    Permissions(Result<Vec<String>, ClientError>),
    /// To [`Call::ByPhone`] / [`Call::ByCode`].
    Found(Result<Customer, ClientError>),
    /// To [`Call::Search`].
    Searched(Result<CustomerPage, ClientError>),
    /// To [`Call::SignUp`].
    SignedUp(Result<Customer, ClientError>),
    /// To [`Call::Payout`].
    PaidOut(Result<Payout, ClientError>),
}

impl Reply {
    /// The error, if the call failed.
    #[must_use]
    pub const fn error(&self) -> Option<&ClientError> {
        match self {
            Self::Permissions(Err(e))
            | Self::Found(Err(e))
            | Self::Searched(Err(e))
            | Self::SignedUp(Err(e))
            | Self::PaidOut(Err(e)) => Some(e),
            _ => None,
        }
    }
}

/// The message for a failed customer call; `asked` is what was looked up. Codes of
/// `crates/server/src/error.rs`; others as [`notice_for`].
#[must_use]
pub fn customer_notice(error: &ClientError, asked: &str) -> Notice {
    let Some(problem) = error.problem() else {
        return notice_for(error);
    };
    match problem.code.as_str() {
        "not_found" => Notice::new("customer_not_found", asked, Tone::Bad),
        "invalid_phone" => Notice::bad("invalid_phone"),
        "invalid_code" => Notice::new("invalid_code", asked, Tone::Bad),
        "phone_taken" => {
            let who = [problem.customer_code(), problem.masked_phone()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
            Notice::new("phone_taken", who, Tone::Bad)
        }
        "insufficient_balance" => Notice::new(
            "insufficient_balance",
            rupiah(problem.balance().unwrap_or_default()),
            Tone::Bad,
        ),
        "invalid_field" if problem.field() == Some("name") => Notice::bad("invalid_name"),
        "invalid_field" if problem.field().is_some_and(|f| f != "amount") => Notice::new(
            "invalid_customer_field",
            problem.field().unwrap_or_default(),
            Tone::Bad,
        ),
        _ => notice_for(error),
    }
}

/// The dialog's pages; see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    /// Phone, code or name.
    Find,
    /// One customer.
    Shown,
    /// A new customer.
    SignUp,
    /// Paying the balance out.
    Payout,
}

impl Page {
    /// The name the UI switches on.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Find => "find",
            Self::Shown => "shown",
            Self::SignUp => "sign-up",
            Self::Payout => "payout",
        }
    }
}

/// The Find page.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Find {
    /// Name search (Tab) instead of phone or code.
    pub by_name: bool,
    /// What is typed.
    pub query: String,
    /// The name search's results.
    pub results: Vec<Customer>,
    /// The highlighted result.
    pub cursor: usize,
}

/// The sign-up form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignUp {
    /// The new customer's id, drawn once per dialog (a retry sends the same).
    pub id: String,
    /// The field being typed: 0 name, 1 phone.
    pub field: usize,
    /// Name.
    pub name: String,
    /// Phone (optional).
    pub phone: String,
    /// `phone_taken`: the member code of the customer holding the phone.
    pub taken: Option<String>,
}

/// The payout form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PayoutForm {
    /// The amount.
    pub amount: Digits,
    /// Asking "pay out …?".
    pub confirm: bool,
    key: String,
}

impl PayoutForm {
    fn new() -> Self {
        Self {
            amount: Digits::amount(),
            confirm: false,
            key: new_key(),
        }
    }
}

/// What a dialog action leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing outside the dialog.
    Stay,
    /// Close the dialog.
    Close,
    /// Make this call.
    Call(Call),
    /// Attach this customer and close the dialog.
    Attach(Box<Customer>),
}

/// The customer dialog; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dialog {
    /// The page.
    pub page: Page,
    /// The Find page.
    pub find: Find,
    /// The customer on the Shown and Payout pages.
    pub shown: Option<Customer>,
    /// The sign-up form.
    pub sign_up: SignUp,
    /// The payout form.
    pub payout: PayoutForm,
    /// The last result or error.
    pub notice: Option<Notice>,
    /// The last phone or code looked up (for its error message and the sign-up's phone).
    asked: Option<Lookup>,
}

impl Dialog {
    /// A dialog on the attached customer, or on the Find page.
    #[must_use]
    pub fn new(attached: Option<&Customer>) -> Self {
        Self {
            page: if attached.is_some() {
                Page::Shown
            } else {
                Page::Find
            },
            find: Find::default(),
            shown: attached.cloned(),
            sign_up: SignUp {
                id: new_key(),
                field: 0,
                name: String::new(),
                phone: String::new(),
                taken: None,
            },
            payout: PayoutForm::new(),
            notice: None,
            asked: None,
        }
    }

    /// Whether F7 pays out: the permission and a balance above 0 (a negative balance is written
    /// off by an Owner, which no register user is).
    fn can_pay_out(&self, perms: Perms) -> bool {
        perms.payout && self.shown.as_ref().is_some_and(|c| c.balance > 0)
    }

    /// The key chips of the page.
    #[must_use]
    pub fn shortcuts(&self, perms: Perms) -> Vec<Shortcut> {
        let s = |key, action, label| Shortcut { key, action, label };
        let mut keys = Vec::new();
        match self.page {
            Page::Find => {
                let find = if self.find.results.is_empty() {
                    "find"
                } else {
                    "open_customer"
                };
                keys.push(s(Key::Enter, Action::Confirm, find));
                if perms.search {
                    let other = if self.find.by_name {
                        "by_phone_or_code"
                    } else {
                        "by_name"
                    };
                    keys.push(s(Key::Tab, Action::SwitchMode, other));
                }
                if perms.create {
                    keys.push(s(Key::F(6), Action::NewCustomer, "new_customer"));
                }
                keys.push(s(Key::Escape, Action::Back, "close"));
            }
            Page::Shown => {
                keys.push(s(Key::Enter, Action::Confirm, "attach"));
                if self.can_pay_out(perms) {
                    keys.push(s(Key::F(7), Action::PayOut, "pay_out"));
                }
                keys.push(s(Key::Escape, Action::Back, "back"));
            }
            Page::SignUp => {
                keys.push(s(Key::Enter, Action::Confirm, "sign_up"));
                keys.push(s(Key::Tab, Action::NextField, "next_field"));
                if self.sign_up.taken.is_some() {
                    keys.push(s(Key::F(8), Action::ShowTaken, "show_that_customer"));
                }
                keys.push(s(Key::Escape, Action::Back, "back"));
            }
            Page::Payout if self.payout.confirm => {
                keys.push(s(Key::Enter, Action::Confirm, "confirm_payout"));
                keys.push(s(Key::Escape, Action::Back, "edit_amount"));
            }
            Page::Payout => {
                keys.push(s(Key::Enter, Action::Confirm, "pay_out"));
                keys.push(s(Key::Escape, Action::Back, "back"));
            }
        }
        keys
    }

    /// Typing and moving within the page. Returns whether the key was used.
    pub fn edit(&mut self, key: Key) -> bool {
        let typed = match key {
            Key::Digit(d) => Some(char::from(b'0' + d)),
            Key::Char(c) => Some(c),
            _ => None,
        };
        match self.page {
            Page::Find => self.find_edit(key, typed),
            Page::SignUp => self.sign_up_edit(key, typed),
            Page::Payout if !self.payout.confirm => {
                match key {
                    Key::Digit(d) => self.payout.amount.push(d),
                    Key::Backspace => self.payout.amount.pop(),
                    _ => return false,
                };
                self.notice = None;
                true
            }
            _ => false,
        }
    }

    fn find_edit(&mut self, key: Key, typed: Option<char>) -> bool {
        let f = &mut self.find;
        let count = f.results.len();
        match (key, typed) {
            (Key::Up, _) if count > 0 => f.cursor = f.cursor.saturating_sub(1),
            (Key::Down, _) if count > 0 => f.cursor = (f.cursor + 1).min(count - 1),
            (Key::Backspace, _) => {
                f.query.pop();
                f.results.clear();
                self.notice = None;
            }
            (_, Some(c)) => {
                if f.query.chars().count() >= MAX_TEXT {
                    return true;
                }
                f.query.push(c);
                f.results.clear();
                f.cursor = 0;
                self.notice = None;
            }
            _ => return false,
        }
        true
    }

    fn sign_up_edit(&mut self, key: Key, typed: Option<char>) -> bool {
        let s = &mut self.sign_up;
        match (key, typed) {
            (Key::Up | Key::Down | Key::BackTab, _) => s.field = 1 - s.field,
            (Key::Backspace, _) if s.field == 0 => {
                s.name.pop();
            }
            (Key::Backspace, _) => {
                s.phone.pop();
                s.taken = None;
            }
            (_, Some(c)) if s.field == 0 => {
                if s.name.chars().count() < MAX_TEXT {
                    s.name.push(c);
                }
            }
            (_, Some(c)) => {
                if s.phone.len() < MAX_PHONE && (c.is_ascii_digit() || "+-.() ".contains(c)) {
                    s.phone.push(c);
                    s.taken = None;
                }
            }
            _ => return false,
        }
        if key != Key::Up && key != Key::Down && key != Key::BackTab {
            self.notice = None;
        }
        true
    }

    /// A click on a name-search result: opens it.
    pub fn pick(&mut self, index: usize) {
        if self.page == Page::Find && index < self.find.results.len() {
            self.find.cursor = index;
            self.open_result();
        }
    }

    /// A click on a sign-up field: types into it.
    pub fn pick_field(&mut self, index: usize) {
        if self.page == Page::SignUp && index < 2 {
            self.sign_up.field = index;
        }
    }

    fn open_result(&mut self) {
        self.shown = self.find.results.get(self.find.cursor).cloned();
        self.page = Page::Shown;
        self.notice = None;
    }

    /// Handles a key chip or button of the page (the caller checked that it is on the page).
    pub fn act(&mut self, action: Action) -> Outcome {
        match action {
            Action::Confirm => return self.confirm(),
            Action::Back => match self.page {
                Page::Find => return Outcome::Close,
                Page::Payout if self.payout.confirm => self.payout.confirm = false,
                Page::Payout => self.page = Page::Shown,
                Page::Shown | Page::SignUp => self.page = Page::Find,
            },
            Action::SwitchMode => {
                self.find = Find {
                    by_name: !self.find.by_name,
                    ..Find::default()
                };
            }
            Action::NewCustomer => {
                let s = &mut self.sign_up;
                let query = self.find.query.trim();
                if self.find.by_name {
                    query.clone_into(&mut s.name);
                } else if let Some(Lookup::Phone(phone)) = classify(query).or(self.asked.clone()) {
                    s.phone = phone;
                }
                s.field = usize::from(!s.name.is_empty());
                s.taken = None;
                self.page = Page::SignUp;
            }
            Action::PayOut => {
                self.payout = PayoutForm::new();
                self.page = Page::Payout;
            }
            Action::NextField => self.sign_up.field = 1 - self.sign_up.field,
            Action::ShowTaken => {
                if let Some(code) = self.sign_up.taken.clone() {
                    self.asked = Some(Lookup::Code(code.clone()));
                    return Outcome::Call(Call::ByCode(code));
                }
            }
            _ => {}
        }
        self.notice = None;
        Outcome::Stay
    }

    fn confirm(&mut self) -> Outcome {
        match self.page {
            Page::Find if !self.find.results.is_empty() => {
                self.open_result();
                Outcome::Stay
            }
            Page::Find if self.find.by_name => {
                let q = self.find.query.trim();
                if q.is_empty() {
                    self.notice = Some(Notice::bad("type_name"));
                    return Outcome::Stay;
                }
                Outcome::Call(Call::Search(q.to_owned()))
            }
            Page::Find => {
                // Emptied at once, like the barcode field, so the next scan starts afresh.
                let Some(lookup) = classify(&std::mem::take(&mut self.find.query)) else {
                    self.notice = Some(Notice::bad("type_phone_or_code"));
                    return Outcome::Stay;
                };
                self.asked = Some(lookup.clone());
                Outcome::Call(match lookup {
                    Lookup::Phone(p) => Call::ByPhone(p),
                    Lookup::Code(c) => Call::ByCode(c),
                })
            }
            Page::Shown => self
                .shown
                .clone()
                .map_or(Outcome::Stay, |c| Outcome::Attach(Box::new(c))),
            Page::SignUp => {
                let name = self.sign_up.name.trim();
                if name.is_empty() {
                    self.sign_up.field = 0;
                    self.notice = Some(Notice::bad("name_required"));
                    return Outcome::Stay;
                }
                let phone = Some(self.sign_up.phone.trim()).filter(|p| !p.is_empty());
                Outcome::Call(Call::SignUp {
                    id: self.sign_up.id.clone(),
                    body: Box::new(CustomerBody::sign_up(name, phone)),
                })
            }
            Page::Payout if !self.payout.confirm => {
                if self.payout.amount.value() <= 0 {
                    self.notice = Some(Notice::bad("invalid_amount"));
                } else {
                    self.payout.confirm = true;
                    self.notice = None;
                }
                Outcome::Stay
            }
            Page::Payout => self.shown.as_ref().map_or(Outcome::Stay, |c| {
                Outcome::Call(Call::Payout {
                    id: c.id.clone(),
                    amount: self.payout.amount.value(),
                    key: self.payout.key.clone(),
                })
            }),
        }
    }

    fn asked_text(&self) -> String {
        match &self.asked {
            Some(Lookup::Phone(t) | Lookup::Code(t)) => t.clone(),
            None => String::new(),
        }
    }

    /// The answer to a call this dialog asked for. Returns the customer after a payout, so the
    /// attached customer's balance can follow.
    pub fn reply(&mut self, reply: Reply) -> Option<Customer> {
        match reply {
            Reply::Found(Ok(c)) => {
                self.shown = Some(c);
                self.page = Page::Shown;
                self.notice = None;
            }
            Reply::Searched(Ok(page)) => {
                self.find.results = page.items;
                self.find.cursor = 0;
                self.notice = self.find.results.is_empty().then(|| {
                    Notice::new("no_customer_named", self.find.query.trim(), Tone::Neutral)
                });
            }
            Reply::SignedUp(Ok(c)) => {
                let code = c.display_code.clone();
                self.shown = Some(c);
                self.page = Page::Shown;
                self.notice = Some(Notice::new("signed_up", code, Tone::Good));
            }
            Reply::SignedUp(Err(e)) => {
                if let Some(p) = e.problem().filter(|p| p.code == "phone_taken") {
                    self.sign_up.taken = p.customer_code().map(str::to_owned);
                }
                self.notice = Some(customer_notice(&e, ""));
            }
            Reply::PaidOut(Ok(paid)) => {
                let shown = self.shown.as_mut()?;
                shown.balance = paid.balance;
                self.page = Page::Shown;
                self.notice = Some(Notice::new(
                    "paid_out",
                    rupiah(paid.entry.amount.abs()),
                    Tone::Good,
                ));
                self.payout = PayoutForm::new();
                return Some(shown.clone());
            }
            Reply::PaidOut(Err(e)) => {
                self.payout.confirm = false;
                if !matches!(e, ClientError::Network(_)) {
                    self.payout.key = new_key();
                }
                self.notice = Some(customer_notice(&e, ""));
            }
            Reply::Found(Err(e)) | Reply::Searched(Err(e)) => {
                self.notice = Some(customer_notice(&e, &self.asked_text()));
            }
            Reply::Permissions(_) => {}
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use sultan_client::{BalanceEntry, Problem};

    use super::*;

    fn api(problem: Problem) -> ClientError {
        ClientError::Api(problem)
    }

    fn budi() -> Customer {
        Customer {
            id: "c1".into(),
            code: "K7QH29XM".into(),
            display_code: "K7QH-29XM".into(),
            name: "Budi Santoso".into(),
            phone: Some("+628123456789".into()),
            tier_name: Some("Gold".into()),
            tier_applies: Some(true),
            balance: 2_200,
            ..Customer::default()
        }
    }

    const ALL: Perms = Perms {
        search: true,
        create: true,
        payout: true,
    };

    fn labels(d: &Dialog, perms: Perms) -> Vec<&'static str> {
        d.shortcuts(perms).iter().map(|s| s.label).collect()
    }

    fn typed(d: &mut Dialog, text: &str) {
        for c in text.chars() {
            let key = c
                .to_digit(10)
                .map_or(Key::Char(c), |n| Key::Digit(u8::try_from(n).unwrap()));
            assert!(d.edit(key), "{c:?}");
        }
    }

    #[test]
    fn phones_and_codes_are_told_apart() {
        #[rustfmt::skip]
        let cases = [
            ("", None),
            ("  ", None),
            ("K7QH-29XM", Some(Lookup::Code("K7QH-29XM".into()))),
            ("k7qh29xm", Some(Lookup::Code("k7qh29xm".into()))),
            ("K7QH-29X0", Some(Lookup::Code("K7QH-29X0".into()))),
            ("23456789", Some(Lookup::Code("23456789".into()))),
            (" 0812-3456-789 ", Some(Lookup::Phone("0812-3456-789".into()))),
            ("+628123456789", Some(Lookup::Phone("+628123456789".into()))),
            ("12", Some(Lookup::Phone("12".into()))),
        ];
        for (text, lookup) in cases {
            assert_eq!(classify(text), lookup, "{text:?}");
        }
    }

    #[test]
    fn receipt_lines_and_permissions() {
        let mut c = budi();
        assert_eq!(receipt_line(&c), "Budi S. · 0812****789 · Gold");
        c.tier_applies = Some(false);
        c.phone = None;
        c.company_name = Some("Toko Budi".into());
        assert_eq!(receipt_line(&c), "Toko Budi");
        let keys = vec![CUSTOMER_CREATE.to_owned(), "sale.discount".to_owned()];
        assert_eq!(
            Perms::of(&keys),
            Perms {
                create: true,
                ..Perms::default()
            }
        );
        for page in [Page::Find, Page::Shown, Page::SignUp, Page::Payout] {
            assert!(!page.name().is_empty());
        }
    }

    #[test]
    fn errors_become_customer_messages() {
        let p = |status, code| Problem::new(status, code, "x");
        #[rustfmt::skip]
        let cases = [
            (api(p(404, "not_found")), "customer_not_found", "K7QH"),
            (api(p(422, "invalid_phone")), "invalid_phone", ""),
            (api(p(422, "invalid_code")), "invalid_code", "K7QH"),
            (api(p(409, "phone_taken").with("customer_code", "K7QH-29XM")
                .with("masked_phone", "0812****789")), "phone_taken", "K7QH-29XM · 0812****789"),
            (api(p(422, "insufficient_balance").with("balance", 2_200)),
                "insufficient_balance", "Rp 2.200"),
            (api(p(422, "invalid_field").with("field", "name")), "invalid_name", ""),
            (api(p(422, "invalid_field").with("field", "email")), "invalid_customer_field", "email"),
            (api(p(422, "invalid_field").with("field", "amount")), "invalid_amount", ""),
            (api(p(409, "no_open_session")), "no_open_session", ""),
            (api(p(403, "permission_denied")), "permission_denied", ""),
            (ClientError::Network("x".into()), "network", ""),
        ];
        for (error, code, arg) in cases {
            let n = customer_notice(&error, "K7QH");
            assert_eq!((n.code, n.arg.as_str()), (code, arg), "{error:?}");
        }
        let failed = Reply::Found(Err(ClientError::Network("x".into())));
        assert!(failed.error().is_some());
        assert!(Reply::Permissions(Ok(Vec::new())).error().is_none());
    }

    #[test]
    fn find_by_phone_or_code_then_attach() {
        let mut d = Dialog::new(None);
        assert_eq!(d.page, Page::Find);
        assert_eq!(labels(&d, Perms::default()), ["find", "close"]);
        assert_eq!(
            labels(&d, ALL),
            ["find", "by_name", "new_customer", "close"]
        );
        // Nothing typed.
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
        assert_eq!(d.notice.as_ref().unwrap().code, "type_phone_or_code");
        typed(&mut d, "k7qh-29xm");
        assert!(d.notice.is_none(), "typing hides the message");
        assert!(d.edit(Key::Backspace));
        assert!(!d.edit(Key::F(1)));
        assert!(!d.edit(Key::Up), "no results to move in");
        typed(&mut d, "m");
        assert_eq!(
            d.act(Action::Confirm),
            Outcome::Call(Call::ByCode("k7qh-29xm".into()))
        );
        assert_eq!(d.find.query, "", "emptied for the next scan");
        d.reply(Reply::Found(Err(api(Problem::new(404, "not_found", "x")))));
        let n = d.notice.clone().unwrap();
        assert_eq!(
            (n.code, n.arg.as_str()),
            ("customer_not_found", "k7qh-29xm")
        );
        d.reply(Reply::Found(Ok(budi())));
        assert_eq!((d.page, d.notice.as_ref()), (Page::Shown, None));
        assert_eq!(labels(&d, Perms::default()), ["attach", "back"]);
        assert_eq!(labels(&d, ALL), ["attach", "pay_out", "back"]);
        assert!(!d.edit(Key::Digit(1)), "nothing to type on the Shown page");
        assert_eq!(d.act(Action::Confirm), Outcome::Attach(Box::new(budi())));
        assert_eq!(d.act(Action::Back), Outcome::Stay);
        assert_eq!(d.page, Page::Find);
        assert_eq!(d.act(Action::Back), Outcome::Close);
        // An unrelated action changes nothing.
        assert_eq!(d.act(Action::Lock), Outcome::Stay);

        // The attached customer opens on the Shown page.
        let d = Dialog::new(Some(&budi()));
        assert_eq!(d.page, Page::Shown);
        // Without a shown customer, Enter does nothing.
        let mut d = Dialog::new(None);
        d.page = Page::Shown;
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
        d.page = Page::Payout;
        d.payout.confirm = true;
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
    }

    #[test]
    fn name_search_lists_and_opens() {
        let mut d = Dialog::new(None);
        assert_eq!(d.act(Action::SwitchMode), Outcome::Stay);
        assert!(d.find.by_name);
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
        assert_eq!(d.notice.as_ref().unwrap().code, "type_name");
        typed(&mut d, "Sa ");
        assert_eq!(
            d.act(Action::Confirm),
            Outcome::Call(Call::Search("Sa".into()))
        );
        assert_eq!(d.find.query, "Sa ", "kept with the results");
        d.reply(Reply::Searched(Ok(CustomerPage::default())));
        let n = d.notice.clone().unwrap();
        assert_eq!((n.code, n.arg.as_str()), ("no_customer_named", "Sa"));
        let siti = Customer {
            id: "c2".into(),
            name: "Siti".into(),
            ..Customer::default()
        };
        d.reply(Reply::Searched(Ok(CustomerPage {
            items: vec![budi(), siti.clone()],
            next_page: None,
        })));
        assert!(d.notice.is_none());
        assert_eq!(labels(&d, ALL)[..2], ["open_customer", "by_phone_or_code"]);
        for (key, cursor) in [(Key::Down, 1), (Key::Down, 1), (Key::Up, 0), (Key::Up, 0)] {
            assert!(d.edit(key));
            assert_eq!(d.find.cursor, cursor);
        }
        d.edit(Key::Down);
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
        assert_eq!(d.shown.as_ref(), Some(&siti));
        d.act(Action::Back);
        d.pick(9);
        assert_eq!(d.page, Page::Find, "no such row");
        d.pick(0);
        assert_eq!(d.shown.as_ref().map(|c| c.id.as_str()), Some("c1"));
        // Typing starts a new search.
        d.act(Action::Back);
        typed(&mut d, "x");
        assert!(d.find.results.is_empty());
        d.find.query = "a".repeat(MAX_TEXT);
        assert!(d.edit(Key::Char('b')));
        assert_eq!(d.find.query.len(), MAX_TEXT, "full");
        d.reply(Reply::Searched(Err(api(Problem::new(
            403,
            "permission_denied",
            "x",
        )))));
        assert_eq!(d.notice.as_ref().unwrap().code, "permission_denied");
        // Back to phone or code.
        d.act(Action::SwitchMode);
        assert!(!d.find.by_name && d.find.query.is_empty());
        // Permissions answers are not the dialog's business.
        assert_eq!(d.reply(Reply::Permissions(Ok(Vec::new()))), None);
    }

    #[test]
    fn sign_up_keeps_its_id_and_offers_the_taken_customer() {
        let mut d = Dialog::new(None);
        // A phone that was not found is offered for the sign-up.
        typed(&mut d, "0812-0000-111");
        d.act(Action::Confirm);
        d.act(Action::NewCustomer);
        assert_eq!(d.page, Page::SignUp);
        assert_eq!(
            (d.sign_up.phone.as_str(), d.sign_up.field),
            ("0812-0000-111", 0)
        );
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
        assert_eq!(d.notice.as_ref().unwrap().code, "name_required");
        typed(&mut d, "Rudi Hartono");
        assert!(d.notice.is_none());
        assert!(d.edit(Key::Backspace));
        typed(&mut d, "o");
        assert!(d.edit(Key::Down));
        assert_eq!(d.sign_up.field, 1);
        typed(&mut d, "x");
        assert_eq!(
            d.sign_up.phone, "0812-0000-111",
            "letters are not part of a phone"
        );
        let id = d.sign_up.id.clone();
        let call = d.act(Action::Confirm);
        assert_eq!(
            call,
            Outcome::Call(Call::SignUp {
                id: id.clone(),
                body: Box::new(CustomerBody::sign_up("Rudi Hartono", Some("0812-0000-111"))),
            })
        );
        // The phone is taken: F8 shows that customer.
        let taken = Problem::new(409, "phone_taken", "x")
            .with("customer_code", "K7QH-29XM")
            .with("masked_phone", "0812****789");
        d.reply(Reply::SignedUp(Err(api(taken))));
        assert_eq!(d.sign_up.taken.as_deref(), Some("K7QH-29XM"));
        assert_eq!(
            labels(&d, ALL),
            ["sign_up", "next_field", "show_that_customer", "back"]
        );
        // The retry sends the same id.
        assert_eq!(d.act(Action::Confirm), call);
        assert_eq!(
            d.act(Action::ShowTaken),
            Outcome::Call(Call::ByCode("K7QH-29XM".into()))
        );
        // Editing the phone forgets the taken customer.
        assert!(d.edit(Key::Backspace));
        assert_eq!(d.sign_up.taken, None);
        assert_eq!(d.act(Action::ShowTaken), Outcome::Stay);
        d.act(Action::NextField);
        assert_eq!(d.sign_up.field, 0);
        d.pick_field(1);
        assert_eq!(d.sign_up.field, 1);
        d.pick_field(5);
        assert_eq!(d.sign_up.field, 1);
        assert!(!d.edit(Key::F(2)));
        d.sign_up.field = 0;
        d.sign_up.name = "n".repeat(MAX_TEXT);
        assert!(d.edit(Key::Char('x')));
        assert_eq!(d.sign_up.name.len(), MAX_TEXT, "full");
        d.reply(Reply::SignedUp(Err(api(Problem::new(
            422,
            "invalid_field",
            "x",
        )
        .with("field", "name")))));
        assert_eq!(d.notice.as_ref().unwrap().code, "invalid_name");
        d.reply(Reply::SignedUp(Ok(budi())));
        assert_eq!(d.page, Page::Shown);
        let n = d.notice.clone().unwrap();
        assert_eq!(
            (n.code, n.arg.as_str(), n.tone),
            ("signed_up", "K7QH-29XM", Tone::Good)
        );
        d.page = Page::SignUp;
        d.act(Action::Back);
        assert_eq!(d.page, Page::Find);

        // A name search's text is offered as the name.
        let mut d = Dialog::new(None);
        d.act(Action::SwitchMode);
        typed(&mut d, "Wati");
        d.act(Action::NewCustomer);
        assert_eq!((d.sign_up.name.as_str(), d.sign_up.field), ("Wati", 1));
        d.pick(0);
        assert_eq!(d.page, Page::SignUp, "no results on this page");
    }

    #[test]
    fn payout_confirms_and_keeps_the_key_for_a_retry() {
        let mut d = Dialog::new(Some(&budi()));
        let mut broke = budi();
        broke.balance = 0;
        let mut empty = Dialog::new(Some(&broke));
        assert_eq!(
            labels(&empty, ALL),
            ["attach", "back"],
            "nothing to pay out"
        );
        empty.act(Action::Back);
        d.act(Action::PayOut);
        assert_eq!(d.page, Page::Payout);
        assert_eq!(labels(&d, ALL), ["pay_out", "back"]);
        assert_eq!(d.act(Action::Confirm), Outcome::Stay);
        assert_eq!(d.notice.as_ref().unwrap().code, "invalid_amount");
        typed(&mut d, "05000");
        assert!(d.notice.is_none());
        assert!(d.edit(Key::Backspace), "Rp 5.000 → Rp 500");
        assert!(!d.edit(Key::Char('x')));
        d.act(Action::Confirm);
        assert!(d.payout.confirm);
        assert!(!d.edit(Key::Digit(1)), "no typing while asking");
        assert_eq!(labels(&d, ALL), ["confirm_payout", "edit_amount"]);
        let Outcome::Call(Call::Payout { id, amount, key }) = d.act(Action::Confirm) else {
            panic!("payout")
        };
        assert_eq!((id.as_str(), amount), ("c1", 500));
        // Lost on the network: the same key on the retry.
        d.reply(Reply::PaidOut(Err(ClientError::Network("x".into()))));
        assert!(!d.payout.confirm);
        d.act(Action::Confirm);
        let Outcome::Call(Call::Payout { key: again, .. }) = d.act(Action::Confirm) else {
            panic!("payout")
        };
        assert_eq!(again, key);
        // Refused: a new key.
        let refused = Problem::new(422, "insufficient_balance", "x").with("balance", 200);
        d.reply(Reply::PaidOut(Err(api(refused))));
        let n = d.notice.clone().unwrap();
        assert_eq!((n.code, n.arg.as_str()), ("insufficient_balance", "Rp 200"));
        d.act(Action::Confirm);
        let Outcome::Call(Call::Payout { key: fresh, .. }) = d.act(Action::Confirm) else {
            panic!("payout")
        };
        assert_ne!(fresh, key);
        let paid = Payout {
            entry: BalanceEntry {
                amount: -500,
                ..BalanceEntry::default()
            },
            balance: 1_700,
        };
        let after = d.reply(Reply::PaidOut(Ok(paid.clone()))).unwrap();
        assert_eq!(after.balance, 1_700);
        assert_eq!(d.page, Page::Shown);
        assert_eq!(d.notice.as_ref().unwrap().arg, "Rp 500");
        // Esc steps back: from the question to the amount, then to the customer.
        d.act(Action::PayOut);
        d.payout.confirm = true;
        d.act(Action::Back);
        assert_eq!((d.page, d.payout.confirm), (Page::Payout, false));
        d.act(Action::Back);
        assert_eq!(d.page, Page::Shown);
        // A payout answer without a customer changes nothing.
        let mut lost = Dialog::new(None);
        assert_eq!(lost.reply(Reply::PaidOut(Ok(paid))), None);
    }
}
