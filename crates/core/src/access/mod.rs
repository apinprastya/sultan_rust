//! Organization access rules: permissions, roles, effective permissions, manager override,
//! login lockout and the Owner invariant (spec 2.1 §3, §4, §6).

mod delegation;
mod effective;
mod lockout;
mod override_;
mod permission;
mod role;

pub use delegation::{covers_grant, ensure_can_grant, scope_permissions};
pub use effective::{Decision, PermissionSet, effective_permissions};
pub use lockout::{LOCKOUT_DURATION_MS, LoginAttempts, MAX_CONSECUTIVE_FAILURES};
pub use override_::validate_override;
pub use permission::{
    FULL_PCT_BASIS_POINTS, Limit, LimitKind, Permission, Requested, UnknownPermission,
};
pub use role::{Assignment, BranchScope, Grant, Role, UserStatus, ensure_active_owner_remains};

use crate::time::Timestamp;

/// Errors of the access rules. Each variant maps to a stable problem+json code (spec 2.1 §8)
/// through [`AccessError::code`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccessError {
    /// The user does not hold the permission and it cannot be overridden (spec 2.1 §3), or an
    /// override approver does not hold it (spec 2.1 §6).
    #[error("permission `{permission}` denied")]
    PermissionDenied {
        /// The permission that was checked.
        permission: Permission,
    },
    /// The action needs a manager override (spec 2.1 §6). `limit` is the requester's own limit,
    /// or `None` when they do not hold the permission at all.
    #[error("`{permission}` needs a manager override")]
    NeedsOverride {
        /// The permission that was checked.
        permission: Permission,
        /// The requester's current limit, if they hold the permission.
        limit: Option<Limit>,
    },
    /// The approver is the requester (spec 2.1 §6).
    #[error("an override cannot be approved by the requester")]
    SelfApproval,
    /// The approver holds the permission, but their limit is below the requested value
    /// (spec 2.1 §6, test case 2).
    #[error("approver limit {limit} for `{permission}` does not cover {requested}")]
    OverrideLimitExceeded {
        /// The permission that was checked.
        permission: Permission,
        /// The approver's limit.
        limit: Limit,
        /// The value that was requested.
        requested: Requested,
    },
    /// Too many consecutive login failures (spec 2.1 §4).
    #[error("locked out until {until}")]
    LockedOut {
        /// When the lockout ends unless someone unlocks the user earlier.
        until: Timestamp,
    },
    /// The change would leave no active user holding Owner for all branches (spec 2.1 §3).
    #[error("at least one active user must hold Owner for all branches")]
    LastOwner,
    /// A role grant whose limit does not fit the permission (spec 2.1 §3).
    #[error("invalid grant for `{permission}`: {reason}")]
    InvalidGrant {
        /// The permission of the grant.
        permission: Permission,
        /// Why the grant is invalid.
        reason: &'static str,
    },
    /// The caller tried to hand out a grant they do not hold themselves, at the scope in question
    /// (assigning a role, or creating/editing a role; see [`ensure_can_grant`]).
    #[error("cannot grant `{permission}`{}: the caller does not hold it with that limit", fmt_limit(*.limit))]
    GrantExceedsOwn {
        /// The permission of the grant.
        permission: Permission,
        /// The grant's limit (`None` = unlimited).
        limit: Option<Limit>,
    },
}

fn fmt_limit(limit: Option<Limit>) -> String {
    limit.map_or_else(|| " (unlimited)".to_owned(), |l| format!(" ({l})"))
}

impl AccessError {
    /// The stable error code used in problem+json responses (spec 2.1 §8).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::PermissionDenied { .. } | Self::GrantExceedsOwn { .. } => "permission_denied",
            Self::NeedsOverride { .. } => "needs_override",
            Self::SelfApproval => "self_approval",
            Self::OverrideLimitExceeded { .. } => "override_limit_exceeded",
            Self::LockedOut { .. } => "locked_out",
            Self::LastOwner => "last_owner",
            Self::InvalidGrant { .. } => "invalid_grant",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Money;

    #[test]
    fn codes_and_messages() {
        let p = Permission::SaleDiscount;
        let cases = [
            (
                AccessError::PermissionDenied { permission: p },
                "permission_denied",
                "permission `sale.discount` denied",
            ),
            (
                AccessError::NeedsOverride {
                    permission: p,
                    limit: None,
                },
                "needs_override",
                "`sale.discount` needs a manager override",
            ),
            (
                AccessError::SelfApproval,
                "self_approval",
                "an override cannot be approved by the requester",
            ),
            (
                AccessError::OverrideLimitExceeded {
                    permission: p,
                    limit: Limit::MaxPct(1_200),
                    requested: Requested::Pct(1_500),
                },
                "override_limit_exceeded",
                "approver limit max 12.00% for `sale.discount` does not cover 15.00%",
            ),
            (
                AccessError::LockedOut {
                    until: Timestamp::from_unix_millis(42),
                },
                "locked_out",
                "locked out until 42ms",
            ),
            (
                AccessError::LastOwner,
                "last_owner",
                "at least one active user must hold Owner for all branches",
            ),
            (
                AccessError::InvalidGrant {
                    permission: Permission::CashOut,
                    reason: "negative amount",
                },
                "invalid_grant",
                "invalid grant for `cash.out`: negative amount",
            ),
            (
                AccessError::GrantExceedsOwn {
                    permission: p,
                    limit: Some(Limit::MaxPct(2_000)),
                },
                "permission_denied",
                "cannot grant `sale.discount` (max 20.00%): the caller does not hold it with that limit",
            ),
            (
                AccessError::GrantExceedsOwn {
                    permission: Permission::UserManage,
                    limit: None,
                },
                "permission_denied",
                "cannot grant `user.manage` (unlimited): the caller does not hold it with that limit",
            ),
        ];
        for (error, code, message) in cases {
            assert_eq!(error.code(), code);
            assert_eq!(error.to_string(), message);
        }
        // Amount display goes through `Money`.
        assert_eq!(
            Limit::MaxAmount(Money::from_minor(500)).to_string(),
            "max 500"
        );
    }
}
