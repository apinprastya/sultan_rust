//! `POST /drawer/open` (spec 2.1 §3 `drawer.open`, spec 2.6 §13 F5; phase S5): an open session of
//! the caller on this register, `drawer.open` with a manager override for a cashier, the audit
//! entry with the reason and the approver, and the `Idempotency-Key` replay.

#![allow(clippy::too_many_lines)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

use crate::common::{BUDI_PIN, Harness, api_tests};
use crate::sales::{ani, approve, open};

api_tests!(drawer_open_is_audited_and_overridable);

async fn drawer_open_is_audited_and_overridable(h: &Harness) {
    let ani = ani(h).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let post = |headers: Vec<(&'static str, String)>, body: Value| async move {
        let headers: Vec<(&str, &str)> = headers.iter().map(|(k, v)| (*k, v.as_str())).collect();
        h.at_register(
            at,
            Method::POST,
            "/api/v1/drawer/open",
            &headers,
            Some(body),
        )
        .await
    };
    // No open session yet.
    post(vec![], json!({}))
        .await
        .assert_problem(409, "no_open_session");
    let session = open(h, at, 50_000).await;
    // A cashier has no `drawer.open`: a manager approves it for this session.
    let r = post(vec![], json!({ "reason": "  change for a customer " })).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "drawer.open");
    post(vec![("x-override", "not-a-token".to_owned())], json!({}))
        .await
        .assert_problem(403, "invalid_override");
    let wrong = approve(h, at, "drawer.open", Value::Null, "another-context").await;
    post(vec![("x-override", wrong)], json!({}))
        .await
        .assert_problem(403, "invalid_override");
    let token = approve(h, at, "drawer.open", Value::Null, &session).await;
    let r = post(
        vec![
            ("x-override", token.clone()),
            ("idempotency-key", "drawer-1".to_owned()),
        ],
        json!({ "reason": "  change for a customer " }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["cashier_session_id"], session);
    assert_eq!(r.body["reason"], "change for a customer");
    assert_eq!(r.body["approved_by"], h.budi.id.to_string());
    // The same key answers the stored response; the token is not used twice.
    let again = post(
        vec![
            ("x-override", token),
            ("idempotency-key", "drawer-1".to_owned()),
        ],
        json!({ "reason": "  change for a customer " }),
    )
    .await;
    assert_eq!(again.status, StatusCode::CREATED);
    assert_eq!(again.body, r.body);
    // A reason above 200 characters.
    post(vec![], json!({ "reason": "x".repeat(201) }))
        .await
        .assert_problem(422, "invalid_field");

    // A manager opens it without approval; budi needs his own session on this register.
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let budi_at = (budi.as_str(), h.jkt_device.as_str());
    h.at_register(
        budi_at,
        Method::POST,
        "/api/v1/drawer/open",
        &[],
        Some(json!({})),
    )
    .await
    .assert_problem(409, "no_open_session");

    // One audit entry per open, on the session.
    let owner = h.owner_token().await;
    let r = h
        .call(
            &owner,
            Method::GET,
            "/api/v1/audit?action=drawer.open",
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let items = r.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{}", r.body);
    assert_eq!(items[0]["target"], format!("cashier_session:{session}"));
    assert_eq!(items[0]["after"]["reason"], "change for a customer");
    assert_eq!(items[0]["after"]["approved_by"], h.budi.id.to_string());
    assert_eq!(items[0]["register_id"], h.jkt_register.id.to_string());
}
