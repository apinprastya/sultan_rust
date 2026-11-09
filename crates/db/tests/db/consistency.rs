//! The two backends must behave the same (Phase 9). `SQLite` compares `TEXT` byte-wise
//! (`BINARY` collation); `MySQL` 8's default `utf8mb4_0900_ai_ci` ignores case and accents, which
//! made usernames, codes and `Idempotency-Key`s match values that `SQLite` keeps apart, and sorted
//! lists differently. The `MySQL` schema now uses `utf8mb4_0900_bin`, so text is compared and
//! sorted byte-wise on both (see `migrations/mysql/20261108000002_binary_collation.sql`).

use sultan_core::time::Timestamp;
use sultan_db::audit::{self, AuditEntry, AuditFilter};
use sultan_db::idempotency::{self, Begin, KeyedRequest};
use sultan_db::{Db, org, users};

use crate::common::{branch, db_tests, hash, org, t, user};

db_tests!(
    usernames_are_case_and_accent_sensitive,
    codes_are_case_sensitive_and_sorted_bytewise,
    idempotency_keys_are_case_sensitive,
    audit_filters_match_exactly,
    trailing_spaces_are_significant,
    read_transactions_read_one_snapshot,
);

/// `Db::begin_read` reads like any transaction; on `MySQL` it refuses writes (`READ ONLY`).
async fn read_transactions_read_one_snapshot(db: &Db) {
    let o = org(db).await;
    let mut tx = db.begin_read().await.unwrap();
    let ani = users::find_by_username(&mut tx, "ani").await.unwrap();
    assert_eq!(ani.map(|u| u.username).as_deref(), Some("ani"));
    let write = org::save_branch(&mut tx, &branch(o.company.id, "RO01"), t(1)).await;
    match db {
        Db::MySql(_) => assert!(write.is_err(), "a read-only transaction wrote"),
        Db::Sqlite(_) => assert!(write.is_ok(), "{write:?}"),
    }
    tx.rollback().await.unwrap();
}

async fn usernames_are_case_and_accent_sensitive(db: &Db) {
    org(db).await; // creates `ani` and `budi`
    let mut tx = db.begin().await.unwrap();
    for name in ["Ani", "josé", "jose"] {
        users::save_user(&mut tx, &user(name), t(1)).await.unwrap();
    }
    tx.commit().await.unwrap();
    #[rustfmt::skip]
    let cases = [
        ("ani", Some("ani")),
        ("Ani", Some("Ani")),
        ("ANI", None),
        ("josé", Some("josé")),
        ("jose", Some("jose")),
        ("JOSE", None),
    ];
    for (lookup, found) in cases {
        let got = users::find_by_username(db, lookup).await.unwrap();
        assert_eq!(got.map(|u| u.username).as_deref(), found, "{lookup}");
    }
}

async fn codes_are_case_sensitive_and_sorted_bytewise(db: &Db) {
    let o = org(db).await; // JKT01, BDG01
    let mut tx = db.begin().await.unwrap();
    org::save_branch(&mut tx, &branch(o.company.id, "jkt01"), t(1))
        .await
        .unwrap();
    org::save_branch(&mut tx, &branch(o.company.id, "Bdg02"), t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        org::find_branch_by_code(db, "jkt01")
            .await
            .unwrap()
            .map(|b| b.code),
        Some("jkt01".to_owned())
    );
    assert_eq!(org::find_branch_by_code(db, "Jkt01").await.unwrap(), None);
    let codes: Vec<String> = org::list_branches(db)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.code)
        .collect();
    // Byte order: upper case before lower case.
    assert_eq!(codes, ["BDG01", "Bdg02", "JKT01", "jkt01"]);
}

async fn idempotency_keys_are_case_sensitive(db: &Db) {
    let now = Timestamp::from_unix_millis(10_000);
    let old = Timestamp::from_unix_millis(0);
    let request = |key: &'static str, body: u8| (key, hash(body));
    let caller = hash(1);
    for (key, body) in [request("key-a", 2), request("KEY-A", 3)] {
        let keyed = KeyedRequest {
            caller: &caller,
            key,
            method: "POST",
            path: "/api/v1/cashier-sessions",
            request_hash: &body,
        };
        let got = idempotency::begin(db, &keyed, now, old, old).await.unwrap();
        assert_eq!(got, Begin::Started, "{key} is a new key");
    }
}

async fn audit_filters_match_exactly(db: &Db) {
    for action in ["user.save", "USER.SAVE"] {
        audit::append(db, &AuditEntry::new(t(5), action))
            .await
            .unwrap();
    }
    let filter = AuditFilter {
        action: Some("user.save".to_owned()),
        ..AuditFilter::default()
    };
    let found = audit::query(db, &filter).await.unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].action, "user.save");
}

async fn trailing_spaces_are_significant(db: &Db) {
    org(db).await;
    let mut tx = db.begin().await.unwrap();
    users::save_user(&mut tx, &user("ani "), t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let got = users::find_by_username(db, "ani").await.unwrap().unwrap();
    assert_eq!(got.username, "ani");
}
