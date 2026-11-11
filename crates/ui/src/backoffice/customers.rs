//! The back office's customer screens (spec 2.5, phase M4), part of the [`Controller`] state
//! machine. F6 opens the section; a row of tabs (Ctrl+1 … Ctrl+6) switches between its screens:
//!
//! 1. **Customer list** ([`Screen::Customers`]) — name / company search (`customer.search`;
//!    without it the list stays empty and a banner says why, but Ctrl+N still signs up), pages
//!    of [`PAGE`] ("load more", or moving past the last row), "archived" (`customer.manage`,
//!    only without a search: the server's name search never finds archived customers). Columns:
//!    code, name, masked phone, group, tier (with an expiry mark), balance.
//! 2. **Customer form** ([`Screen::CustomerForm`], Enter on a row, Ctrl+N new) — the profile
//!    ([`Profile`]: name, phone, email, birthday day / month / optional year, address, company,
//!    tax id, tax address, notes), checked with `core::customer` before the `PUT`; group, tier,
//!    valid-until and the manual-tier flag (editable with `customer.manage` only); a read-only
//!    balance with its recent entries. A new customer gets its code from the server. Ctrl+S
//!    save, Esc back (asks first with unsaved changes), Ctrl+Shift+A archive / unarchive
//!    (`customer.manage`, asks first), Ctrl+Shift+E erase personal data (`customer.erase`; the
//!    dialog only accepts after the customer's code is typed; refused by the server while the
//!    balance is not 0).
//! 3. **Groups** and 4. **Tiers** — like the brand editor: pick, Ctrl+N add, Ctrl+S save,
//!    Ctrl+Shift+A archive / unarchive (`customer.manage`); tiers also have a unique rank.
//! 5. **Possible duplicates** (`customer.manage`) — the customers whose phone clashed at sync.
//!    Picking a row makes it the customer to merge **from** (B) and looks up the customer that
//!    holds its phone as the one to **keep** (A); another A can be typed as a member code.
//!    Ctrl+M merges after a confirmation that explains the field rule (spec 2.5 §8).
//! 6. **Negative balances** (`customer.manage`) — a list; Enter opens the customer.
//! 7. **Balance settings** — max saved per sale and max balance (writes need
//!    `customer.manage`).
//!
//! `customer.changed` events reload the list or review screen on view.

use sultan_client::customers::{
    CUSTOMER_CREATE, CUSTOMER_EDIT, CUSTOMER_ERASE, CUSTOMER_MANAGE, CUSTOMER_SEARCH,
};
use sultan_client::{
    BalanceSettings, ClientError, Customer, CustomerBalance, CustomerBody, CustomerGroup,
    CustomerPage, CustomerQuery, GroupBody, MembershipTier, Problem, TierBody,
};
use sultan_core::customer::{
    Birthday, CustomerCode, Phone, TaxId, normalize_email, normalize_name,
};

use super::form::new_id;
use super::{Confirm, Controller, Effect, Press, PressKey, Screen, Step, key, notice};
use crate::controller::{Notice, Tone};

/// Customers per page of the list (the server's maximum).
pub const PAGE: u32 = 20;
/// Balance entries shown on the form.
pub const SHOWN_ENTRIES: usize = 8;
/// The `kind`s of [`super::Msg::Edit`] handled here.
pub const EDIT_KINDS: [&str; 7] = [
    "customer",
    "customer-list",
    "group",
    "tier",
    "merge",
    "settings",
    "erase",
];

/// A customer call (inside [`Effect::Customers`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Call {
    /// `search_customers`; appended when `append`.
    Search {
        /// Query.
        query: CustomerQuery,
        /// Next page.
        append: bool,
    },
    /// `customer` for the form.
    Get(String),
    /// `customer_balance` (first page).
    Balance(String),
    /// `put_customer`.
    Put {
        /// Customer.
        id: String,
        /// Body.
        body: Box<CustomerBody>,
    },
    /// `set_customer_archived`.
    SetArchived {
        /// Customer.
        id: String,
        /// Archive (true) or unarchive.
        archived: bool,
        /// Idempotency key.
        key: String,
    },
    /// `merge_customer`.
    Merge {
        /// Kept (A).
        into: String,
        /// Merged away (B).
        from: String,
        /// Idempotency key.
        key: String,
    },
    /// `erase_customer`.
    Erase {
        /// Customer.
        id: String,
        /// Idempotency key.
        key: String,
    },
    /// `customer_duplicates` (page).
    Duplicates(u32),
    /// `negative_balances` (page).
    Negative(u32),
    /// `customer_groups`.
    Groups(bool),
    /// `membership_tiers`.
    Tiers(bool),
    /// `put_customer_group`.
    PutGroup {
        /// Group.
        id: String,
        /// Body.
        body: GroupBody,
    },
    /// `put_membership_tier`.
    PutTier {
        /// Tier.
        id: String,
        /// Body.
        body: TierBody,
    },
    /// `customer_balance_settings`.
    Settings,
    /// `put_customer_balance_settings`.
    PutSettings(BalanceSettings),
    /// `customer_by_code` (the customer to keep in a merge).
    FindByCode(String),
    /// `customer_by_phone` (the holder of a duplicate's phone).
    FindByPhone(String),
}

/// The answer of a [`Call`] (inside [`super::Msg::Customers`]).
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// [`Call::Search`] (append, page).
    Page(bool, Result<CustomerPage, ClientError>),
    /// [`Call::Get`].
    Opened(Result<Customer, ClientError>),
    /// [`Call::Balance`] (customer, balance).
    Balance(String, Result<CustomerBalance, ClientError>),
    /// [`Call::Put`].
    Saved(Result<Customer, ClientError>),
    /// [`Call::SetArchived`].
    Archived(Result<Customer, ClientError>),
    /// [`Call::Merge`].
    Merged(Result<Customer, ClientError>),
    /// [`Call::Erase`].
    Erased(Result<Customer, ClientError>),
    /// [`Call::Duplicates`] (false) or [`Call::Negative`] (true).
    Review(bool, Result<CustomerPage, ClientError>),
    /// [`Call::Groups`].
    Groups(Result<Vec<CustomerGroup>, ClientError>),
    /// [`Call::Tiers`].
    Tiers(Result<Vec<MembershipTier>, ClientError>),
    /// [`Call::PutGroup`].
    GroupSaved(Result<CustomerGroup, ClientError>),
    /// [`Call::PutTier`].
    TierSaved(Result<MembershipTier, ClientError>),
    /// [`Call::Settings`].
    Settings(Result<BalanceSettings, ClientError>),
    /// [`Call::PutSettings`].
    SettingsSaved(Result<BalanceSettings, ClientError>),
    /// [`Call::FindByCode`] or [`Call::FindByPhone`].
    Partner(Result<Customer, ClientError>),
}

/// A field error of a customer screen: `field` (`name`, `phone`, `email`, `birthday`, `tax_id`,
/// `tier_valid_until`, `max_saved_per_sale`, `max_balance`), a message code and its argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Field.
    pub field: &'static str,
    /// Message code (`bo_widgets.slint`).
    pub code: &'static str,
    /// The `{}` argument.
    pub arg: String,
}

impl Issue {
    fn new(field: &'static str, code: &'static str) -> Self {
        Self {
            field,
            code,
            arg: String::new(),
        }
    }
}

/// A birthday as `(month, day, year)`.
type BirthParts = (Option<u32>, Option<u32>, Option<i32>);

/// The editable profile as typed.
#[allow(clippy::struct_excessive_bools)] // one flag only; clippy counts the Option fields too
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Profile {
    /// Name.
    pub name: String,
    /// Phone (shown in local form, `08123456789`).
    pub phone: String,
    /// Email.
    pub email: String,
    /// Birthday day.
    pub birth_day: String,
    /// Birthday month.
    pub birth_month: String,
    /// Birth year (optional).
    pub birth_year: String,
    /// Address.
    pub address: String,
    /// Company name.
    pub company: String,
    /// NPWP or NIK.
    pub tax_id: String,
    /// Tax address.
    pub tax_address: String,
    /// Staff notes.
    pub notes: String,
    /// Group.
    pub group: Option<String>,
    /// Tier.
    pub tier: Option<String>,
    /// Last day of the tier, `YYYY-MM-DD`.
    pub valid_until: String,
    /// Manual tier.
    pub manual_tier: bool,
}

fn text(t: Option<&String>) -> String {
    t.cloned().unwrap_or_default()
}

fn optional(t: &str) -> Option<String> {
    Some(t.trim().to_owned()).filter(|t| !t.is_empty())
}

fn number<T: ToString>(n: Option<T>) -> String {
    n.map(|n| n.to_string()).unwrap_or_default()
}

/// `+628123456789` → `08123456789` (anything that does not parse is kept).
#[must_use]
pub fn local_phone(phone: &str) -> String {
    Phone::parse(phone).map_or_else(|_| phone.to_owned(), |p| p.local_form())
}

impl Profile {
    /// The profile of a stored customer.
    #[must_use]
    pub fn of(c: &Customer) -> Self {
        Self {
            name: c.name.clone(),
            phone: c.phone.as_deref().map(local_phone).unwrap_or_default(),
            email: text(c.email.as_ref()),
            birth_day: number(c.birth_day),
            birth_month: number(c.birth_month),
            birth_year: number(c.birth_year),
            address: text(c.address.as_ref()),
            company: text(c.company_name.as_ref()),
            tax_id: text(c.tax_id.as_ref()),
            tax_address: text(c.tax_address.as_ref()),
            notes: text(c.notes.as_ref()),
            group: c.group_id.clone(),
            tier: c.tier_id.clone(),
            valid_until: text(c.tier_valid_until.as_ref()),
            manual_tier: c.manual_tier,
        }
    }

    /// The birthday as `(month, day, year)`; `Err` when it is incomplete or not a date.
    fn birthday(&self) -> Result<BirthParts, ()> {
        let (d, m, y) = (
            self.birth_day.trim(),
            self.birth_month.trim(),
            self.birth_year.trim(),
        );
        if d.is_empty() && m.is_empty() && y.is_empty() {
            return Ok((None, None, None));
        }
        let year = if y.is_empty() {
            None
        } else {
            Some(y.parse::<i32>().map_err(|_| ())?)
        };
        let (day, month) = (
            d.parse::<u32>().map_err(|_| ())?,
            m.parse::<u32>().map_err(|_| ())?,
        );
        Birthday::new(month, day, year).map_err(|_| ())?;
        Ok((Some(month), Some(day), year))
    }

    /// The `PUT` body after the `core::customer` checks, or the field errors.
    ///
    /// # Errors
    /// Every field that breaks a rule.
    pub fn body(&self, code: Option<&str>) -> Result<CustomerBody, Vec<Issue>> {
        let mut issues = Vec::new();
        if normalize_name(&self.name).is_err() {
            issues.push(Issue::new("name", "invalid_customer_name"));
        }
        let phone = optional(&self.phone);
        if phone.as_deref().is_some_and(|p| Phone::parse(p).is_err()) {
            issues.push(Issue::new("phone", "invalid_phone"));
        }
        let email = optional(&self.email);
        if email
            .as_deref()
            .is_some_and(|e| normalize_email(e).is_err())
        {
            issues.push(Issue::new("email", "invalid_email"));
        }
        let tax_id = optional(&self.tax_id);
        if tax_id.as_deref().is_some_and(|t| TaxId::parse(t).is_err()) {
            issues.push(Issue::new("tax_id", "invalid_tax_id"));
        }
        let born = self.birthday();
        if born.is_err() {
            issues.push(Issue::new("birthday", "invalid_birthday"));
        }
        let until = optional(&self.valid_until);
        let until = match until.as_deref().map(|d| {
            chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d")
                .map(|d| d.format("%Y-%m-%d").to_string())
        }) {
            None => None,
            Some(Ok(d)) if self.tier.is_some() => Some(d),
            Some(Ok(_)) => {
                issues.push(Issue::new("tier_valid_until", "valid_until_needs_tier"));
                None
            }
            Some(Err(_)) => {
                issues.push(Issue::new("tier_valid_until", "invalid_date"));
                None
            }
        };
        if !issues.is_empty() {
            return Err(issues);
        }
        let (birth_month, birth_day, birth_year) = born.unwrap_or_default();
        Ok(CustomerBody {
            code: code.map(str::to_owned),
            name: self.name.trim().to_owned(),
            phone,
            email,
            birth_month,
            birth_day,
            birth_year,
            address: optional(&self.address),
            company_name: optional(&self.company),
            tax_id,
            tax_address: optional(&self.tax_address),
            notes: optional(&self.notes),
            group_id: self.group.clone(),
            tier_id: self.tier.clone(),
            tier_valid_until: until,
            manual_tier: self.manual_tier,
        })
    }
}

/// The customer list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomerList {
    /// Search text as typed.
    pub search: String,
    /// Bumped per keystroke; the debounce only runs the latest.
    pub search_serial: u64,
    /// Include archived customers (without a search).
    pub archived: bool,
    /// Loaded rows.
    pub rows: Vec<Customer>,
    /// The next page.
    pub next_page: Option<u32>,
    /// The highlighted row.
    pub cursor: usize,
    /// A page is loading.
    pub loading: bool,
    /// Errors, or why the list is empty.
    pub notice: Option<Notice>,
}

impl CustomerList {
    /// The query of `page` (the first when `None`).
    #[must_use]
    pub fn query(&self, page: Option<u32>) -> CustomerQuery {
        let q = self.search.trim().to_owned();
        CustomerQuery {
            include_archived: self.archived && q.is_empty(),
            q,
            page,
            limit: Some(PAGE),
        }
    }
}

/// The open customer form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerForm {
    /// Customer ID (a new UUID for a new customer).
    pub id: String,
    /// The customer as stored; `None` while new.
    pub stored: Option<Customer>,
    /// The profile as typed.
    pub fields: Profile,
    /// The profile as loaded (to know about unsaved changes).
    pub initial: Profile,
    /// Field errors.
    pub issues: Vec<Issue>,
    /// Saved, errors, permission…
    pub notice: Option<Notice>,
    /// A write is running.
    pub saving: bool,
    /// The balance and its recent entries.
    pub balance: Option<CustomerBalance>,
}

impl CustomerForm {
    fn new(id: String) -> Self {
        Self {
            id,
            stored: None,
            fields: Profile::default(),
            initial: Profile::default(),
            issues: Vec::new(),
            notice: None,
            saving: false,
            balance: None,
        }
    }

    fn of(c: Customer) -> Self {
        let fields = Profile::of(&c);
        Self {
            id: c.id.clone(),
            stored: Some(c),
            initial: fields.clone(),
            fields,
            issues: Vec::new(),
            notice: None,
            saving: false,
            balance: None,
        }
    }

    /// Not saved yet.
    #[must_use]
    pub const fn is_new(&self) -> bool {
        self.stored.is_none()
    }

    /// Unsaved changes.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.fields != self.initial
    }

    /// The first error of `field`.
    #[must_use]
    pub fn issue(&self, field: &str) -> Option<&Issue> {
        self.issues.iter().find(|i| i.field == field)
    }

    /// Archived (also after a merge or an erase).
    #[must_use]
    pub fn archived(&self) -> bool {
        self.stored.as_ref().is_some_and(|c| c.archived)
    }

    /// Erased.
    #[must_use]
    pub fn erased(&self) -> bool {
        self.stored.as_ref().is_some_and(|c| c.erased_at.is_some())
    }

    /// Takes a server error: a field error goes to its field (true), anything else is left to
    /// the banner (false).
    fn apply_problem(&mut self, p: &Problem) -> bool {
        let issue = match (p.code.as_str(), p.field()) {
            ("invalid_phone", _) => Issue::new("phone", "invalid_phone"),
            ("invalid_tax_id", _) => Issue::new("tax_id", "invalid_tax_id"),
            ("phone_taken", _) => Issue {
                field: "phone",
                code: "phone_taken",
                arg: taken_by(p),
            },
            ("invalid_field" | "invalid_code", Some("name")) => {
                Issue::new("name", "invalid_customer_name")
            }
            ("invalid_field", Some("email")) => Issue::new("email", "invalid_email"),
            ("invalid_field", Some("birthday" | "birth_month" | "birth_day" | "birth_year")) => {
                Issue::new("birthday", "invalid_birthday")
            }
            ("invalid_field", Some("tier_valid_until")) => {
                Issue::new("tier_valid_until", "invalid_date")
            }
            _ => return false,
        };
        self.issues = vec![issue];
        true
    }
}

/// `K7QH-29XM · 0812****789`: who holds a phone (`phone_taken`).
fn taken_by(p: &Problem) -> String {
    [p.customer_code(), p.masked_phone()]
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// The group or tier editor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NamedEditor {
    /// The highlighted row.
    pub cursor: usize,
    /// The entry being edited, `None` for a new one.
    pub editing: Option<String>,
    /// Name as typed.
    pub name: String,
    /// Rank as typed (tiers).
    pub rank: String,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// A review list (possible duplicates, negative balances).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Review {
    /// Loaded rows.
    pub rows: Vec<Customer>,
    /// The next page.
    pub next_page: Option<u32>,
    /// The highlighted row.
    pub cursor: usize,
    /// A page is loading.
    pub loading: bool,
    /// Errors, "merged".
    pub notice: Option<Notice>,
}

/// The merge panel of the duplicates screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeState {
    /// B: merged away (archived, its code and phone find A).
    pub from: Option<Customer>,
    /// A: kept.
    pub into: Option<Customer>,
    /// A's member code as typed.
    pub code: String,
    /// Not found, errors.
    pub notice: Option<Notice>,
}

/// The balance settings form.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SettingsForm {
    /// As stored.
    pub stored: Option<BalanceSettings>,
    /// Max saved per sale as typed.
    pub per_sale: String,
    /// Max balance as typed.
    pub max_balance: String,
    /// Field errors.
    pub issues: Vec<Issue>,
    /// Saved, errors.
    pub notice: Option<Notice>,
}

/// Everything of the customer section.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CustomerState {
    /// Groups (archived ones with `customer.manage`).
    pub groups: Vec<CustomerGroup>,
    /// Tiers, best first.
    pub tiers: Vec<MembershipTier>,
    /// The list.
    pub list: CustomerList,
    /// The open form.
    pub form: Option<CustomerForm>,
    /// Where Esc on the form goes back to.
    pub back: Option<Screen>,
    /// The group editor.
    pub group_editor: NamedEditor,
    /// The tier editor.
    pub tier_editor: NamedEditor,
    /// Possible duplicates.
    pub duplicates: Review,
    /// Negative balances.
    pub negative: Review,
    /// The merge panel.
    pub merge: MergeState,
    /// Balance settings.
    pub settings: SettingsForm,
    /// The code typed into the erase dialog.
    pub erase_code: String,
    /// Bumped when texts change under the user (load, pick, save): the view rebuilds the text
    /// fields of the customer screens.
    pub serial: u64,
}

impl CustomerState {
    /// Group choices: `None` first, then active groups (and `keep`).
    #[must_use]
    pub fn group_choices(&self, keep: Option<&str>) -> Vec<(Option<String>, String)> {
        let mut out = vec![(None, String::new())];
        out.extend(
            self.groups
                .iter()
                .filter(|g| !g.archived || keep == Some(g.id.as_str()))
                .map(|g| (Some(g.id.clone()), g.name.clone())),
        );
        out
    }

    /// Tier choices: `None` first, then active tiers best first (and `keep`).
    #[must_use]
    pub fn tier_choices(&self, keep: Option<&str>) -> Vec<(Option<String>, String)> {
        let mut out = vec![(None, String::new())];
        out.extend(
            self.tiers
                .iter()
                .filter(|t| !t.archived || keep == Some(t.id.as_str()))
                .map(|t| (Some(t.id.clone()), t.name.clone())),
        );
        out
    }
}

/// The screens of the customer section, in tab order (Ctrl+1 … Ctrl+6).
pub const TABS: [Screen; 6] = [
    Screen::Customers,
    Screen::CustomerGroups,
    Screen::CustomerTiers,
    Screen::Duplicates,
    Screen::NegativeBalances,
    Screen::BalanceSettings,
];

/// Whether `screen` belongs to the customer section.
#[must_use]
pub fn in_section(screen: Screen) -> bool {
    screen == Screen::CustomerForm || TABS.contains(&screen)
}

fn amount_text(text: &str) -> Option<i64> {
    let digits: String = text
        .trim()
        .chars()
        .filter(|c| !matches!(c, '.' | ' ' | ','))
        .collect();
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

impl Controller {
    /// Whether the user holds `key` (customer permissions are company-wide).
    #[must_use]
    pub fn may(&self, key: &str) -> bool {
        self.session
            .as_ref()
            .is_some_and(|s| s.me.holds_anywhere(key))
    }

    pub(super) fn customer_call(&self, call: Call) -> Effect {
        Effect::Customers {
            token: self.token(),
            call,
        }
    }

    fn denied(permission: &str) -> Notice {
        notice("permission_denied_catalog", permission, Tone::Bad)
    }

    fn bump(&mut self) {
        self.customers.serial += 1;
    }

    fn load_groups_tiers(&self) -> Vec<Effect> {
        let all = self.may(CUSTOMER_MANAGE);
        vec![
            self.customer_call(Call::Groups(all)),
            self.customer_call(Call::Tiers(all)),
        ]
    }

    /// Entering a screen of the section (from `enter`).
    pub(super) fn enter_customers(&mut self, target: Screen, from: Screen) -> Vec<Effect> {
        let mut effects = if in_section(from) {
            Vec::new()
        } else {
            self.load_groups_tiers()
        };
        match target {
            Screen::Customers => {
                self.focus("list");
                if from == Screen::CustomerForm || self.customers.list.rows.is_empty() {
                    effects.extend(self.reload_customers());
                }
            }
            Screen::CustomerGroups => {
                self.select_named(false, self.customers.group_editor.cursor);
                self.focus("group-name");
                if in_section(from) {
                    effects.extend(self.load_groups_tiers());
                }
            }
            Screen::CustomerTiers => {
                self.select_named(true, self.customers.tier_editor.cursor);
                self.focus("tier-name");
                if in_section(from) {
                    effects.extend(self.load_groups_tiers());
                }
            }
            Screen::Duplicates | Screen::NegativeBalances => {
                self.focus("list");
                effects.extend(self.reload_review(target == Screen::NegativeBalances));
            }
            Screen::BalanceSettings => {
                self.focus("settings-per-sale");
                effects.push(self.customer_call(Call::Settings));
            }
            _ => {}
        }
        effects
    }

    fn reload_customers(&mut self) -> Vec<Effect> {
        let l = &mut self.customers.list;
        l.next_page = None;
        if self
            .session
            .as_ref()
            .is_none_or(|s| s.me.user_id.is_empty())
        {
            return Vec::new();
        }
        if !self.may(CUSTOMER_SEARCH) {
            let l = &mut self.customers.list;
            l.rows.clear();
            l.loading = false;
            l.notice = Some(notice(
                "customer_search_needed",
                CUSTOMER_SEARCH,
                Tone::Neutral,
            ));
            return Vec::new();
        }
        self.customers.list.loading = true;
        let query = self.customers.list.query(None);
        vec![self.customer_call(Call::Search {
            query,
            append: false,
        })]
    }

    fn more_customers(&mut self) -> Vec<Effect> {
        let l = &self.customers.list;
        let Some(page) = l.next_page.filter(|_| !l.loading) else {
            return Vec::new();
        };
        self.customers.list.loading = true;
        let query = self.customers.list.query(Some(page));
        vec![self.customer_call(Call::Search {
            query,
            append: true,
        })]
    }

    fn reload_review(&mut self, negative: bool) -> Vec<Effect> {
        let allowed = self.may(CUSTOMER_MANAGE);
        let review = if negative {
            &mut self.customers.negative
        } else {
            &mut self.customers.duplicates
        };
        review.next_page = None;
        if !allowed {
            review.rows.clear();
            review.notice = Some(Self::denied(CUSTOMER_MANAGE));
            return Vec::new();
        }
        review.loading = true;
        let call = if negative {
            Call::Negative(1)
        } else {
            Call::Duplicates(1)
        };
        vec![self.customer_call(call)]
    }

    /// Keys on the customer screens (after F-keys of the header).
    pub(super) fn customers_capture(&mut self, p: Press, screen: Screen) -> Step {
        if screen == Screen::CustomerForm {
            return self.customer_form_capture(p);
        }
        for (i, tab) in TABS.iter().enumerate() {
            let digit = char::from_digit(u32::try_from(i + 1).unwrap_or(0), 10).unwrap_or('0');
            if p.ctrl_char(digit) {
                return Step::handled(self.go(*tab));
            }
        }
        if p.f(5) {
            return Step::handled(self.customer_action("refresh").unwrap_or_default());
        }
        match screen {
            Screen::Customers => self.customer_list_capture(p),
            Screen::CustomerGroups | Screen::CustomerTiers => {
                self.named_capture(p, screen == Screen::CustomerTiers)
            }
            Screen::Duplicates | Screen::NegativeBalances => {
                self.review_capture(p, screen == Screen::NegativeBalances)
            }
            Screen::BalanceSettings => {
                if p.ctrl_char('s') {
                    return Step::handled(self.save_settings());
                }
                if p.plain(PressKey::Escape) {
                    return Step::handled(self.go(Screen::Customers));
                }
                Step::default()
            }
            _ => Step::default(),
        }
    }

    fn customer_form_capture(&mut self, p: Press) -> Step {
        let action = if p.ctrl_char('s') {
            "customer-save"
        } else if p.ctrl_shift_char('a') {
            "customer-archive"
        } else if p.ctrl_shift_char('e') {
            "customer-erase"
        } else if p.plain(PressKey::Escape) {
            "customer-back"
        } else {
            return Step::default();
        };
        Step::handled(self.customer_action(action).unwrap_or_default())
    }

    fn customer_list_capture(&mut self, p: Press) -> Step {
        if p.ctrl_char('f') {
            self.focus("customer-search");
            return Step::handled(Vec::new());
        }
        if p.ctrl_char('n') {
            return Step::handled(self.new_customer());
        }
        let l = &mut self.customers.list;
        if p.plain(PressKey::Up) {
            l.cursor = l.cursor.saturating_sub(1);
            return Step::handled(Vec::new());
        }
        if p.plain(PressKey::Down) {
            if l.cursor + 1 < l.rows.len() {
                l.cursor += 1;
                return Step::handled(Vec::new());
            }
            return Step::handled(self.more_customers());
        }
        if p.plain(PressKey::Enter) {
            let row = self.customers.list.cursor;
            return Step::handled(self.customer_pick("customer-open", row).unwrap_or_default());
        }
        if p.plain(PressKey::Escape) {
            self.focus("list");
            return Step::handled(Vec::new());
        }
        Step::default()
    }

    fn named_capture(&mut self, p: Press, tiers: bool) -> Step {
        let prefix = if tiers { "tier" } else { "group" };
        let action = if p.ctrl_char('s') {
            format!("{prefix}-save")
        } else if p.ctrl_char('n') {
            format!("{prefix}-add")
        } else if p.ctrl_shift_char('a') {
            format!("{prefix}-archive")
        } else if p.plain(PressKey::Escape) {
            return Step::handled(self.go(Screen::Customers));
        } else {
            let (cursor, count) = if tiers {
                (
                    self.customers.tier_editor.cursor,
                    self.customers.tiers.len(),
                )
            } else {
                (
                    self.customers.group_editor.cursor,
                    self.customers.groups.len(),
                )
            };
            let row = if p.plain(PressKey::Up) {
                cursor.saturating_sub(1)
            } else if p.plain(PressKey::Down) {
                (cursor + 1).min(count.saturating_sub(1))
            } else {
                return Step::default();
            };
            self.select_named(tiers, row);
            return Step::handled(Vec::new());
        };
        Step::handled(self.customer_action(&action).unwrap_or_default())
    }

    fn review_capture(&mut self, p: Press, negative: bool) -> Step {
        if !negative && p.ctrl_char('m') {
            return Step::handled(self.customer_action("merge").unwrap_or_default());
        }
        let (kind, open) = if negative {
            ("negative", "negative-open")
        } else {
            ("duplicate", "duplicate-open")
        };
        let review = if negative {
            &self.customers.negative
        } else {
            &self.customers.duplicates
        };
        let (cursor, count) = (review.cursor, review.rows.len());
        if p.plain(PressKey::Up) {
            let effects = self.customer_pick(kind, cursor.saturating_sub(1));
            return Step::handled(effects.unwrap_or_default());
        }
        if p.plain(PressKey::Down) {
            let row = (cursor + 1).min(count.saturating_sub(1));
            return Step::handled(self.customer_pick(kind, row).unwrap_or_default());
        }
        if p.plain(PressKey::Enter) {
            return Step::handled(self.customer_pick(open, cursor).unwrap_or_default());
        }
        if p.plain(PressKey::Escape) {
            return Step::handled(self.go(Screen::Customers));
        }
        Step::default()
    }

    /// The customer buttons; `None` when `id` is not one of them.
    #[allow(clippy::too_many_lines)] // one arm per button
    pub(super) fn customer_action(&mut self, id: &str) -> Option<Vec<Effect>> {
        let effects = match id {
            "nav-customers" | "tab-customers" => self.go(Screen::Customers),
            "tab-groups" => self.go(Screen::CustomerGroups),
            "tab-tiers" => self.go(Screen::CustomerTiers),
            "tab-duplicates" => self.go(Screen::Duplicates),
            "tab-negative" => self.go(Screen::NegativeBalances),
            "tab-balance-settings" => self.go(Screen::BalanceSettings),
            "refresh" => match self.screen() {
                Screen::Customers => self.reload_customers(),
                Screen::CustomerGroups | Screen::CustomerTiers => self.load_groups_tiers(),
                Screen::Duplicates => self.reload_review(false),
                Screen::NegativeBalances => self.reload_review(true),
                Screen::BalanceSettings => vec![self.customer_call(Call::Settings)],
                _ => return None,
            },
            "load-more" if self.screen() == Screen::Customers => self.more_customers(),
            "open" if self.screen() == Screen::Customers => {
                let row = self.customers.list.cursor;
                return self.customer_pick("customer-open", row);
            }
            "customer-new" => self.new_customer(),
            "customer-save" => self.save_customer(),
            "customer-back" => {
                let back = self.customers.back.unwrap_or(Screen::Customers);
                self.go(back)
            }
            "customer-archive" => {
                let ok = self
                    .customers
                    .form
                    .as_ref()
                    .is_some_and(|f| !f.is_new() && !f.erased());
                if !ok {
                } else if self.may(CUSTOMER_MANAGE) {
                    self.confirm = Some(Confirm::CustomerArchive);
                } else if let Some(f) = &mut self.customers.form {
                    f.notice = Some(Self::denied(CUSTOMER_MANAGE));
                }
                Vec::new()
            }
            "customer-erase" => {
                let ok = self
                    .customers
                    .form
                    .as_ref()
                    .is_some_and(|f| !f.is_new() && !f.erased());
                if !ok {
                } else if self.may(CUSTOMER_ERASE) {
                    self.customers.erase_code.clear();
                    self.confirm = Some(Confirm::Erase);
                    self.focus("erase-code");
                } else if let Some(f) = &mut self.customers.form {
                    f.notice = Some(Self::denied(CUSTOMER_ERASE));
                }
                Vec::new()
            }
            "group-add" | "tier-add" => {
                let tiers = id == "tier-add";
                let rank = self
                    .customers
                    .tiers
                    .iter()
                    .map(|t| t.rank)
                    .max()
                    .unwrap_or(0)
                    + 1;
                let editor = self.named_mut(tiers);
                *editor = NamedEditor {
                    cursor: editor.cursor,
                    rank: if tiers {
                        rank.to_string()
                    } else {
                        String::new()
                    },
                    ..NamedEditor::default()
                };
                self.bump();
                self.focus(if tiers { "tier-name" } else { "group-name" });
                Vec::new()
            }
            "group-save" => self.save_named(false, None),
            "tier-save" => self.save_named(true, None),
            "group-archive" | "tier-archive" => {
                let tiers = id == "tier-archive";
                match self.named_archived(tiers) {
                    Some(archived) => self.save_named(tiers, Some(!archived)),
                    None => Vec::new(),
                }
            }
            "merge" => self.ask_merge(),
            "merge-swap" => {
                let m = &mut self.customers.merge;
                if m.from.is_some() && m.into.is_some() {
                    std::mem::swap(&mut m.from, &mut m.into);
                    m.code = m
                        .into
                        .as_ref()
                        .map(|c| c.display_code.clone())
                        .unwrap_or_default();
                    self.bump();
                }
                Vec::new()
            }
            "settings-save" => self.save_settings(),
            _ => return None,
        };
        Some(effects)
    }

    fn named_mut(&mut self, tiers: bool) -> &mut NamedEditor {
        if tiers {
            &mut self.customers.tier_editor
        } else {
            &mut self.customers.group_editor
        }
    }

    /// Whether the edited group / tier is archived (`None` for a new one).
    fn named_archived(&self, tiers: bool) -> Option<bool> {
        let c = &self.customers;
        if tiers {
            let id = c.tier_editor.editing.as_ref()?;
            c.tiers.iter().find(|t| t.id == *id).map(|t| t.archived)
        } else {
            let id = c.group_editor.editing.as_ref()?;
            c.groups.iter().find(|g| g.id == *id).map(|g| g.archived)
        }
    }

    fn select_named(&mut self, tiers: bool, row: usize) {
        let picked = if tiers {
            self.customers
                .tiers
                .get(row)
                .map(|t| (t.id.clone(), t.name.clone(), t.rank.to_string()))
        } else {
            self.customers
                .groups
                .get(row)
                .map(|g| (g.id.clone(), g.name.clone(), String::new()))
        };
        let Some((id, name, rank)) = picked else {
            return;
        };
        *self.named_mut(tiers) = NamedEditor {
            cursor: row,
            editing: Some(id),
            name,
            rank,
            notice: None,
        };
        self.bump();
    }

    fn save_named(&mut self, tiers: bool, archived: Option<bool>) -> Vec<Effect> {
        if !self.may(CUSTOMER_MANAGE) {
            self.named_mut(tiers).notice = Some(Self::denied(CUSTOMER_MANAGE));
            return Vec::new();
        }
        let archived = archived.unwrap_or_else(|| self.named_archived(tiers).unwrap_or(false));
        let editor = if tiers {
            &self.customers.tier_editor
        } else {
            &self.customers.group_editor
        };
        let name = editor.name.trim().to_owned();
        let id = editor.editing.clone().unwrap_or_else(new_id);
        if name.is_empty() {
            self.named_mut(tiers).notice = Some(Notice::bad("name_required"));
            return Vec::new();
        }
        let call = if tiers {
            let Ok(rank) = editor.rank.trim().parse::<i64>() else {
                self.named_mut(tiers).notice = Some(Notice::bad("invalid_rank"));
                return Vec::new();
            };
            Call::PutTier {
                id,
                body: TierBody {
                    name,
                    rank,
                    archived,
                },
            }
        } else {
            Call::PutGroup {
                id,
                body: GroupBody { name, archived },
            }
        };
        vec![self.customer_call(call)]
    }

    fn new_customer(&mut self) -> Vec<Effect> {
        if !self.may(CUSTOMER_CREATE) {
            self.customers.list.notice = Some(Self::denied(CUSTOMER_CREATE));
            return Vec::new();
        }
        self.customers.back = Some(Screen::Customers);
        self.customers.form = Some(CustomerForm::new(new_id()));
        self.screen = Some(Screen::CustomerForm);
        self.bump();
        self.focus("customer-name");
        Vec::new()
    }

    /// Whether the open form may be edited (create / edit, not archived).
    #[must_use]
    pub fn customer_editable(&self) -> bool {
        self.customers.form.as_ref().is_some_and(|f| {
            if f.is_new() {
                self.may(CUSTOMER_CREATE)
            } else {
                self.may(CUSTOMER_EDIT) && !f.archived()
            }
        })
    }

    fn save_customer(&mut self) -> Vec<Effect> {
        let editable = self.customer_editable();
        let create = self
            .customers
            .form
            .as_ref()
            .is_some_and(CustomerForm::is_new);
        let token = self.token();
        let Some(f) = &mut self.customers.form else {
            return Vec::new();
        };
        if f.saving {
            return Vec::new();
        }
        if !editable {
            f.notice = if f.archived() {
                Some(Notice::bad("customer_archived"))
            } else {
                Some(Self::denied(if create {
                    CUSTOMER_CREATE
                } else {
                    CUSTOMER_EDIT
                }))
            };
            return Vec::new();
        }
        let code = f.stored.as_ref().map(|c| c.code.clone());
        match f.fields.body(code.as_deref()) {
            Err(issues) => {
                f.issues = issues;
                f.notice = Some(Notice::bad("fix_errors"));
                Vec::new()
            }
            Ok(body) => {
                f.issues.clear();
                f.notice = None;
                f.saving = true;
                vec![Effect::Customers {
                    token,
                    call: Call::Put {
                        id: f.id.clone(),
                        body: Box::new(body),
                    },
                }]
            }
        }
    }

    fn ask_merge(&mut self) -> Vec<Effect> {
        if !self.may(CUSTOMER_MANAGE) {
            self.customers.merge.notice = Some(Self::denied(CUSTOMER_MANAGE));
            return Vec::new();
        }
        let m = &mut self.customers.merge;
        match (&m.from, &m.into) {
            (Some(b), Some(a)) if a.id != b.id => self.confirm = Some(Confirm::Merge),
            _ => m.notice = Some(Notice::bad("merge_pick_both")),
        }
        Vec::new()
    }

    fn save_settings(&mut self) -> Vec<Effect> {
        if !self.may(CUSTOMER_MANAGE) {
            self.customers.settings.notice = Some(Self::denied(CUSTOMER_MANAGE));
            return Vec::new();
        }
        let s = &mut self.customers.settings;
        let per_sale = amount_text(&s.per_sale);
        let max = amount_text(&s.max_balance);
        s.issues.clear();
        if per_sale.is_none() {
            s.issues
                .push(Issue::new("max_saved_per_sale", "invalid_amount"));
        }
        if max.is_none() {
            s.issues.push(Issue::new("max_balance", "invalid_amount"));
        }
        let (Some(max_saved_per_sale), Some(max_balance)) = (per_sale, max) else {
            s.notice = Some(Notice::bad("fix_errors"));
            return Vec::new();
        };
        s.notice = None;
        vec![self.customer_call(Call::PutSettings(BalanceSettings {
            max_saved_per_sale,
            max_balance,
        }))]
    }

    /// A confirmed customer dialog.
    pub(super) fn customer_confirmed(&mut self, confirm: Confirm) -> Vec<Effect> {
        match confirm {
            Confirm::CustomerArchive => {
                let Some(f) = &mut self.customers.form else {
                    return Vec::new();
                };
                f.saving = true;
                let call = Call::SetArchived {
                    id: f.id.clone(),
                    archived: !f.archived(),
                    key: key(),
                };
                vec![self.customer_call(call)]
            }
            Confirm::Erase => {
                if !self.erase_code_matches() {
                    // Keep asking until the code is typed.
                    self.confirm = Some(Confirm::Erase);
                    return Vec::new();
                }
                let Some(f) = &mut self.customers.form else {
                    return Vec::new();
                };
                f.saving = true;
                let call = Call::Erase {
                    id: f.id.clone(),
                    key: key(),
                };
                self.customers.erase_code.clear();
                vec![self.customer_call(call)]
            }
            Confirm::Merge => {
                let m = &self.customers.merge;
                let (Some(b), Some(a)) = (&m.from, &m.into) else {
                    return Vec::new();
                };
                let call = Call::Merge {
                    into: a.id.clone(),
                    from: b.id.clone(),
                    key: key(),
                };
                vec![self.customer_call(call)]
            }
            _ => Vec::new(),
        }
    }

    /// Whether the code typed into the erase dialog is the open customer's.
    #[must_use]
    pub fn erase_code_matches(&self) -> bool {
        let typed = CustomerCode::parse(self.customers.erase_code.trim());
        self.customers
            .form
            .as_ref()
            .and_then(|f| f.stored.as_ref())
            .is_some_and(|c| typed.is_ok_and(|t| t.as_str() == c.code))
    }

    /// Text field edits of the customer screens.
    pub(super) fn customer_edit(&mut self, kind: &str, field: &str, value: String) -> Vec<Effect> {
        match kind {
            "customer-list" => {
                let l = &mut self.customers.list;
                l.search = value;
                l.search_serial += 1;
                return vec![Effect::Debounce(l.search_serial)];
            }
            "group" | "tier" => {
                let editor = self.named_mut(kind == "tier");
                match field {
                    "name" => editor.name = value,
                    "rank" => editor.rank = value,
                    _ => {}
                }
            }
            "settings" => {
                let s = &mut self.customers.settings;
                match field {
                    "max_saved_per_sale" => s.per_sale = value,
                    "max_balance" => s.max_balance = value,
                    _ => {}
                }
            }
            "erase" => self.customers.erase_code = value,
            "merge" => {
                let m = &mut self.customers.merge;
                m.code.clone_from(&value);
                m.notice = None;
                m.into = None;
                if let Ok(code) = CustomerCode::parse(value.trim()) {
                    return vec![self.customer_call(Call::FindByCode(code.as_str().to_owned()))];
                }
            }
            "customer" => {
                if let Some(f) = &mut self.customers.form {
                    let p = &mut f.fields;
                    let slot = match field {
                        "name" => &mut p.name,
                        "phone" => &mut p.phone,
                        "email" => &mut p.email,
                        "birth_day" => &mut p.birth_day,
                        "birth_month" => &mut p.birth_month,
                        "birth_year" => &mut p.birth_year,
                        "address" => &mut p.address,
                        "company" => &mut p.company,
                        "tax_id" => &mut p.tax_id,
                        "tax_address" => &mut p.tax_address,
                        "notes" => &mut p.notes,
                        "valid_until" => &mut p.valid_until,
                        _ => return Vec::new(),
                    };
                    *slot = value;
                }
            }
            _ => {}
        }
        Vec::new()
    }

    /// Check boxes of the customer screens; `None` when not one of them.
    pub(super) fn customer_toggle(
        &mut self,
        kind: &str,
        field: &str,
        on: bool,
    ) -> Option<Vec<Effect>> {
        match (kind, field) {
            ("customer-list", "archived") => {
                if !self.may(CUSTOMER_MANAGE) {
                    return Some(Vec::new());
                }
                self.customers.list.archived = on;
                Some(self.reload_customers())
            }
            ("customer", "manual_tier") => {
                let manage = self.may(CUSTOMER_MANAGE);
                if let Some(f) = self.customers.form.as_mut().filter(|_| manage) {
                    f.fields.manual_tier = on;
                }
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Pickers of the customer form; `None` when not one of them.
    pub(super) fn customer_choose(
        &mut self,
        kind: &str,
        field: &str,
        index: usize,
    ) -> Option<Vec<Effect>> {
        if kind != "customer" {
            return None;
        }
        let manage = self.may(CUSTOMER_MANAGE);
        let c = &self.customers;
        let Some(f) = c.form.as_ref().filter(|_| manage) else {
            return Some(Vec::new());
        };
        let picked = match field {
            "group" => c
                .group_choices(f.fields.group.as_deref())
                .get(index)
                .map(|x| x.0.clone()),
            "tier" => c
                .tier_choices(f.fields.tier.as_deref())
                .get(index)
                .map(|x| x.0.clone()),
            _ => None,
        };
        if let (Some(id), Some(f)) = (picked, self.customers.form.as_mut()) {
            if field == "group" {
                f.fields.group = id;
            } else {
                f.fields.tier = id;
                if f.fields.tier.is_none() {
                    f.fields.valid_until.clear();
                    self.customers.serial += 1;
                }
            }
        }
        Some(Vec::new())
    }

    fn open_customer(&mut self, id: String) -> Vec<Effect> {
        self.customers.back = Some(self.screen());
        vec![self.customer_call(Call::Get(id))]
    }

    /// Row clicks of the customer screens; `None` when not one of them.
    pub(super) fn customer_pick(&mut self, kind: &str, row: usize) -> Option<Vec<Effect>> {
        let effects = match kind {
            "customer-row" => {
                if row < self.customers.list.rows.len() {
                    self.customers.list.cursor = row;
                }
                Vec::new()
            }
            "customer-open" => {
                let Some(id) = self.customers.list.rows.get(row).map(|c| c.id.clone()) else {
                    return Some(Vec::new());
                };
                self.customers.list.cursor = row;
                self.open_customer(id)
            }
            "group" | "tier" => {
                self.select_named(kind == "tier", row);
                Vec::new()
            }
            "negative" => {
                if row < self.customers.negative.rows.len() {
                    self.customers.negative.cursor = row;
                }
                Vec::new()
            }
            "duplicate" => self.pick_duplicate(row),
            "negative-open" | "duplicate-open" => {
                let review = if kind == "negative-open" {
                    &mut self.customers.negative
                } else {
                    &mut self.customers.duplicates
                };
                let Some(id) = review.rows.get(row).map(|c| c.id.clone()) else {
                    return Some(Vec::new());
                };
                review.cursor = row;
                self.open_customer(id)
            }
            _ => return None,
        };
        Some(effects)
    }

    /// Makes duplicate `row` the customer to merge away and looks up the holder of its phone.
    fn pick_duplicate(&mut self, row: usize) -> Vec<Effect> {
        let Some(b) = self.customers.duplicates.rows.get(row).cloned() else {
            return Vec::new();
        };
        self.customers.duplicates.cursor = row;
        if self
            .customers
            .merge
            .from
            .as_ref()
            .is_some_and(|f| f.id == b.id)
        {
            return Vec::new();
        }
        let phone = b.phone_conflict.clone();
        self.customers.merge = MergeState {
            from: Some(b),
            ..MergeState::default()
        };
        self.bump();
        phone
            .map(|p| vec![self.customer_call(Call::FindByPhone(p))])
            .unwrap_or_default()
    }

    /// `customer.changed`: the list or review on view reloads.
    pub(super) fn customer_event(&mut self) -> Vec<Effect> {
        match self.screen() {
            Screen::Customers if !self.customers.list.loading => self.reload_customers(),
            Screen::Duplicates if !self.customers.duplicates.loading => self.reload_review(false),
            Screen::NegativeBalances if !self.customers.negative.loading => {
                self.reload_review(true)
            }
            _ => Vec::new(),
        }
    }

    /// The debounced customer search.
    pub(super) fn customer_search_due(&mut self, serial: u64) -> Vec<Effect> {
        if serial == self.customers.list.search_serial && self.session.is_some() {
            self.customers.list.cursor = 0;
            self.reload_customers()
        } else {
            Vec::new()
        }
    }

    /// The answer of a customer call.
    #[allow(clippy::too_many_lines)] // one arm per answer
    pub(super) fn customer_reply(&mut self, reply: Reply) -> Vec<Effect> {
        match reply {
            Reply::Page(append, result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let l = &mut self.customers.list;
                l.loading = false;
                match result {
                    Ok(page) => {
                        if append {
                            l.rows.extend(page.items);
                        } else {
                            l.rows = page.items;
                            l.cursor = l.cursor.min(l.rows.len().saturating_sub(1));
                        }
                        l.next_page = page.next_page;
                        l.notice = l.notice.take().filter(|n| n.tone == Tone::Good);
                    }
                    Err(_) => l.notice = failed,
                }
            }
            Reply::Opened(result) => match result {
                Ok(c) => {
                    let id = c.id.clone();
                    self.customers.form = Some(CustomerForm::of(c));
                    self.screen = Some(Screen::CustomerForm);
                    self.bump();
                    self.focus("customer-name");
                    return vec![self.customer_call(Call::Balance(id))];
                }
                Err(e) => {
                    let n = self.failed(&e);
                    self.review_notice(n);
                }
            },
            Reply::Balance(id, result) => {
                if let (Some(f), Ok(b)) = (&mut self.customers.form, result)
                    && f.id == id
                {
                    f.balance = Some(b);
                }
            }
            Reply::Saved(result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let Some(f) = &mut self.customers.form else {
                    return Vec::new();
                };
                f.saving = false;
                match result {
                    Ok(c) => {
                        let balance = f.balance.take();
                        let id = c.id.clone();
                        let mut form = CustomerForm::of(c);
                        form.balance = balance;
                        form.notice = Some(notice("saved", "", Tone::Good));
                        let load = form.balance.is_none();
                        self.customers.form = Some(form);
                        self.bump();
                        if load {
                            return vec![self.customer_call(Call::Balance(id))];
                        }
                    }
                    Err(e) => {
                        if e.problem().is_some_and(|p| f.apply_problem(p)) {
                            f.notice = Some(Notice::bad("fix_errors"));
                        } else if e.code() == Some("invalid_reference") {
                            f.issues.clear();
                            f.notice = Some(Notice::bad("customer_reference_gone"));
                        } else {
                            f.issues.clear();
                            f.notice = failed;
                        }
                    }
                }
            }
            Reply::Archived(result) => return self.customer_written(result, false),
            Reply::Erased(result) => return self.customer_written(result, true),
            Reply::Merged(result) => match result {
                Ok(a) => {
                    self.customers.merge = MergeState::default();
                    self.bump();
                    self.customers.duplicates.notice =
                        Some(notice("merged_done", a.display_code, Tone::Good));
                    return self.reload_review(false);
                }
                Err(e) => self.customers.merge.notice = self.failed(&e),
            },
            Reply::Review(negative, result) => {
                let failed = result.as_ref().err().and_then(|e| self.failed(e));
                let review = if negative {
                    &mut self.customers.negative
                } else {
                    &mut self.customers.duplicates
                };
                review.loading = false;
                match result {
                    Ok(page) => {
                        review.rows = page.items;
                        review.next_page = page.next_page;
                        review.cursor = review.cursor.min(review.rows.len().saturating_sub(1));
                        review.notice = review.notice.take().filter(|n| n.tone == Tone::Good);
                    }
                    Err(_) => review.notice = failed,
                }
                if !negative {
                    let cursor = self.customers.duplicates.cursor;
                    let still = self.customers.merge.from.as_ref().is_some_and(|b| {
                        self.customers.duplicates.rows.iter().any(|r| r.id == b.id)
                    });
                    if !still {
                        self.customers.merge = MergeState::default();
                        return self.pick_duplicate(cursor);
                    }
                }
            }
            Reply::Groups(result) => match result {
                Ok(groups) => {
                    self.customers.groups = groups;
                    self.reselect(false);
                }
                Err(e) => self.customers.group_editor.notice = self.failed(&e),
            },
            Reply::Tiers(result) => match result {
                Ok(tiers) => {
                    self.customers.tiers = tiers;
                    self.reselect(true);
                }
                Err(e) => self.customers.tier_editor.notice = self.failed(&e),
            },
            Reply::GroupSaved(result) => {
                return self.named_saved(false, result.map(|g| g.id));
            }
            Reply::TierSaved(result) => {
                return self.named_saved(true, result.map(|t| t.id));
            }
            Reply::Settings(result) => self.settings_loaded(result, false),
            Reply::SettingsSaved(result) => self.settings_loaded(result, true),
            Reply::Partner(result) => {
                let failed = result.as_ref().err().and_then(|e| {
                    if e.code() == Some("not_found") {
                        Some(Notice::bad("partner_not_found"))
                    } else {
                        self.failed(e)
                    }
                });
                let m = &mut self.customers.merge;
                match result {
                    Ok(a) if m.from.as_ref().is_some_and(|b| b.id == a.id) => {
                        m.notice = Some(Notice::bad("merge_same"));
                    }
                    Ok(a) => {
                        let typed = CustomerCode::parse(m.code.trim()).ok();
                        if typed.is_none() {
                            // Found by phone: show its code in the field.
                            m.code.clone_from(&a.display_code);
                            self.customers.serial += 1;
                        }
                        let m = &mut self.customers.merge;
                        m.into = Some(a);
                        m.notice = None;
                    }
                    Err(_) => m.notice = failed,
                }
            }
        }
        Vec::new()
    }

    /// An archive, unarchive or erase answered.
    fn customer_written(
        &mut self,
        result: Result<Customer, ClientError>,
        erase: bool,
    ) -> Vec<Effect> {
        let failed = result.as_ref().err().and_then(|e| self.failed(e));
        let Some(f) = &mut self.customers.form else {
            return Vec::new();
        };
        f.saving = false;
        match result {
            Ok(c) => {
                let done = if erase {
                    "erased_done"
                } else if c.archived {
                    "customer_archived_done"
                } else {
                    "customer_unarchived_done"
                };
                let keep = (f.is_dirty() && !erase).then(|| f.fields.clone());
                let balance = f.balance.take();
                let mut form = CustomerForm::of(c);
                if let Some(fields) = keep {
                    form.fields = fields;
                }
                form.balance = balance;
                form.notice = Some(notice(done, "", Tone::Good));
                self.customers.form = Some(form);
                self.bump();
            }
            Err(_) => f.notice = failed,
        }
        Vec::new()
    }

    /// The balance settings arrived (`saved`: after a save).
    fn settings_loaded(&mut self, result: Result<BalanceSettings, ClientError>, saved: bool) {
        let failed = result.as_ref().err().and_then(|e| self.failed(e));
        let s = &mut self.customers.settings;
        match result {
            Ok(stored) => {
                s.stored = Some(stored);
                s.per_sale = stored.max_saved_per_sale.to_string();
                s.max_balance = stored.max_balance.to_string();
                s.issues.clear();
                s.notice = saved.then(|| notice("saved", "", Tone::Good));
                self.bump();
            }
            Err(e) => {
                s.notice = failed;
                match e.problem().and_then(Problem::field) {
                    Some("max_saved_per_sale") => s
                        .issues
                        .push(Issue::new("max_saved_per_sale", "invalid_amount")),
                    Some("max_balance") => {
                        s.issues.push(Issue::new("max_balance", "invalid_amount"));
                    }
                    _ => {}
                }
            }
        }
    }

    fn review_notice(&mut self, n: Option<Notice>) {
        match self.screen() {
            Screen::Duplicates => self.customers.duplicates.notice = n,
            Screen::NegativeBalances => self.customers.negative.notice = n,
            _ => self.customers.list.notice = n,
        }
    }

    /// After a reload: the editor keeps its entry (and its notice).
    fn reselect(&mut self, tiers: bool) {
        let screen = if tiers {
            Screen::CustomerTiers
        } else {
            Screen::CustomerGroups
        };
        if self.screen() != screen {
            return;
        }
        let editing = self.named_mut(tiers).editing.clone();
        let row = editing.and_then(|id| {
            if tiers {
                self.customers.tiers.iter().position(|t| t.id == id)
            } else {
                self.customers.groups.iter().position(|g| g.id == id)
            }
        });
        if let Some(row) = row {
            let n = self.named_mut(tiers).notice.take();
            self.select_named(tiers, row);
            self.named_mut(tiers).notice = n;
        }
    }

    fn named_saved(&mut self, tiers: bool, result: Result<String, ClientError>) -> Vec<Effect> {
        match result {
            Ok(id) => {
                let editor = self.named_mut(tiers);
                editor.editing = Some(id);
                editor.notice = Some(notice("saved", "", Tone::Good));
                self.load_groups_tiers()
            }
            Err(e) => {
                let n = if e.code() == Some("conflict") {
                    let rank = e.problem().and_then(Problem::field) == Some("rank");
                    Some(Notice::bad(if rank {
                        "duplicate_rank"
                    } else {
                        "duplicate_name"
                    }))
                } else {
                    self.failed(&e)
                };
                self.named_mut(tiers).notice = n;
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests;
