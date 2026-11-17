//! Returns, exchanges, held carts, till voids, the close report and the purchase history over
//! HTTP (spec 2.6 §3, §4, §8, §10; phase S3). Spec 2.6 §15 cases 11, 13–16 and 18.

// The tests are scripts that read top to bottom with short names (`h`, `r`, `t`), as elsewhere in
// this suite.
#![allow(clippy::many_single_char_names, clippy::too_many_lines)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;

use crate::common::{ANI_PIN, BUDI_PIN, Harness, Response, api_tests, new_id};
use crate::sales::{
    BALANCE, CASH, Item, ani, approve, balance_of, customer, item, line, on_hand, open, sale, sell,
    tender,
};

api_tests!(
    returns_by_receipt_split_the_line_amount,
    return_window_and_branch_rules,
    no_receipt_return_needs_an_override_above_the_limit,
    exchange_nets_the_refund,
    held_carts_are_taken_once,
    close_report_counts_the_session,
    purchase_history_and_recent_receipts,
);

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

async fn put_return(
    h: &Harness,
    at: (&str, &str),
    id: &str,
    body: &Value,
    token: Option<&str>,
) -> Response {
    let headers: Vec<(&str, &str)> = token.map(|t| ("x-override", t)).into_iter().collect();
    h.at_register(
        at,
        Method::PUT,
        &format!("/api/v1/returns/{id}"),
        &headers,
        Some(body.clone()),
    )
    .await
}

fn returned(sale: &Value, line_no: u32, item: &Item, qty: i64) -> Value {
    json!({
        "sale_id": sale["id"],
        "refund": "cash",
        "lines": [{
            "sale_line_no": line_no, "variant_id": item.variant, "unit_id": item.pcs,
            "qty_thousandths": qty * 1000,
        }],
    })
}

/// Spec 2.6 §15 cases 13 and 14, and `has_returns`.
async fn returns_by_receipt_split_the_line_amount(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let mut l = line(&teh, 3);
    l["discount"] = json!({ "amount": 1_000 });
    let s = sell(h, at, sale(vec![l], vec![tender(CASH, 29_000)])).await;
    assert_eq!(s["lines"][0]["amount"], 29_000);
    let id = new_id();
    let body = returned(&s, 1, &teh, 1);
    let r = put_return(h, at, &id, &body, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["refund"], 9_667);
    assert_eq!(r.body["tenders"][0]["kind"], "cash");
    assert!(r.body["number"].as_str().unwrap().contains("-RT-"));
    let again = put_return(h, at, &id, &body, None).await;
    assert_eq!(
        (again.status, again.body.clone()),
        (StatusCode::OK, r.body.clone())
    );
    let mut other = body.clone();
    other["reason"] = "rusak".into();
    put_return(h, at, &id, &other, None)
        .await
        .assert_problem(409, "conflict");
    let mut rest = returned(&s, 1, &teh, 2);
    rest["lines"][0]["write_off_reason"] = "rusak".into();
    let r = put_return(h, at, &new_id(), &rest, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["refund"], 19_333);
    assert_eq!(r.body["lines"][0]["disposition"], "write_off");
    // 1 restocked, 2 written off.
    assert_eq!(on_hand(h, &teh.variant).await, -2_000);
    let r = put_return(h, at, &new_id(), &returned(&s, 1, &teh, 1), None).await;
    r.assert_problem(422, "return_qty_exceeded");
    // Bad lines.
    put_return(h, at, &new_id(), &returned(&s, 9, &teh, 1), None)
        .await
        .assert_problem(422, "invalid_field");
    let mut bad = returned(&s, 1, &teh, 1);
    bad["refund"] = "card".into();
    put_return(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_field");
    bad = returned(&s, 1, &teh, 1);
    bad["lines"] = json!([]);
    put_return(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_field");
    bad = returned(&s, 1, &teh, 1);
    bad["sale_id"] = new_id().into();
    put_return(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(404, "not_found");
    // A sale with returns cannot be voided.
    let sale_id = s["id"].as_str().unwrap();
    let t = approve(h, at, "sale.post_void", Value::Null, sale_id).await;
    h.at_register(
        at,
        Method::POST,
        &format!("/api/v1/sales/{sale_id}/void"),
        &[("x-override", &t)],
        Some(json!({ "reason": "wrong_item", "refund": "cash" })),
    )
    .await
    .assert_problem(409, "has_returns");
    // Lists and GET.
    let r = h
        .at_register(at, Method::GET, &format!("/api/v1/returns/{id}"), &[], None)
        .await;
    assert_eq!(r.body["refund"], 9_667);
    let budi = h.manager_token().await;
    let r = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/returns?branch_id={}", h.jkt.id),
            None,
        )
        .await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 2);
    h.call(
        &budi,
        Method::GET,
        &format!("/api/v1/returns/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

async fn return_window_and_branch_rules(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let s = sell(h, at, sale(vec![line(&teh, 2)], vec![tender(CASH, 20_000)])).await;
    // At BDG01 the sale is from another branch.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let bdg = (cici.as_str(), h.bdg_device.as_str());
    open(h, bdg, 0).await;
    put_return(h, bdg, &new_id(), &returned(&s, 1, &teh, 1), None)
        .await
        .assert_problem(422, "invalid_field");
    // 31 days later the window is over: a manager override.
    h.clock.advance(31 * DAY_MS);
    let token = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let id = new_id();
    let body = returned(&s, 1, &teh, 1);
    let r = put_return(h, at, &id, &body, None).await;
    r.assert_problem(403, "return_window");
    assert_eq!(r.body["days"], 30);
    let t = approve(h, at, "refund.with_receipt", Value::Null, &id).await;
    let r = put_return(h, at, &id, &body, Some(&t)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
}

/// Spec 2.6 §15 case 15.
async fn no_receipt_return_needs_an_override_above_the_limit(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let cust = customer(h, &owner, "Bu Ani").await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let id = new_id();
    let body = json!({
        "customer_id": cust, "refund": "balance", "reason": "tanpa struk",
        "lines": [{ "variant_id": teh.variant, "unit_id": teh.pcs, "qty_thousandths": 2000 }],
    });
    let r = put_return(h, at, &id, &body, None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "refund.no_receipt");
    let t = approve(h, at, "refund.no_receipt", json!(20_000), &id).await;
    let r = put_return(h, at, &id, &body, Some(&t)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["refund"], 20_000);
    assert_eq!(r.body["tenders"][0]["kind"], "customer_balance");
    assert_eq!(balance_of(h, &owner, &cust).await, Some(20_000));
    assert_eq!(on_hand(h, &teh.variant).await, 2_000);
    // A sale line on a return without a receipt is refused.
    let mut bad = body.clone();
    bad["lines"][0]["sale_line_no"] = 1.into();
    put_return(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_field");
}

/// Spec 2.6 §15 case 16.
async fn exchange_nets_the_refund(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let gula = item(h, &owner, "GULA", 5_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let s = sell(
        h,
        at,
        sale(vec![line(&teh, 10)], vec![tender(CASH, 100_000)]),
    )
    .await;
    // Return 5 (50 000), new sale 65 000: the customer pays 15 000.
    let new_sale = sale(
        vec![line(&teh, 6), line(&gula, 1)],
        vec![tender(CASH, 15_000)],
    );
    let mut body = returned(&s, 1, &teh, 5);
    body["exchange"] = json!({ "sale_id": new_id(), "sale": new_sale });
    let r = put_return(h, at, &new_id(), &body, None).await;
    r.assert_problem(409, "cart_stale");
    body["exchange"]["sale"]["totals"] = r.body["quote"]["totals"].clone();
    let r = put_return(h, at, &new_id(), &body, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["refund"], 50_000);
    assert_eq!(
        r.body["tenders"],
        json!([{ "tender_type_id": null, "kind": "exchange", "amount": 50_000 }])
    );
    let ex = &r.body["exchange_sale"];
    assert_eq!(ex["total"], 65_000);
    assert_eq!(ex["tenders"][0]["kind"], "exchange");
    assert_eq!(ex["tenders"][1]["amount"], 15_000);
    // Return the other 5, new sale 40 000: 10 000 back in cash.
    let small = sale(vec![line(&teh, 4)], vec![]);
    let mut body = returned(&s, 1, &teh, 5);
    body["exchange"] = json!({ "sale_id": new_id(), "sale": small });
    let r = put_return(h, at, &new_id(), &body, None).await;
    r.assert_problem(409, "cart_stale");
    body["exchange"]["sale"]["totals"] = r.body["quote"]["totals"].clone();
    let r = put_return(h, at, &new_id(), &body, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let kinds: Vec<(&str, i64)> = r.body["tenders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["kind"].as_str().unwrap(), t["amount"].as_i64().unwrap()))
        .collect();
    assert_eq!(kinds, [("exchange", 40_000), ("cash", 10_000)]);
    assert_eq!(
        r.body["exchange_sale"]["tenders"].as_array().unwrap().len(),
        1
    );
    // A refund-only role may return, but not sell through an exchange.
    let role = new_id();
    let grants = json!({ "name": "Retur", "grants": [{ "permission": "refund.with_receipt" }] });
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/roles/{role}"),
            Some(grants),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let retur = h
        .add_user(
            "retur",
            role.parse().unwrap(),
            BranchScope::Branch(h.jkt.id),
        )
        .await;
    let retur = h.pin_token(&h.jkt_device, &retur, ANI_PIN).await;
    let mut body = returned(&s, 1, &teh, 1);
    body["exchange"] = json!({ "sale_id": new_id(), "sale": sale(vec![line(&teh, 1)], vec![]) });
    let r = put_return(
        h,
        (retur.as_str(), h.jkt_device.as_str()),
        &new_id(),
        &body,
        None,
    )
    .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "sale.create");
}

/// Spec 2.6 §15 case 11.
async fn held_carts_are_taken_once(h: &Harness) {
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let r2 = h.second_jkt_register().await;
    let budi = h.pin_token(&r2, &h.budi, BUDI_PIN).await;
    let at2 = (budi.as_str(), r2.as_str());
    let id = new_id();
    let uri = format!("/api/v1/held-carts/{id}");
    let body = json!({ "note": " meja 3 ", "payload": { "lines": [1, 2] }, "item_count": 2, "total": 15_000 });
    let mut rx = h.events.subscribe();
    let r = h
        .at_register(at, Method::PUT, &uri, &[], Some(body.clone()))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["note"], "meja 3");
    assert_eq!(rx.try_recv().unwrap().kind, "cart.held");
    let r = h
        .at_register(at, Method::PUT, &uri, &[], Some(body.clone()))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = h
        .at_register(at2, Method::GET, "/api/v1/held-carts", &[], None)
        .await;
    assert_eq!(r.body.as_array().unwrap().len(), 1);
    let take = format!("{uri}/take");
    let (a, b) = tokio::join!(
        h.at_register(at, Method::POST, &take, &[], None),
        h.at_register(at2, Method::POST, &take, &[], None),
    );
    let mut statuses = [a.status.as_u16(), b.status.as_u16()];
    statuses.sort_unstable();
    assert_eq!(statuses, [200, 409], "{} {}", a.body, b.body);
    let loser = if a.status == StatusCode::CONFLICT {
        a
    } else {
        b
    };
    assert_eq!(loser.body["code"], "cart_taken");
    let r = h
        .at_register(at, Method::GET, "/api/v1/held-carts", &[], None)
        .await;
    assert_eq!(r.body, json!([]));
    h.at_register(at2, Method::DELETE, &uri, &[], None)
        .await
        .assert_problem(409, "cart_taken");
    // Dropping a cart needs `sale.void` (the cashier: an override).
    let id = new_id();
    let uri = format!("/api/v1/held-carts/{id}");
    h.at_register(at, Method::PUT, &uri, &[], Some(body.clone()))
        .await;
    h.at_register(at, Method::DELETE, &uri, &[], None)
        .await
        .assert_problem(403, "needs_override");
    let t = approve(h, at, "sale.void", Value::Null, &id).await;
    let r = h
        .at_register(at, Method::DELETE, &uri, &[("x-override", &t)], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(h.audit_count("held_cart.delete").await, 1);
    h.at_register(at, Method::POST, &format!("{uri}/take"), &[], None)
        .await
        .assert_problem(404, "not_found");
    // Another branch does not see it.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let id = new_id();
    h.at_register(
        at,
        Method::PUT,
        &format!("/api/v1/held-carts/{id}"),
        &[],
        Some(body),
    )
    .await;
    let bdg = (cici.as_str(), h.bdg_device.as_str());
    let r = h
        .at_register(bdg, Method::GET, "/api/v1/held-carts", &[], None)
        .await;
    assert_eq!(r.body, json!([]));
    h.at_register(
        bdg,
        Method::POST,
        &format!("/api/v1/held-carts/{id}/take"),
        &[],
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

/// Spec 2.6 §15 case 18: every count and amount of §10 from a scripted session.
async fn close_report_counts_the_session(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let cust = customer(h, &owner, "Bu Sari").await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 50_000).await;
    // Sale 1: 2 Teh with a line discount of 1 000, cash 20 000 → change 1 000, 500 saved.
    let mut l = line(&teh, 2);
    l["discount"] = json!({ "amount": 1_000 });
    let mut body = sale(vec![l], vec![tender(CASH, 20_000)]);
    body["customer_id"] = cust.clone().into();
    body["save_change"] = 500.into();
    sell(h, at, body).await;
    // Sale 2: 1 Teh with a cart discount of 500, paid 500 from the balance and 9 000 cash.
    let mut body = sale(
        vec![line(&teh, 1)],
        vec![tender(BALANCE, 500), tender(CASH, 9_000)],
    );
    body["customer_id"] = cust.clone().into();
    body["cart_discount"] = json!({ "amount": 500 });
    let s2 = sell(h, at, body).await;
    // A return of sale 2 in cash (the balance part goes back to the balance first).
    let r = put_return(h, at, &new_id(), &returned(&s2, 1, &teh, 1), None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    // Till voids: a line (the cashier may), a cart (an override).
    let cart = new_id();
    let void_line = json!({
        "kind": "line", "cart_id": cart, "reason": "wrong_item",
        "lines": [{ "variant_id": teh.variant, "name": "Teh", "qty_thousandths": 1000, "amount": 10_000 }],
    });
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/cart-voids",
            &[("idempotency-key", "lv1")],
            Some(void_line.clone()),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let mut void_cart = void_line.clone();
    void_cart["kind"] = "cart".into();
    void_cart["lines"] = json!([
        { "variant_id": teh.variant, "qty_thousandths": 1000, "amount": 10_000 },
        { "variant_id": teh.variant, "qty_thousandths": 2000, "amount": 20_000 },
    ]);
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/cart-voids",
            &[],
            Some(void_cart.clone()),
        )
        .await;
    r.assert_problem(403, "needs_override");
    let t = approve(h, at, "sale.void", Value::Null, &cart).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/cart-voids",
            &[("x-override", &t)],
            Some(void_cart),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["amount"], 30_000);
    let mut bad = void_line.clone();
    bad["kind"] = "all".into();
    h.at_register(at, Method::POST, "/api/v1/cart-voids", &[], Some(bad))
        .await
        .assert_problem(422, "invalid_field");
    let mut bad = void_line.clone();
    bad["lines"] = json!([]);
    h.at_register(at, Method::POST, "/api/v1/cart-voids", &[], Some(bad))
        .await
        .assert_problem(422, "invalid_field");

    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/cashier-sessions/{session}/close"),
            &[],
            Some(json!({ "counted_cash": 70_000 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let s = &r.body["sales"];
    // Cash: received 20 000 + 9 000, change given 500 + 0, refund 9 000 (the return's cash).
    assert_eq!(s["cash_received"], 29_000);
    assert_eq!(s["change_given"], 500);
    assert_eq!(s["saved_change"], 500);
    assert_eq!(s["balance_paid"], 500);
    assert_eq!(s["cash_refunds"], 9_000);
    assert_eq!(s["expected_cash"], 50_000 + 29_000 - 500 - 9_000);
    assert_eq!(r.body["expected_cash"], s["expected_cash"]);
    assert_eq!(r.body["over_short"], 500);
    assert_eq!(s["line_voids"], json!({ "count": 1, "amount": 10_000 }));
    assert_eq!(s["cart_voids"], json!({ "count": 1, "amount": 30_000 }));
    assert_eq!(s["returns"], json!({ "count": 1, "amount": 9_500 }));
    assert_eq!(s["line_discounts"], json!({ "count": 1, "amount": 1_000 }));
    assert_eq!(s["cart_discounts"], json!({ "count": 1, "amount": 500 }));
    assert_eq!(s["post_voids"], json!({ "count": 0, "amount": 0 }));
    let cash = s["tenders"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["kind"] == "cash")
        .unwrap();
    assert_eq!(cash["sales"], json!({ "count": 2, "amount": 28_000 }));
    assert_eq!(cash["refunds"], json!({ "count": 1, "amount": 9_000 }));
    // The stored report reads the same later.
    let budi = h.manager_token().await;
    let again = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}"),
            None,
        )
        .await;
    assert_eq!(again.body["close_report"], r.body);
    assert_eq!(balance_of(h, &owner, &cust).await, Some(500));
}

async fn purchase_history_and_recent_receipts(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let cust = customer(h, &owner, "Pak Dodi").await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let mut body = sale(vec![line(&teh, 1)], vec![tender(CASH, 10_000)]);
    body["customer_id"] = cust.clone().into();
    let first = sell(h, at, body.clone()).await;
    h.clock.advance(1_000);
    let second = sell(h, at, body).await;
    h.clock.advance(1_000);
    let sale_id = first["id"].as_str().unwrap();
    let t = approve(h, at, "sale.post_void", Value::Null, sale_id).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/sales/{sale_id}/void"),
            &[("x-override", &t)],
            Some(json!({ "reason": "wrong_item", "refund": "cash" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = put_return(h, at, &new_id(), &returned(&second, 1, &teh, 1), None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let uri = format!("/api/v1/customers/{cust}/history?limit=2");
    let r = h.at_register(at, Method::GET, &uri, &[], None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let items = r.body["items"].as_array().unwrap();
    assert_eq!(items[0]["kind"], "return");
    assert_eq!(items[1]["document_id"], second["id"]);
    let next = r.body["next_before_id"].as_str().unwrap();
    let r = h
        .at_register(at, Method::GET, &format!("{uri}&before={next}"), &[], None)
        .await;
    assert_eq!(r.body["items"][0]["document_id"], first["id"]);
    assert_eq!(r.body["items"][0]["voided"], true);
    h.at_register(at, Method::GET, &format!("{uri}&before=nope"), &[], None)
        .await
        .assert_problem(422, "invalid_field");
    // Recent receipts: posted sales only.
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/customers/{cust}/recent-receipts"),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let ids: Vec<&Value> = r
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|s| &s["id"])
        .collect();
    assert_eq!(ids, [&second["id"]]);
    h.at_register(
        at,
        Method::GET,
        &format!("/api/v1/customers/{cust}/recent-receipts?limit=0"),
        &[],
        None,
    )
    .await
    .assert_problem(422, "invalid_field");
    h.at_register(
        at,
        Method::GET,
        &format!("/api/v1/customers/{}/recent-receipts", new_id()),
        &[],
        None,
    )
    .await
    .assert_problem(404, "not_found");
}
