//! Customers (spec 2.5 §2, §6–§8): create/update, lookup, name search, archive, erase, merge,
//! and the back-office lists (possible duplicates, negative balances).
//!
//! Every customer write appends one audit row and one outbox row `customer.put` carrying the full
//! stored record (balance included), so other nodes can apply it as is.

use serde_json::{Value, json};
use sultan_core::catalog::search_key;
use sultan_core::ids::BranchId;
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use super::balance::{BalanceEntry, BalanceEntryKind, insert_entry};
use super::groups::{get_group, get_tier};
use super::{
    Actor, Change, CustomerDbError, CustomerGroupId, CustomerId, CustomerResult, MembershipTierId,
    Written, clamp_limit, clean, clean_name, record, written,
};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};

/// The name an erased customer gets (spec 2.5 §7).
pub const ERASED_NAME: &str = "Erased customer";
/// Largest page of [`list_duplicates`] and [`list_negative_balances`].
pub const MAX_PAGE: u32 = 100;
/// Largest page of [`search_customers`] (spec 2.5 §6: at most 20 results).
pub const MAX_SEARCH_PAGE: u32 = 20;

/// A birthday: month and day, with an optional year (spec 2.5 §2). 29 February is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Birthday {
    /// 1–12.
    pub month: u8,
    /// 1–31, valid for the month (29 for February).
    pub day: u8,
    /// The year, if known.
    pub year: Option<u16>,
}

impl Birthday {
    fn is_valid(self) -> bool {
        let days = match self.month {
            2 => 29,
            4 | 6 | 9 | 11 => 30,
            1..=12 => 31,
            _ => return false,
        };
        (1..=days).contains(&self.day)
    }
}

/// The writable part of a customer, as `PUT /customers/{id}` sends it (spec 2.5 §2). Text is
/// expected already normalized by `core::customer` (phone `+62…`, upper-case code without the
/// dash, lower-case email); blank optional fields are stored as `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Customer {
    /// ID (`UUIDv7`, created by the client).
    pub id: CustomerId,
    /// Member code, e.g. `K7QH29XM`. Set on create and never changed (an update keeps the
    /// stored code whatever is sent).
    pub code: String,
    /// Name, 1–100 characters after trimming.
    pub name: String,
    /// Normalized phone; unique among non-archived customers.
    pub phone: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Birthday.
    pub birthday: Option<Birthday>,
    /// Address.
    pub address: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// NPWP or NIK.
    pub tax_id: Option<String>,
    /// Tax address.
    pub tax_address: Option<String>,
    /// Staff notes (not printed).
    pub notes: Option<String>,
    /// Customer group.
    pub group: Option<CustomerGroupId>,
    /// Membership tier.
    pub tier: Option<MembershipTierId>,
    /// Last day the tier applies, `YYYYMMDD` in the selling branch's time zone.
    pub tier_valid_until: Option<u32>,
    /// "Manual tier": never changed by the automatic tier job (spec 2.5 §5).
    pub manual_tier: bool,
    /// Branch where the customer signed up. Kept from the first save.
    pub signed_up_branch: Option<BranchId>,
    /// When the customer signed up. Kept from the first save.
    pub signed_up_at: Timestamp,
}

/// A stored customer with its state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomerRecord {
    /// The profile.
    pub customer: Customer,
    /// A phone that clashed with another customer when a branch's sign-up was synced to HQ: the
    /// customer appears under "Possible duplicates" (spec 2.5 §7).
    pub phone_conflict: Option<String>,
    /// The cached balance (sum of the balance ledger, spec 2.5 §11).
    pub balance: Money,
    /// When it was archived (also set by a merge and an erase).
    pub archived_at: Option<Timestamp>,
    /// The customer it was merged into (spec 2.5 §8).
    pub merged_into: Option<CustomerId>,
    /// When its personal data was erased.
    pub erased_at: Option<Timestamp>,
    /// Created.
    pub created_at: Timestamp,
    /// Last changed.
    pub updated_at: Timestamp,
}

impl CustomerRecord {
    /// Not archived (and so neither merged nor erased).
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.archived_at.is_none()
    }
}

const SELECT: &str = "SELECT id, code, name, phone, phone_conflict, email, birth_month, \
     birth_day, birth_year, address, company_name, tax_id, tax_address, notes, group_id, \
     tier_id, tier_valid_until, manual_tier, signed_up_branch_id, signed_up_at, balance, \
     archived_at, merged_into_id, erased_at, created_at, updated_at FROM customer";

fn small<T: TryFrom<i64>>(row: &dyn Row, col: &str) -> Result<Option<T>> {
    row.opt_i64(col)?
        .map(|v| T::try_from(v).map_err(|_| DbError::Corrupt(format!("`{col}` out of range"))))
        .transpose()
}

fn map_record(row: &dyn Row) -> Result<CustomerRecord> {
    let month: Option<u8> = small(row, "birth_month")?;
    let day: Option<u8> = small(row, "birth_day")?;
    let birthday = match (month, day) {
        (Some(month), Some(day)) => Some(Birthday {
            month,
            day,
            year: small(row, "birth_year")?,
        }),
        _ => None,
    };
    Ok(CustomerRecord {
        customer: Customer {
            id: sql::id(row, "id")?,
            code: row.text("code")?,
            name: row.text("name")?,
            phone: row.opt_text("phone")?,
            email: row.opt_text("email")?,
            birthday,
            address: row.opt_text("address")?,
            company_name: row.opt_text("company_name")?,
            tax_id: row.opt_text("tax_id")?,
            tax_address: row.opt_text("tax_address")?,
            notes: row.opt_text("notes")?,
            group: sql::opt_id(row, "group_id")?,
            tier: sql::opt_id(row, "tier_id")?,
            tier_valid_until: small(row, "tier_valid_until")?,
            manual_tier: row.bool("manual_tier")?,
            signed_up_branch: sql::opt_id(row, "signed_up_branch_id")?,
            signed_up_at: sql::ts(row, "signed_up_at")?,
        },
        phone_conflict: row.opt_text("phone_conflict")?,
        balance: sql::money(row, "balance")?,
        archived_at: sql::opt_ts(row, "archived_at")?,
        merged_into: sql::opt_id(row, "merged_into_id")?,
        erased_at: sql::opt_ts(row, "erased_at")?,
        created_at: sql::ts(row, "created_at")?,
        updated_at: sql::ts(row, "updated_at")?,
    })
}

fn profile_json(c: &Customer) -> Value {
    json!({
        "id": c.id.to_string(),
        "code": c.code,
        "name": c.name,
        "phone": c.phone,
        "email": c.email,
        "birth_month": c.birthday.map(|b| b.month),
        "birth_day": c.birthday.map(|b| b.day),
        "birth_year": c.birthday.and_then(|b| b.year),
        "address": c.address,
        "company_name": c.company_name,
        "tax_id": c.tax_id,
        "tax_address": c.tax_address,
        "notes": c.notes,
        "group_id": c.group.map(|g| g.to_string()),
        "tier_id": c.tier.map(|t| t.to_string()),
        "tier_valid_until": c.tier_valid_until,
        "manual_tier": c.manual_tier,
        "signed_up_branch_id": c.signed_up_branch.map(|b| b.to_string()),
        "signed_up_at": c.signed_up_at.unix_millis(),
    })
}

/// The outbox payload: the profile plus the stored state.
fn record_json(r: &CustomerRecord) -> Value {
    let mut value = profile_json(&r.customer);
    if let Value::Object(map) = &mut value {
        map.insert("phone_conflict".into(), json!(r.phone_conflict));
        map.insert("balance".into(), json!(r.balance.minor()));
        map.insert(
            "archived_at".into(),
            json!(r.archived_at.map(Timestamp::unix_millis)),
        );
        map.insert(
            "merged_into_id".into(),
            json!(r.merged_into.map(|m| m.to_string())),
        );
        map.insert(
            "erased_at".into(),
            json!(r.erased_at.map(Timestamp::unix_millis)),
        );
    }
    value
}

/// One customer, in any state.
///
/// # Errors
/// The database error.
pub async fn get_customer<'c>(
    conn: impl Into<Conn<'c>>,
    id: CustomerId,
) -> Result<Option<CustomerRecord>> {
    let sql = format!("{SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_record).await
}

async fn load(tx: &mut Tx, id: CustomerId) -> CustomerResult<CustomerRecord> {
    get_customer(Conn::Tx(&mut *tx), id)
        .await?
        .ok_or(CustomerDbError::NotFound("customer"))
}

/// Trims text, drops blank optional fields and checks the name and birthday.
fn normalized(c: &Customer) -> CustomerResult<Customer> {
    let code = c.code.trim().to_owned();
    if code.is_empty() {
        return Err(CustomerDbError::invalid("code", "is required"));
    }
    if let Some(b) = c.birthday
        && !b.is_valid()
    {
        return Err(CustomerDbError::invalid(
            "birthday",
            format!("{}-{} is not a day of the year", b.month, b.day),
        ));
    }
    Ok(Customer {
        id: c.id,
        code,
        name: clean_name(&c.name)?,
        phone: clean(c.phone.as_deref()),
        email: clean(c.email.as_deref()),
        birthday: c.birthday,
        address: clean(c.address.as_deref()),
        company_name: clean(c.company_name.as_deref()),
        tax_id: clean(c.tax_id.as_deref()),
        tax_address: clean(c.tax_address.as_deref()),
        notes: clean(c.notes.as_deref()),
        group: c.group,
        tier: c.tier,
        tier_valid_until: c.tier_valid_until,
        manual_tier: c.manual_tier,
        signed_up_branch: c.signed_up_branch,
        signed_up_at: c.signed_up_at,
    })
}

/// The non-archived customer other than `except` that holds `phone`.
async fn phone_holder(
    tx: &mut Tx,
    phone: &str,
    except: CustomerId,
) -> Result<Option<(CustomerId, String)>> {
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT id, code FROM customer WHERE phone = ? AND archived_at IS NULL AND id <> ?",
        args![phone, except],
        |row| Ok((sql::id(row, "id")?, row.text("code")?)),
    )
    .await
}

async fn check_phone_free(tx: &mut Tx, phone: &str, id: CustomerId) -> CustomerResult<()> {
    match phone_holder(tx, phone, id).await? {
        Some((customer, code)) => Err(CustomerDbError::PhoneTaken {
            customer,
            code,
            phone: phone.to_owned(),
        }),
        None => Ok(()),
    }
}

fn customer_search_key(c: &Customer) -> String {
    search_key(&format!(
        "{} {}",
        c.name,
        c.company_name.as_deref().unwrap_or("")
    ))
}

/// Writes the profile columns (insert or overwrite) and `phone_conflict`.
async fn write_profile(
    tx: &mut Tx,
    c: &Customer,
    phone_conflict: Option<&str>,
    now: Timestamp,
) -> CustomerResult<crate::Saved> {
    let opt = |v: Option<u32>| v.map(i64::from);
    Ok(sql::upsert_by_id(
        tx,
        "customer",
        c.id.into(),
        &[
            "code",
            "name",
            "phone",
            "phone_conflict",
            "email",
            "birth_month",
            "birth_day",
            "birth_year",
            "address",
            "company_name",
            "tax_id",
            "tax_address",
            "notes",
            "group_id",
            "tier_id",
            "tier_valid_until",
            "manual_tier",
            "signed_up_branch_id",
            "signed_up_at",
            "search_key",
        ],
        args![
            c.code.as_str(),
            c.name.as_str(),
            c.phone.as_deref(),
            phone_conflict,
            c.email.as_deref(),
            opt(c.birthday.map(|b| u32::from(b.month))),
            opt(c.birthday.map(|b| u32::from(b.day))),
            opt(c.birthday.and_then(|b| b.year).map(u32::from)),
            c.address.as_deref(),
            c.company_name.as_deref(),
            c.tax_id.as_deref(),
            c.tax_address.as_deref(),
            c.notes.as_deref(),
            c.group,
            c.tier,
            opt(c.tier_valid_until),
            c.manual_tier,
            c.signed_up_branch,
            c.signed_up_at,
            customer_search_key(c),
        ],
        now,
    )
    .await?)
}

/// What to do when the phone is held by another non-archived customer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PhoneClash {
    /// Refuse with [`CustomerDbError::PhoneTaken`].
    Reject,
    /// Keep the customer, move the phone to `phone_conflict` (spec 2.5 §7, sync at HQ).
    Park,
}

/// Creates or updates a customer (`PUT /customers/{id}`, spec 2.5 §7). An identical replay
/// changes nothing ([`Written::Unchanged`]). On an update the code, `signed_up_branch` and
/// `signed_up_at` keep their stored values. Audit and outbox `customer.put`.
///
/// Permission rules (which fields a cashier may change) are the caller's job.
///
/// # Errors
/// [`CustomerDbError::Invalid`] (name, birthday, empty code),
/// [`CustomerDbError::InvalidReference`] (unknown group or tier),
/// [`CustomerDbError::CodeTaken`] (draw a new code and retry), [`CustomerDbError::PhoneTaken`],
/// [`CustomerDbError::Archived`] / [`CustomerDbError::Merged`] (archived customers are not
/// edited), or the database error.
pub async fn put_customer(
    tx: &mut Tx,
    customer: &Customer,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    put(tx, customer, actor, now, PhoneClash::Reject).await
}

/// Like [`put_customer`], for a customer created at a branch and synced to HQ (spec 2.5 §7): a
/// phone held by another non-archived customer is moved to `phone_conflict` instead of being
/// refused, and the customer shows up in [`list_duplicates`].
///
/// # Errors
/// As [`put_customer`], except [`CustomerDbError::PhoneTaken`].
pub async fn put_synced_customer(
    tx: &mut Tx,
    customer: &Customer,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    put(tx, customer, actor, now, PhoneClash::Park).await
}

async fn put(
    tx: &mut Tx,
    customer: &Customer,
    actor: &Actor,
    now: Timestamp,
    clash: PhoneClash,
) -> CustomerResult<Written> {
    let mut new = normalized(customer)?;
    let old = get_customer(Conn::Tx(&mut *tx), new.id).await?;
    let mut phone_conflict = None;
    if let Some(old) = &old {
        if old.merged_into.is_some() {
            return Err(CustomerDbError::Merged);
        }
        if !old.is_active() {
            return Err(CustomerDbError::Archived);
        }
        new.code.clone_from(&old.customer.code);
        new.signed_up_branch = old.customer.signed_up_branch;
        new.signed_up_at = old.customer.signed_up_at;
        phone_conflict.clone_from(&old.phone_conflict);
        if old.customer == new {
            return Ok(Written::Unchanged);
        }
    } else {
        let taken = sql::fetch_optional(
            Conn::Tx(&mut *tx),
            "SELECT 1 FROM customer WHERE code = ?",
            args![new.code.as_str()],
            |_| Ok(()),
        )
        .await?;
        if taken.is_some() {
            return Err(CustomerDbError::CodeTaken { code: new.code });
        }
    }
    let phone_changed = old.as_ref().is_none_or(|o| o.customer.phone != new.phone);
    if let Some(phone) = new.phone.clone()
        && phone_changed
    {
        match clash {
            PhoneClash::Reject => check_phone_free(tx, &phone, new.id).await?,
            PhoneClash::Park => {
                if phone_holder(tx, &phone, new.id).await?.is_some() {
                    new.phone = None;
                    phone_conflict = Some(phone);
                    if let Some(old) = &old
                        && old.customer == new
                        && old.phone_conflict == phone_conflict
                    {
                        return Ok(Written::Unchanged);
                    }
                }
            }
        }
    }
    if let Some(group) = new.group
        && get_group(Conn::Tx(&mut *tx), group).await?.is_none()
    {
        return Err(CustomerDbError::InvalidReference("customer_group"));
    }
    if let Some(tier) = new.tier
        && get_tier(Conn::Tx(&mut *tx), tier).await?.is_none()
    {
        return Err(CustomerDbError::InvalidReference("membership_tier"));
    }
    let saved = write_profile(tx, &new, phone_conflict.as_deref(), now).await?;
    let stored = load(tx, new.id).await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "customer.put",
            target: format!("customer:{}", new.id),
            before: old.as_ref().map(|o| profile_json(&o.customer)),
            after: Some(profile_json(&stored.customer)),
            outbox_kind: "customer.put",
            payload: record_json(&stored),
        },
    )
    .await?;
    Ok(written(saved))
}

/// Archives a customer (spec 2.5 §7): hidden from lookup and from the phone check. Sales and
/// the balance stay. Audit `customer.archive`, outbox `customer.put`.
///
/// # Errors
/// [`CustomerDbError::NotFound`] (also for an erased customer) or the database error.
pub async fn archive_customer(
    tx: &mut Tx,
    id: CustomerId,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    set_archived(tx, id, Some(now), actor, now).await
}

/// Unarchives a customer (spec 2.5 §7).
///
/// # Errors
/// [`CustomerDbError::PhoneTaken`] when another customer took the phone meanwhile;
/// [`CustomerDbError::Merged`] for a merged customer; [`CustomerDbError::NotFound`] (also for an
/// erased customer), or the database error.
pub async fn unarchive_customer(
    tx: &mut Tx,
    id: CustomerId,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    set_archived(tx, id, None, actor, now).await
}

async fn set_archived(
    tx: &mut Tx,
    id: CustomerId,
    archived_at: Option<Timestamp>,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<Written> {
    let old = load(tx, id).await?;
    if old.erased_at.is_some() {
        return Err(CustomerDbError::NotFound("customer"));
    }
    if old.archived_at.is_some() == archived_at.is_some() {
        return Ok(Written::Unchanged);
    }
    if archived_at.is_none() {
        if old.merged_into.is_some() {
            return Err(CustomerDbError::Merged);
        }
        if let Some(phone) = &old.customer.phone {
            check_phone_free(tx, phone, id).await?;
        }
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE customer SET archived_at = ?, updated_at = ? WHERE id = ?",
        args![archived_at, now, id],
    )
    .await?;
    let stored = load(tx, id).await?;
    let action = if archived_at.is_some() {
        "customer.archive"
    } else {
        "customer.unarchive"
    };
    let state = |at: Option<Timestamp>| json!({ "archived_at": at.map(Timestamp::unix_millis) });
    record(
        tx,
        actor,
        now,
        Change {
            action,
            target: format!("customer:{id}"),
            before: Some(state(old.archived_at)),
            after: Some(state(stored.archived_at)),
            outbox_kind: "customer.put",
            payload: record_json(&stored),
        },
    )
    .await?;
    Ok(Written::Updated)
}

/// Erases a customer's personal data (spec 2.5 §7): the name becomes [`ERASED_NAME`]; phone,
/// phone conflict, email, birthday, address, company name, tax id, tax address and notes are
/// blanked; the record is archived and `erased_at` set. Code, group, tier, sales and the ledger
/// stay. Cannot be undone. The audit row `customer.erase` records that it happened and who did
/// it, but not the erased values; older audit rows are not touched. Outbox `customer.put` with the
/// blanked record.
///
/// # Errors
/// [`CustomerDbError::BalanceNotZero`]; [`CustomerDbError::NotFound`] for an unknown or already
/// erased customer; or the database error.
pub async fn erase_customer(
    tx: &mut Tx,
    id: CustomerId,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<CustomerRecord> {
    let old = load(tx, id).await?;
    if old.erased_at.is_some() {
        return Err(CustomerDbError::NotFound("customer"));
    }
    if old.balance != Money::ZERO {
        return Err(CustomerDbError::BalanceNotZero {
            balance: old.balance,
        });
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE customer SET name = ?, phone = NULL, phone_conflict = NULL, email = NULL, \
         birth_month = NULL, birth_day = NULL, birth_year = NULL, address = NULL, \
         company_name = NULL, tax_id = NULL, tax_address = NULL, notes = NULL, search_key = ?, \
         archived_at = COALESCE(archived_at, ?), erased_at = ?, updated_at = ? WHERE id = ?",
        args![ERASED_NAME, search_key(ERASED_NAME), now, now, now, id],
    )
    .await?;
    let stored = load(tx, id).await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "customer.erase",
            target: format!("customer:{id}"),
            before: None,
            after: Some(json!({ "erased": true })),
            outbox_kind: "customer.put",
            payload: record_json(&stored),
        },
    )
    .await?;
    Ok(stored)
}

/// Fills `target` from `source` when it is empty.
fn fill<T: Clone>(target: &mut Option<T>, source: Option<&T>) {
    if target.is_none() {
        *target = source.cloned();
    }
}

/// A's profile after merging B into it (spec 2.5 §8 field rule), and A's `phone_conflict`.
fn merged_profile(a: &CustomerRecord, b: &CustomerRecord) -> (Customer, Option<String>) {
    let mut merged = a.customer.clone();
    let src = &b.customer;
    fill(&mut merged.phone, src.phone.as_ref());
    fill(&mut merged.email, src.email.as_ref());
    fill(&mut merged.birthday, src.birthday.as_ref());
    fill(&mut merged.address, src.address.as_ref());
    fill(&mut merged.company_name, src.company_name.as_ref());
    fill(&mut merged.tax_id, src.tax_id.as_ref());
    fill(&mut merged.tax_address, src.tax_address.as_ref());
    fill(&mut merged.notes, src.notes.as_ref());
    fill(&mut merged.group, src.group.as_ref());
    if merged.tier.is_none() {
        merged.tier = src.tier;
        merged.tier_valid_until = src.tier_valid_until;
        merged.manual_tier = src.manual_tier;
    }
    let phone_conflict = a
        .phone_conflict
        .clone()
        .filter(|p| merged.phone.as_ref() != Some(p));
    (merged, phone_conflict)
}

/// Merges customer `from` (B) into `into` (A) (`POST /customers/{a}/merge`, spec 2.5 §8), in
/// `tx`.
///
/// The §8 field rule is applied here: A keeps its own fields and empty ones are filled from B
/// (phone, email, birthday, address, company name, tax id, tax address, notes); A keeps its
/// group, or takes B's when it has none, and likewise its tier (with B's valid-until date and
/// manual flag). A's `phone_conflict` is cleared when A ends up with that phone.
///
/// B is archived with `merged_into = A`; customers merged into B earlier now point at A, so the
/// lookup never follows a chain. A non-zero balance of B moves to A with a `merge_out` entry on
/// B and a `merge_in` entry on A (the result may exceed `max_balance`). These two entries add no
/// outbox rows: both customers' `customer.put` rows carry the new balances.
///
/// Audit `customer.merge` (target A) and `customer.merged` (target B), and one outbox
/// `customer.put` for each. Returns A as stored.
///
/// # Errors
/// [`CustomerDbError::SameCustomer`]; [`CustomerDbError::NotFound`];
/// [`CustomerDbError::Merged`] when either was merged already; [`CustomerDbError::Archived`]
/// when either is archived or erased; or the database error.
pub async fn merge_customers(
    tx: &mut Tx,
    into: CustomerId,
    from: CustomerId,
    actor: &Actor,
    now: Timestamp,
) -> CustomerResult<CustomerRecord> {
    if into == from {
        return Err(CustomerDbError::SameCustomer);
    }
    let a = load(tx, into).await?;
    let b = load(tx, from).await?;
    for c in [&a, &b] {
        if c.merged_into.is_some() {
            return Err(CustomerDbError::Merged);
        }
        if !c.is_active() {
            return Err(CustomerDbError::Archived);
        }
    }
    let (merged, phone_conflict) = merged_profile(&a, &b);
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE customer SET archived_at = ?, merged_into_id = ?, updated_at = ? WHERE id = ?",
        args![now, into, now, from],
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE customer SET merged_into_id = ?, updated_at = ? WHERE merged_into_id = ?",
        args![into, now, from],
    )
    .await?;
    if b.balance != Money::ZERO {
        let entry = |customer, kind, amount| BalanceEntry {
            branch: actor.branch,
            register: actor.register,
            user: actor.user,
            ..BalanceEntry::new(customer, kind, amount, now)
        };
        let moved = b.balance.minor();
        insert_entry(
            tx,
            &entry(from, BalanceEntryKind::MergeOut, Money::from_minor(-moved)),
        )
        .await?;
        insert_entry(
            tx,
            &entry(into, BalanceEntryKind::MergeIn, Money::from_minor(moved)),
        )
        .await?;
    }
    write_profile(tx, &merged, phone_conflict.as_deref(), now).await?;

    let stored_a = load(tx, into).await?;
    let stored_b = load(tx, from).await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "customer.merge",
            target: format!("customer:{into}"),
            before: Some(profile_json(&a.customer)),
            after: Some(json!({
                "customer": profile_json(&stored_a.customer),
                "merged_from": from.to_string(),
                "balance_moved": b.balance.minor(),
            })),
            outbox_kind: "customer.put",
            payload: record_json(&stored_a),
        },
    )
    .await?;
    record(
        tx,
        actor,
        now,
        Change {
            action: "customer.merged",
            target: format!("customer:{from}"),
            before: None,
            after: Some(json!({ "merged_into": into.to_string() })),
            outbox_kind: "customer.put",
            payload: record_json(&stored_b),
        },
    )
    .await?;
    Ok(stored_a)
}

/// Follows `merged_into` from `id` to a non-archived customer (at most a few hops; merges never
/// create chains). `None` when it ends at an archived or erased customer.
async fn follow(mut conn: Conn<'_>, id: CustomerId) -> Result<Option<CustomerRecord>> {
    let mut next = Some(id);
    for _ in 0..4 {
        let Some(id) = next else { break };
        let Some(c) = get_customer(conn.reborrow(), id).await? else {
            break;
        };
        if c.is_active() {
            return Ok(Some(c));
        }
        next = c.merged_into;
    }
    Ok(None)
}

/// The customer with this exact (normalized) phone at the till (spec 2.5 §6). A non-archived
/// holder wins; otherwise the phone of a merged customer finds the customer it was merged into.
/// Archived and erased customers are not found.
///
/// # Errors
/// The database error.
pub async fn lookup_by_phone<'c>(
    conn: impl Into<Conn<'c>>,
    phone: &str,
) -> Result<Option<CustomerRecord>> {
    let mut conn = conn.into();
    let sql = format!("{SELECT} WHERE phone = ? AND archived_at IS NULL");
    if let Some(c) = sql::fetch_optional(conn.reborrow(), &sql, args![phone], map_record).await? {
        return Ok(Some(c));
    }
    let merged = sql::fetch_optional(
        conn.reborrow(),
        "SELECT merged_into_id FROM customer WHERE phone = ? AND merged_into_id IS NOT NULL \
         ORDER BY archived_at DESC, id DESC LIMIT 1",
        args![phone],
        |row| sql::id::<CustomerId>(row, "merged_into_id"),
    )
    .await?;
    match merged {
        Some(id) => follow(conn, id).await,
        None => Ok(None),
    }
}

/// The customer with this member code (upper case, without the dash) at the till (spec 2.5 §6).
/// A merged customer's code finds the customer it was merged into; archived and erased customers
/// are not found.
///
/// # Errors
/// The database error.
pub async fn lookup_by_code<'c>(
    conn: impl Into<Conn<'c>>,
    code: &str,
) -> Result<Option<CustomerRecord>> {
    let mut conn = conn.into();
    let id = sql::fetch_optional(
        conn.reborrow(),
        "SELECT id FROM customer WHERE code = ?",
        args![code],
        |row| sql::id::<CustomerId>(row, "id"),
    )
    .await?;
    match id {
        Some(id) => follow(conn, id).await,
        None => Ok(None),
    }
}

/// Escapes `LIKE` wildcards with `!` (used with `ESCAPE '!'`).
fn escape_like(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(c);
    }
    out
}

fn page_args(values: &mut Vec<Arg>, offset: u32, limit: u32, default: u32, max: u32) {
    values.push(clamp_limit(limit, default, max).into());
    values.push(i64::from(offset).into());
}

/// Name search at the till (`GET /customers?q=`, spec 2.5 §6): non-archived customers whose
/// name or company name contains `q`, ignoring case and accents (`core::catalog::search_key`),
/// ordered by name. `limit` 0 or above [`MAX_SEARCH_PAGE`] means 20; `offset` skips rows. A blank
/// query finds nothing.
///
/// # Errors
/// The database error.
pub async fn search_customers<'c>(
    conn: impl Into<Conn<'c>>,
    q: &str,
    offset: u32,
    limit: u32,
) -> Result<Vec<CustomerRecord>> {
    let key = search_key(q);
    if key.is_empty() {
        return Ok(Vec::new());
    }
    let mut values = args![format!("%{}%", escape_like(&key))];
    page_args(&mut values, offset, limit, MAX_SEARCH_PAGE, MAX_SEARCH_PAGE);
    let sql = format!(
        "{SELECT} WHERE archived_at IS NULL AND search_key LIKE ? ESCAPE '!' \
         ORDER BY name, id LIMIT ? OFFSET ?"
    );
    sql::fetch_all(conn.into(), &sql, values, map_record).await
}

/// The back office's customer list (`GET /customers` without `q`, phase M3): customers ordered
/// by name, archived ones (merged and erased included) only with `include_archived`. `limit` 0
/// = 50, at most [`MAX_PAGE`]; `offset` skips rows.
///
/// # Errors
/// The database error.
pub async fn list_customers<'c>(
    conn: impl Into<Conn<'c>>,
    include_archived: bool,
    offset: u32,
    limit: u32,
) -> Result<Vec<CustomerRecord>> {
    let mut values = Vec::new();
    page_args(&mut values, offset, limit, 50, MAX_PAGE);
    let filter = if include_archived {
        ""
    } else {
        " WHERE archived_at IS NULL"
    };
    let sql = format!("{SELECT}{filter} ORDER BY name, id LIMIT ? OFFSET ?");
    sql::fetch_all(conn.into(), &sql, values, map_record).await
}

/// "Possible duplicates" (spec 2.5 §7): non-archived customers whose phone clashed at sync,
/// ordered by the clashing phone. `limit` 0 = 50, at most [`MAX_PAGE`].
///
/// # Errors
/// The database error.
pub async fn list_duplicates<'c>(
    conn: impl Into<Conn<'c>>,
    offset: u32,
    limit: u32,
) -> Result<Vec<CustomerRecord>> {
    let mut values = Vec::new();
    page_args(&mut values, offset, limit, 50, MAX_PAGE);
    let sql = format!(
        "{SELECT} WHERE phone_conflict IS NOT NULL AND archived_at IS NULL \
         ORDER BY phone_conflict, id LIMIT ? OFFSET ?"
    );
    sql::fetch_all(conn.into(), &sql, values, map_record).await
}

/// "Negative balances" (spec 2.5 §11): customers (archived ones included) whose balance is below
/// zero, most negative first. `limit` 0 = 50, at most [`MAX_PAGE`].
///
/// # Errors
/// The database error.
pub async fn list_negative_balances<'c>(
    conn: impl Into<Conn<'c>>,
    offset: u32,
    limit: u32,
) -> Result<Vec<CustomerRecord>> {
    let mut values = Vec::new();
    page_args(&mut values, offset, limit, 50, MAX_PAGE);
    let sql = format!("{SELECT} WHERE balance < 0 ORDER BY balance, id LIMIT ? OFFSET ?");
    sql::fetch_all(conn.into(), &sql, values, map_record).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn birthdays() {
        let b = |month, day| Birthday {
            month,
            day,
            year: None,
        };
        assert!(b(2, 29).is_valid());
        assert!(!b(2, 30).is_valid());
        assert!(b(4, 30).is_valid());
        assert!(!b(4, 31).is_valid());
        assert!(b(12, 31).is_valid());
        assert!(!b(13, 1).is_valid());
        assert!(!b(0, 1).is_valid());
        assert!(!b(1, 0).is_valid());
    }

    #[test]
    fn like_escaping() {
        assert_eq!(escape_like("a%b_c!d"), "a!%b!_c!!d");
    }

    #[test]
    fn fill_only_empty() {
        let mut a = Some(1);
        fill(&mut a, Some(&2));
        assert_eq!(a, Some(1));
        let mut none: Option<i32> = None;
        fill(&mut none, Some(&2));
        assert_eq!(none, Some(2));
        let mut still: Option<i32> = None;
        fill(&mut still, None);
        assert_eq!(still, None);
    }
}
