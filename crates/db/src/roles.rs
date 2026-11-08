//! Roles, grants and assignments (spec 2.1 §3), mapped to and from `core::access`.
//!
//! Roles and assignments are HQ-owned master data: every write appends an outbox event in the same
//! transaction. Access rules (effective permissions, the Owner invariant) stay in `core`; this
//! module only loads their inputs.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sultan_core::access::{Assignment, BranchScope, Grant, Limit, LimitKind, Permission, Role};
use sultan_core::ids::{RoleId, UserId};
use sultan_core::money::Money;
use sultan_core::time::Timestamp;

use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Arg, Row, args};
use crate::{Saved, outbox};

/// Creates or overwrites a role and replaces its grants (`PUT /roles/{id}`), with an outbox event
/// `role.saved` carrying the grants. Saving an archived role restores it. Check the Owner invariant
/// with `core` before changing the Owner role.
///
/// # Errors
/// [`DbError::Conflict`] for a duplicate role name.
pub async fn save_role(tx: &mut Tx, role: &Role, now: Timestamp) -> Result<Saved> {
    let saved = write_role(tx, role, now).await?;
    outbox::append(tx, "role.saved", &role_json(role), now).await?;
    Ok(saved)
}

/// Writes the role row and its grants without an outbox event (used by the seed, which runs
/// identically on every node).
pub(crate) async fn write_role(tx: &mut Tx, role: &Role, now: Timestamp) -> Result<Saved> {
    let saved = sql::upsert_by_id(
        tx,
        "role",
        role.id.into(),
        &["name", "archived"],
        args![role.name.as_str(), false],
        now,
    )
    .await?;
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM role_permission WHERE role_id = ?",
        args![role.id],
    )
    .await?;
    for grant in &role.grants {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO role_permission (role_id, perm, limit_value) VALUES (?, ?, ?)",
            args![
                role.id,
                grant.permission().as_str(),
                limit_value(grant.limit())
            ],
        )
        .await?;
    }
    Ok(saved)
}

/// Archives (soft-deletes) a role, with an outbox event `role.archived`. Archived roles no longer
/// grant anything, but their assignments are kept for history. Check the Owner invariant with
/// `core` first: the Owner role must never be archived (spec 2.1 §3).
///
/// # Errors
/// [`DbError::NotFound`] if the role does not exist.
pub async fn archive_role(tx: &mut Tx, id: RoleId, now: Timestamp) -> Result<()> {
    let changed = sql::execute(
        Conn::Tx(&mut *tx),
        "UPDATE role SET archived = ?, updated_at = ? WHERE id = ?",
        args![true, now, id],
    )
    .await?;
    if changed == 0 {
        return Err(DbError::NotFound("role"));
    }
    outbox::append(tx, "role.archived", &json!({ "id": id.to_string() }), now).await?;
    Ok(())
}

/// One role with its grants; `None` if missing or archived.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for an unknown permission or invalid limit.
pub async fn get_role<'c>(conn: impl Into<Conn<'c>>, id: RoleId) -> Result<Option<Role>> {
    Ok(load_roles(conn.into(), "r.id = ?", args![id]).await?.pop())
}

/// All non-archived roles with their grants, ordered by name.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for an unknown permission or invalid limit.
pub async fn list_roles<'c>(conn: impl Into<Conn<'c>>) -> Result<Vec<Role>> {
    load_roles(conn.into(), "1 = 1", args![]).await
}

/// Replaces all assignments of `user` (spec 2.1 §3: user, role, branch | all branches), with an
/// outbox event `user.assignments_saved`. Duplicates are dropped. Check the Owner invariant with
/// `core` first.
///
/// # Errors
/// [`DbError::MissingReference`] for an unknown user, role or branch.
pub async fn replace_assignments(
    tx: &mut Tx,
    user: UserId,
    assignments: &[(RoleId, BranchScope)],
    now: Timestamp,
) -> Result<()> {
    let mut unique: Vec<(RoleId, BranchScope)> = Vec::with_capacity(assignments.len());
    for a in assignments {
        if !unique.contains(a) {
            unique.push(*a);
        }
    }
    sql::execute(
        Conn::Tx(&mut *tx),
        "DELETE FROM user_role WHERE user_id = ?",
        args![user],
    )
    .await?;
    for (role, scope) in &unique {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO user_role (user_id, role_id, branch_id) VALUES (?, ?, ?)",
            args![user, *role, scope_branch(*scope)],
        )
        .await?;
    }
    let items: Vec<Value> = unique
        .iter()
        .map(|(role, scope)| {
            json!({
                "role_id": role.to_string(),
                "branch_id": scope_branch(*scope).map(|b| b.to_string()),
            })
        })
        .collect();
    let payload = json!({ "user_id": user.to_string(), "assignments": items });
    outbox::append(tx, "user.assignments_saved", &payload, now).await?;
    Ok(())
}

/// The assignments of one user, including those of archived roles.
///
/// # Errors
/// The database error.
pub async fn assignments_for_user<'c>(
    conn: impl Into<Conn<'c>>,
    user: UserId,
) -> Result<Vec<Assignment>> {
    sql::fetch_all(
        conn.into(),
        "SELECT user_id, role_id, branch_id FROM user_role WHERE user_id = ?",
        args![user],
        map_assignment,
    )
    .await
}

/// Every assignment: input to `core::access::ensure_active_owner_remains`.
///
/// # Errors
/// The database error.
pub async fn list_assignments<'c>(conn: impl Into<Conn<'c>>) -> Result<Vec<Assignment>> {
    sql::fetch_all(
        conn.into(),
        "SELECT user_id, role_id, branch_id FROM user_role",
        args![],
        map_assignment,
    )
    .await
}

/// Everything `core::access::effective_permissions` needs for one user (spec 2.1 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserAccess {
    /// The user's assignments.
    pub assignments: Vec<Assignment>,
    /// The non-archived roles those assignments refer to, with grants.
    pub roles: Vec<Role>,
}

/// Loads a user's assignments and their roles. Use as
/// `effective_permissions(&a.assignments, &a.roles, branch)`.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for an unknown permission or invalid limit.
pub async fn load_user_access<'c>(conn: impl Into<Conn<'c>>, user: UserId) -> Result<UserAccess> {
    let mut conn = conn.into();
    let assignments = assignments_for_user(conn.reborrow(), user).await?;
    let roles = load_roles(
        conn,
        "r.id IN (SELECT role_id FROM user_role WHERE user_id = ?)",
        args![user],
    )
    .await?;
    Ok(UserAccess { assignments, roles })
}

/// Loads non-archived roles matching `condition` (over alias `r`) with their grants.
async fn load_roles(mut conn: Conn<'_>, condition: &str, values: Vec<Arg>) -> Result<Vec<Role>> {
    let mut roles = sql::fetch_all(
        conn.reborrow(),
        &format!(
            "SELECT r.id, r.name FROM role r WHERE r.archived = ? AND {condition} ORDER BY r.name"
        ),
        [args![false], values.clone()].concat(),
        |row| {
            Ok(Role {
                id: sql::id(row, "id")?,
                name: row.text("name")?,
                grants: Vec::new(),
            })
        },
    )
    .await?;
    let grants = sql::fetch_all(
        conn,
        &format!(
            "SELECT rp.role_id, rp.perm, rp.limit_value FROM role_permission rp \
             JOIN role r ON r.id = rp.role_id WHERE r.archived = ? AND {condition} \
             ORDER BY rp.perm"
        ),
        [args![false], values].concat(),
        |row| Ok((sql::id::<RoleId>(row, "role_id")?, map_grant(row)?)),
    )
    .await?;
    let mut by_role: BTreeMap<RoleId, Vec<Grant>> = BTreeMap::new();
    for (role, grant) in grants {
        by_role.entry(role).or_default().push(grant);
    }
    for role in &mut roles {
        role.grants = by_role.remove(&role.id).unwrap_or_default();
    }
    Ok(roles)
}

/// Every distinct permission key stored in grants (for the catalog check in [`crate::seed`]).
pub(crate) async fn stored_permission_keys(conn: Conn<'_>) -> Result<Vec<String>> {
    sql::fetch_all(
        conn,
        "SELECT DISTINCT perm FROM role_permission ORDER BY perm",
        args![],
        |row| row.text("perm"),
    )
    .await
}

fn map_assignment(row: &dyn Row) -> Result<Assignment> {
    Ok(Assignment {
        user: sql::id(row, "user_id")?,
        role: sql::id(row, "role_id")?,
        scope: sql::opt_id(row, "branch_id")?.map_or(BranchScope::All, BranchScope::Branch),
    })
}

fn map_grant(row: &dyn Row) -> Result<Grant> {
    let key = row.text("perm")?;
    let permission: Permission = key.parse().map_err(|e| DbError::Corrupt(format!("{e}")))?;
    grant_from_parts(permission, row.opt_i64("limit_value")?)
}

/// Rebuilds a grant from its stored `limit_value`. The limit kind comes from the permission.
fn grant_from_parts(permission: Permission, value: Option<i64>) -> Result<Grant> {
    let Some(value) = value else {
        return Ok(Grant::unlimited(permission));
    };
    let corrupt = |reason: String| DbError::Corrupt(format!("grant `{permission}`: {reason}"));
    let limit = match permission.limit_kind() {
        Some(LimitKind::Pct) => Limit::MaxPct(
            u16::try_from(value).map_err(|_| corrupt(format!("{value} is not basis points")))?,
        ),
        Some(LimitKind::Amount) => Limit::MaxAmount(Money::from_minor(value)),
        None => return Err(corrupt("permission takes no limit".into())),
    };
    Grant::limited(permission, limit).map_err(|e| corrupt(e.to_string()))
}

/// The stored form of a limit: basis points or minor units. `None` = unlimited.
fn limit_value(limit: Option<Limit>) -> Option<i64> {
    limit.map(|l| match l {
        Limit::MaxPct(bp) => i64::from(bp),
        Limit::MaxAmount(m) => m.minor(),
    })
}

fn scope_branch(scope: BranchScope) -> Option<sultan_core::ids::BranchId> {
    match scope {
        BranchScope::All => None,
        BranchScope::Branch(b) => Some(b),
    }
}

fn role_json(role: &Role) -> Value {
    let grants: Vec<Value> = role
        .grants
        .iter()
        .map(|g| json!({ "perm": g.permission().as_str(), "limit": limit_value(g.limit()) }))
        .collect();
    json!({ "id": role.id.to_string(), "name": role.name, "grants": grants })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grants_round_trip_through_their_stored_form() {
        let grants = [
            Grant::unlimited(Permission::SaleCreate),
            Grant::unlimited(Permission::SaleDiscount),
            Grant::limited(Permission::SaleDiscount, Limit::MaxPct(1_000)).unwrap(),
            Grant::limited(
                Permission::CashOut,
                Limit::MaxAmount(Money::from_minor(50_000)),
            )
            .unwrap(),
        ];
        for g in grants {
            let back = grant_from_parts(g.permission(), limit_value(g.limit())).unwrap();
            assert_eq!(back, g);
        }
    }

    #[test]
    fn invalid_stored_limits_are_corrupt() {
        let cases = [
            (Permission::SaleCreate, 5, "permission takes no limit"),
            (Permission::SaleDiscount, -1, "-1 is not basis points"),
            (Permission::SaleDiscount, 20_000, "percentage above 100%"),
            (Permission::CashOut, -5, "negative amount"),
        ];
        for (perm, value, reason) in cases {
            let err = grant_from_parts(perm, Some(value)).unwrap_err();
            assert!(err.to_string().contains(reason), "{err}");
        }
    }
}
