//! Users (spec 2.1 §3–4). HQ-owned master data: every save appends an outbox event.
//!
//! `pin_hash` and `password_hash` are argon2id PHC strings produced by the server (spec 2.1 §4);
//! this module stores them as given. They are part of the outbox payload because branch nodes
//! authenticate against their replica while HQ is offline (spec 2.1 §4).

use serde_json::json;
use sultan_core::access::UserStatus;
use sultan_core::ids::{BranchId, UserId};
use sultan_core::time::Timestamp;

use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::sql::{self, Row, args};
use crate::{Saved, outbox};

/// A user account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    /// ID.
    pub id: UserId,
    /// Back-office login name. Unique.
    pub username: String,
    /// Name shown on the register lock screen.
    pub display_name: String,
    /// argon2id hash of the PIN, if a PIN is set.
    pub pin_hash: Option<String>,
    /// argon2id hash of the password, if a password is set.
    pub password_hash: Option<String>,
    /// Deactivated users cannot log in and do not count as Owners (spec 2.1 §3).
    pub active: bool,
}

const USER_SELECT: &str = "SELECT u.id, u.username, u.display_name, u.pin_hash, u.password_hash, u.active FROM app_user u";

/// Creates or overwrites a user (`PUT /users/{id}`), with an outbox event `user.saved`.
/// Check the Owner invariant with `core` before deactivating.
///
/// # Errors
/// [`DbError::Conflict`](crate::DbError::Conflict) for a duplicate username.
pub async fn save_user(tx: &mut Tx, user: &User, now: Timestamp) -> Result<Saved> {
    let saved = sql::upsert_by_id(
        tx,
        "app_user",
        user.id.into(),
        &[
            "username",
            "display_name",
            "pin_hash",
            "password_hash",
            "active",
        ],
        args![
            user.username.as_str(),
            user.display_name.as_str(),
            user.pin_hash.as_deref(),
            user.password_hash.as_deref(),
            user.active,
        ],
        now,
    )
    .await?;
    let payload = json!({
        "id": user.id.to_string(),
        "username": user.username,
        "display_name": user.display_name,
        "pin_hash": user.pin_hash,
        "password_hash": user.password_hash,
        "active": user.active,
    });
    outbox::append(tx, "user.saved", &payload, now).await?;
    Ok(saved)
}

/// One user.
///
/// # Errors
/// The database error.
pub async fn get_user<'c>(conn: impl Into<Conn<'c>>, id: UserId) -> Result<Option<User>> {
    let sql = format!("{USER_SELECT} WHERE u.id = ?");
    sql::fetch_optional(conn.into(), &sql, args![id], map_user).await
}

/// One user by username (back-office password login, spec 2.1 §4).
///
/// # Errors
/// The database error.
pub async fn find_by_username<'c>(
    conn: impl Into<Conn<'c>>,
    username: &str,
) -> Result<Option<User>> {
    let sql = format!("{USER_SELECT} WHERE u.username = ?");
    sql::fetch_optional(conn.into(), &sql, args![username], map_user).await
}

/// All users, including inactive ones, ordered by username.
///
/// # Errors
/// The database error.
pub async fn list_users<'c>(conn: impl Into<Conn<'c>>) -> Result<Vec<User>> {
    let sql = format!("{USER_SELECT} ORDER BY u.username");
    sql::fetch_all(conn.into(), &sql, args![], map_user).await
}

/// Active users with at least one (non-archived) role at `branch` or at all branches, ordered by
/// display name: the register lock screen (`GET /registers/me/users`, spec 2.1 §4).
///
/// # Errors
/// The database error.
pub async fn list_for_branch<'c>(conn: impl Into<Conn<'c>>, branch: BranchId) -> Result<Vec<User>> {
    let sql = format!(
        "{USER_SELECT} WHERE u.active = ? AND EXISTS (\
           SELECT 1 FROM user_role ur JOIN role r ON r.id = ur.role_id \
           WHERE ur.user_id = u.id AND r.archived = ? \
             AND (ur.branch_id IS NULL OR ur.branch_id = ?)) \
         ORDER BY u.display_name"
    );
    sql::fetch_all(conn.into(), &sql, args![true, false, branch], map_user).await
}

/// The status of every user: input to `core::access::ensure_active_owner_remains`.
///
/// # Errors
/// The database error.
pub async fn list_statuses<'c>(conn: impl Into<Conn<'c>>) -> Result<Vec<UserStatus>> {
    sql::fetch_all(
        conn.into(),
        "SELECT id, active FROM app_user",
        args![],
        |row| {
            Ok(UserStatus {
                id: sql::id(row, "id")?,
                active: row.bool("active")?,
            })
        },
    )
    .await
}

fn map_user(row: &dyn Row) -> Result<User> {
    Ok(User {
        id: sql::id(row, "id")?,
        username: row.text("username")?,
        display_name: row.text("display_name")?,
        pin_hash: row.opt_text("pin_hash")?,
        password_hash: row.opt_text("password_hash")?,
        active: row.bool("active")?,
    })
}
