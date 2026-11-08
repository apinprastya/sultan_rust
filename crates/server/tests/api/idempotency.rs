//! `Idempotency-Key` on action `POST`s over HTTP (spec 2.1 §9 case 17, `docs/plan.md` §1).

use axum::http::{Method, StatusCode};
use serde_json::json;

use crate::common::{ANI_PIN, BUDI_PIN, Harness, api_tests};

api_tests!(
    retried_open_returns_the_original_response,
    retried_close_and_logout_replay,
    key_rules_and_unkeyed_endpoints,
);

const OPEN: &str = "/api/v1/cashier-sessions";

async fn retried_open_returns_the_original_response(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let key = [("idempotency-key", "open-1")];
    let body = || Some(json!({ "opening_cash": 500_000 }));

    let first = h.at_register(at, Method::POST, OPEN, &key, body()).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert!(first.headers.get("idempotency-replayed").is_none());

    // The retry is not executed again: same 201 and body, no second session, no 409.
    let retry = h.at_register(at, Method::POST, OPEN, &key, body()).await;
    assert_eq!(
        (retry.status, &retry.body),
        (StatusCode::CREATED, &first.body)
    );
    assert_eq!(retry.headers["idempotency-replayed"], "true");
    assert_eq!(retry.headers["content-type"], "application/json");
    assert_eq!(h.audit_count("cashier_session.open").await, 1);

    // The same key with another body is refused.
    h.at_register(
        at,
        Method::POST,
        OPEN,
        &key,
        Some(json!({ "opening_cash": 1 })),
    )
    .await
    .assert_problem(422, "idempotency_key_reused");
    // A new key really runs again, and hits the open-session rule.
    h.at_register(
        at,
        Method::POST,
        OPEN,
        &[("idempotency-key", "open-2")],
        body(),
    )
    .await
    .assert_problem(409, "session_already_open");
    // A stored error is replayed as well.
    let again = h
        .at_register(
            at,
            Method::POST,
            OPEN,
            &[("idempotency-key", "open-2")],
            body(),
        )
        .await;
    again.assert_problem(409, "session_already_open");
    assert_eq!(again.headers["idempotency-replayed"], "true");

    // Keys belong to the caller: another session using "open-1" is a different request.
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let r = h
        .at_register((&budi, &h.jkt_device), Method::POST, OPEN, &key, body())
        .await;
    r.assert_problem(409, "session_already_open");
    assert!(r.headers.get("idempotency-replayed").is_none());
}

async fn retried_close_and_logout_replay(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let opened = h
        .at_register(
            at,
            Method::POST,
            OPEN,
            &[],
            Some(json!({ "opening_cash": 0 })),
        )
        .await;
    let uri = format!(
        "/api/v1/cashier-sessions/{}/close",
        opened.body["id"].as_str().unwrap()
    );
    let key = [("idempotency-key", "close-1")];
    let body = || Some(json!({ "counted_cash": 0 }));
    let first = h.at_register(at, Method::POST, &uri, &key, body()).await;
    assert_eq!(first.status, StatusCode::OK);
    // Without the key the retry would be `409 session_closed`; with it, the same report.
    let retry = h.at_register(at, Method::POST, &uri, &key, body()).await;
    assert_eq!((retry.status, &retry.body), (StatusCode::OK, &first.body));
    h.at_register(at, Method::POST, &uri, &[], body())
        .await
        .assert_problem(409, "session_closed");
    assert_eq!(h.audit_count("cashier_session.close").await, 1);

    // A retried logout answers 204 again instead of 401.
    let logout = |key: &'static str| async move {
        h.at_register(
            at,
            Method::POST,
            "/api/v1/auth/logout",
            &[("idempotency-key", key)],
            None,
        )
        .await
    };
    assert_eq!(logout("bye").await.status, StatusCode::NO_CONTENT);
    let replay = logout("bye").await;
    assert_eq!(replay.status, StatusCode::NO_CONTENT);
    assert_eq!(replay.headers["idempotency-replayed"], "true");
    logout("bye-2").await.assert_problem(401, "invalid_session");
}

async fn key_rules_and_unkeyed_endpoints(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let body = || Some(json!({ "opening_cash": 0 }));
    h.at_register(
        at,
        Method::POST,
        OPEN,
        &[("idempotency-key", "has space")],
        body(),
    )
    .await
    .assert_problem(400, "invalid_idempotency_key");
    let long = "k".repeat(256);
    h.at_register(
        at,
        Method::POST,
        OPEN,
        &[("idempotency-key", &long)],
        body(),
    )
    .await
    .assert_problem(400, "invalid_idempotency_key");
    assert_eq!(h.audit_count("cashier_session.open").await, 0);

    // Without credentials nothing is stored: the request just fails authentication each time.
    for _ in 0..2 {
        let r = h
            .send(Method::POST, OPEN, &[("idempotency-key", "anon")], body())
            .await;
        r.assert_problem(401, "invalid_session");
        assert!(r.headers.get("idempotency-replayed").is_none());
    }

    // Endpoints whose response carries a secret ignore the key: every login is a new session.
    let login = || async {
        h.send(
            Method::POST,
            "/api/v1/auth/pin",
            &[
                ("x-register-token", &h.jkt_device),
                ("idempotency-key", "login"),
            ],
            Some(json!({ "user_id": h.ani.id.to_string(), "pin": ANI_PIN })),
        )
        .await
    };
    let a = login().await;
    let b = login().await;
    assert_eq!((a.status, b.status), (StatusCode::OK, StatusCode::OK));
    assert_ne!(a.token(), b.token());
    assert!(b.headers.get("idempotency-replayed").is_none());
}
