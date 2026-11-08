//! Organization structure: company, branches, registers, stock locations (spec 2.1 §1).
//!
//! This is HQ-owned master data (spec 2.1 §1): every save appends an outbox event in the same
//! transaction. "Delete" is archiving (`docs/plan.md` §1: soft delete for master data), done by
//! saving with `archived: true`.

use serde_json::{Value, json};
use sultan_core::ids::{BranchId, CompanyId, LocationId, RegisterId};
use sultan_core::time::Timestamp;

use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Row, args};
use crate::{Saved, outbox};

/// The company (exactly one per install for now, spec 2.1 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Company {
    /// ID.
    pub id: CompanyId,
    /// Legal or trading name.
    pub name: String,
}

/// A branch (store), spec 2.1 §1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Branch {
    /// ID.
    pub id: BranchId,
    /// Owning company.
    pub company: CompanyId,
    /// Short code used in document numbers, e.g. `JKT01`. Unique.
    pub code: String,
    /// Display name.
    pub name: String,
    /// Address (printed on receipts).
    pub address: String,
    /// Tax number (NPWP), if registered.
    pub npwp: Option<String>,
    /// Receipt header text.
    pub receipt_header: String,
    /// Receipt footer text.
    pub receipt_footer: String,
    /// IANA time zone of document dates, e.g. `Asia/Jakarta` (validated by the caller with
    /// `sultan_core::doc_number::BranchTimeZone`).
    pub timezone: String,
    /// Archived (soft-deleted).
    pub archived: bool,
}

/// A register (till), spec 2.1 §1–2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Register {
    /// ID.
    pub id: RegisterId,
    /// Branch it belongs to.
    pub branch: BranchId,
    /// Short code used in document numbers, e.g. `R2`. Unique per branch.
    pub code: String,
    /// Display name.
    pub name: String,
    /// Stock location sales are taken from.
    pub default_location: Option<LocationId>,
    /// Archived (soft-deleted). An archived register's device tokens stop working.
    pub archived: bool,
}

/// A stock location within a branch, e.g. store floor or warehouse (spec 2.1 §1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockLocation {
    /// ID.
    pub id: LocationId,
    /// Branch it belongs to.
    pub branch: BranchId,
    /// Display name. Unique per branch.
    pub name: String,
    /// The branch's default receiving location.
    pub receiving_default: bool,
    /// Archived (soft-deleted).
    pub archived: bool,
}

/// Creates or overwrites the company, with an outbox event `company.saved`.
///
/// # Errors
/// The database error.
pub async fn save_company(tx: &mut Tx, company: &Company, now: Timestamp) -> Result<Saved> {
    let saved = sql::upsert_by_id(
        tx,
        "company",
        company.id.into(),
        &["name"],
        args![company.name.as_str()],
        now,
    )
    .await?;
    let payload = json!({ "id": company.id.to_string(), "name": company.name });
    outbox::append(tx, "company.saved", &payload, now).await?;
    Ok(saved)
}

/// The company, if set up.
///
/// # Errors
/// The database error.
pub async fn get_company<'c>(conn: impl Into<Conn<'c>>) -> Result<Option<Company>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT id, name FROM company ORDER BY created_at LIMIT 1",
        args![],
        |row| {
            Ok(Company {
                id: sql::id(row, "id")?,
                name: row.text("name")?,
            })
        },
    )
    .await
}

const BRANCH_COLUMNS: [&str; 9] = [
    "company_id",
    "code",
    "name",
    "address",
    "npwp",
    "receipt_header",
    "receipt_footer",
    "timezone",
    "archived",
];
const BRANCH_SELECT: &str = "SELECT id, company_id, code, name, address, npwp, receipt_header, \
                             receipt_footer, timezone, archived FROM branch";

/// Creates or overwrites a branch (`PUT /branches/{id}`), with an outbox event `branch.saved`.
///
/// # Errors
/// [`DbError::Conflict`](crate::DbError::Conflict) for a duplicate code,
/// [`DbError::MissingReference`](crate::DbError::MissingReference) for an unknown company.
pub async fn save_branch(tx: &mut Tx, branch: &Branch, now: Timestamp) -> Result<Saved> {
    let saved = sql::upsert_by_id(
        tx,
        "branch",
        branch.id.into(),
        &BRANCH_COLUMNS,
        args![
            branch.company,
            branch.code.as_str(),
            branch.name.as_str(),
            branch.address.as_str(),
            branch.npwp.as_deref(),
            branch.receipt_header.as_str(),
            branch.receipt_footer.as_str(),
            branch.timezone.as_str(),
            branch.archived,
        ],
        now,
    )
    .await?;
    outbox::append(tx, "branch.saved", &branch_json(branch), now).await?;
    Ok(saved)
}

/// One branch.
///
/// # Errors
/// The database error.
pub async fn get_branch<'c>(conn: impl Into<Conn<'c>>, id: BranchId) -> Result<Option<Branch>> {
    let sql = format!("{BRANCH_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_branch).await
}

/// One branch by code.
///
/// # Errors
/// The database error.
pub async fn find_branch_by_code<'c>(
    conn: impl Into<Conn<'c>>,
    code: &str,
) -> Result<Option<Branch>> {
    let sql = format!("{BRANCH_SELECT} WHERE code = ?");
    sql::fetch_optional(conn.into(), &sql, args![code], map_branch).await
}

/// All branches, including archived ones, ordered by code.
///
/// # Errors
/// The database error.
pub async fn list_branches<'c>(conn: impl Into<Conn<'c>>) -> Result<Vec<Branch>> {
    let sql = format!("{BRANCH_SELECT} ORDER BY code");
    sql::fetch_all(conn.into(), &sql, args![], map_branch).await
}

fn map_branch(row: &dyn Row) -> Result<Branch> {
    Ok(Branch {
        id: sql::id(row, "id")?,
        company: sql::id(row, "company_id")?,
        code: row.text("code")?,
        name: row.text("name")?,
        address: row.text("address")?,
        npwp: row.opt_text("npwp")?,
        receipt_header: row.text("receipt_header")?,
        receipt_footer: row.text("receipt_footer")?,
        timezone: row.text("timezone")?,
        archived: row.bool("archived")?,
    })
}

fn branch_json(b: &Branch) -> Value {
    json!({
        "id": b.id.to_string(),
        "company_id": b.company.to_string(),
        "code": b.code,
        "name": b.name,
        "address": b.address,
        "npwp": b.npwp,
        "receipt_header": b.receipt_header,
        "receipt_footer": b.receipt_footer,
        "timezone": b.timezone,
        "archived": b.archived,
    })
}

const REGISTER_SELECT: &str =
    "SELECT id, branch_id, code, name, default_location_id, archived FROM register";

/// Creates or overwrites a register (`PUT /registers/{id}`), with an outbox event
/// `register.saved`.
///
/// # Errors
/// [`DbError::Conflict`](crate::DbError::Conflict) for a duplicate code in the branch,
/// [`DbError::MissingReference`](crate::DbError::MissingReference) for an unknown branch or
/// location.
pub async fn save_register(tx: &mut Tx, register: &Register, now: Timestamp) -> Result<Saved> {
    let saved = sql::upsert_by_id(
        tx,
        "register",
        register.id.into(),
        &[
            "branch_id",
            "code",
            "name",
            "default_location_id",
            "archived",
        ],
        args![
            register.branch,
            register.code.as_str(),
            register.name.as_str(),
            register.default_location,
            register.archived,
        ],
        now,
    )
    .await?;
    let payload = json!({
        "id": register.id.to_string(),
        "branch_id": register.branch.to_string(),
        "code": register.code,
        "name": register.name,
        "default_location_id": register.default_location.map(|l| l.to_string()),
        "archived": register.archived,
    });
    outbox::append(tx, "register.saved", &payload, now).await?;
    Ok(saved)
}

/// One register.
///
/// # Errors
/// The database error.
pub async fn get_register<'c>(
    conn: impl Into<Conn<'c>>,
    id: RegisterId,
) -> Result<Option<Register>> {
    let sql = format!("{REGISTER_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_register).await
}

/// The registers of a branch (or of all branches), including archived ones, ordered by code.
///
/// # Errors
/// The database error.
pub async fn list_registers<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
) -> Result<Vec<Register>> {
    let (condition, values) = branch.map_or(("", args![]), |b| ("WHERE branch_id = ?", args![b]));
    let sql = format!("{REGISTER_SELECT} {condition} ORDER BY code");
    sql::fetch_all(conn.into(), &sql, values, map_register).await
}

fn map_register(row: &dyn Row) -> Result<Register> {
    Ok(Register {
        id: sql::id(row, "id")?,
        branch: sql::id(row, "branch_id")?,
        code: row.text("code")?,
        name: row.text("name")?,
        default_location: sql::opt_id(row, "default_location_id")?,
        archived: row.bool("archived")?,
    })
}

const LOCATION_SELECT: &str =
    "SELECT id, branch_id, name, receiving_default, archived FROM stock_location";

/// Creates or overwrites a stock location (`PUT /locations/{id}`), with an outbox event
/// `location.saved`.
///
/// # Errors
/// [`DbError::Conflict`](crate::DbError::Conflict) for a duplicate name in the branch,
/// [`DbError::MissingReference`](crate::DbError::MissingReference) for an unknown branch.
pub async fn save_location(tx: &mut Tx, location: &StockLocation, now: Timestamp) -> Result<Saved> {
    let saved = sql::upsert_by_id(
        tx,
        "stock_location",
        location.id.into(),
        &["branch_id", "name", "receiving_default", "archived"],
        args![
            location.branch,
            location.name.as_str(),
            location.receiving_default,
            location.archived,
        ],
        now,
    )
    .await?;
    let payload = json!({
        "id": location.id.to_string(),
        "branch_id": location.branch.to_string(),
        "name": location.name,
        "receiving_default": location.receiving_default,
        "archived": location.archived,
    });
    outbox::append(tx, "location.saved", &payload, now).await?;
    Ok(saved)
}

/// One stock location.
///
/// # Errors
/// The database error.
pub async fn get_location<'c>(
    conn: impl Into<Conn<'c>>,
    id: LocationId,
) -> Result<Option<StockLocation>> {
    let sql = format!("{LOCATION_SELECT} WHERE id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_location).await
}

/// The stock locations of a branch (or of all branches), including archived ones, ordered by
/// name.
///
/// # Errors
/// The database error.
pub async fn list_locations<'c>(
    conn: impl Into<Conn<'c>>,
    branch: Option<BranchId>,
) -> Result<Vec<StockLocation>> {
    let (condition, values) = branch.map_or(("", args![]), |b| ("WHERE branch_id = ?", args![b]));
    let sql = format!("{LOCATION_SELECT} {condition} ORDER BY name");
    sql::fetch_all(conn.into(), &sql, values, map_location).await
}

fn map_location(row: &dyn Row) -> Result<StockLocation> {
    Ok(StockLocation {
        id: sql::id(row, "id")?,
        branch: sql::id(row, "branch_id")?,
        name: row.text("name")?,
        receiving_default: row.bool("receiving_default")?,
        archived: row.bool("archived")?,
    })
}
