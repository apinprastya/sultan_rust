//! Outbox (same transaction as the master write) and the append-only audit log.

use serde_json::json;
use sultan_db::audit::{self, AuditEntry, AuditFilter};
use sultan_db::{Db, org, outbox};

use crate::common::{branch, count, db_tests, org, raw, t};

db_tests!(
    outbox_rolls_back_with_the_master_write,
    outbox_list_and_mark_synced,
    audit_append_and_filter,
    audit_log_is_append_only,
);

async fn outbox_rolls_back_with_the_master_write(db: &Db) {
    let o = org(db).await;
    let before_branches = count(db, "branch").await;
    let before_events = count(db, "outbox").await;

    let mut tx = db.begin().await.unwrap();
    org::save_branch(&mut tx, &branch(o.company.id, "SBY01"), t(5))
        .await
        .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(count(db, "branch").await, before_branches);
    assert_eq!(count(db, "outbox").await, before_events);

    // Dropping a transaction without commit also rolls both back.
    {
        let mut tx = db.begin().await.unwrap();
        org::save_branch(&mut tx, &branch(o.company.id, "SBY01"), t(5))
            .await
            .unwrap();
    }
    assert_eq!(count(db, "outbox").await, before_events);

    let mut tx = db.begin().await.unwrap();
    let sby = branch(o.company.id, "SBY01");
    org::save_branch(&mut tx, &sby, t(5)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(count(db, "outbox").await, before_events + 1);
    let events = outbox::list_unsynced(db, 100).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, "branch.saved");
    assert_eq!(last.payload["code"], "SBY01");
    assert_eq!(last.payload["id"], sby.id.to_string());
}

async fn outbox_list_and_mark_synced(db: &Db) {
    let ids = [
        outbox::append(db, "a", &json!({"n": 1}), t(1))
            .await
            .unwrap(),
        outbox::append(db, "b", &json!({"n": 2}), t(2))
            .await
            .unwrap(),
        outbox::append(db, "c", &json!({"n": 3}), t(3))
            .await
            .unwrap(),
    ];
    let pending = outbox::list_unsynced(db, 2).await.unwrap();
    assert_eq!(
        pending.iter().map(|e| e.event_id).collect::<Vec<_>>(),
        ids[..2]
    );
    assert_eq!(pending[0].payload, json!({"n": 1}));
    assert_eq!(pending[0].created_at, t(1));
    assert_eq!(pending[0].synced_at, None);

    assert_eq!(outbox::mark_synced(db, &ids[..2], t(10)).await.unwrap(), 2);
    assert_eq!(outbox::mark_synced(db, &ids[..1], t(11)).await.unwrap(), 0);
    assert_eq!(outbox::mark_synced(db, &[], t(11)).await.unwrap(), 0);
    let pending = outbox::list_unsynced(db, 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].event_id, ids[2]);
}

async fn audit_append_and_filter(db: &Db) {
    let o = org(db).await;
    let mut login = AuditEntry::new(t(100), "login.failed");
    login.branch = Some(o.jkt.id);
    login.register = Some(o.register.id);
    login.user = Some(o.cashier.id);
    let mut role_save = AuditEntry::new(t(200), "role.save");
    role_save.user = Some(o.manager.id);
    role_save.target = Some("role:x".into());
    role_save.before = Some(json!({"name": "A"}));
    role_save.after = Some(json!({"name": "B", "grants": ["sale.create"]}));
    let mut override_ = AuditEntry::new(t(300), "override.approve");
    override_.branch = Some(o.bdg.id);

    // Inside the same transaction as a change, like the server does.
    let mut tx = db.begin().await.unwrap();
    audit::append(&mut tx, &login).await.unwrap();
    audit::append(&mut tx, &role_save).await.unwrap();
    tx.commit().await.unwrap();
    audit::append(db, &override_).await.unwrap();

    let all = audit::query(db, &AuditFilter::default()).await.unwrap();
    assert_eq!(
        all,
        vec![override_.clone(), role_save.clone(), login.clone()]
    );

    let check = |filter: AuditFilter, expected: Vec<AuditEntry>| async move {
        assert_eq!(
            audit::query(db, &filter).await.unwrap(),
            expected,
            "{filter:?}"
        );
    };
    check(
        AuditFilter {
            branch: Some(o.jkt.id),
            ..AuditFilter::default()
        },
        vec![login.clone()],
    )
    .await;
    check(
        AuditFilter {
            register: Some(o.register.id),
            user: Some(o.cashier.id),
            ..AuditFilter::default()
        },
        vec![login.clone()],
    )
    .await;
    check(
        AuditFilter {
            action: Some("role.save".into()),
            target: Some("role:x".into()),
            ..AuditFilter::default()
        },
        vec![role_save.clone()],
    )
    .await;
    check(
        AuditFilter {
            from: Some(t(200)),
            until: Some(t(300)),
            ..AuditFilter::default()
        },
        vec![role_save.clone()],
    )
    .await;
    // Paging: newest first, then older than the last ID seen.
    check(
        AuditFilter {
            limit: 1,
            ..AuditFilter::default()
        },
        vec![override_.clone()],
    )
    .await;
    check(
        AuditFilter {
            before_id: Some(override_.id),
            limit: 1,
            ..AuditFilter::default()
        },
        vec![role_save.clone()],
    )
    .await;
}

async fn audit_log_is_append_only(db: &Db) {
    audit::append(db, &AuditEntry::new(t(1), "user.unlock"))
        .await
        .unwrap();
    assert!(raw(db, "UPDATE audit_log SET action = 'x'").await.is_err());
    assert!(raw(db, "DELETE FROM audit_log").await.is_err());
    assert_eq!(count(db, "audit_log").await, 1);
}
