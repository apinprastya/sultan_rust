//! Effective permissions of a user at a branch, and the permission check (spec 2.1 §3, §6).

use std::collections::BTreeMap;

use super::AccessError;
use super::permission::{Limit, Permission, Requested};
use super::role::{Assignment, Grant, Role};
use crate::ids::BranchId;

/// The outcome of [`PermissionSet::check`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Decision {
    /// The user may do it.
    Allowed,
    /// A manager must approve it (spec 2.1 §6). `limit` is the user's own limit when they hold the
    /// permission but asked for more, and `None` when they do not hold it at all.
    NeedsOverride {
        /// The user's own limit, if any.
        limit: Option<Limit>,
    },
    /// Refused; no override is possible.
    Denied,
}

/// The permissions a user holds at one branch, with the highest limit per permission.
///
/// Build it with [`effective_permissions`], or collect it from grants.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PermissionSet {
    /// `None` = held without a limit.
    grants: BTreeMap<Permission, Option<Limit>>,
}

impl PermissionSet {
    /// Adds a grant. Grants only add (spec 2.1 §3): for a permission held twice, the highest limit
    /// wins and no limit beats any limit.
    pub fn insert(&mut self, grant: Grant) {
        self.grants
            .entry(grant.permission())
            .and_modify(|held| {
                *held = match (*held, grant.limit()) {
                    (Some(a), Some(b)) => Some(a.wider(b)),
                    (None, _) | (_, None) => None,
                }
            })
            .or_insert(grant.limit());
    }

    /// Whether the permission is held, at any limit.
    #[must_use]
    pub fn contains(&self, permission: Permission) -> bool {
        self.grants.contains_key(&permission)
    }

    /// The effective limit: `None` if the permission is not held, `Some(None)` if it is held
    /// without a limit, `Some(Some(limit))` otherwise.
    #[must_use]
    pub fn limit(&self, permission: Permission) -> Option<Option<Limit>> {
        self.grants.get(&permission).copied()
    }

    /// Whether the set holds no permissions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }

    /// Checks one action (spec 2.1 §3, §6).
    ///
    /// `requested` is the value the action asks for (e.g. a 15% discount); pass `None` for
    /// permissions without a limit, or to ask "may the user do this at all".
    ///
    /// - Held, and no limit or `requested` within the limit (inclusive) → [`Decision::Allowed`].
    /// - Held, but `requested` is above the limit → [`Decision::NeedsOverride`] with the limit.
    /// - **Not held at all** → [`Decision::NeedsOverride`] without a limit if
    ///   [`Permission::is_overridable`], otherwise [`Decision::Denied`]. Spec 2.1 §6 lets a
    ///   manager approve till actions the cashier lacks ("cashier triggers an action beyond their
    ///   permission → manager enters PIN"), so a cashier without `sale.void` gets the override
    ///   dialog. Back-office permissions cannot be borrowed through an override.
    /// - `requested` of the wrong kind for the permission (e.g. an amount for `sale.discount`, or
    ///   any value for `sale.create`) is a caller bug and fails closed → [`Decision::Denied`].
    #[must_use]
    pub fn check(&self, permission: Permission, requested: Option<Requested>) -> Decision {
        if let Some(value) = requested
            && Some(value.kind()) != permission.limit_kind()
        {
            return Decision::Denied;
        }
        match (self.limit(permission), requested) {
            (None, _) if permission.is_overridable() => Decision::NeedsOverride { limit: None },
            (None, _) => Decision::Denied,
            (Some(None), _) | (Some(Some(_)), None) => Decision::Allowed,
            (Some(Some(limit)), Some(value)) if limit.covers(value) => Decision::Allowed,
            (Some(Some(limit)), Some(_)) => Decision::NeedsOverride { limit: Some(limit) },
        }
    }

    /// [`check`](Self::check) as a `Result`, for handlers that cannot continue without approval.
    ///
    /// # Errors
    /// [`AccessError::NeedsOverride`] or [`AccessError::PermissionDenied`].
    pub fn require(
        &self,
        permission: Permission,
        requested: Option<Requested>,
    ) -> Result<(), AccessError> {
        match self.check(permission, requested) {
            Decision::Allowed => Ok(()),
            Decision::NeedsOverride { limit } => {
                Err(AccessError::NeedsOverride { permission, limit })
            }
            Decision::Denied => Err(AccessError::PermissionDenied { permission }),
        }
    }
}

impl FromIterator<Grant> for PermissionSet {
    fn from_iter<I: IntoIterator<Item = Grant>>(iter: I) -> Self {
        let mut set = Self::default();
        for grant in iter {
            set.insert(grant);
        }
        set
    }
}

/// The effective permissions of a user at `branch` (spec 2.1 §3): the union of the grants of
/// every role assigned for that branch or for all branches. The highest limit wins.
///
/// `assignments` must be **this user's** assignments. Assignments to a role missing from `roles`
/// (e.g. a deleted role) are ignored.
#[must_use]
pub fn effective_permissions(
    assignments: &[Assignment],
    roles: &[Role],
    branch: BranchId,
) -> PermissionSet {
    assignments
        .iter()
        .filter(|a| a.scope.applies_to(branch))
        .filter_map(|a| roles.iter().find(|r| r.id == a.role))
        .flat_map(|r| r.grants.iter().copied())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::access::BranchScope;
    use crate::ids::{RoleId, UserId};
    use crate::money::Money;

    fn pct(bp: u16) -> Limit {
        Limit::MaxPct(bp)
    }

    fn discount(bp: u16) -> Grant {
        Grant::limited(Permission::SaleDiscount, pct(bp)).unwrap()
    }

    fn role(name: &str, grants: Vec<Grant>) -> Role {
        Role {
            id: RoleId::generate(),
            name: name.into(),
            grants,
        }
    }

    #[test]
    fn check_table() {
        use Decision::{Allowed, Denied, NeedsOverride};
        use Permission::{CashOut, SaleCreate, SaleDiscount, SaleVoid, UserManage};
        let cashier: PermissionSet = [
            Grant::unlimited(SaleCreate),
            discount(1_000),
            Grant::limited(CashOut, Limit::MaxAmount(Money::from_minor(100_000))).unwrap(),
        ]
        .into_iter()
        .collect();
        let owner: PermissionSet = Permission::ALL.into_iter().map(Grant::unlimited).collect();
        let nobody = PermissionSet::default();
        let p = |bp| Some(Requested::Pct(bp));
        let amt = |m| Some(Requested::Amount(Money::from_minor(m)));

        #[rustfmt::skip]
        let cases = [
            // §9 case 1: 10% limit, 15% requested.
            ("over limit", &cashier, SaleDiscount, p(1_500), NeedsOverride { limit: Some(pct(1_000)) }),
            ("exactly at limit", &cashier, SaleDiscount, p(1_000), Allowed),
            ("below limit", &cashier, SaleDiscount, p(0), Allowed),
            ("limited, no value asked", &cashier, SaleDiscount, None, Allowed),
            ("amount limit", &cashier, CashOut, amt(100_001), NeedsOverride {
                limit: Some(Limit::MaxAmount(Money::from_minor(100_000))) }),
            ("plain permission", &cashier, SaleCreate, None, Allowed),
            ("lacks overridable", &cashier, SaleVoid, None, NeedsOverride { limit: None }),
            ("lacks back-office", &cashier, UserManage, None, Denied),
            ("no grants, overridable", &nobody, SaleDiscount, p(1), NeedsOverride { limit: None }),
            ("no grants, plain", &nobody, SaleCreate, None, Denied),
            ("owner is unlimited", &owner, SaleDiscount, p(10_000), Allowed),
            ("owner, max amount", &owner, CashOut, amt(i64::MAX), Allowed),
            ("wrong value kind", &owner, SaleDiscount, amt(1), Denied),
            ("value for plain permission", &owner, SaleCreate, p(1), Denied),
        ];
        for (name, set, permission, requested, expected) in cases {
            assert_eq!(set.check(permission, requested), expected, "{name}");
        }
    }

    #[test]
    fn require_maps_decisions_to_errors() {
        let set: PermissionSet = [discount(1_000)].into_iter().collect();
        assert_eq!(
            set.require(Permission::SaleDiscount, Some(Requested::Pct(500))),
            Ok(())
        );
        assert_eq!(
            set.require(Permission::SaleDiscount, Some(Requested::Pct(1_500))),
            Err(AccessError::NeedsOverride {
                permission: Permission::SaleDiscount,
                limit: Some(pct(1_000)),
            })
        );
        assert_eq!(
            set.require(Permission::AuditView, None),
            Err(AccessError::PermissionDenied {
                permission: Permission::AuditView
            })
        );
    }

    /// §9 case 5 and the merge rules.
    #[test]
    fn highest_limit_wins() {
        let cases = [
            (
                "5% + 10%",
                vec![discount(500), discount(1_000)],
                Some(pct(1_000)),
            ),
            (
                "10% + 5%",
                vec![discount(1_000), discount(500)],
                Some(pct(1_000)),
            ),
            (
                "limit + unlimited",
                vec![discount(500), Grant::unlimited(Permission::SaleDiscount)],
                None,
            ),
            (
                "unlimited + limit",
                vec![Grant::unlimited(Permission::SaleDiscount), discount(500)],
                None,
            ),
        ];
        for (name, grants, expected) in cases {
            let set: PermissionSet = grants.into_iter().collect();
            assert_eq!(
                set.limit(Permission::SaleDiscount),
                Some(expected),
                "{name}"
            );
        }
    }

    #[test]
    fn effective_permissions_by_branch() {
        let jkt = BranchId::generate();
        let bdg = BranchId::generate();
        let user = UserId::generate();
        let manager = role(
            "Manager",
            vec![discount(2_000), Grant::unlimited(Permission::SaleVoid)],
        );
        let cashier = role(
            "Cashier",
            vec![discount(1_000), Grant::unlimited(Permission::SaleCreate)],
        );
        let viewer = role("Viewer", vec![Grant::unlimited(Permission::ReportSales)]);
        let roles = [manager.clone(), cashier.clone(), viewer.clone()];
        let at = |role: &Role, scope| Assignment {
            user,
            role: role.id,
            scope,
        };

        // §9 case 4: Manager@JKT01 + Cashier@BDG01, logged in at BDG01 → Cashier only.
        let assignments = [
            at(&manager, BranchScope::Branch(jkt)),
            at(&cashier, BranchScope::Branch(bdg)),
        ];
        let at_bdg = effective_permissions(&assignments, &roles, bdg);
        let cashier_only: PermissionSet = cashier.grants.iter().copied().collect();
        assert_eq!(at_bdg, cashier_only);
        assert!(!at_bdg.contains(Permission::SaleVoid));
        assert_eq!(
            at_bdg.limit(Permission::SaleDiscount),
            Some(Some(pct(1_000)))
        );

        let at_jkt = effective_permissions(&assignments, &roles, jkt);
        assert_eq!(
            at_jkt.limit(Permission::SaleDiscount),
            Some(Some(pct(2_000)))
        );
        assert!(!at_jkt.contains(Permission::SaleCreate));

        // An all-branches role combines with a branch role.
        let assignments = [
            at(&viewer, BranchScope::All),
            at(&cashier, BranchScope::Branch(bdg)),
        ];
        let at_bdg = effective_permissions(&assignments, &roles, bdg);
        assert!(at_bdg.contains(Permission::ReportSales));
        assert!(at_bdg.contains(Permission::SaleCreate));
        let at_jkt = effective_permissions(&assignments, &roles, jkt);
        assert!(at_jkt.contains(Permission::ReportSales));
        assert!(!at_jkt.contains(Permission::SaleCreate));
    }

    #[test]
    fn no_assignments_or_missing_role_gives_nothing() {
        let branch = BranchId::generate();
        assert!(effective_permissions(&[], &[], branch).is_empty());

        let dangling = Assignment {
            user: UserId::generate(),
            role: RoleId::generate(),
            scope: BranchScope::All,
        };
        assert!(effective_permissions(&[dangling], &[], branch).is_empty());
    }
}
