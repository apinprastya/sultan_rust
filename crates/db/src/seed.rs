//! Default roles and the permission catalog check (spec 2.1 §3, plan Phase 3 "Seed").
//!
//! Written in Rust because the grants come from [`Permission`]. The roles have **fixed,
//! well-known IDs**, so every node (HQ, branches, standalone) seeds the same rows and no outbox
//! events are needed.

use sultan_core::access::{Grant, Limit, Permission, Role};
use sultan_core::ids::RoleId;
use sultan_core::money::Money;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::conn::{Conn, Db};
use crate::error::{DbError, Result};
use crate::roles;

/// Well-known role IDs. They are valid `UUIDv7` values with a zero timestamp.
const fn well_known(n: u128) -> RoleId {
    RoleId::from_uuid(Uuid::from_u128(
        0x0000_0000_0000_7000_8000_0000_0000_0000 | n,
    ))
}

/// Owner: every permission, no limits; cannot be deleted or emptied (spec 2.1 §3).
pub const OWNER_ROLE: RoleId = well_known(1);
/// Manager: runs a branch.
pub const MANAGER_ROLE: RoleId = well_known(2);
/// Cashier: sells at the register.
pub const CASHIER_ROLE: RoleId = well_known(3);
/// Stock clerk: stock work.
pub const STOCK_CLERK_ROLE: RoleId = well_known(4);
/// Viewer: reports only.
pub const VIEWER_ROLE: RoleId = well_known(5);

/// Rupiah amount in minor units (minor unit = 1, `docs/plan.md` §1).
const fn rp(amount: i64) -> Limit {
    Limit::MaxAmount(Money::from_minor(amount))
}

/// Percentage in basis points.
const fn pct(percent: u16) -> Limit {
    Limit::MaxPct(percent * 100)
}

/// The five seeded roles with their default grants (spec 2.1 §3; catalog grants spec 2.2 §11,
/// delivered to existing installs by the migration `20261109000001_catalog.sql`; customer grants
/// spec 2.5 §12, by `20261111000002_customers.sql`; pricing grants spec 2.3 §6, §9, by
/// `20261111000003_pricing.sql`, which adds `price.manage` but keeps existing `price.override`
/// grants as they are, so an older Manager keeps an unlimited one). Only Owner is reset by
/// [`seed_defaults`]; the others are created once and are then editable.
///
/// # Panics
/// Never: every limit below is valid for its permission (checked by the unit tests).
#[must_use]
pub fn default_roles() -> Vec<Role> {
    use Permission as P;
    let role = |id, name: &str, grants: &[(Permission, Option<Limit>)]| Role {
        id,
        name: name.to_owned(),
        grants: grants
            .iter()
            .map(|&(p, limit)| match limit {
                None => Grant::unlimited(p),
                Some(l) => Grant::limited(p, l).expect("seed limits are valid"),
            })
            .collect(),
    };
    vec![
        Role::owner(OWNER_ROLE),
        role(
            MANAGER_ROLE,
            "Manager",
            &[
                (P::SaleCreate, None),
                (P::SaleDiscount, Some(pct(20))),
                (P::SaleCartDiscount, Some(pct(20))),
                (P::SaleVoidLine, None),
                (P::SaleVoid, None),
                (P::SalePostVoid, None),
                (P::PriceOverride, Some(pct(20))),
                (P::RefundWithReceipt, None),
                (P::RefundNoReceipt, Some(rp(1_000_000))),
                (P::CashIn, None),
                (P::CashOut, Some(rp(2_000_000))),
                (P::DrawerOpen, None),
                (P::SessionOpenClose, None),
                (P::SessionViewExpected, None),
                (P::SessionForceClose, None),
                (P::UserUnlock, None),
                (P::StockAdjust, None),
                (P::ReportSales, None),
                (P::ReportProfit, None),
                (P::AuditView, None),
                (P::CatalogManage, None),
                (P::CatalogViewArchived, None),
                (P::CustomerCreate, None),
                (P::CustomerSearch, None),
                (P::CustomerEdit, None),
                (P::CustomerManage, None),
                (P::CustomerBalancePayout, None),
                (P::PriceManage, None),
            ],
        ),
        role(
            CASHIER_ROLE,
            "Cashier",
            &[
                (P::SaleCreate, None),
                (P::SaleDiscount, Some(pct(10))),
                (P::SaleCartDiscount, Some(pct(5))),
                (P::SaleVoidLine, None),
                (P::RefundWithReceipt, None),
                (P::CashIn, None),
                (P::SessionOpenClose, None),
                (P::CustomerCreate, None),
            ],
        ),
        role(
            STOCK_CLERK_ROLE,
            "Stock clerk",
            &[(P::StockAdjust, None), (P::CatalogManage, None)],
        ),
        role(
            VIEWER_ROLE,
            "Viewer",
            &[(P::ReportSales, None), (P::ReportProfit, None)],
        ),
    ]
}

/// Seeds the default roles and checks the permission catalog. Idempotent; run at every start.
///
/// - Missing seeded roles are created with [`default_roles`]. Existing ones (except Owner) are
///   left as edited.
/// - The Owner role is always rewritten to the full catalog without limits, so a new permission
///   added to `core` reaches Owner on upgrade (spec 2.1 §3).
/// - Every permission key stored in any role must exist in the catalog; otherwise the database
///   was written by a newer version and the call fails rather than silently dropping grants.
///
/// # Errors
/// [`DbError::Corrupt`] listing unknown permission keys, or the database error.
pub async fn seed_defaults(db: &Db, now: Timestamp) -> Result<()> {
    let mut tx = db.begin().await?;
    for role in default_roles() {
        let exists = crate::sql::fetch_optional(
            Conn::Tx(&mut tx),
            "SELECT 1 FROM role WHERE id = ?",
            crate::sql::args![role.id],
            |_| Ok(()),
        )
        .await?
        .is_some();
        if role.id == OWNER_ROLE || !exists {
            roles::write_role(&mut tx, &role, now).await?;
        }
    }
    let unknown: Vec<String> = roles::stored_permission_keys(Conn::Tx(&mut tx))
        .await?
        .into_iter()
        .filter(|key| key.parse::<Permission>().is_err())
        .collect();
    if !unknown.is_empty() {
        return Err(DbError::Corrupt(format!(
            "unknown permission keys in role_permission: {}",
            unknown.join(", ")
        )));
    }
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roles_are_valid_and_have_fixed_ids() {
        let roles = default_roles();
        let ids: Vec<_> = roles.iter().map(|r| r.id).collect();
        assert_eq!(
            ids,
            [
                OWNER_ROLE,
                MANAGER_ROLE,
                CASHIER_ROLE,
                STOCK_CLERK_ROLE,
                VIEWER_ROLE
            ]
        );
        assert_eq!(
            OWNER_ROLE.to_string(),
            "00000000-0000-7000-8000-000000000001"
        );
        assert_eq!(OWNER_ROLE.as_uuid().get_version_num(), 7);
        assert!(roles[0].grants_everything_unlimited());
        assert!(roles[1..].iter().all(|r| !r.grants_everything_unlimited()));
    }
}
