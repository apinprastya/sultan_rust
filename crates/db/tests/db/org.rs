//! Company, branches, registers, locations and users.

use sultan_core::ids::{BranchId, LocationId, UserId};
use sultan_db::org::{self, StockLocation};
use sultan_db::users;
use sultan_db::{Db, DbError, Saved};

use crate::common::{branch, count, db_tests, org, t, user};

db_tests!(
    migrate_is_idempotent,
    org_round_trip,
    put_replay_updates_in_place,
    duplicate_codes_conflict,
    missing_reference_is_reported,
    users_round_trip,
);

async fn migrate_is_idempotent(db: &Db) {
    db.migrate().await.unwrap();
    db.migrate().await.unwrap();
}

async fn org_round_trip(db: &Db) {
    let o = org(db).await;

    assert_eq!(org::get_company(db).await.unwrap(), Some(o.company.clone()));
    assert_eq!(
        org::get_branch(db, o.jkt.id).await.unwrap(),
        Some(o.jkt.clone())
    );
    assert_eq!(
        org::find_branch_by_code(db, "BDG01").await.unwrap(),
        Some(o.bdg.clone())
    );
    assert_eq!(
        org::get_branch(db, BranchId::generate()).await.unwrap(),
        None
    );
    assert_eq!(
        org::list_branches(db).await.unwrap(),
        vec![o.bdg.clone(), o.jkt.clone()]
    );

    assert_eq!(
        org::get_register(db, o.register.id).await.unwrap(),
        Some(o.register.clone())
    );
    assert_eq!(
        org::list_registers(db, Some(o.jkt.id)).await.unwrap(),
        vec![o.register.clone()]
    );
    assert!(
        org::list_registers(db, Some(o.bdg.id))
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(org::list_registers(db, None).await.unwrap().len(), 1);

    assert_eq!(
        org::get_location(db, o.location.id).await.unwrap(),
        Some(o.location.clone())
    );
    assert_eq!(
        org::list_locations(db, Some(o.jkt.id)).await.unwrap(),
        vec![o.location.clone()]
    );
    assert_eq!(
        org::list_locations(db, None).await.unwrap(),
        vec![o.location.clone()]
    );

    // company, 2 branches, location, register, 2 users.
    assert_eq!(count(db, "outbox").await, 7);
}

async fn put_replay_updates_in_place(db: &Db) {
    let o = org(db).await;
    let mut changed = o.jkt.clone();
    changed.name = "Jakarta Pusat".into();
    changed.npwp = Some("01.234.567.8-901.000".into());
    changed.archived = true;

    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        org::save_branch(&mut tx, &changed, t(2_000)).await.unwrap(),
        Saved::Updated
    );
    assert_eq!(
        org::save_branch(&mut tx, &changed, t(2_000)).await.unwrap(),
        Saved::Updated
    );
    let warehouse = StockLocation {
        id: LocationId::generate(),
        branch: o.jkt.id,
        name: "Warehouse".into(),
        receiving_default: false,
        archived: false,
    };
    assert_eq!(
        org::save_location(&mut tx, &warehouse, t(2_000))
            .await
            .unwrap(),
        Saved::Created
    );
    tx.commit().await.unwrap();

    assert_eq!(count(db, "branch").await, 2);
    assert_eq!(org::get_branch(db, o.jkt.id).await.unwrap(), Some(changed));
    assert_eq!(
        org::list_locations(db, Some(o.jkt.id)).await.unwrap().len(),
        2
    );
}

async fn duplicate_codes_conflict(db: &Db) {
    let o = org(db).await;
    let mut tx = db.begin().await.unwrap();
    let dup = branch(o.company.id, "JKT01");
    let err = org::save_branch(&mut tx, &dup, t(2_000)).await.unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "{err}");
    drop(tx);

    let mut tx = db.begin().await.unwrap();
    let mut other = user("ani");
    other.id = UserId::generate();
    let err = users::save_user(&mut tx, &other, t(2_000))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::Conflict(_)), "{err}");
}

async fn missing_reference_is_reported(db: &Db) {
    let o = org(db).await;
    let mut orphan = o.register.clone();
    orphan.id = sultan_core::ids::RegisterId::generate();
    orphan.code = "R9".into();
    orphan.branch = BranchId::generate();
    let mut tx = db.begin().await.unwrap();
    let err = org::save_register(&mut tx, &orphan, t(2_000))
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::MissingReference(_)), "{err}");
}

async fn users_round_trip(db: &Db) {
    let o = org(db).await;
    assert_eq!(
        users::get_user(db, o.cashier.id).await.unwrap(),
        Some(o.cashier.clone())
    );
    assert_eq!(
        users::find_by_username(db, "budi").await.unwrap(),
        Some(o.manager.clone())
    );
    assert_eq!(users::find_by_username(db, "nobody").await.unwrap(), None);
    assert_eq!(
        users::list_users(db).await.unwrap(),
        vec![o.cashier.clone(), o.manager.clone()]
    );

    let mut tx = db.begin().await.unwrap();
    let mut changed = o.manager.clone();
    changed.active = false;
    changed.pin_hash = None;
    changed.password_hash = Some("$argon2id$pw".into());
    assert_eq!(
        users::save_user(&mut tx, &changed, t(3_000)).await.unwrap(),
        Saved::Updated
    );
    tx.commit().await.unwrap();
    assert_eq!(
        users::get_user(db, o.manager.id).await.unwrap(),
        Some(changed)
    );

    let mut statuses = users::list_statuses(db).await.unwrap();
    statuses.sort_by_key(|s| s.id);
    let mut expected = vec![
        sultan_core::access::UserStatus {
            id: o.cashier.id,
            active: true,
        },
        sultan_core::access::UserStatus {
            id: o.manager.id,
            active: false,
        },
    ];
    expected.sort_by_key(|s| s.id);
    assert_eq!(statuses, expected);
}
