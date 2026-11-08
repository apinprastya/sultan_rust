//! Manager override tokens (spec 2.1 §6): single use, bound to (action, value, register,
//! context), short TTL. Only the hash of the token is stored.

use sultan_core::access::{Permission, Requested};
use sultan_core::ids::{RegisterId, UserId};
use sultan_core::time::Timestamp;

use crate::TokenHash;
use crate::conn::Conn;
use crate::error::Result;
use crate::sql::{self, args};

/// An issued override token. The server has already validated the approval with
/// `core::access::validate_override`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverrideToken {
    /// SHA-256 of the token sent as `X-Override`.
    pub token_hash: TokenHash,
    /// The approved action.
    pub permission: Permission,
    /// The approved value, if the permission has a limit.
    pub value: Option<Requested>,
    /// The register the action runs on.
    pub register: RegisterId,
    /// The document or cart the action applies to.
    pub context_ref: String,
    /// The cashier who asked.
    pub requester: UserId,
    /// The manager who approved (recorded as `approved_by`, spec 2.1 §2).
    pub approver: UserId,
    /// Issue time.
    pub created_at: Timestamp,
    /// End of validity (spec 2.1 §6: 2 minutes).
    pub expires_at: Timestamp,
}

/// What the action request presents with its `X-Override` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OverrideUse<'a> {
    /// SHA-256 of the presented token.
    pub token_hash: &'a TokenHash,
    /// The action being performed.
    pub permission: Permission,
    /// The value being performed.
    pub value: Option<Requested>,
    /// The register (from the device token).
    pub register: RegisterId,
    /// The document or cart.
    pub context_ref: &'a str,
}

/// Who asked and who approved; returned by a successful [`consume`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Approval {
    /// The cashier who asked.
    pub requester: UserId,
    /// The manager who approved.
    pub approver: UserId,
}

/// Stores an issued token.
///
/// # Errors
/// [`DbError::MissingReference`](crate::DbError::MissingReference) for an unknown register or
/// user.
pub async fn insert<'c>(conn: impl Into<Conn<'c>>, token: &OverrideToken) -> Result<()> {
    sql::execute(
        conn.into(),
        "INSERT INTO override_token (token_hash, permission, requested_value, register_id, \
         context_ref, requester_id, approver_id, created_at, expires_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        args![
            &token.token_hash,
            token.permission.as_str(),
            token.value.map(stored_value),
            token.register,
            token.context_ref.as_str(),
            token.requester,
            token.approver,
            token.created_at,
            token.expires_at,
        ],
    )
    .await?;
    Ok(())
}

/// Uses up a token for one action. Succeeds only if the token is unused, unexpired
/// (`expires_at > now`) and was issued for exactly this permission, value, register and context
/// (spec 2.1 §6, test case 15). Returns `None` otherwise; a mismatch does not use the token up.
///
/// Atomic: one conditional `UPDATE` marks it used, so it cannot be used twice.
///
/// # Errors
/// The database error.
pub async fn consume<'c>(
    conn: impl Into<Conn<'c>>,
    presented: &OverrideUse<'_>,
    now: Timestamp,
) -> Result<Option<Approval>> {
    let mut conn = conn.into();
    let value = presented.value.map(stored_value);
    let used = sql::execute(
        conn.reborrow(),
        "UPDATE override_token SET consumed_at = ? \
         WHERE token_hash = ? AND consumed_at IS NULL AND expires_at > ? \
           AND permission = ? AND register_id = ? AND context_ref = ? \
           AND ((requested_value IS NULL AND ? IS NULL) OR requested_value = ?)",
        args![
            now,
            presented.token_hash,
            now,
            presented.permission.as_str(),
            presented.register,
            presented.context_ref,
            value,
            value,
        ],
    )
    .await?;
    if used == 0 {
        return Ok(None);
    }
    sql::fetch_optional(
        conn,
        "SELECT requester_id, approver_id FROM override_token WHERE token_hash = ?",
        args![presented.token_hash],
        |row| {
            Ok(Approval {
                requester: sql::id(row, "requester_id")?,
                approver: sql::id(row, "approver_id")?,
            })
        },
    )
    .await
}

/// Basis points or minor units; the kind follows from the permission.
fn stored_value(value: Requested) -> i64 {
    match value {
        Requested::Pct(bp) => i64::from(bp),
        Requested::Amount(m) => m.minor(),
    }
}
