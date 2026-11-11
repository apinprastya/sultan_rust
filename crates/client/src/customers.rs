//! Customers as the till and the back office see them (spec 2.5, phase M3; the JSON of
//! `crates/server/src/routes/{customers,customer_groups,customer_balance}.rs`).
//!
//! Money is minor units (`i64`), times are unix ms, dates are `YYYY-MM-DD`, IDs are UUID strings.
//! [`Customer`] is the answer of every customer call; [`CustomerBody`] is what `PUT
//! /customers/{id}` takes ([`Customer::body`] turns an answer back into a body for an edit).

use serde::{Deserialize, Serialize};

/// `customer.create`: sign up customers.
pub const CUSTOMER_CREATE: &str = "customer.create";
/// `customer.search`: name search at the till (and the back-office list).
pub const CUSTOMER_SEARCH: &str = "customer.search";
/// `customer.edit`: edit the profile.
pub const CUSTOMER_EDIT: &str = "customer.edit";
/// `customer.manage`: groups, tiers, archive, merge, review lists, balance settings.
pub const CUSTOMER_MANAGE: &str = "customer.manage";
/// `customer.erase`: erase personal data.
pub const CUSTOMER_ERASE: &str = "customer.erase";
/// `customer.balance_payout`: pay a balance out in cash at the till.
pub const CUSTOMER_BALANCE_PAYOUT: &str = "customer.balance_payout";

/// A customer (`GET /customers/{id}`, lookup, search, every customer write).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Customer {
    /// ID.
    pub id: String,
    /// Member code as stored, e.g. `K7QH29XM`.
    pub code: String,
    /// The code as shown and printed, e.g. `K7QH-29XM`.
    pub display_code: String,
    /// Name (`Erased customer` after an erase).
    pub name: String,
    /// Phone, `+62…`.
    pub phone: Option<String>,
    /// The phone as the receipt shows it, e.g. `0812****789`.
    pub masked_phone: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Birthday month.
    pub birth_month: Option<u8>,
    /// Birthday day.
    pub birth_day: Option<u8>,
    /// Birth year.
    pub birth_year: Option<u16>,
    /// Address.
    pub address: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// NPWP or NIK.
    pub tax_id: Option<String>,
    /// Tax address.
    pub tax_address: Option<String>,
    /// Staff notes.
    pub notes: Option<String>,
    /// Group.
    pub group_id: Option<String>,
    /// The group's name.
    pub group_name: Option<String>,
    /// Membership tier.
    pub tier_id: Option<String>,
    /// The tier's name.
    pub tier_name: Option<String>,
    /// Last day the tier applies, `YYYY-MM-DD`.
    pub tier_valid_until: Option<String>,
    /// Whether the tier applies today; `None` without a tier. `Some(false)`: show
    /// "Gold (expired …)".
    pub tier_applies: Option<bool>,
    /// Manual tier.
    pub manual_tier: bool,
    /// Branch of the sign-up.
    pub signed_up_branch_id: Option<String>,
    /// Sign-up time.
    pub signed_up_at: i64,
    /// A phone that clashed at sync ("Possible duplicates").
    pub phone_conflict: Option<String>,
    /// Customer balance (saved change).
    pub balance: i64,
    /// Archived (also after a merge or an erase).
    pub archived: bool,
    /// When it was archived.
    pub archived_at: Option<i64>,
    /// The customer it was merged into.
    pub merged_into_id: Option<String>,
    /// When its personal data was erased.
    pub erased_at: Option<i64>,
    /// Created.
    pub created_at: i64,
    /// Last changed.
    pub updated_at: i64,
}

impl Customer {
    /// The body that writes this customer back unchanged (start of an edit).
    #[must_use]
    pub fn body(&self) -> CustomerBody {
        CustomerBody {
            code: Some(self.code.clone()).filter(|c| !c.is_empty()),
            name: self.name.clone(),
            phone: self.phone.clone(),
            email: self.email.clone(),
            birth_month: self.birth_month.map(u32::from),
            birth_day: self.birth_day.map(u32::from),
            birth_year: self.birth_year.map(i32::from),
            address: self.address.clone(),
            company_name: self.company_name.clone(),
            tax_id: self.tax_id.clone(),
            tax_address: self.tax_address.clone(),
            notes: self.notes.clone(),
            group_id: self.group_id.clone(),
            tier_id: self.tier_id.clone(),
            tier_valid_until: self.tier_valid_until.clone(),
            manual_tier: self.manual_tier,
        }
    }
}

/// Body of `PUT /customers/{id}` (sign-up and edit). Blank text means "none".
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CustomerBody {
    /// Member code; optional on create (the server draws one), ignored on update.
    pub code: Option<String>,
    /// Name, 1–100 characters.
    pub name: String,
    /// Phone in any accepted form (`0812-3456-789`).
    pub phone: Option<String>,
    /// Email.
    pub email: Option<String>,
    /// Birthday month (with `birth_day`).
    pub birth_month: Option<u32>,
    /// Birthday day.
    pub birth_day: Option<u32>,
    /// Birth year.
    pub birth_year: Option<i32>,
    /// Address.
    pub address: Option<String>,
    /// Company name.
    pub company_name: Option<String>,
    /// NPWP or NIK, 15 or 16 digits.
    pub tax_id: Option<String>,
    /// Tax address.
    pub tax_address: Option<String>,
    /// Staff notes.
    pub notes: Option<String>,
    /// Group (`customer.manage`).
    pub group_id: Option<String>,
    /// Tier (`customer.manage`).
    pub tier_id: Option<String>,
    /// Last day of the tier, `YYYY-MM-DD` (`customer.manage`).
    pub tier_valid_until: Option<String>,
    /// Manual tier (`customer.manage`).
    pub manual_tier: bool,
}

impl CustomerBody {
    /// A sign-up with a name and an optional phone (what the till asks for).
    #[must_use]
    pub fn sign_up(name: &str, phone: Option<&str>) -> Self {
        Self {
            name: name.to_owned(),
            phone: phone.map(str::to_owned),
            ..Self::default()
        }
    }
}

/// Query of `GET /customers`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerQuery {
    /// Name or company name to find; empty lists every customer by name.
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub q: String,
    /// Page from 1.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub page: Option<u32>,
    /// Page size 1–20.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub limit: Option<u32>,
    /// Without `q`: include archived customers (`customer.manage`).
    #[serde(skip_serializing_if = "std::ops::Not::not", default)]
    pub include_archived: bool,
}

impl CustomerQuery {
    /// A name search, first page.
    #[must_use]
    pub fn search(q: &str) -> Self {
        Self {
            q: q.to_owned(),
            ..Self::default()
        }
    }
}

/// A page of customers.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerPage {
    /// The customers.
    pub items: Vec<Customer>,
    /// The next page's number; `None` at the end.
    #[serde(default)]
    pub next_page: Option<u32>,
}

/// A customer group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerGroup {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// Body of `PUT /customer-groups/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupBody {
    /// Name.
    pub name: String,
    /// `true` archives.
    #[serde(default)]
    pub archived: bool,
}

/// A membership tier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MembershipTier {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// Rank, higher = better.
    pub rank: i64,
    /// Archived.
    #[serde(default)]
    pub archived: bool,
}

/// Body of `PUT /membership-tiers/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TierBody {
    /// Name.
    pub name: String,
    /// Rank, unique.
    pub rank: i64,
    /// `true` archives.
    #[serde(default)]
    pub archived: bool,
}

/// One balance ledger entry.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BalanceEntry {
    /// ID.
    pub id: String,
    /// The customer.
    pub customer_id: String,
    /// `change_saved`, `payment`, `change_saved_reversed`, `payment_reversed`, `payout`,
    /// `merge_out` or `merge_in`.
    pub kind: String,
    /// Signed amount.
    pub amount: i64,
    /// Branch.
    pub branch_id: Option<String>,
    /// Register.
    pub register_id: Option<String>,
    /// User.
    pub user_id: Option<String>,
    /// Cashier session.
    pub cashier_session_id: Option<String>,
    /// Document.
    pub document_id: Option<String>,
    /// When.
    pub occurred_at: i64,
}

/// `GET /customers/{id}/balance`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomerBalance {
    /// The customer.
    pub customer_id: String,
    /// The balance.
    pub balance: i64,
    /// Entries, newest first.
    pub entries: Vec<BalanceEntry>,
    /// Cursor of the next page (`before_id`); `None` at the end.
    #[serde(default)]
    pub next_before_id: Option<String>,
}

/// The answer of `POST /customers/{id}/balance/payout`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payout {
    /// The `payout` entry (amount −paid).
    pub entry: BalanceEntry,
    /// The balance after it.
    pub balance: i64,
}

/// `GET/PUT /settings/customer-balance`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BalanceSettings {
    /// Most change saved on one sale (0 turns saving off).
    pub max_saved_per_sale: i64,
    /// Most balance a customer may reach through saved change.
    pub max_balance: i64,
}

impl Default for BalanceSettings {
    /// The server's defaults: Rp 5 000 per sale, Rp 100 000 per customer.
    fn default() -> Self {
        Self {
            max_saved_per_sale: 5_000,
            max_balance: 100_000,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn body_round_trip_and_queries() {
        let c = Customer {
            code: "K7QH29XM".into(),
            name: "Budi".into(),
            birth_month: Some(2),
            birth_day: Some(29),
            birth_year: Some(1990),
            tier_id: Some("t".into()),
            manual_tier: true,
            ..Customer::default()
        };
        let b = c.body();
        assert_eq!(b.code.as_deref(), Some("K7QH29XM"));
        assert_eq!(
            (b.birth_month, b.birth_day, b.birth_year),
            (Some(2), Some(29), Some(1990))
        );
        assert!(b.manual_tier);
        assert_eq!(Customer::default().body().code, None);
        assert_eq!(
            CustomerBody::sign_up("A", Some("1")).phone.as_deref(),
            Some("1")
        );
        let q = serde_json::to_value(CustomerQuery::search("budi")).unwrap();
        assert_eq!(q, serde_json::json!({ "q": "budi" }));
        let all = CustomerQuery {
            page: Some(2),
            limit: Some(5),
            include_archived: true,
            ..CustomerQuery::default()
        };
        assert_eq!(
            serde_json::to_value(all).unwrap(),
            serde_json::json!({ "page": 2, "limit": 5, "include_archived": true })
        );
        assert_eq!(BalanceSettings::default().max_balance, 100_000);
    }
}
