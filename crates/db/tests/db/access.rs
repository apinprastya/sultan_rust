//! Roles, grants, assignments, effective-permission loading and the seed.

use sultan_core::access::{
    BranchScope, Decision, Grant, Limit, Permission, Requested, Role, effective_permissions,
    ensure_active_owner_remains,
};
use sultan_core::ids::{RoleId, UserId};
use sultan_core::money::Money;
use sultan_db::seed::{self, CASHIER_ROLE, MANAGER_ROLE, OWNER_ROLE, VIEWER_ROLE};
use sultan_db::{Db, DbError, Saved, roles, users};

use crate::common::{count, db_tests, org, raw, t};

db_tests!(
    role_with_grants_round_trip,
    archived_role_grants_nothing,
    assignments_feed_effective_permissions,
    owner_invariant_inputs,
    seed_is_idempotent_and_restores_owner,
    seed_rejects_unknown_permission_keys,
    corrupt_limit_is_reported,
);

fn sample_role(name: &str) -> Role {
    Role {
        id: RoleId::generate(),
        name: name.to_owned(),
        grants: vec![
            Grant::unlimited(Permission::SaleCreate),
            Grant::limited(Permission::SaleDiscount, Limit::MaxPct(1_500)).unwrap(),
            Grant::limited(
                Permission::CashOut,
                Limit::MaxAmount(Money::from_minor(250_000)),
            )
            .unwrap(),
        ],
    }
}

/// Grants come back ordered by permission key.
fn sorted(mut role: Role) -> Role {
    role.grants.sort_by_key(|g| g.permission().as_str());
    role
}

async fn role_with_grants_round_trip(db: &Db) {
    let role = sample_role("Supervisor");
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        roles::save_role(&mut tx, &role, t(1)).await.unwrap(),
        Saved::Created
    );
    tx.commit().await.unwrap();
    assert_eq!(
        roles::get_role(db, role.id).await.unwrap(),
        Some(sorted(role.clone()))
    );

    // Overwrite: grants are replaced, not merged.
    let mut edited = role.clone();
    edited.name = "Shift lead".into();
    edited.grants = vec![Grant::unlimited(Permission::DrawerOpen)];
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        roles::save_role(&mut tx, &edited, t(2)).await.unwrap(),
        Saved::Updated
    );
    tx.commit().await.unwrap();
    assert_eq!(roles::list_roles(db).await.unwrap(), vec![edited]);
    assert_eq!(count(db, "role_permission").await, 1);

    let mut tx = db.begin().await.unwrap();
    let dup = sample_role("Shift lead");
    let err = roles::save_role(&mut tx, &dup, t(3)).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "{err}");
}

async fn archived_role_grants_nothing(db: &Db) {
    let o = org(db).await;
    let role = sample_role("Temp");
    let mut tx = db.begin().await.unwrap();
    roles::save_role(&mut tx, &role, t(1)).await.unwrap();
    roles::replace_assignments(&mut tx, o.cashier.id, &[(role.id, BranchScope::All)], t(1))
        .await
        .unwrap();
    roles::archive_role(&mut tx, role.id, t(2)).await.unwrap();
    let err = roles::archive_role(&mut tx, RoleId::generate(), t(2))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound("role")));
    tx.commit().await.unwrap();

    assert_eq!(roles::get_role(db, role.id).await.unwrap(), None);
    assert!(roles::list_roles(db).await.unwrap().is_empty());
    let access = roles::load_user_access(db, o.cashier.id).await.unwrap();
    assert_eq!(access.assignments.len(), 1, "assignment kept for history");
    assert!(access.roles.is_empty());
    assert!(
        users::list_for_branch(db, o.jkt.id)
            .await
            .unwrap()
            .is_empty()
    );

    // Saving it again restores it.
    let mut tx = db.begin().await.unwrap();
    roles::save_role(&mut tx, &role, t(3)).await.unwrap();
    tx.commit().await.unwrap();
    assert!(roles::get_role(db, role.id).await.unwrap().is_some());
}

/// Spec 2.1 §9 case 4 through the database: Manager@JKT01 + Cashier@BDG01, logged in at BDG01 →
/// Cashier permissions only.
async fn assignments_feed_effective_permissions(db: &Db) {
    let o = org(db).await;
    seed::seed_defaults(db, t(1)).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    let assignments = [
        (MANAGER_ROLE, BranchScope::Branch(o.jkt.id)),
        (CASHIER_ROLE, BranchScope::Branch(o.bdg.id)),
        (CASHIER_ROLE, BranchScope::Branch(o.bdg.id)), // duplicate, dropped
        (VIEWER_ROLE, BranchScope::All),
    ];
    roles::replace_assignments(&mut tx, o.cashier.id, &assignments, t(2))
        .await
        .unwrap();
    tx.commit().await.unwrap();

    let access = roles::load_user_access(db, o.cashier.id).await.unwrap();
    assert_eq!(access.assignments.len(), 3);
    assert_eq!(access.roles.len(), 3);
    assert_eq!(
        roles::assignments_for_user(db, o.cashier.id)
            .await
            .unwrap()
            .len(),
        3
    );

    let at_bdg = effective_permissions(&access.assignments, &access.roles, o.bdg.id);
    assert_eq!(
        at_bdg.check(Permission::SaleDiscount, Some(Requested::Pct(1_500))),
        Decision::NeedsOverride {
            limit: Some(Limit::MaxPct(1_000))
        }
    );
    assert!(!at_bdg.contains(Permission::SessionForceClose));
    assert!(
        at_bdg.contains(Permission::ReportSales),
        "Viewer at all branches"
    );

    let at_jkt = effective_permissions(&access.assignments, &access.roles, o.jkt.id);
    assert_eq!(
        at_jkt.check(Permission::SaleDiscount, Some(Requested::Pct(1_500))),
        Decision::Allowed
    );

    // The lock screen lists users with a role at the branch.
    let at_bdg_users = users::list_for_branch(db, o.bdg.id).await.unwrap();
    assert_eq!(at_bdg_users, vec![o.cashier.clone()]);

    // Replacing with an empty list removes everything.
    let mut tx = db.begin().await.unwrap();
    roles::replace_assignments(&mut tx, o.cashier.id, &[], t(3))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(roles::list_assignments(db).await.unwrap().is_empty());
}

async fn owner_invariant_inputs(db: &Db) {
    let o = org(db).await;
    seed::seed_defaults(db, t(1)).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    roles::replace_assignments(
        &mut tx,
        o.manager.id,
        &[(OWNER_ROLE, BranchScope::All)],
        t(2),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let all_roles = roles::list_roles(db).await.unwrap();
    let assignments = roles::list_assignments(db).await.unwrap();
    let mut statuses = users::list_statuses(db).await.unwrap();
    assert_eq!(
        ensure_active_owner_remains(OWNER_ROLE, &all_roles, &assignments, &statuses),
        Ok(())
    );
    // Proposed change: deactivate the only Owner → rejected by core.
    for s in &mut statuses {
        s.active = s.id != o.manager.id;
    }
    assert!(ensure_active_owner_remains(OWNER_ROLE, &all_roles, &assignments, &statuses).is_err());
}

async fn seed_is_idempotent_and_restores_owner(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    let first = roles::list_roles(db).await.unwrap();
    assert_eq!(first.len(), 5);
    let expected: Vec<Role> = {
        let mut v: Vec<Role> = seed::default_roles().into_iter().map(sorted).collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    };
    assert_eq!(first, expected);

    // Edit Manager and empty Owner; a second seed keeps the Manager edit and restores Owner.
    let mut manager = roles::get_role(db, MANAGER_ROLE).await.unwrap().unwrap();
    manager.grants.truncate(1);
    let mut owner = roles::get_role(db, OWNER_ROLE).await.unwrap().unwrap();
    owner.grants.clear();
    let mut tx = db.begin().await.unwrap();
    roles::save_role(&mut tx, &manager, t(2)).await.unwrap();
    roles::save_role(&mut tx, &owner, t(2)).await.unwrap();
    tx.commit().await.unwrap();

    seed::seed_defaults(db, t(3)).await.unwrap();
    seed::seed_defaults(db, t(4)).await.unwrap();
    assert_eq!(count(db, "role").await, 5);
    assert_eq!(
        roles::get_role(db, MANAGER_ROLE).await.unwrap(),
        Some(manager)
    );
    let owner = roles::get_role(db, OWNER_ROLE).await.unwrap().unwrap();
    assert!(owner.grants_everything_unlimited());
    assert_eq!(owner.grants.len(), Permission::ALL.len());
}

async fn seed_rejects_unknown_permission_keys(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    let role = sample_role("Legacy");
    let mut tx = db.begin().await.unwrap();
    roles::save_role(&mut tx, &role, t(1)).await.unwrap();
    tx.commit().await.unwrap();
    let hex = role.id.as_uuid().simple();
    raw(
        db,
        &format!(
            "INSERT INTO role_permission (role_id, perm, limit_value) \
             VALUES (X'{hex}', 'sale.teleport', NULL)"
        ),
    )
    .await
    .unwrap();

    let err = seed::seed_defaults(db, t(2)).await.unwrap_err();
    assert!(err.to_string().contains("sale.teleport"), "{err}");
    let err = roles::get_role(db, role.id).await.unwrap_err();
    assert!(matches!(err, DbError::Corrupt(_)), "{err}");
}

async fn corrupt_limit_is_reported(db: &Db) {
    let role = sample_role("Broken");
    let mut tx = db.begin().await.unwrap();
    roles::save_role(&mut tx, &role, t(1)).await.unwrap();
    tx.commit().await.unwrap();
    raw(
        db,
        "UPDATE role_permission SET limit_value = 99999 WHERE perm = 'sale.discount'",
    )
    .await
    .unwrap();
    let err = roles::list_roles(db).await.unwrap_err();
    assert!(err.to_string().contains("sale.discount"), "{err}");
    // Unrelated reads still work.
    assert!(
        users::get_user(db, UserId::generate())
            .await
            .unwrap()
            .is_none()
    );
}
