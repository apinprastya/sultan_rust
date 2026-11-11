//! The fake's customers (spec 2.5, phase M3): an in-memory customer book that follows the
//! server's rules closely enough for the register and back-office UI tests (M4, M5).
//!
//! **Seed** (every new fake): the group `Grosir` ([`SEED_GROUP`]), the tiers `Silver` (rank 1)
//! and `Gold` (rank 2), and two customers:
//! - [`SEED_BUDI`] *Budi Santoso*, code `K7QH-29XM`, phone `0812-3456-789`, Gold until
//!   2027-12-31, balance Rp 2 200 (one `change_saved` entry);
//! - [`SEED_SITI`] *Siti Rahma* of *Toko Siti*, code `M4RT-2PQA`, phone `0812-9876-5432`,
//!   group Grosir, Silver, balance 0.
//!
//! **Who may do what** follows the seeded roles: the back-office `owner` is an Owner (every
//! customer permission), `budi` (back office) and a register user marked
//! [`super::FakeUser::with_reports`] (Budi at the register) are Managers (everything but
//! `customer.erase`), everyone else is a Cashier (`customer.create` only). Only a register user
//! can pay out (`403 register_session_required` otherwise); no register user is an Owner, so a
//! write-off (negative payout) is always `403 permission_denied` with `reason: owner_only`.
//!
//! Rules kept: `core::customer` checks every field (`invalid_phone`, `invalid_tax_id`,
//! `invalid_code`, `invalid_field`); unique phone among active customers (`409 phone_taken` with
//! `customer_code` and `masked_phone`); replays change nothing; archive, unarchive, merge (balance
//! moved, chains refused) and erase like the server; lookups follow merges; the balance payout
//! needs the caller's open session and lowers its expected cash in the close report. Action
//! calls with a key already used return the stored answer, like the server's `Idempotency-Key`.
//! Writes emit `customer.changed {customer_id}`. Codes of new customers are drawn
//! deterministically, so tests are repeatable.

use std::collections::HashMap;

use serde_json::{Value, json};
use sultan_core::customer::{
    Birthday, CustomerCode, CustomerError, Phone, TaxId, normalize_email, normalize_name,
    payout as check_payout,
};
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use super::{BRANCH_ID, FakeService, REGISTER_ID, State, api, ready};
use crate::customers::{
    BalanceEntry, BalanceSettings, CUSTOMER_BALANCE_PAYOUT, CUSTOMER_CREATE, CUSTOMER_EDIT,
    CUSTOMER_ERASE, CUSTOMER_MANAGE, CUSTOMER_SEARCH, Customer, CustomerBalance, CustomerBody,
    CustomerGroup, CustomerPage, CustomerQuery, GroupBody, MembershipTier, Payout, TierBody,
};
use crate::{ClientError, Problem, ServerEvent};

/// The seeded group `Grosir`.
pub const SEED_GROUP: &str = "group-grosir";
/// The seeded tier `Silver` (rank 1).
pub const SEED_SILVER: &str = "tier-silver";
/// The seeded tier `Gold` (rank 2).
pub const SEED_GOLD: &str = "tier-gold";
/// The seeded customer Budi Santoso (Gold, balance 2 200).
pub const SEED_BUDI: &str = "customer-budi";
/// The seeded customer Siti Rahma (Grosir, Silver).
pub const SEED_SITI: &str = "customer-siti";
/// The name of an erased customer (spec 2.5 §7).
pub const ERASED_NAME: &str = "Erased customer";
/// Page size of the name search and the list.
const SEARCH_PAGE: u32 = 20;
/// Page size of the review lists.
const REVIEW_PAGE: usize = 50;
/// Page size of the balance entries.
const ENTRY_PAGE: usize = 50;

/// A customer role, as seeded on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Role {
    Owner,
    Manager,
    Cashier,
}

impl Role {
    pub(super) fn holds(self, permission: &str) -> bool {
        match self {
            Self::Owner => true,
            Self::Manager => permission != CUSTOMER_ERASE,
            Self::Cashier => permission == CUSTOMER_CREATE,
        }
    }

    /// The customer permission keys the role holds (for `GET /auth/me`).
    pub(super) fn keys(self) -> Vec<String> {
        [
            CUSTOMER_CREATE,
            CUSTOMER_SEARCH,
            CUSTOMER_EDIT,
            CUSTOMER_MANAGE,
            CUSTOMER_ERASE,
            CUSTOMER_BALANCE_PAYOUT,
        ]
        .into_iter()
        .filter(|k| self.holds(k))
        .map(str::to_owned)
        .collect()
    }
}

struct Caller {
    id: String,
    role: Role,
    register: bool,
}

/// The fake's customer state.
pub(super) struct Customers {
    list: Vec<Customer>,
    groups: Vec<CustomerGroup>,
    tiers: Vec<MembershipTier>,
    entries: Vec<BalanceEntry>,
    settings: BalanceSettings,
    replays: HashMap<String, Value>,
    paid_out: HashMap<String, i64>,
    serial: u64,
    events: Vec<ServerEvent>,
}

fn seed_customer(id: &str, code: &str, name: &str, phone: &str) -> Customer {
    let mut c = Customer {
        id: id.to_owned(),
        code: code.to_owned(),
        name: name.to_owned(),
        phone: Some(phone.to_owned()),
        signed_up_branch_id: Some(BRANCH_ID.to_owned()),
        signed_up_at: 1_790_000_000_000,
        created_at: 1_790_000_000_000,
        updated_at: 1_790_000_000_000,
        ..Customer::default()
    };
    c.display_code = shown(code);
    c
}

impl Default for Customers {
    fn default() -> Self {
        let mut budi = seed_customer(SEED_BUDI, "K7QH29XM", "Budi Santoso", "+628123456789");
        budi.tier_id = Some(SEED_GOLD.to_owned());
        budi.tier_valid_until = Some("2027-12-31".to_owned());
        budi.balance = 2_200;
        let mut siti = seed_customer(SEED_SITI, "M4RT2PQA", "Siti Rahma", "+6281298765432");
        siti.company_name = Some("Toko Siti".to_owned());
        siti.group_id = Some(SEED_GROUP.to_owned());
        siti.tier_id = Some(SEED_SILVER.to_owned());
        Self {
            list: vec![budi, siti],
            groups: vec![CustomerGroup {
                id: SEED_GROUP.to_owned(),
                name: "Grosir".to_owned(),
                archived: false,
            }],
            tiers: vec![
                MembershipTier {
                    id: SEED_GOLD.to_owned(),
                    name: "Gold".to_owned(),
                    rank: 2,
                    archived: false,
                },
                MembershipTier {
                    id: SEED_SILVER.to_owned(),
                    name: "Silver".to_owned(),
                    rank: 1,
                    archived: false,
                },
            ],
            entries: vec![BalanceEntry {
                id: "entry-seed-1".to_owned(),
                customer_id: SEED_BUDI.to_owned(),
                kind: "change_saved".to_owned(),
                amount: 2_200,
                branch_id: Some(BRANCH_ID.to_owned()),
                occurred_at: 1_790_000_000_000,
                ..BalanceEntry::default()
            }],
            settings: BalanceSettings::default(),
            replays: HashMap::new(),
            paid_out: HashMap::new(),
            serial: 0,
            events: Vec::new(),
        }
    }
}

impl Customers {
    /// Cash a cashier session paid out for balance payouts (its close report's `cash_out`).
    pub(super) fn paid_out(&self, session: &str) -> i64 {
        self.paid_out.get(session).copied().unwrap_or_default()
    }
}

/// `K7QH29XM` → `K7QH-29XM`.
fn shown(code: &str) -> String {
    CustomerCode::parse(code).map_or_else(|_| code.to_owned(), |c| c.to_string())
}

fn denied(permission: &str) -> ClientError {
    Problem::new(403, "permission_denied", "denied")
        .with("permission", permission)
        .into()
}

fn not_found() -> ClientError {
    api(404, "not_found")
}

fn invalid(field: &str, detail: &str) -> ClientError {
    Problem::new(422, "invalid_field", detail)
        .with("field", field)
        .into()
}

/// A `core::customer` rule failure, shaped like the server's.
fn rule(error: &CustomerError) -> ClientError {
    let status = match error {
        CustomerError::Merged | CustomerError::Archived | CustomerError::BalanceNotZero { .. } => {
            409
        }
        _ => 422,
    };
    let mut p = Problem::new(status, error.code(), &error.to_string());
    if let Some(field) = error.field() {
        p = p.with("field", field);
    }
    match error {
        CustomerError::InsufficientBalance { balance }
        | CustomerError::BalanceNotZero { balance } => p = p.with("balance", balance.minor()),
        CustomerError::BalanceLimit { max } => p = p.with("max", max.minor()),
        _ => {}
    }
    p.into()
}

/// `YYYY-MM-DD` → `(text, YYYYMMDD)`.
fn parse_date(text: &str) -> Option<(String, i64)> {
    let mut parts = text.trim().splitn(3, '-');
    let mut next = || parts.next().and_then(|p| p.parse::<u32>().ok());
    let (y, m, d) = (next()?, next()?, next()?);
    if !(1..=9999).contains(&y) || Birthday::new(m, d, i32::try_from(y).ok()).is_err() {
        return None;
    }
    Some((
        format!("{y:04}-{m:02}-{d:02}"),
        i64::from(y * 10_000 + m * 100 + d),
    ))
}

fn present(text: Option<&String>) -> Option<&str> {
    text.map(|t| t.trim()).filter(|t| !t.is_empty())
}

/// The profile of `body` checked with `core::customer` (code, display and state left empty).
fn profile(body: &CustomerBody) -> Result<(Option<String>, Customer), ClientError> {
    let code = present(body.code.as_ref())
        .map(|c| CustomerCode::parse(c).map(|c| c.as_str().to_owned()))
        .transpose()
        .map_err(|e| rule(&e))?;
    let phone = present(body.phone.as_ref())
        .map(|p| Phone::parse(p).map(|p| p.as_str().to_owned()))
        .transpose()
        .map_err(|e| rule(&e))?;
    let tax_id = present(body.tax_id.as_ref())
        .map(|t| TaxId::parse(t).map(|t| t.as_str().to_owned()))
        .transpose()
        .map_err(|e| rule(&e))?;
    let email = present(body.email.as_ref())
        .map(normalize_email)
        .transpose()
        .map_err(|e| rule(&e))?;
    let name = normalize_name(&body.name).map_err(|e| rule(&e))?;
    let (birth_month, birth_day, birth_year) = match (body.birth_month, body.birth_day) {
        (None, None) if body.birth_year.is_none() => (None, None, None),
        (Some(m), Some(d)) => {
            Birthday::new(m, d, body.birth_year).map_err(|e| rule(&e))?;
            let year = body
                .birth_year
                .map(|y| u16::try_from(y).ok().filter(|y| (1..=9999).contains(y)))
                .map(|y| y.ok_or_else(|| invalid("birth_year", "out of range")))
                .transpose()?;
            (u8::try_from(m).ok(), u8::try_from(d).ok(), year)
        }
        _ => return Err(invalid("birthday", "month and day go together")),
    };
    let tier_id = present(body.tier_id.as_ref()).map(str::to_owned);
    let tier_valid_until = present(body.tier_valid_until.as_ref())
        .map(|d| parse_date(d).map(|(t, _)| t))
        .map(|d| d.ok_or_else(|| invalid("tier_valid_until", "not a YYYY-MM-DD date")))
        .transpose()?;
    if tier_id.is_none() && tier_valid_until.is_some() {
        return Err(invalid("tier_valid_until", "needs tier_id"));
    }
    let text = |t: &Option<String>| present(t.as_ref()).map(str::to_owned);
    Ok((
        code,
        Customer {
            name,
            phone,
            email,
            birth_month,
            birth_day,
            birth_year,
            address: text(&body.address),
            company_name: text(&body.company_name),
            tax_id,
            tax_address: text(&body.tax_address),
            notes: text(&body.notes),
            group_id: present(body.group_id.as_ref()).map(str::to_owned),
            tier_id,
            tier_valid_until,
            manual_tier: body.manual_tier,
            ..Customer::default()
        },
    ))
}

/// Whether two customers have the same writable profile.
fn same_profile(a: &Customer, b: &Customer) -> bool {
    let key = |c: &Customer| {
        (
            c.name.clone(),
            c.phone.clone(),
            c.email.clone(),
            (c.birth_month, c.birth_day, c.birth_year),
            (c.address.clone(), c.company_name.clone()),
            (c.tax_id.clone(), c.tax_address.clone(), c.notes.clone()),
        )
    };
    key(a) == key(b) && tier_part(a) == tier_part(b)
}

fn tier_part(c: &Customer) -> (Option<String>, Option<String>, Option<String>, bool) {
    (
        c.group_id.clone(),
        c.tier_id.clone(),
        c.tier_valid_until.clone(),
        c.manual_tier,
    )
}

fn page_window(page: Option<u32>) -> Result<usize, ClientError> {
    match page.unwrap_or(1) {
        0 => Err(invalid("page", "pages start at 1")),
        p => Ok(p as usize - 1),
    }
}

impl State {
    fn customer_caller(&self, token: &str) -> Result<Caller, ClientError> {
        if let Ok(user) = self.caller(token) {
            let role = if user.reports {
                Role::Manager
            } else {
                Role::Cashier
            };
            return Ok(Caller {
                id: user.id,
                role,
                register: true,
            });
        }
        let user = self.bo_user(token)?;
        let role = if user.manage {
            Role::Owner
        } else if user.manage_here {
            Role::Manager
        } else {
            Role::Cashier
        };
        Ok(Caller {
            id: user.id,
            role,
            register: false,
        })
    }

    fn allowed(&self, token: &str, permission: &str) -> Result<Caller, ClientError> {
        let caller = self.customer_caller(token)?;
        if caller.role.holds(permission) {
            Ok(caller)
        } else {
            Err(denied(permission))
        }
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.customers.list.iter().position(|c| c.id == id)
    }

    /// The customer as the API shows it: names, masked phone, tier status.
    fn view(&self, c: &Customer) -> Customer {
        let mut c = c.clone();
        let cs = &self.customers;
        c.display_code = shown(&c.code);
        c.masked_phone = c
            .phone
            .as_deref()
            .and_then(|p| Phone::parse(p).ok())
            .map(|p| p.masked());
        c.group_name = c
            .group_id
            .as_ref()
            .and_then(|g| cs.groups.iter().find(|x| x.id == *g))
            .map(|g| g.name.clone());
        let tier = c
            .tier_id
            .as_ref()
            .and_then(|t| cs.tiers.iter().find(|x| x.id == *t));
        c.tier_name = tier.map(|t| t.name.clone());
        let today = LocalDay::of(
            Timestamp::from_unix_millis(self.now),
            BranchTimeZone::default(),
        );
        c.tier_applies = c.tier_id.as_ref().map(|_| {
            tier.is_some_and(|t| !t.archived)
                && c.tier_valid_until
                    .as_deref()
                    .and_then(parse_date)
                    .is_none_or(|(_, until)| until >= today.key())
        });
        c.archived = c.archived_at.is_some();
        c
    }

    fn changed(&mut self, id: &str) {
        self.customers.events.push(ServerEvent {
            kind: "customer.changed".to_owned(),
            data: json!({ "customer_id": id }),
        });
    }

    fn draw_code(&mut self) -> String {
        loop {
            self.customers.serial += 1;
            let mut x = self
                .customers
                .serial
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                .wrapping_add(0x2545_F491);
            let code = CustomerCode::generate(|| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x.to_le_bytes()[0]
            });
            let code = code.as_str().to_owned();
            if !self.customers.list.iter().any(|c| c.code == code) {
                return code;
            }
        }
    }

    fn phone_holder(&self, phone: &str, except: &str) -> Option<&Customer> {
        self.customers.list.iter().find(|c| {
            c.id != except && c.archived_at.is_none() && c.phone.as_deref() == Some(phone)
        })
    }

    fn phone_taken(holder: &Customer) -> ClientError {
        Problem::new(409, "phone_taken", "phone taken")
            .with("field", "phone")
            .with("customer_id", holder.id.clone())
            .with("customer_code", shown(&holder.code))
            .with(
                "masked_phone",
                holder
                    .phone
                    .as_deref()
                    .and_then(|p| Phone::parse(p).ok())
                    .map(|p| p.masked())
                    .unwrap_or_default(),
            )
            .into()
    }

    fn put_customer(
        &mut self,
        token: &str,
        id: &str,
        body: &CustomerBody,
    ) -> Result<Customer, ClientError> {
        let caller = self.customer_caller(token)?;
        let (code, mut new) = profile(body)?;
        let need = |p: &str| {
            if caller.role.holds(p) {
                Ok(())
            } else {
                Err(denied(p))
            }
        };
        let existing = self.index(id);
        if let Some(i) = existing {
            let old = &self.customers.list[i];
            if same_profile(old, &new) {
                if !caller.role.holds(CUSTOMER_CREATE) && !caller.role.holds(CUSTOMER_EDIT) {
                    return Err(denied(CUSTOMER_CREATE));
                }
                if old.merged_into_id.is_some() {
                    return Err(rule(&CustomerError::Merged));
                }
                if old.archived_at.is_some() {
                    return Err(rule(&CustomerError::Archived));
                }
                return Ok(self.view(old));
            }
            need(CUSTOMER_EDIT)?;
            if tier_part(old) != tier_part(&new) {
                need(CUSTOMER_MANAGE)?;
            }
            if old.merged_into_id.is_some() {
                return Err(rule(&CustomerError::Merged));
            }
            if old.archived_at.is_some() {
                return Err(rule(&CustomerError::Archived));
            }
        } else {
            need(CUSTOMER_CREATE)?;
            if tier_part(&new) != (None, None, None, false) {
                need(CUSTOMER_MANAGE)?;
            }
        }
        if let Some(phone) = &new.phone
            && let Some(holder) = self.phone_holder(phone, id)
        {
            return Err(Self::phone_taken(holder));
        }
        if let Some(g) = &new.group_id
            && !self.customers.groups.iter().any(|x| x.id == *g)
        {
            return Err(Problem::new(422, "invalid_reference", "unknown group")
                .with("field", "group_id")
                .into());
        }
        if let Some(t) = &new.tier_id
            && !self.customers.tiers.iter().any(|x| x.id == *t)
        {
            return Err(Problem::new(422, "invalid_reference", "unknown tier")
                .with("field", "tier_id")
                .into());
        }
        let now = self.now;
        if let Some(i) = existing {
            let old = &self.customers.list[i];
            new.id.clone_from(&old.id);
            new.code.clone_from(&old.code);
            new.signed_up_branch_id.clone_from(&old.signed_up_branch_id);
            new.signed_up_at = old.signed_up_at;
            new.phone_conflict.clone_from(&old.phone_conflict);
            new.balance = old.balance;
            new.created_at = old.created_at;
            new.updated_at = now;
            self.customers.list[i] = new.clone();
        } else {
            new.code = match code {
                Some(code) if self.customers.list.iter().any(|c| c.code == code) => {
                    return Err(Problem::new(409, "conflict", "code taken")
                        .with("field", "code")
                        .into());
                }
                Some(code) => code,
                None => self.draw_code(),
            };
            id.clone_into(&mut new.id);
            new.signed_up_branch_id = Some(BRANCH_ID.to_owned());
            new.signed_up_at = now;
            new.created_at = now;
            new.updated_at = now;
            self.customers.list.push(new.clone());
        }
        self.changed(id);
        Ok(self.view(&new))
    }

    /// The active customer `id` ends at, following merges.
    fn follow(&self, id: &str) -> Option<&Customer> {
        let mut next = Some(id.to_owned());
        for _ in 0..4 {
            let c = self
                .customers
                .list
                .iter()
                .find(|c| Some(&c.id) == next.as_ref())?;
            if c.archived_at.is_none() {
                return Some(c);
            }
            next.clone_from(&c.merged_into_id);
        }
        None
    }

    fn lookup_phone(&self, token: &str, phone: &str) -> Result<Customer, ClientError> {
        self.customer_caller(token)?;
        let phone = Phone::parse(phone).map_err(|e| rule(&e))?;
        let list = &self.customers.list;
        let found = self.phone_holder(phone.as_str(), "").or_else(|| {
            list.iter()
                .rev()
                .find(|c| c.phone.as_deref() == Some(phone.as_str()) && c.merged_into_id.is_some())
                .and_then(|c| self.follow(c.merged_into_id.as_deref().unwrap_or_default()))
        });
        found.map(|c| self.view(c)).ok_or_else(not_found)
    }

    fn lookup_code(&self, token: &str, code: &str) -> Result<Customer, ClientError> {
        self.customer_caller(token)?;
        let code = CustomerCode::parse(code).map_err(|e| rule(&e))?;
        let id = self
            .customers
            .list
            .iter()
            .find(|c| c.code == code.as_str())
            .map(|c| c.id.clone())
            .ok_or_else(not_found)?;
        self.follow(&id).map(|c| self.view(c)).ok_or_else(not_found)
    }

    fn search_customers(
        &self,
        token: &str,
        query: &CustomerQuery,
    ) -> Result<CustomerPage, ClientError> {
        let caller = self.allowed(token, CUSTOMER_SEARCH)?;
        let limit = query.limit.unwrap_or(SEARCH_PAGE);
        if !(1..=SEARCH_PAGE).contains(&limit) {
            return Err(invalid("limit", "1 to 20"));
        }
        let skip = page_window(query.page)? * limit as usize;
        let q = query.q.trim().to_lowercase();
        if q.is_empty() && query.include_archived && !caller.role.holds(CUSTOMER_MANAGE) {
            return Err(denied(CUSTOMER_MANAGE));
        }
        let mut found: Vec<&Customer> = self
            .customers
            .list
            .iter()
            .filter(|c| {
                if q.is_empty() {
                    query.include_archived || c.archived_at.is_none()
                } else {
                    c.archived_at.is_none()
                        && format!(
                            "{} {}",
                            c.name,
                            c.company_name.as_deref().unwrap_or_default()
                        )
                        .to_lowercase()
                        .contains(&q)
                }
            })
            .collect();
        found.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
        Ok(self.page(&found, skip, limit as usize, query.page))
    }

    fn page(
        &self,
        found: &[&Customer],
        skip: usize,
        limit: usize,
        page: Option<u32>,
    ) -> CustomerPage {
        let items: Vec<Customer> = found
            .iter()
            .skip(skip)
            .take(limit)
            .map(|c| self.view(c))
            .collect();
        let next_page = (items.len() == limit).then(|| page.unwrap_or(1) + 1);
        CustomerPage { items, next_page }
    }

    fn get_customer(&self, token: &str, id: &str) -> Result<Customer, ClientError> {
        let caller = self.customer_caller(token)?;
        let c = self
            .customers
            .list
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(not_found)?;
        if c.archived_at.is_some() && !caller.role.holds(CUSTOMER_MANAGE) {
            return Err(denied(CUSTOMER_MANAGE));
        }
        Ok(self.view(c))
    }

    /// Runs a keyed action once; a used key answers the stored result.
    fn keyed<T: serde::Serialize + serde::de::DeserializeOwned>(
        &mut self,
        key: &str,
        action: impl FnOnce(&mut Self) -> Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        if let Some(stored) = self.customers.replays.get(key) {
            return serde_json::from_value(stored.clone())
                .map_err(|e| ClientError::Unexpected(e.to_string()));
        }
        let result = action(self)?;
        let stored = serde_json::to_value(&result).unwrap_or(Value::Null);
        self.customers.replays.insert(key.to_owned(), stored);
        Ok(result)
    }

    fn set_customer_archived(
        &mut self,
        token: &str,
        id: &str,
        archived: bool,
    ) -> Result<Customer, ClientError> {
        self.allowed(token, CUSTOMER_MANAGE)?;
        let i = self.index(id).ok_or_else(not_found)?;
        let c = &self.customers.list[i];
        if c.erased_at.is_some() {
            return Err(not_found());
        }
        if c.archived_at.is_some() == archived {
            return Ok(self.view(c));
        }
        if !archived {
            if c.merged_into_id.is_some() {
                return Err(rule(&CustomerError::Merged));
            }
            if let Some(phone) = &c.phone
                && let Some(holder) = self.phone_holder(phone, id)
            {
                return Err(Self::phone_taken(holder));
            }
        }
        let now = self.now;
        let c = &mut self.customers.list[i];
        c.archived_at = archived.then_some(now);
        c.updated_at = now;
        let c = c.clone();
        self.changed(id);
        Ok(self.view(&c))
    }

    fn erase(&mut self, token: &str, id: &str) -> Result<Customer, ClientError> {
        self.allowed(token, CUSTOMER_ERASE)?;
        let i = self.index(id).ok_or_else(not_found)?;
        if self.customers.list[i].erased_at.is_some() {
            return Err(not_found());
        }
        let balance = self.customers.list[i].balance;
        if balance != 0 {
            return Err(rule(&CustomerError::BalanceNotZero {
                balance: Money::from_minor(balance),
            }));
        }
        let now = self.now;
        let old = self.customers.list[i].clone();
        let erased = Customer {
            id: old.id,
            code: old.code,
            name: ERASED_NAME.to_owned(),
            group_id: old.group_id,
            tier_id: old.tier_id,
            tier_valid_until: old.tier_valid_until,
            manual_tier: old.manual_tier,
            signed_up_branch_id: old.signed_up_branch_id,
            signed_up_at: old.signed_up_at,
            archived_at: old.archived_at.or(Some(now)),
            merged_into_id: old.merged_into_id,
            erased_at: Some(now),
            created_at: old.created_at,
            updated_at: now,
            ..Customer::default()
        };
        self.customers.list[i] = erased.clone();
        self.changed(id);
        Ok(self.view(&erased))
    }

    fn entry(&mut self, customer: &str, kind: &str, amount: i64) -> BalanceEntry {
        self.customers.serial += 1;
        let entry = BalanceEntry {
            id: format!("entry-{}", self.customers.serial),
            customer_id: customer.to_owned(),
            kind: kind.to_owned(),
            amount,
            branch_id: Some(BRANCH_ID.to_owned()),
            occurred_at: self.now,
            ..BalanceEntry::default()
        };
        if let Some(i) = self.index(customer) {
            self.customers.list[i].balance += amount;
        }
        self.customers.entries.push(entry.clone());
        entry
    }

    fn merge(&mut self, token: &str, into: &str, from: &str) -> Result<Customer, ClientError> {
        self.allowed(token, CUSTOMER_MANAGE)?;
        if into == from {
            return Err(invalid("from", "a customer cannot be merged into itself"));
        }
        let (a, b) = (
            self.index(into).ok_or_else(not_found)?,
            self.index(from).ok_or_else(not_found)?,
        );
        for i in [a, b] {
            let c = &self.customers.list[i];
            if c.merged_into_id.is_some() {
                return Err(rule(&CustomerError::Merged));
            }
            if c.archived_at.is_some() {
                return Err(rule(&CustomerError::Archived));
            }
        }
        let src = self.customers.list[b].clone();
        let now = self.now;
        {
            let dst = &mut self.customers.list[a];
            let fill = |t: &mut Option<String>, s: &Option<String>| {
                if t.is_none() {
                    t.clone_from(s);
                }
            };
            fill(&mut dst.phone, &src.phone);
            fill(&mut dst.email, &src.email);
            if dst.birth_month.is_none() {
                (dst.birth_month, dst.birth_day, dst.birth_year) =
                    (src.birth_month, src.birth_day, src.birth_year);
            }
            fill(&mut dst.address, &src.address);
            fill(&mut dst.company_name, &src.company_name);
            fill(&mut dst.tax_id, &src.tax_id);
            fill(&mut dst.tax_address, &src.tax_address);
            fill(&mut dst.notes, &src.notes);
            fill(&mut dst.group_id, &src.group_id);
            if dst.tier_id.is_none() {
                dst.tier_id.clone_from(&src.tier_id);
                dst.tier_valid_until.clone_from(&src.tier_valid_until);
                dst.manual_tier = src.manual_tier;
            }
            if dst.phone_conflict == dst.phone {
                dst.phone_conflict = None;
            }
            dst.updated_at = now;
        }
        if src.balance != 0 {
            self.entry(from, "merge_out", -src.balance);
            self.entry(into, "merge_in", src.balance);
        }
        for c in &mut self.customers.list {
            if c.id == from || c.merged_into_id.as_deref() == Some(from) {
                c.merged_into_id = Some(into.to_owned());
                c.archived_at = c.archived_at.or(Some(now));
                c.updated_at = now;
            }
        }
        self.changed(into);
        self.changed(from);
        let a = self.customers.list[a].clone();
        Ok(self.view(&a))
    }

    fn review(&self, token: &str, page: u32, negative: bool) -> Result<CustomerPage, ClientError> {
        self.allowed(token, CUSTOMER_MANAGE)?;
        let skip = page_window(Some(page))? * REVIEW_PAGE;
        let mut found: Vec<&Customer> = self
            .customers
            .list
            .iter()
            .filter(|c| {
                if negative {
                    c.balance < 0
                } else {
                    c.phone_conflict.is_some() && c.archived_at.is_none()
                }
            })
            .collect();
        if negative {
            found.sort_by_key(|c| (c.balance, c.id.clone()));
        } else {
            found.sort_by(|a, b| (&a.phone_conflict, &a.id).cmp(&(&b.phone_conflict, &b.id)));
        }
        Ok(self.page(&found, skip, REVIEW_PAGE, Some(page)))
    }

    fn balance(
        &self,
        token: &str,
        id: &str,
        before: Option<&str>,
    ) -> Result<CustomerBalance, ClientError> {
        self.customer_caller(token)?;
        let c = self
            .customers
            .list
            .iter()
            .find(|c| c.id == id)
            .ok_or_else(not_found)?;
        let newest_first = self
            .customers
            .entries
            .iter()
            .rev()
            .filter(|e| e.customer_id == id);
        let entries: Vec<BalanceEntry> = match before {
            None => newest_first.take(ENTRY_PAGE).cloned().collect(),
            Some(cursor) => newest_first
                .skip_while(|e| e.id != cursor)
                .skip(1)
                .take(ENTRY_PAGE)
                .cloned()
                .collect(),
        };
        let next_before_id = entries
            .last()
            .filter(|_| entries.len() == ENTRY_PAGE)
            .map(|e| e.id.clone());
        Ok(CustomerBalance {
            customer_id: id.to_owned(),
            balance: c.balance,
            entries,
            next_before_id,
        })
    }

    fn payout(&mut self, token: &str, id: &str, amount: i64) -> Result<Payout, ClientError> {
        let caller = self.allowed(token, CUSTOMER_BALANCE_PAYOUT)?;
        if !caller.register {
            return Err(api(403, "register_session_required"));
        }
        if amount < 0 && caller.role != Role::Owner {
            return Err(Problem::new(403, "permission_denied", "owner only")
                .with("permission", CUSTOMER_BALANCE_PAYOUT)
                .with("reason", "owner_only")
                .into());
        }
        let session = self
            .session
            .clone()
            .filter(|s| s.user_id == caller.id)
            .ok_or_else(|| api(409, "no_open_session"))?;
        let c = self
            .customers
            .list
            .iter()
            .find(|c| c.id == id && c.erased_at.is_none())
            .ok_or_else(not_found)?;
        let checked = check_payout(Money::from_minor(c.balance), Money::from_minor(amount))
            .map_err(|e| rule(&e))?;
        let mut entry = self.entry(id, "payout", checked.amount().minor());
        entry.register_id = Some(REGISTER_ID.to_owned());
        entry.user_id = Some(caller.id);
        entry.cashier_session_id = Some(session.id.clone());
        if let Some(stored) = self.customers.entries.last_mut() {
            *stored = entry.clone();
        }
        if amount > 0 {
            *self.customers.paid_out.entry(session.id).or_default() += amount;
        }
        self.changed(id);
        let balance = self
            .customers
            .list
            .iter()
            .find(|c| c.id == id)
            .map_or(0, |c| c.balance);
        Ok(Payout { entry, balance })
    }

    fn put_group(
        &mut self,
        token: &str,
        id: &str,
        body: &GroupBody,
    ) -> Result<CustomerGroup, ClientError> {
        self.allowed(token, CUSTOMER_MANAGE)?;
        let name = normalize_name(&body.name).map_err(|e| rule(&e))?;
        if self
            .customers
            .groups
            .iter()
            .any(|g| g.name == name && g.id != id)
        {
            return Err(Problem::new(409, "conflict", "name taken")
                .with("field", "name")
                .into());
        }
        let group = CustomerGroup {
            id: id.to_owned(),
            name,
            archived: body.archived,
        };
        match self.customers.groups.iter_mut().find(|g| g.id == id) {
            Some(g) => *g = group.clone(),
            None => self.customers.groups.push(group.clone()),
        }
        Ok(group)
    }

    fn put_tier(
        &mut self,
        token: &str,
        id: &str,
        body: &TierBody,
    ) -> Result<MembershipTier, ClientError> {
        self.allowed(token, CUSTOMER_MANAGE)?;
        let name = normalize_name(&body.name).map_err(|e| rule(&e))?;
        let others = self.customers.tiers.iter().filter(|t| t.id != id);
        for (field, clash) in [
            ("name", others.clone().any(|t| t.name == name)),
            ("rank", others.clone().any(|t| t.rank == body.rank)),
        ] {
            if clash {
                return Err(Problem::new(409, "conflict", "taken")
                    .with("field", field)
                    .into());
            }
        }
        let tier = MembershipTier {
            id: id.to_owned(),
            name,
            rank: body.rank,
            archived: body.archived,
        };
        match self.customers.tiers.iter_mut().find(|t| t.id == id) {
            Some(t) => *t = tier.clone(),
            None => self.customers.tiers.push(tier.clone()),
        }
        Ok(tier)
    }

    fn put_settings(
        &mut self,
        token: &str,
        body: BalanceSettings,
    ) -> Result<BalanceSettings, ClientError> {
        self.allowed(token, CUSTOMER_MANAGE)?;
        for (field, value) in [
            ("max_saved_per_sale", body.max_saved_per_sale),
            ("max_balance", body.max_balance),
        ] {
            if value < 0 {
                return Err(invalid(field, "must not be negative"));
            }
        }
        self.customers.settings = body;
        Ok(body)
    }
}

/// Runs one customer call: records it, takes an injected failure, then sends the events the
/// call produced after the lock is dropped.
fn call<T: Send + 'static>(
    fake: &FakeService,
    method: &'static str,
    detail: String,
    body: impl FnOnce(&mut State) -> Result<T, ClientError>,
) -> crate::BoxFuture<T> {
    let (mut s, failure) = fake.begin(method, detail);
    let result = match failure {
        Some(e) => Err(e),
        None => body(&mut s),
    };
    let events = std::mem::take(&mut s.customers.events);
    let sink = s.sink.clone();
    drop(s);
    if let Some(sink) = sink {
        for event in events {
            sink(event);
        }
    }
    ready(result)
}

impl FakeService {
    /// Adds (or replaces) a customer as is, without any rule (test setup: duplicates, archived
    /// or merged customers). Keep `balance` in step with [`FakeService::add_customer_balance`].
    pub fn seed_customer(&self, customer: Customer) {
        let mut s = self.state();
        match s.index(&customer.id) {
            Some(i) => s.customers.list[i] = customer,
            None => s.customers.list.push(customer),
        }
    }

    /// Appends a balance entry (e.g. `change_saved` +700 or `payment` −300) as a sale would.
    pub fn add_customer_balance(&self, customer: &str, kind: &str, amount: i64) {
        let mut s = self.state();
        s.entry(customer, kind, amount);
    }

    /// The balance settings as stored (what saving change at the till will check, spec 2.6).
    #[must_use]
    pub fn balance_settings(&self) -> BalanceSettings {
        self.state().customers.settings
    }

    pub(super) fn customer_by_phone_call(
        &self,
        token: &str,
        phone: &str,
    ) -> crate::BoxFuture<Customer> {
        call(self, "customer_by_phone", phone.to_owned(), |s| {
            s.lookup_phone(token, phone)
        })
    }

    /// The customer permission keys of the caller's role (the register's `permissions`).
    pub(super) fn permissions_call(&self, token: &str) -> crate::BoxFuture<Vec<String>> {
        call(self, "permissions", String::new(), |s| {
            s.customer_caller(token).map(|c| c.role.keys())
        })
    }

    pub(super) fn customer_by_code_call(
        &self,
        token: &str,
        code: &str,
    ) -> crate::BoxFuture<Customer> {
        call(self, "customer_by_code", code.to_owned(), |s| {
            s.lookup_code(token, code)
        })
    }

    pub(super) fn search_customers_call(
        &self,
        token: &str,
        query: &CustomerQuery,
    ) -> crate::BoxFuture<CustomerPage> {
        let detail = serde_json::to_string(query).unwrap_or_default();
        call(self, "search_customers", detail, |s| {
            s.search_customers(token, query)
        })
    }

    pub(super) fn put_customer_call(
        &self,
        method: &'static str,
        token: &str,
        id: &str,
        body: &CustomerBody,
    ) -> crate::BoxFuture<Customer> {
        call(self, method, id.to_owned(), |s| {
            s.put_customer(token, id, body)
        })
    }

    pub(super) fn customer_balance_call(
        &self,
        token: &str,
        id: &str,
        before: Option<&str>,
    ) -> crate::BoxFuture<CustomerBalance> {
        call(self, "customer_balance", id.to_owned(), |s| {
            s.balance(token, id, before)
        })
    }

    pub(super) fn balance_payout_call(
        &self,
        token: &str,
        id: &str,
        amount: i64,
        key: &str,
    ) -> crate::BoxFuture<Payout> {
        let detail = format!("{id} amount={amount} key={key}");
        call(self, "balance_payout", detail, |s| {
            s.customer_caller(token)?;
            s.keyed(key, |s| s.payout(token, id, amount))
        })
    }
}

/// The [`crate::BackOffice`] customer methods, called from `backoffice.rs`.
impl FakeService {
    pub(super) fn bo_customer(&self, token: &str, id: &str) -> crate::BoxFuture<Customer> {
        call(self, "customer", id.to_owned(), |s| {
            s.get_customer(token, id)
        })
    }

    pub(super) fn bo_set_customer_archived(
        &self,
        token: &str,
        id: &str,
        archived: bool,
        key: &str,
    ) -> crate::BoxFuture<Customer> {
        let detail = format!("{id} archived={archived} key={key}");
        call(self, "set_customer_archived", detail, |s| {
            s.customer_caller(token)?;
            s.keyed(key, |s| s.set_customer_archived(token, id, archived))
        })
    }

    pub(super) fn bo_merge_customer(
        &self,
        token: &str,
        into: &str,
        from: &str,
        key: &str,
    ) -> crate::BoxFuture<Customer> {
        let detail = format!("{into} from={from} key={key}");
        call(self, "merge_customer", detail, |s| {
            s.customer_caller(token)?;
            s.keyed(key, |s| s.merge(token, into, from))
        })
    }

    pub(super) fn bo_erase_customer(
        &self,
        token: &str,
        id: &str,
        key: &str,
    ) -> crate::BoxFuture<Customer> {
        let detail = format!("{id} key={key}");
        call(self, "erase_customer", detail, |s| {
            s.customer_caller(token)?;
            s.keyed(key, |s| s.erase(token, id))
        })
    }

    pub(super) fn bo_review(
        &self,
        token: &str,
        page: u32,
        negative: bool,
    ) -> crate::BoxFuture<CustomerPage> {
        let method = if negative {
            "negative_balances"
        } else {
            "customer_duplicates"
        };
        call(self, method, page.to_string(), |s| {
            s.review(token, page, negative)
        })
    }

    pub(super) fn bo_customer_groups(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<CustomerGroup>> {
        call(self, "customer_groups", String::new(), |s| {
            s.customer_caller(token)?;
            let mut groups: Vec<CustomerGroup> = s
                .customers
                .groups
                .iter()
                .filter(|g| include_archived || !g.archived)
                .cloned()
                .collect();
            groups.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
            Ok(groups)
        })
    }

    pub(super) fn bo_put_customer_group(
        &self,
        token: &str,
        id: &str,
        body: &GroupBody,
    ) -> crate::BoxFuture<CustomerGroup> {
        call(self, "put_customer_group", id.to_owned(), |s| {
            s.put_group(token, id, body)
        })
    }

    pub(super) fn bo_membership_tiers(
        &self,
        token: &str,
        include_archived: bool,
    ) -> crate::BoxFuture<Vec<MembershipTier>> {
        call(self, "membership_tiers", String::new(), |s| {
            s.customer_caller(token)?;
            let mut tiers: Vec<MembershipTier> = s
                .customers
                .tiers
                .iter()
                .filter(|t| include_archived || !t.archived)
                .cloned()
                .collect();
            tiers.sort_by_key(|t| std::cmp::Reverse(t.rank));
            Ok(tiers)
        })
    }

    pub(super) fn bo_put_membership_tier(
        &self,
        token: &str,
        id: &str,
        body: &TierBody,
    ) -> crate::BoxFuture<MembershipTier> {
        call(self, "put_membership_tier", id.to_owned(), |s| {
            s.put_tier(token, id, body)
        })
    }

    pub(super) fn bo_balance_settings(&self, token: &str) -> crate::BoxFuture<BalanceSettings> {
        call(self, "customer_balance_settings", String::new(), |s| {
            s.customer_caller(token)?;
            Ok(s.customers.settings)
        })
    }

    pub(super) fn bo_put_balance_settings(
        &self,
        token: &str,
        body: &BalanceSettings,
    ) -> crate::BoxFuture<BalanceSettings> {
        let body = *body;
        call(self, "put_customer_balance_settings", String::new(), |s| {
            s.put_settings(token, body)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_and_helpers() {
        assert_eq!(Role::Owner.keys().len(), 6);
        assert!(!Role::Manager.keys().contains(&CUSTOMER_ERASE.to_owned()));
        assert_eq!(Role::Cashier.keys(), [CUSTOMER_CREATE]);
        assert_eq!(
            parse_date(" 2027-1-5 "),
            Some(("2027-01-05".to_owned(), 20_270_105))
        );
        assert_eq!(parse_date("2027-02-29"), None);
        assert_eq!(parse_date("0-01-01"), None);
        assert_eq!(parse_date("x"), None);
        assert_eq!(shown("x"), "x");
        let limit = rule(&CustomerError::BalanceLimit {
            max: Money::from_minor(5),
        });
        assert_eq!(limit.problem().and_then(Problem::max), Some(5));
        let overflow = rule(&CustomerError::AmountOverflow);
        assert_eq!(overflow.problem().map(|p| p.status), Some(422));
    }
}
