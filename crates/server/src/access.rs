//! Permission checks and branch visibility for handlers (spec 2.1 §3, §8). The rules themselves
//! live in `core::access`; this module only loads their inputs for the calling session.
//!
//! - Permissions are the caller's **effective permissions at the session's branch**
//!   ([`effective_permissions`]), loaded fresh on every request so role changes apply at once.
//! - "Reports and lookups only show branches the user is assigned to" (spec 2.1 §3): a caller
//!   who holds `org.manage`, or any role for all branches, sees every branch ([`BranchView::All`]);
//!   anyone else sees only the branches of their own assignments.
//! - The Owner invariant ([`ensure_owner_remains`]) is checked against the state **inside** the
//!   write transaction, after the change, so a rejected change rolls back together with its
//!   outbox row.

use std::collections::BTreeSet;

use sultan_core::access::{
    Assignment, BranchScope, Grant, Permission, PermissionSet, effective_permissions,
    ensure_active_owner_remains, ensure_can_grant, scope_permissions,
};
use sultan_core::ids::BranchId;
use sultan_db::roles::UserAccess;
use sultan_db::{Db, Tx, roles, seed, users};

use crate::error::ApiError;
use crate::extract::UserSession;

/// Which branches a caller may see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchView {
    /// Every branch.
    All,
    /// Only these branches.
    Only(BTreeSet<BranchId>),
}

impl BranchView {
    /// Whether `branch` is visible.
    #[must_use]
    pub fn allows(&self, branch: BranchId) -> bool {
        match self {
            Self::All => true,
            Self::Only(set) => set.contains(&branch),
        }
    }
}

/// What the caller may do: permissions at the session branch, plus their assignments.
#[derive(Debug, Clone)]
pub struct Caller {
    /// Effective permissions at the session's branch.
    pub permissions: PermissionSet,
    access: UserAccess,
}

impl Caller {
    /// Loads the caller's roles and assignments.
    ///
    /// # Errors
    /// The database error.
    pub async fn load(db: &Db, session: &UserSession) -> Result<Self, ApiError> {
        let access = roles::load_user_access(db, session.user.id).await?;
        let permissions =
            effective_permissions(&access.assignments, &access.roles, session.session.branch);
        Ok(Self {
            permissions,
            access,
        })
    }

    /// `403 permission_denied` unless the caller holds `permission`.
    ///
    /// # Errors
    /// See above.
    pub fn require(&self, permission: Permission) -> Result<(), ApiError> {
        Ok(self.permissions.require(permission, None)?)
    }

    /// `403 permission_denied` (naming the first) unless the caller holds one of `permissions`.
    ///
    /// # Errors
    /// See above.
    pub fn require_any(&self, permissions: &[Permission]) -> Result<(), ApiError> {
        if permissions.iter().any(|p| self.permissions.contains(*p)) {
            Ok(())
        } else {
            self.require(permissions[0])
        }
    }

    /// Assignments whose role still exists (archived roles grant nothing).
    fn live_assignments(&self) -> impl Iterator<Item = &Assignment> {
        self.access
            .assignments
            .iter()
            .filter(|a| self.access.roles.iter().any(|r| r.id == a.role))
    }

    /// The branches the caller may see (spec 2.1 §3, §8).
    #[must_use]
    pub fn branch_view(&self) -> BranchView {
        if self.permissions.contains(Permission::OrgManage) {
            return BranchView::All;
        }
        let mut only = BTreeSet::new();
        for a in self.live_assignments() {
            match a.scope {
                BranchScope::All => return BranchView::All,
                BranchScope::Branch(b) => {
                    only.insert(b);
                }
            }
        }
        BranchView::Only(only)
    }

    /// Whether `permission` comes from a role assigned for **all** branches, i.e. the caller holds
    /// it everywhere and not only at the session's branch.
    #[must_use]
    pub fn holds_everywhere(&self, permission: Permission) -> bool {
        self.at_scope(BranchScope::All).contains(permission)
    }

    /// Where the caller holds `permission`: `None` = at every branch (through an all-branches
    /// role), otherwise the branches of their single-branch assignments that grant it, by ID.
    /// Used to filter lists such as `GET /cashier-sessions` (spec 2.1 §3: lookups only show
    /// branches the user is assigned to).
    #[must_use]
    pub fn branches_with(&self, permission: Permission) -> Option<Vec<BranchId>> {
        if self.holds_everywhere(permission) {
            return None;
        }
        let branches: BTreeSet<BranchId> = self
            .live_assignments()
            .filter_map(|a| match a.scope {
                BranchScope::Branch(b) => Some(b),
                BranchScope::All => None,
            })
            .filter(|b| self.at_scope(BranchScope::Branch(*b)).contains(permission))
            .collect();
        Some(branches.into_iter().collect())
    }

    /// The caller's permissions everywhere in `scope` (`core::access::scope_permissions`).
    #[must_use]
    pub fn at_scope(&self, scope: BranchScope) -> PermissionSet {
        scope_permissions(&self.access.assignments, &self.access.roles, scope)
    }

    /// `403 permission_denied` (`reason: grant_exceeds_own`) unless the caller holds every grant
    /// in `grants` with a limit at least as high, everywhere in `scope` (spec 2.1 §3: nobody hands
    /// out more than they hold).
    ///
    /// # Errors
    /// See above.
    pub fn ensure_can_grant(&self, scope: BranchScope, grants: &[Grant]) -> Result<(), ApiError> {
        Ok(ensure_can_grant(&self.at_scope(scope), grants)?)
    }
}

/// Checks the Owner invariant against the state inside `tx` (spec 2.1 §3, §9 case 6).
///
/// # Errors
/// `409 last_owner`, or the database error.
pub async fn ensure_owner_remains(tx: &mut Tx) -> Result<(), ApiError> {
    let roles = roles::list_roles(&mut *tx).await?;
    let assignments = roles::list_assignments(&mut *tx).await?;
    let statuses = users::list_statuses(&mut *tx).await?;
    ensure_active_owner_remains(seed::OWNER_ROLE, &roles, &assignments, &statuses)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_view_allows() {
        let a = BranchId::generate();
        let b = BranchId::generate();
        assert!(BranchView::All.allows(a));
        let only = BranchView::Only([a].into_iter().collect());
        assert!(only.allows(a));
        assert!(!only.allows(b));
    }
}
