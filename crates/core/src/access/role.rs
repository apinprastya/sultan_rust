//! Roles, grants, assignments and the Owner invariant (spec 2.1 §3).

use super::AccessError;
use super::permission::{FULL_PCT_BASIS_POINTS, Limit, Permission};
use crate::ids::{BranchId, RoleId, UserId};

/// One permission in a role, with an optional limit (spec 2.1 §3).
///
/// `limit: None` on a limited permission means **unlimited** (as in the Owner role).
/// The constructor validates the limit, so a `Grant` always fits its permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Grant {
    permission: Permission,
    limit: Option<Limit>,
}

impl Grant {
    /// A grant without a limit: all-or-nothing permissions, or unlimited for limited ones.
    #[must_use]
    pub const fn unlimited(permission: Permission) -> Self {
        Self {
            permission,
            limit: None,
        }
    }

    /// A grant with a limit.
    ///
    /// # Errors
    /// [`AccessError::InvalidGrant`] if the permission takes no limit, the limit is of the wrong
    /// kind, a percentage is above 100% or an amount is negative.
    pub fn limited(permission: Permission, limit: Limit) -> Result<Self, AccessError> {
        let invalid = |reason| AccessError::InvalidGrant { permission, reason };
        match permission.limit_kind() {
            None => return Err(invalid("permission takes no limit")),
            Some(kind) if kind != limit.kind() => return Err(invalid("wrong limit kind")),
            Some(_) => {}
        }
        match limit {
            Limit::MaxPct(bp) if bp > FULL_PCT_BASIS_POINTS => {
                Err(invalid("percentage above 100%"))
            }
            Limit::MaxAmount(amount) if amount.is_negative() => Err(invalid("negative amount")),
            _ => Ok(Self {
                permission,
                limit: Some(limit),
            }),
        }
    }

    /// The granted permission.
    #[must_use]
    pub const fn permission(&self) -> Permission {
        self.permission
    }

    /// The limit, or `None` for no limit.
    #[must_use]
    pub const fn limit(&self) -> Option<Limit> {
        self.limit
    }
}

/// A role: a name and a set of grants. Roles are data, edited at HQ (spec 2.1 §3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    /// Role ID.
    pub id: RoleId,
    /// Display name, e.g. "Cashier".
    pub name: String,
    /// The grants. Grants only add, never deny.
    pub grants: Vec<Grant>,
}

impl Role {
    /// The seeded Owner role: every permission in the catalog, no limits (spec 2.1 §3).
    #[must_use]
    pub fn owner(id: RoleId) -> Self {
        Self {
            id,
            name: "Owner".to_owned(),
            grants: Permission::ALL.into_iter().map(Grant::unlimited).collect(),
        }
    }

    /// Whether the role grants every catalog permission without a limit, i.e. it is a complete
    /// Owner role that has not been emptied.
    #[must_use]
    pub fn grants_everything_unlimited(&self) -> bool {
        Permission::ALL.into_iter().all(|p| {
            self.grants
                .iter()
                .any(|g| g.permission == p && g.limit.is_none())
        })
    }
}

/// Where an [`Assignment`] applies (spec 2.1 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BranchScope {
    /// Every branch, including branches created later.
    All,
    /// One branch.
    Branch(BranchId),
}

impl BranchScope {
    /// Whether this scope covers `branch`.
    #[must_use]
    pub fn applies_to(self, branch: BranchId) -> bool {
        match self {
            Self::All => true,
            Self::Branch(b) => b == branch,
        }
    }
}

/// A role given to a user at a branch or at all branches (spec 2.1 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Assignment {
    /// The user.
    pub user: UserId,
    /// The role.
    pub role: RoleId,
    /// Where the role applies.
    pub scope: BranchScope,
}

/// The part of a user record the Owner invariant needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct UserStatus {
    /// The user.
    pub id: UserId,
    /// Deactivated users do not count as Owners.
    pub active: bool,
}

/// Checks the invariant "at least one active user holds Owner for all branches"
/// (spec 2.1 §3, test case 6).
///
/// Pass the **proposed** state after the change (users, roles, assignments). Removing,
/// deactivating or unassigning the last Owner, narrowing their assignment to one branch, and
/// deleting or emptying the Owner role are all rejected. A user missing from `users` counts as
/// removed.
///
/// The Owner role must still grant the whole catalog without limits. Adding a permission to the
/// catalog therefore requires migrating the seeded Owner role (as spec 2.1 §3 already demands).
///
/// # Errors
/// [`AccessError::LastOwner`] if the invariant would be broken.
pub fn ensure_active_owner_remains(
    owner_role: RoleId,
    roles: &[Role],
    assignments: &[Assignment],
    users: &[UserStatus],
) -> Result<(), AccessError> {
    let role_intact = roles
        .iter()
        .any(|r| r.id == owner_role && r.grants_everything_unlimited());
    let is_active = |user: UserId| users.iter().any(|u| u.id == user && u.active);
    let has_active_owner = assignments
        .iter()
        .any(|a| a.role == owner_role && a.scope == BranchScope::All && is_active(a.user));

    if role_intact && has_active_owner {
        Ok(())
    } else {
        Err(AccessError::LastOwner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::money::Money;

    /// (name, roles, assignments, users, invariant holds)
    type OwnerCase = (
        &'static str,
        Vec<Role>,
        Vec<Assignment>,
        Vec<UserStatus>,
        bool,
    );

    #[test]
    fn grant_validation() {
        let pct = |bp| Limit::MaxPct(bp);
        let amount = |m| Limit::MaxAmount(Money::from_minor(m));
        let cases = [
            (Permission::SaleDiscount, pct(1_000), None),
            (Permission::SaleDiscount, pct(10_000), None),
            (Permission::CashOut, amount(0), None),
            (
                Permission::SaleCreate,
                pct(1_000),
                Some("permission takes no limit"),
            ),
            (
                Permission::SaleDiscount,
                amount(1),
                Some("wrong limit kind"),
            ),
            (
                Permission::SaleDiscount,
                pct(10_001),
                Some("percentage above 100%"),
            ),
            (
                Permission::RefundNoReceipt,
                amount(-1),
                Some("negative amount"),
            ),
        ];
        for (permission, limit, error) in cases {
            let result = Grant::limited(permission, limit);
            match error {
                None => {
                    let grant = result.unwrap();
                    assert_eq!(grant.permission(), permission);
                    assert_eq!(grant.limit(), Some(limit));
                }
                Some(reason) => assert_eq!(
                    result,
                    Err(AccessError::InvalidGrant { permission, reason })
                ),
            }
        }
        assert_eq!(Grant::unlimited(Permission::AuditView).limit(), None);
    }

    #[test]
    fn branch_scope() {
        let jkt = BranchId::generate();
        let bdg = BranchId::generate();
        assert!(BranchScope::All.applies_to(jkt));
        assert!(BranchScope::Branch(jkt).applies_to(jkt));
        assert!(!BranchScope::Branch(jkt).applies_to(bdg));
    }

    /// Spec 2.1 §9 case 6, plus the ways the invariant can still hold.
    #[test]
    fn last_owner_invariant() {
        let owner_role = RoleId::generate();
        let cashier_role = RoleId::generate();
        let alice = UserId::generate();
        let bob = UserId::generate();
        let jkt = BranchId::generate();

        let owner = Role::owner(owner_role);
        let mut emptied = Role::owner(owner_role);
        emptied.grants.clear();
        let mut limited = Role::owner(owner_role);
        limited
            .grants
            .retain(|g| g.permission() != Permission::SaleDiscount);
        limited
            .grants
            .push(Grant::limited(Permission::SaleDiscount, Limit::MaxPct(500)).unwrap());
        let cashier = Role {
            id: cashier_role,
            name: "Cashier".into(),
            grants: vec![Grant::unlimited(Permission::SaleCreate)],
        };

        let assign = |user, role, scope| Assignment { user, role, scope };
        let active = |id| UserStatus { id, active: true };
        let inactive = |id| UserStatus { id, active: false };
        let alice_owner = assign(alice, owner_role, BranchScope::All);
        let bob_owner = assign(bob, owner_role, BranchScope::All);

        #[rustfmt::skip]
        let cases: Vec<OwnerCase> = vec![
            ("one active owner", vec![owner.clone()], vec![alice_owner], vec![active(alice)], true),
            ("second owner remains after first is deactivated",
                vec![owner.clone()], vec![alice_owner, bob_owner], vec![inactive(alice), active(bob)], true),
            ("deactivate last owner", vec![owner.clone()], vec![alice_owner], vec![inactive(alice)], false),
            ("remove last owner user", vec![owner.clone()], vec![alice_owner], vec![active(bob)], false),
            ("unassign last owner", vec![owner.clone()], vec![], vec![active(alice)], false),
            ("narrow last owner to one branch",
                vec![owner.clone()], vec![assign(alice, owner_role, BranchScope::Branch(jkt))], vec![active(alice)], false),
            ("owner user switched to cashier role",
                vec![owner.clone(), cashier.clone()], vec![assign(alice, cashier_role, BranchScope::All)], vec![active(alice)], false),
            ("delete owner role", vec![cashier.clone()], vec![alice_owner], vec![active(alice)], false),
            ("empty owner role", vec![emptied], vec![alice_owner], vec![active(alice)], false),
            ("limit owner role", vec![limited], vec![alice_owner], vec![active(alice)], false),
        ];

        for (name, roles, assignments, users, ok) in cases {
            let result = ensure_active_owner_remains(owner_role, &roles, &assignments, &users);
            let expected = if ok {
                Ok(())
            } else {
                Err(AccessError::LastOwner)
            };
            assert_eq!(result, expected, "{name}");
        }
    }
}
