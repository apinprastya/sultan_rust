//! Delegation: who may hand out which permissions (spec 2.1 §3, plan Phase 6 privilege fix).
//!
//! Spec 2.1 §3 makes `user.manage` / `role.manage` the gate for editing users and roles, but on its
//! own that lets any holder of `user.manage` assign the Owner role to themselves. The rule here
//! closes that hole: **nobody can hand out more than they hold.**
//!
//! - Assigning or unassigning a role at a branch scope needs every grant of that role, with a limit
//!   at least as high, in the caller's permissions **at that scope** ([`scope_permissions`]).
//! - Creating, editing or archiving a role needs its grants (before and after) at the
//!   **all-branches** scope, because a role applies wherever it is assigned.
//! - The Owner (every permission, no limits, for all branches) therefore may assign anything.

use super::AccessError;
use super::effective::{PermissionSet, effective_permissions};
use super::permission::Limit;
use super::role::{Assignment, BranchScope, Grant, Role};

/// The permissions a caller holds **everywhere in `scope`**.
///
/// - `BranchScope::Branch(b)`: the effective permissions at `b` (roles assigned for `b` or for all
///   branches).
/// - `BranchScope::All`: only roles assigned for all branches. A role held at every existing branch
///   one by one does not count, because "all branches" also covers branches created later.
///
/// `assignments` must be the caller's own assignments.
#[must_use]
pub fn scope_permissions(
    assignments: &[Assignment],
    roles: &[Role],
    scope: BranchScope,
) -> PermissionSet {
    match scope {
        BranchScope::Branch(branch) => effective_permissions(assignments, roles, branch),
        BranchScope::All => assignments
            .iter()
            .filter(|a| a.scope == BranchScope::All)
            .filter_map(|a| roles.iter().find(|r| r.id == a.role))
            .flat_map(|r| r.grants.iter().copied())
            .collect(),
    }
}

/// Whether a held limit is at least as wide as a granted one. `None` = unlimited.
fn limit_covers(held: Option<Limit>, granted: Option<Limit>) -> bool {
    match (held, granted) {
        (None, _) => true,
        (Some(Limit::MaxPct(h)), Some(Limit::MaxPct(g))) => g <= h,
        (Some(Limit::MaxAmount(h)), Some(Limit::MaxAmount(g))) => g <= h,
        // A limit never covers "unlimited". Mixed kinds cannot be built through
        // `Grant::limited`; fail closed.
        (Some(_), _) => false,
    }
}

/// Whether `held` covers `grant`: the permission is held with a limit at least as high.
#[must_use]
pub fn covers_grant(held: &PermissionSet, grant: Grant) -> bool {
    held.limit(grant.permission())
        .is_some_and(|limit| limit_covers(limit, grant.limit()))
}

/// Checks that `held` covers every grant in `grants` (spec 2.1 §3: nobody hands out more than they
/// hold).
///
/// # Errors
/// [`AccessError::GrantExceedsOwn`] naming the first grant that is not covered.
pub fn ensure_can_grant(held: &PermissionSet, grants: &[Grant]) -> Result<(), AccessError> {
    match grants.iter().find(|g| !covers_grant(held, **g)) {
        Some(grant) => Err(AccessError::GrantExceedsOwn {
            permission: grant.permission(),
            limit: grant.limit(),
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::Permission;
    use crate::ids::{BranchId, RoleId, UserId};
    use crate::money::Money;

    fn pct(bp: u16) -> Grant {
        Grant::limited(Permission::SaleDiscount, Limit::MaxPct(bp)).unwrap()
    }

    fn cash_out(amount: i64) -> Grant {
        Grant::limited(
            Permission::CashOut,
            Limit::MaxAmount(Money::from_minor(amount)),
        )
        .unwrap()
    }

    fn set(grants: &[Grant]) -> PermissionSet {
        grants.iter().copied().collect()
    }

    #[test]
    fn coverage_table() {
        let void = Grant::unlimited(Permission::SaleVoid);
        let unlimited_discount = Grant::unlimited(Permission::SaleDiscount);
        #[rustfmt::skip]
        let cases = [
            ("plain permission held", set(&[void]), void, true),
            ("plain permission missing", set(&[]), void, false),
            ("higher limit held", set(&[pct(2_000)]), pct(1_500), true),
            ("equal limit held", set(&[pct(1_500)]), pct(1_500), true),
            ("lower limit held", set(&[pct(1_000)]), pct(1_500), false),
            ("unlimited held covers a limit", set(&[unlimited_discount]), pct(10_000), true),
            ("limit held does not cover unlimited", set(&[pct(10_000)]), unlimited_discount, false),
            ("amount: higher held", set(&[cash_out(500)]), cash_out(400), true),
            ("amount: lower held", set(&[cash_out(300)]), cash_out(400), false),
            ("other permission held", set(&[cash_out(500)]), pct(1), false),
        ];
        for (name, held, grant, expected) in cases {
            assert_eq!(covers_grant(&held, grant), expected, "{name}");
        }
    }

    #[test]
    fn mixed_limit_kinds_fail_closed() {
        let pct = Some(Limit::MaxPct(1));
        let amount = Some(Limit::MaxAmount(Money::from_minor(1)));
        assert!(!limit_covers(pct, amount));
        assert!(!limit_covers(amount, pct));
    }

    #[test]
    fn ensure_can_grant_names_the_first_uncovered_grant() {
        let held = set(&[pct(1_000), Grant::unlimited(Permission::SaleCreate)]);
        assert_eq!(ensure_can_grant(&held, &[]), Ok(()));
        assert_eq!(
            ensure_can_grant(&held, &[Grant::unlimited(Permission::SaleCreate), pct(500)]),
            Ok(())
        );
        assert_eq!(
            ensure_can_grant(
                &held,
                &[pct(2_000), Grant::unlimited(Permission::UserManage)]
            ),
            Err(AccessError::GrantExceedsOwn {
                permission: Permission::SaleDiscount,
                limit: Some(Limit::MaxPct(2_000)),
            })
        );
        // The Owner role covers every other role.
        let owner = Role::owner(RoleId::generate());
        let all: PermissionSet = owner.grants.iter().copied().collect();
        assert_eq!(ensure_can_grant(&all, &owner.grants), Ok(()));
        assert!(ensure_can_grant(&held, &owner.grants).is_err());
    }

    #[test]
    fn scope_permissions_table() {
        let user = UserId::generate();
        let jkt = BranchId::generate();
        let bdg = BranchId::generate();
        let everywhere = Role {
            id: RoleId::generate(),
            name: "Everywhere".into(),
            grants: vec![Grant::unlimited(Permission::UserManage)],
        };
        let local = Role {
            id: RoleId::generate(),
            name: "Local".into(),
            grants: vec![pct(2_000)],
        };
        let roles = [everywhere.clone(), local.clone()];
        let assignments = [
            Assignment {
                user,
                role: everywhere.id,
                scope: BranchScope::All,
            },
            Assignment {
                user,
                role: local.id,
                scope: BranchScope::Branch(jkt),
            },
            // A role that no longer exists grants nothing.
            Assignment {
                user,
                role: RoleId::generate(),
                scope: BranchScope::All,
            },
        ];
        let um = Permission::UserManage;
        let d = Permission::SaleDiscount;
        #[rustfmt::skip]
        let cases = [
            ("all branches: only all-branch roles", BranchScope::All, true, false),
            ("assigned branch: both", BranchScope::Branch(jkt), true, true),
            ("other branch: all-branch roles", BranchScope::Branch(bdg), true, false),
        ];
        for (name, scope, has_um, has_d) in cases {
            let held = scope_permissions(&assignments, &roles, scope);
            assert_eq!(
                (held.contains(um), held.contains(d)),
                (has_um, has_d),
                "{name}"
            );
        }
    }
}
