//! The customer balance over HTTP (spec 2.5 §15 server cases 18–19, phase M3): the balance view,
//! the cash payout at the till (permission, open cashier session, expected cash, idempotency) and
//! the balance settings.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_db::customers::BalanceEntryKind;
use sultan_db::seed;
use sultan_server::{NodeRole, router};

use crate::common::{ANI_PIN, BUDI_PIN, Harness, Response, api_tests, new_id};
use crate::customers::{add_balance, body, create, id, uri};

api_tests!(
    payout_lowers_the_balance_and_the_expected_cash,
    write_off_is_owner_only_and_moves_no_cash,
    balance_view_is_paged,
    balance_settings,
);

fn payout_uri(customer: &str) -> String {
    format!("{}/balance/payout", uri(customer))
}

async fn payout(h: &Harness, at: (&str, &str), customer: &str, amount: i64, key: &str) -> Response {
    h.at_register(
        at,
        Method::POST,
        &payout_uri(customer),
        &[("idempotency-key", key)],
        Some(json!({ "amount": amount })),
    )
    .await
}

async fn open_session(h: &Harness, at: (&str, &str), opening: i64) -> String {
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/cashier-sessions",
            &[],
            Some(json!({ "opening_cash": opening })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body["id"].as_str().unwrap().to_owned()
}

async fn close(h: &Harness, at: (&str, &str), session: &str, counted: i64) -> Value {
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/cashier-sessions/{session}/close"),
            &[],
            Some(json!({ "counted_cash": counted })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body
}

/// Case 18: payout needs `customer.balance_payout`, a register session and an open cashier
/// session; 2 200 out → balance 0 and the expected cash −2 200; a replay with the same key pays
/// once.
async fn payout_lowers_the_balance_and_the_expected_cash(h: &Harness) {
    let owner = h.owner_token().await;
    let c = create(h, &owner, body("Budi", Some("0812-3456-789"))).await;
    let cid = id(&c);
    add_balance(h, &cid, BalanceEntryKind::ChangeSaved, 2_200).await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let budi_at = (budi.as_str(), h.jkt_device.as_str());

    let r = payout(h, ani_at, &cid, 2_200, "p-ani").await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.balance_payout");
    // A back-office session is not at a till.
    let bearer = format!("Bearer {owner}");
    h.send(
        Method::POST,
        &payout_uri(&cid),
        &[("authorization", &bearer)],
        Some(json!({ "amount": 1 })),
    )
    .await
    .assert_problem(403, "register_session_required");
    // No open cashier session of Budi on this register (Ani's does not count).
    payout(h, budi_at, &cid, 2_200, "p-0")
        .await
        .assert_problem(409, "no_open_session");
    let ani_session = open_session(h, ani_at, 0).await;
    payout(h, budi_at, &cid, 2_200, "p-1")
        .await
        .assert_problem(409, "no_open_session");
    close(h, ani_at, &ani_session, 0).await;

    let session = open_session(h, budi_at, 100_000).await;
    let r = payout(h, budi_at, &cid, 2_500, "p-2").await;
    r.assert_problem(422, "insufficient_balance");
    assert_eq!(r.body["balance"], 2_200);
    payout(h, budi_at, &cid, 0, "p-3")
        .await
        .assert_problem(422, "invalid_field");
    payout(h, budi_at, &new_id(), 1, "p-4")
        .await
        .assert_problem(404, "not_found");

    let mut events = h.events.subscribe();
    let (outbox, audits) = (
        h.outbox_count().await,
        h.audit_count("customer.balance_payout").await,
    );
    let r = payout(h, budi_at, &cid, 2_200, "p-5").await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["balance"], 0);
    let entry = &r.body["entry"];
    assert_eq!(
        (entry["kind"].as_str(), entry["amount"].as_i64()),
        (Some("payout"), Some(-2_200))
    );
    assert_eq!(entry["cashier_session_id"], session);
    assert_eq!(entry["register_id"], h.jkt_register.id.to_string());
    assert_eq!(entry["user_id"], h.budi.id.to_string());
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("customer.balance_payout").await, audits + 1);
    assert_eq!(
        events.try_recv().unwrap().data,
        json!({ "customer_id": cid })
    );
    // The retry with the same key replays the answer: one payout.
    let replay = payout(h, budi_at, &cid, 2_200, "p-5").await;
    assert_eq!((replay.status, &replay.body), (StatusCode::OK, &r.body));
    assert_eq!(replay.headers["idempotency-replayed"], "true");
    assert_eq!(h.outbox_count().await, outbox + 1);
    let balance = h
        .call(&owner, Method::GET, &format!("{}/balance", uri(&cid)), None)
        .await
        .body;
    assert_eq!(balance["balance"], 0);
    assert_eq!(balance["entries"].as_array().unwrap().len(), 2);

    // The manager sees the live expected cash; the close report counts the payout as cash out.
    let current = h
        .at_register(
            budi_at,
            Method::GET,
            "/api/v1/cashier-sessions/current",
            &[],
            None,
        )
        .await;
    assert_eq!(current.body["expected_cash"], 97_800);
    let report = close(h, budi_at, &session, 97_800).await;
    assert_eq!(report["cash_out"], 2_200);
    assert_eq!(report["expected_cash"], 97_800);
    assert_eq!(report["over_short"], 0);

    // The balance is 0 now, so the erase goes through.
    let r = h
        .send(
            Method::POST,
            &format!("{}/erase", uri(&cid)),
            &[("authorization", &bearer)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // An erased customer has nothing to pay out.
    let session = open_session(h, budi_at, 0).await;
    payout(h, budi_at, &cid, 1, "p-6")
        .await
        .assert_problem(404, "not_found");
    close(h, budi_at, &session, 0).await;
}

/// A negative balance is written off with a negative payout, by the Owner only; it moves no
/// cash. Payouts work on a branch node too (balance entries are transactional data).
async fn write_off_is_owner_only_and_moves_no_cash(h: &Harness) {
    let owner = h.owner_token().await;
    let c = create(h, &owner, body("Siti", None)).await;
    let cid = id(&c);
    add_balance(h, &cid, BalanceEntryKind::Payment, -300).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let budi_at = (budi.as_str(), h.jkt_device.as_str());
    let s = open_session(h, budi_at, 0).await;
    let r = payout(h, budi_at, &cid, -300, "w-1").await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["reason"], "owner_only");
    close(h, budi_at, &s, 0).await;

    let boss = h.add_user("boss", seed::OWNER_ROLE, BranchScope::All).await;
    let boss_token = h.pin_token(&h.bdg_device, &boss, ANI_PIN).await;
    let boss_at = (boss_token.as_str(), h.bdg_device.as_str());
    let s = open_session(h, boss_at, 50_000).await;
    payout(h, boss_at, &cid, -400, "w-2")
        .await
        .assert_problem(422, "insufficient_balance");
    payout(h, boss_at, &cid, 300, "w-3")
        .await
        .assert_problem(422, "insufficient_balance");
    // On a branch node.
    let bearer = format!("Bearer {boss_token}");
    let r = Harness::send_to(
        router(h.state(NodeRole::Branch)),
        Method::POST,
        &payout_uri(&cid),
        &[
            ("authorization", &bearer),
            ("x-register-token", &h.bdg_device),
        ],
        Some(json!({ "amount": -300 })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        (
            r.body["balance"].as_i64(),
            r.body["entry"]["amount"].as_i64()
        ),
        (Some(0), Some(300))
    );
    let report = close(h, boss_at, &s, 50_000).await;
    assert_eq!(
        (
            report["cash_out"].as_i64(),
            report["expected_cash"].as_i64()
        ),
        (Some(0), Some(50_000))
    );
}

/// `GET /customers/{id}/balance`: newest first, paged by `before_id`.
async fn balance_view_is_paged(h: &Harness) {
    let owner = h.owner_token().await;
    let c = create(h, &owner, body("Budi", None)).await;
    let cid = id(&c);
    for amount in [700, 2_000, 500] {
        add_balance(h, &cid, BalanceEntryKind::ChangeSaved, amount).await;
    }
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let get = |query: String| {
        let uri = format!("{}/balance{query}", uri(&cid));
        async move { h.at_register(at, Method::GET, &uri, &[], None).await }
    };
    let first = get("?limit=2".into()).await.body;
    assert_eq!(first["balance"], 3_200);
    let amounts = |page: &Value| -> Vec<i64> {
        page["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["amount"].as_i64().unwrap())
            .collect()
    };
    assert_eq!(amounts(&first), [500, 2_000]);
    let cursor = first["next_before_id"].as_str().unwrap().to_owned();
    let second = get(format!("?limit=2&before_id={cursor}")).await.body;
    assert_eq!(amounts(&second), [700]);
    assert_eq!(second["next_before_id"], Value::Null);
    assert_eq!(amounts(&get(String::new()).await.body).len(), 3);
    get("?limit=0".into())
        .await
        .assert_problem(422, "invalid_field");
    get("?before_id=x".into())
        .await
        .assert_problem(400, "invalid_body");
    let unknown = format!("{}/balance", uri(&new_id()));
    h.at_register(at, Method::GET, &unknown, &[], None)
        .await
        .assert_problem(404, "not_found");
}

/// Case 19: settings are read by anyone, written with `customer.manage`; negative limits are
/// `422`.
async fn balance_settings(h: &Harness) {
    const URI: &str = "/api/v1/settings/customer-balance";
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let r = h.at_register(at, Method::GET, URI, &[], None).await;
    assert_eq!(
        r.body,
        json!({ "max_saved_per_sale": 5_000, "max_balance": 100_000 })
    );
    let new = json!({ "max_saved_per_sale": 2_000, "max_balance": 50_000 });
    let r = h
        .at_register(at, Method::PUT, URI, &[], Some(new.clone()))
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.manage");

    let budi = h.manager_token().await;
    let r = h
        .call(
            &budi,
            Method::PUT,
            URI,
            Some(json!({ "max_saved_per_sale": -1, "max_balance": 50_000 })),
        )
        .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "max_saved_per_sale");
    let audits = h.audit_count("customer.balance_settings.put").await;
    let r = h.call(&budi, Method::PUT, URI, Some(new.clone())).await;
    assert_eq!((r.status, &r.body), (StatusCode::OK, &new));
    let r = h.call(&budi, Method::PUT, URI, Some(new.clone())).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(
        h.audit_count("customer.balance_settings.put").await,
        audits + 1,
        "a replay writes nothing"
    );
    assert_eq!(
        h.at_register(at, Method::GET, URI, &[], None).await.body,
        new
    );
}
