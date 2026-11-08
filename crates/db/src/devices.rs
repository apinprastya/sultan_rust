//! Register identity: pairing codes and device tokens (spec 2.1 §2). Only hashes are stored.

use sultan_core::ids::{BranchId, RegisterId};
use sultan_core::time::Timestamp;

use crate::TokenHash;
use crate::conn::Conn;
use crate::error::Result;
use crate::sql::{self, args};

/// Stores a one-time pairing code for `register`, valid until `expires_at` (spec 2.1 §2: 10
/// minutes; the server picks the time).
///
/// # Errors
/// [`DbError::MissingReference`](crate::DbError::MissingReference) for an unknown register.
pub async fn create_pairing_code<'c>(
    conn: impl Into<Conn<'c>>,
    code_hash: &TokenHash,
    register: RegisterId,
    now: Timestamp,
    expires_at: Timestamp,
) -> Result<()> {
    sql::execute(
        conn.into(),
        "INSERT INTO register_pairing_code (code_hash, register_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?)",
        args![code_hash, register, now, expires_at],
    )
    .await?;
    Ok(())
}

/// Expires every pairing code of `register` that is still usable, so only the code issued next
/// works (spec 2.1 §2: issuing a new code replaces the old one). Returns how many were expired.
///
/// # Errors
/// The database error.
pub async fn expire_pairing_codes<'c>(
    conn: impl Into<Conn<'c>>,
    register: RegisterId,
    now: Timestamp,
) -> Result<u64> {
    sql::execute(
        conn.into(),
        "UPDATE register_pairing_code SET expires_at = ? \
         WHERE register_id = ? AND consumed_at IS NULL AND expires_at > ?",
        args![now, register, now],
    )
    .await
}

/// Uses up a pairing code and returns its register. Returns `None` if the code is unknown,
/// already used or expired (`expires_at <= now`).
///
/// Atomic: the code is marked used by one conditional `UPDATE`, so of two concurrent attempts
/// only one succeeds (spec 2.1 §2: single use).
///
/// # Errors
/// The database error.
pub async fn consume_pairing_code<'c>(
    conn: impl Into<Conn<'c>>,
    code_hash: &TokenHash,
    now: Timestamp,
) -> Result<Option<RegisterId>> {
    let mut conn = conn.into();
    let used = sql::execute(
        conn.reborrow(),
        "UPDATE register_pairing_code SET consumed_at = ? \
         WHERE code_hash = ? AND consumed_at IS NULL AND expires_at > ?",
        args![now, code_hash, now],
    )
    .await?;
    if used == 0 {
        return Ok(None);
    }
    sql::fetch_optional(
        conn,
        "SELECT register_id FROM register_pairing_code WHERE code_hash = ?",
        args![code_hash],
        |row| sql::id(row, "register_id"),
    )
    .await
}

/// Stores the hash of a new device token for `register` (spec 2.1 §2).
///
/// # Errors
/// [`DbError::MissingReference`](crate::DbError::MissingReference) for an unknown register.
pub async fn insert_device_token<'c>(
    conn: impl Into<Conn<'c>>,
    token_hash: &TokenHash,
    register: RegisterId,
    now: Timestamp,
) -> Result<()> {
    sql::execute(
        conn.into(),
        "INSERT INTO device_token (token_hash, register_id, created_at) VALUES (?, ?, ?)",
        args![token_hash, register, now],
    )
    .await?;
    Ok(())
}

/// What a valid device token identifies. The server takes register and branch from here only,
/// never from a request body (spec 2.1 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceIdentity {
    /// The register.
    pub register: RegisterId,
    /// The register's branch.
    pub branch: BranchId,
}

/// Looks up a device token. Returns `None` for an unknown or revoked token, or one whose register
/// is archived: all of these are `401` (spec 2.1 §2).
///
/// # Errors
/// The database error.
pub async fn find_device<'c>(
    conn: impl Into<Conn<'c>>,
    token_hash: &TokenHash,
) -> Result<Option<DeviceIdentity>> {
    sql::fetch_optional(
        conn.into(),
        "SELECT r.id, r.branch_id FROM device_token t JOIN register r ON r.id = t.register_id \
         WHERE t.token_hash = ? AND t.revoked_at IS NULL AND r.archived = ?",
        args![token_hash, false],
        |row| {
            Ok(DeviceIdentity {
                register: sql::id(row, "id")?,
                branch: sql::id(row, "branch_id")?,
            })
        },
    )
    .await
}

/// Revokes one device token. Returns whether an active token was revoked.
///
/// # Errors
/// The database error.
pub async fn revoke_device_token<'c>(
    conn: impl Into<Conn<'c>>,
    token_hash: &TokenHash,
    now: Timestamp,
) -> Result<bool> {
    let n = sql::execute(
        conn.into(),
        "UPDATE device_token SET revoked_at = ? WHERE token_hash = ? AND revoked_at IS NULL",
        args![now, token_hash],
    )
    .await?;
    Ok(n > 0)
}

/// Unpairs a register: revokes all its active device tokens (spec 2.1 §2). Returns how many.
///
/// # Errors
/// The database error.
pub async fn revoke_register_tokens<'c>(
    conn: impl Into<Conn<'c>>,
    register: RegisterId,
    now: Timestamp,
) -> Result<u64> {
    sql::execute(
        conn.into(),
        "UPDATE device_token SET revoked_at = ? WHERE register_id = ? AND revoked_at IS NULL",
        args![now, register],
    )
    .await
}
