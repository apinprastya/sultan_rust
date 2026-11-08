//! The append-only audit log (spec 2.1 §7).
//!
//! This module only appends and reads; the schema also rejects `UPDATE` and `DELETE` with
//! triggers. The server appends an entry in the same transaction as the change it records.

use serde_json::Value;
use sultan_core::ids::{BranchId, RegisterId, UserId};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::conn::Conn;
use crate::error::Result;
use crate::sql::{self, Arg, Row, args};

/// One audit entry: time, branch, register, user, action, target, before/after (spec 2.1 §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// `UUIDv7`; also the pagination cursor.
    pub id: Uuid,
    /// When it happened.
    pub at: Timestamp,
    /// Branch where it happened, if any.
    pub branch: Option<BranchId>,
    /// Register where it happened, if any.
    pub register: Option<RegisterId>,
    /// Acting user, if any (e.g. `None` for a failed login of an unknown name).
    pub user: Option<UserId>,
    /// What happened, e.g. `role.save`, `override.approve`, `login.locked_out`.
    pub action: String,
    /// What it happened to, e.g. `role:<uuid>`.
    pub target: Option<String>,
    /// State before the change.
    pub before: Option<Value>,
    /// State after the change.
    pub after: Option<Value>,
}

impl AuditEntry {
    /// A new entry with a fresh ID; set the optional fields directly.
    #[must_use]
    pub fn new(at: Timestamp, action: impl Into<String>) -> Self {
        Self {
            id: Uuid::now_v7(),
            at,
            branch: None,
            register: None,
            user: None,
            action: action.into(),
            target: None,
            before: None,
            after: None,
        }
    }
}

/// Filters for [`query`]. All set fields must match. Results are newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AuditFilter {
    /// Only this branch.
    pub branch: Option<BranchId>,
    /// Only this register.
    pub register: Option<RegisterId>,
    /// Only this acting user.
    pub user: Option<UserId>,
    /// Only this action.
    pub action: Option<String>,
    /// Only this target.
    pub target: Option<String>,
    /// At or after this time.
    pub from: Option<Timestamp>,
    /// Before this time.
    pub until: Option<Timestamp>,
    /// Page cursor: only entries older than this ID (the last ID of the previous page).
    pub before_id: Option<Uuid>,
    /// Page size; `0` means 100.
    pub limit: u32,
}

/// Appends an entry. There is deliberately no update or delete.
///
/// # Errors
/// The database error.
pub async fn append<'c>(conn: impl Into<Conn<'c>>, entry: &AuditEntry) -> Result<()> {
    sql::execute(
        conn.into(),
        "INSERT INTO audit_log (id, occurred_at, branch_id, register_id, user_id, action, target, \
         before_json, after_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            entry.id,
            entry.at,
            entry.branch,
            entry.register,
            entry.user,
            entry.action.as_str(),
            entry.target.as_deref(),
            entry.before.as_ref().map(Value::to_string).as_deref(),
            entry.after.as_ref().map(Value::to_string).as_deref(),
        ],
    )
    .await?;
    Ok(())
}

/// Reads entries matching `filter`, newest first (spec 2.1 §8 `/audit`).
///
/// # Errors
/// The database error.
pub async fn query<'c>(conn: impl Into<Conn<'c>>, filter: &AuditFilter) -> Result<Vec<AuditEntry>> {
    let mut conditions: Vec<&str> = Vec::new();
    let mut values: Vec<Arg> = Vec::new();
    let mut add = |condition, value: Arg| {
        conditions.push(condition);
        values.push(value);
    };
    if let Some(b) = filter.branch {
        add("branch_id = ?", b.into());
    }
    if let Some(r) = filter.register {
        add("register_id = ?", r.into());
    }
    if let Some(u) = filter.user {
        add("user_id = ?", u.into());
    }
    if let Some(a) = &filter.action {
        add("action = ?", a.as_str().into());
    }
    if let Some(t) = &filter.target {
        add("target = ?", t.as_str().into());
    }
    if let Some(from) = filter.from {
        add("occurred_at >= ?", from.into());
    }
    if let Some(until) = filter.until {
        add("occurred_at < ?", until.into());
    }
    if let Some(id) = filter.before_id {
        add("id < ?", id.into());
    }
    let where_clause = if conditions.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", conditions.join(" AND "))
    };
    let limit = if filter.limit == 0 { 100 } else { filter.limit };
    values.push(i64::from(limit).into());
    sql::fetch_all(
        conn.into(),
        &format!(
            "SELECT id, occurred_at, branch_id, register_id, user_id, action, target, before_json, \
             after_json FROM audit_log {where_clause} ORDER BY id DESC LIMIT ?"
        ),
        values,
        map_entry,
    )
    .await
}

fn map_entry(row: &dyn Row) -> Result<AuditEntry> {
    Ok(AuditEntry {
        id: sql::id(row, "id")?,
        at: sql::ts(row, "occurred_at")?,
        branch: sql::opt_id(row, "branch_id")?,
        register: sql::opt_id(row, "register_id")?,
        user: sql::opt_id(row, "user_id")?,
        action: row.text("action")?,
        target: row.opt_text("target")?,
        before: sql::opt_json(row, "before_json")?,
        after: sql::opt_json(row, "after_json")?,
    })
}
