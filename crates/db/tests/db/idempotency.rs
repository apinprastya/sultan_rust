//! `Idempotency-Key` storage (spec 2.1 §9 case 17).

use sultan_db::Db;
use sultan_db::idempotency::{self, Begin, KeyedRequest, StoredResponse};

use crate::common::{count, db_tests, hash, t};

db_tests!(
    first_request_reserves_then_replays,
    mismatch_and_in_progress,
    abandon_stale_and_retention,
);

fn request<'a>(key: &'a str, path: &'a str, body: &'a [u8; 32]) -> KeyedRequest<'a> {
    KeyedRequest {
        caller: body_caller(),
        key,
        method: "POST",
        path,
        request_hash: body,
    }
}

fn body_caller() -> &'static [u8; 32] {
    static CALLER: [u8; 32] = [7; 32];
    &CALLER
}

fn created() -> StoredResponse {
    StoredResponse {
        status: 201,
        content_type: Some("application/json".into()),
        body: r#"{"id":"x"}"#.into(),
    }
}

async fn begin(db: &Db, r: &KeyedRequest<'_>, now: i64) -> Begin {
    idempotency::begin(db, r, t(now), t(now - 1_000), t(now - 100))
        .await
        .unwrap()
}

async fn first_request_reserves_then_replays(db: &Db) {
    let h = hash(1);
    let r = request("k1", "/api/v1/cashier-sessions", &h);
    assert_eq!(begin(db, &r, 500).await, Begin::Started);
    idempotency::complete(db, r.caller, r.key, &created())
        .await
        .unwrap();
    assert_eq!(begin(db, &r, 510).await, Begin::Replay(created()));

    // An empty body without content type (204) round-trips too.
    let r2 = request("k2", "/api/v1/auth/logout", &h);
    assert_eq!(begin(db, &r2, 520).await, Begin::Started);
    let empty = StoredResponse {
        status: 204,
        content_type: None,
        body: String::new(),
    };
    idempotency::complete(db, r2.caller, r2.key, &empty)
        .await
        .unwrap();
    assert_eq!(begin(db, &r2, 530).await, Begin::Replay(empty.clone()));

    // Completing twice, or a key that was never reserved, is an error.
    assert!(
        idempotency::complete(db, r2.caller, r2.key, &empty)
            .await
            .is_err()
    );
    // The same key from another caller is independent.
    let other = [8; 32];
    let mut r3 = request("k1", "/api/v1/cashier-sessions", &h);
    r3.caller = &other;
    assert_eq!(begin(db, &r3, 540).await, Begin::Started);
}

async fn mismatch_and_in_progress(db: &Db) {
    let h = hash(1);
    let r = request("k", "/a", &h);
    assert_eq!(begin(db, &r, 500).await, Begin::Started);
    // Still running.
    assert_eq!(begin(db, &r, 550).await, Begin::InProgress);
    // Another body, path or method under the same key.
    let other_hash = hash(2);
    assert_eq!(
        begin(db, &request("k", "/a", &other_hash), 550).await,
        Begin::Mismatch
    );
    assert_eq!(
        begin(db, &request("k", "/b", &h), 550).await,
        Begin::Mismatch
    );
    let mut put = request("k", "/a", &h);
    put.method = "PUT";
    assert_eq!(begin(db, &put, 550).await, Begin::Mismatch);
}

async fn abandon_stale_and_retention(db: &Db) {
    let h = hash(1);
    let r = request("k", "/a", &h);
    assert_eq!(begin(db, &r, 500).await, Begin::Started);
    // Abandoned: the key can be used again at once.
    idempotency::abandon(db, r.caller, r.key).await.unwrap();
    assert_eq!(begin(db, &r, 510).await, Begin::Started);
    // A reservation older than the stale cut-off (100 ms in these tests) is taken over.
    assert_eq!(begin(db, &r, 650).await, Begin::Started);

    idempotency::complete(db, r.caller, r.key, &created())
        .await
        .unwrap();
    assert_eq!(count(db, "idempotency_key").await, 1);
    // A completed row is never stale, only expired after the retention window (1 s here).
    assert_eq!(begin(db, &r, 1_600).await, Begin::Replay(created()));
    assert_eq!(begin(db, &r, 1_700).await, Begin::Started);
    assert_eq!(idempotency::purge(db, t(10_000)).await.unwrap(), 1);
    assert_eq!(count(db, "idempotency_key").await, 0);
}
