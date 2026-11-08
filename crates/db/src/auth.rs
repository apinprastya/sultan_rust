//! User sessions and login-attempt state (spec 2.1 §4). Token columns hold hashes only.

use sultan_core::access::LoginAttempts;
use sultan_core::ids::{BranchId, RegisterId, UserId};
use sultan_core::time::Timestamp;

use crate::TokenHash;
use crate::conn::{Backend, Conn};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// A login session (spec 2.1 §4): bound to (user, register) for PIN login, or to the user alone
/// for back-office password login.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthSession {
    /// SHA-256 of the bearer token.
    pub token_hash: TokenHash,
    /// The user.
    pub user: UserId,
    /// The register (PIN login), `None` for back office.
    pub register: Option<RegisterId>,
    /// The branch the session acts at (effective permissions are computed for it).
    pub branch: BranchId,
    /// Login time.
    pub created_at: Timestamp,
    /// Last request.
    pub last_seen: Timestamp,
    /// Idle expiry; moved forward by [`touch_session`].
    pub expires_at: Timestamp,
    /// When it was revoked (logout, user deactivated).
    pub revoked_at: Option<Timestamp>,
}

impl AuthSession {
    /// Whether the session is usable at `now`: not revoked and not expired.
    #[must_use]
    pub fn is_active(&self, now: Timestamp) -> bool {
        self.revoked_at.is_none() && now < self.expires_at
    }
}

/// Stores a new session.
///
/// # Errors
/// [`DbError::MissingReference`] for an unknown user, register or branch.
pub async fn create_session<'c>(conn: impl Into<Conn<'c>>, s: &AuthSession) -> Result<()> {
    sql::execute(
        conn.into(),
        "INSERT INTO auth_session (token_hash, user_id, register_id, branch_id, created_at, \
         last_seen, expires_at, revoked_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            &s.token_hash,
            s.user,
            s.register,
            s.branch,
            s.created_at,
            s.last_seen,
            s.expires_at,
            s.revoked_at,
        ],
    )
    .await?;
    Ok(())
}

/// Looks up a session by token hash, active or not; check [`AuthSession::is_active`].
///
/// # Errors
/// The database error.
pub async fn find_session<'c>(
    conn: impl Into<Conn<'c>>,
    token_hash: &TokenHash,
) -> Result<Option<AuthSession>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT token_hash, user_id, register_id, branch_id, created_at, last_seen, expires_at, \
         revoked_at FROM auth_session WHERE token_hash = ?",
        args![token_hash],
        map_session,
    )
    .await
}

/// Records activity on an active session: sets `last_seen = now` and the new idle expiry.
/// Returns `false` (and changes nothing) if the session is unknown, revoked or already expired.
///
/// # Errors
/// The database error.
pub async fn touch_session<'c>(
    conn: impl Into<Conn<'c>>,
    token_hash: &TokenHash,
    now: Timestamp,
    expires_at: Timestamp,
) -> Result<bool> {
    let n = sql::execute(
        conn.into(),
        "UPDATE auth_session SET last_seen = ?, expires_at = ? \
         WHERE token_hash = ? AND revoked_at IS NULL AND expires_at > ?",
        args![now, expires_at, token_hash, now],
    )
    .await?;
    Ok(n > 0)
}

/// Revokes one session (logout, spec 2.1 §4). Returns whether an unrevoked session was revoked.
///
/// # Errors
/// The database error.
pub async fn revoke_session<'c>(
    conn: impl Into<Conn<'c>>,
    token_hash: &TokenHash,
    now: Timestamp,
) -> Result<bool> {
    let n = sql::execute(
        conn.into(),
        "UPDATE auth_session SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL",
        args![now, token_hash],
    )
    .await?;
    Ok(n > 0)
}

/// Revokes every unrevoked session of a user (e.g. deactivated). Returns how many.
///
/// # Errors
/// The database error.
pub async fn revoke_user_sessions<'c>(
    conn: impl Into<Conn<'c>>,
    user: UserId,
    now: Timestamp,
) -> Result<u64> {
    sql::execute(
        conn.into(),
        "UPDATE auth_session SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL",
        args![now, user],
    )
    .await
}

fn map_session(row: &dyn Row) -> Result<AuthSession> {
    Ok(AuthSession {
        token_hash: sql::hash(row, "token_hash")?,
        user: sql::id(row, "user_id")?,
        register: sql::opt_id(row, "register_id")?,
        branch: sql::id(row, "branch_id")?,
        created_at: sql::ts(row, "created_at")?,
        last_seen: sql::ts(row, "last_seen")?,
        expires_at: sql::ts(row, "expires_at")?,
        revoked_at: sql::opt_ts(row, "revoked_at")?,
    })
}

/// Loads the lockout state of `user` at `branch` (spec 2.1 §4); the default (no failures) if none
/// is stored.
///
/// # Errors
/// The database error, or [`DbError::Corrupt`] for an out-of-range counter.
pub async fn load_login_attempts<'c>(
    conn: impl Into<Conn<'c>>,
    user: UserId,
    branch: BranchId,
) -> Result<LoginAttempts> {
    let stored = sql::fetch_optional(
        conn.into(),
        "SELECT failures, locked_until FROM login_attempt WHERE user_id = ? AND branch_id = ?",
        args![user, branch],
        |row| {
            let failures = row.i64("failures")?;
            Ok(LoginAttempts {
                failures: u8::try_from(failures).map_err(|_| {
                    DbError::Corrupt(format!("login failure count {failures} out of range"))
                })?,
                locked_until: sql::opt_ts(row, "locked_until")?,
            })
        },
    )
    .await?;
    Ok(stored.unwrap_or_default())
}

/// Stores the lockout state of `user` at `branch` after `core` updated it.
///
/// An upsert keyed by the primary key only, so this is one of the few statements that differ per
/// backend (`ON CONFLICT` vs `ON DUPLICATE KEY`).
///
/// # Errors
/// [`DbError::MissingReference`] for an unknown user or branch.
pub async fn save_login_attempts<'c>(
    conn: impl Into<Conn<'c>>,
    user: UserId,
    branch: BranchId,
    attempts: &LoginAttempts,
) -> Result<()> {
    let conn = conn.into();
    let sql = match conn.backend() {
        Backend::Sqlite => {
            "INSERT INTO login_attempt (user_id, branch_id, failures, locked_until) \
             VALUES (?, ?, ?, ?) ON CONFLICT (user_id, branch_id) \
             DO UPDATE SET failures = excluded.failures, locked_until = excluded.locked_until"
        }
        Backend::MySql => {
            "INSERT INTO login_attempt (user_id, branch_id, failures, locked_until) \
             VALUES (?, ?, ?, ?) AS new \
             ON DUPLICATE KEY UPDATE failures = new.failures, locked_until = new.locked_until"
        }
    };
    sql::execute(
        conn,
        sql,
        args![
            user,
            branch,
            i64::from(attempts.failures),
            attempts.locked_until
        ],
    )
    .await?;
    Ok(())
}
