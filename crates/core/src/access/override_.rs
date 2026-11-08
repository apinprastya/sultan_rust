//! Manager override approval rules (spec 2.1 §6).

use super::AccessError;
use super::effective::{Decision, PermissionSet};
use super::permission::{Permission, Requested};
use crate::ids::UserId;

/// Validates that `approver` may approve `requester`'s action (spec 2.1 §6).
///
/// `approver_perms` must be the approver's effective permissions **at the requester's branch**
/// (see [`effective_permissions`](super::effective_permissions)). PIN verification, lockout and the
/// single-use override token are the server's job.
///
/// # Errors
/// - [`AccessError::SelfApproval`] if the approver is the requester.
/// - [`AccessError::OverrideLimitExceeded`] (carrying the approver's limit) if the approver holds
///   the permission but their limit is below `value`.
/// - [`AccessError::PermissionDenied`] if the permission is not overridable, the approver does
///   not hold it, or `value` is of the wrong kind.
pub fn validate_override(
    requester: UserId,
    approver: UserId,
    approver_perms: &PermissionSet,
    permission: Permission,
    value: Option<Requested>,
) -> Result<(), AccessError> {
    if approver == requester {
        return Err(AccessError::SelfApproval);
    }
    if !permission.is_overridable() {
        return Err(AccessError::PermissionDenied { permission });
    }
    match (approver_perms.check(permission, value), value) {
        (Decision::Allowed, _) => Ok(()),
        (Decision::NeedsOverride { limit: Some(limit) }, Some(value)) => {
            Err(AccessError::OverrideLimitExceeded {
                permission,
                limit,
                requested: value,
            })
        }
        // The approver lacks the permission themselves (an approver cannot borrow an override).
        _ => Err(AccessError::PermissionDenied { permission }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::{Grant, Limit};
    use crate::money::Money;

    fn perms(grants: &[Grant]) -> PermissionSet {
        grants.iter().copied().collect()
    }

    fn discount(bp: u16) -> Grant {
        Grant::limited(Permission::SaleDiscount, Limit::MaxPct(bp)).unwrap()
    }

    /// Spec 2.1 §9 cases 1–3: a cashier at 10% asks for 15%.
    #[test]
    fn override_table() {
        let cashier = UserId::generate();
        let manager = UserId::generate();
        let cashier_perms = perms(&[discount(1_000)]);
        let fifteen = Some(Requested::Pct(1_500));
        let d = Permission::SaleDiscount;

        // Case 1, first half: the cashier needs an override.
        assert_eq!(
            cashier_perms.check(d, fifteen),
            Decision::NeedsOverride {
                limit: Some(Limit::MaxPct(1_000))
            }
        );

        #[rustfmt::skip]
        let cases = [
            ("case 1: manager at 20% approves", manager, perms(&[discount(2_000)]), d, fifteen, Ok(())),
            ("manager limit exactly equal", manager, perms(&[discount(1_500)]), d, fifteen, Ok(())),
            ("case 2: manager at 12% is rejected with the limit", manager, perms(&[discount(1_200)]), d, fifteen,
                Err(AccessError::OverrideLimitExceeded {
                    permission: d, limit: Limit::MaxPct(1_200), requested: Requested::Pct(1_500) })),
            ("case 3: self-approval", cashier, perms(&[discount(10_000)]), d, fifteen,
                Err(AccessError::SelfApproval)),
            ("approver lacks the permission", manager, perms(&[]), d, fifteen,
                Err(AccessError::PermissionDenied { permission: d })),
            ("approver unlimited", manager, perms(&[Grant::unlimited(d)]), d, fifteen, Ok(())),
            ("plain permission", manager, perms(&[Grant::unlimited(Permission::SaleVoid)]),
                Permission::SaleVoid, None, Ok(())),
            ("not overridable", manager, perms(&[Grant::unlimited(Permission::UserManage)]),
                Permission::UserManage, None,
                Err(AccessError::PermissionDenied { permission: Permission::UserManage })),
            ("wrong value kind", manager, perms(&[discount(2_000)]), d,
                Some(Requested::Amount(Money::from_minor(1))),
                Err(AccessError::PermissionDenied { permission: d })),
        ];
        for (name, approver, approver_perms, permission, value, expected) in cases {
            assert_eq!(
                validate_override(cashier, approver, &approver_perms, permission, value),
                expected,
                "{name}"
            );
        }
    }
}
