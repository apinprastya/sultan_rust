//! The back-office additions of spec 2.6 phase S6 over HTTP: customer names in the lists and the
//! purchase history, the status filter, the return receipt with its reprint rules, and the sales
//! settings (`return_days`, receipt width per branch).

#![allow(clippy::many_single_char_names, clippy::too_many_lines)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_server::{NodeRole, router};

use crate::common::{Harness, api_tests, new_id};
use crate::sales::{CASH, Item, ani, approve, customer, item, line, open, sale, sell, tender};

api_tests!(
    lists_carry_customer_names_and_status,
    return_receipts_and_reprints,
    sales_settings_drive_the_window_and_width,
);

const DAY_MS: i64 = 24 * 60 * 60 * 1_000;

fn returned(sale: &Value, item: &Item, qty: i64) -> Value {
    json!({
        "sale_id": sale["id"],
        "refund": "cash",
        "lines": [{
            "sale_line_no": 1, "variant_id": item.variant, "unit_id": item.pcs,
            "qty_thousandths": qty * 1000,
        }],
    })
}

async fn put_return(
    h: &Harness,
    at: (&str, &str),
    id: &str,
    body: &Value,
) -> crate::common::Response {
    h.at_register(
        at,
        Method::PUT,
        &format!("/api/v1/returns/{id}"),
        &[],
        Some(body.clone()),
    )
    .await
}

async fn lists_carry_customer_names_and_status(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let cust = customer(h, &owner, "Bu Sari").await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let walk_in = sell(h, at, sale(vec![line(&teh, 1)], vec![tender(CASH, 10_000)])).await;
    let mut body = sale(vec![line(&teh, 2)], vec![tender(CASH, 20_000)]);
    body["customer_id"] = cust.clone().into();
    let named = sell(h, at, body).await;
    assert_eq!(named["customer_name"], "Bu Sari");
    let code = named["customer_code"].as_str().unwrap().to_owned();
    let id = walk_in["id"].as_str().unwrap();
    let t = approve(h, at, "sale.post_void", Value::Null, id).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/sales/{id}/void"),
            &[("x-override", &t)],
            Some(json!({ "reason": "wrong_item", "refund": "cash" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = h.call(&owner, Method::GET, "/api/v1/sales", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let items = r.body["items"].as_array().unwrap();
    assert_eq!(items[0]["customer_name"], "Bu Sari");
    assert_eq!(items[0]["customer_code"], code.as_str());
    assert!(items[1]["customer_name"].is_null());
    assert_eq!(items[0]["cashier_name"], h.ani.display_name.as_str());
    assert_eq!(items[0]["register_code"], h.jkt_register.code.as_str());
    for (status, want) in [("posted", &named["id"]), ("voided", &walk_in["id"])] {
        let r = h
            .call(
                &owner,
                Method::GET,
                &format!("/api/v1/sales?status={status}"),
                None,
            )
            .await;
        let items = r.body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{status}");
        assert_eq!(&items[0]["id"], want);
    }
    h.call(&owner, Method::GET, "/api/v1/sales?status=held", None)
        .await
        .assert_problem(422, "invalid_field");
    // Returns and the purchase history carry the name too.
    let r = put_return(h, at, &new_id(), &returned(&named, &teh, 1)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["customer_name"], "Bu Sari");
    let r = h.call(&owner, Method::GET, "/api/v1/returns", None).await;
    assert_eq!(r.body["items"][0]["customer_name"], "Bu Sari");
    assert_eq!(r.body["items"][0]["customer_code"], code.as_str());
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/customers/{cust}/history"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    for entry in r.body["items"].as_array().unwrap() {
        assert_eq!(entry["customer_name"], "Bu Sari");
        assert_eq!(entry["customer_code"], code.as_str());
    }
}

async fn return_receipts_and_reprints(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let s = sell(h, at, sale(vec![line(&teh, 3)], vec![tender(CASH, 30_000)])).await;
    let older = new_id();
    let r = put_return(h, at, &older, &returned(&s, &teh, 1)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let mut write_off = returned(&s, &teh, 1);
    write_off["lines"][0]["write_off_reason"] = "pecah".into();
    let last = new_id();
    let r = put_return(h, at, &last, &write_off).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let number = r.body["number"].as_str().unwrap().to_owned();
    let uri = |id: &str, q: &str| format!("/api/v1/returns/{id}/receipt{q}");
    let r = h
        .at_register(at, Method::GET, &uri(&last, ""), &[], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["number"], number.as_str());
    assert_eq!(r.body["barcode"], number.as_str());
    assert_eq!(r.body["copy"], false);
    let text = r.body["text"].as_str().unwrap();
    for want in ["*** RETURN ***", "REFUND", "10.000", "(write-off)", "Tunai"] {
        assert!(text.contains(want), "{want:?} in\n{text}");
    }
    assert!(text.contains(s["number"].as_str().unwrap()), "{text}");
    assert!(!text.contains("COPY"));
    let r = h
        .at_register(at, Method::GET, &uri(&last, "?lang=id&width=42"), &[], None)
        .await;
    let text = r.body["text"].as_str().unwrap();
    assert!(text.contains("RETUR") && text.contains("Nota"), "{text}");
    h.at_register(at, Method::GET, &uri(&last, "?width=40"), &[], None)
        .await
        .assert_problem(422, "invalid_field");
    h.at_register(at, Method::GET, &uri(&new_id(), ""), &[], None)
        .await
        .assert_problem(404, "not_found");
    // A copy of the last return of the session is free; an older one needs `sale.reprint`.
    let r = h
        .at_register(at, Method::GET, &uri(&last, "?copy=true"), &[], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["text"].as_str().unwrap().contains("COPY"));
    assert_eq!(h.audit_count("return.reprint").await, 1);
    h.at_register(at, Method::GET, &uri(&older, "?copy=true"), &[], None)
        .await
        .assert_problem(403, "needs_override");
    let t = approve(h, at, "sale.reprint", Value::Null, &older).await;
    let r = h
        .at_register(
            at,
            Method::GET,
            &uri(&older, "?copy=true"),
            &[("x-override", &t)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = h
        .call(&owner, Method::GET, &uri(&older, "?copy=true"), None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(h.audit_count("return.reprint").await, 3);
}

async fn sales_settings_drive_the_window_and_width(h: &Harness) {
    let owner = h.owner_token().await;
    let r = h
        .call(&owner, Method::GET, "/api/v1/settings/sales", None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["return_days"], 30);
    let widths = r.body["receipt_widths"].as_array().unwrap();
    assert!(!widths.is_empty());
    assert!(widths.iter().all(|w| w["receipt_width"] == 32));
    let jkt = h.jkt.id.to_string();
    let body = json!({
        "return_days": 0,
        "receipt_widths": [{ "branch_id": jkt, "receipt_width": 48 }],
    });
    // A cashier may not; a branch node is read-only.
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    h.at_register(
        at,
        Method::PUT,
        "/api/v1/settings/sales",
        &[],
        Some(body.clone()),
    )
    .await
    .assert_problem(403, "permission_denied");
    let bearer = format!("Bearer {owner}");
    Harness::send_to(
        router(h.state(NodeRole::Branch)),
        Method::PUT,
        "/api/v1/settings/sales",
        &[("authorization", &bearer)],
        Some(body.clone()),
    )
    .await
    .assert_problem(403, "read_only_replica");
    for (bad, field) in [
        (json!({ "return_days": 4000 }), "return_days"),
        (
            json!({ "return_days": 30, "receipt_widths": [{ "branch_id": jkt, "receipt_width": 40 }] }),
            "receipt_width",
        ),
    ] {
        let r = h
            .call(&owner, Method::PUT, "/api/v1/settings/sales", Some(bad))
            .await;
        r.assert_problem(422, "invalid_field");
        assert_eq!(r.body["field"], field, "{}", r.body);
    }
    h.call(
        &owner,
        Method::PUT,
        "/api/v1/settings/sales",
        Some(json!({ "return_days": 30, "receipt_widths": [{ "branch_id": "nope", "receipt_width": 32 }] })),
    )
    .await
    .assert_problem(400, "invalid_body");
    h.call(
        &owner,
        Method::PUT,
        "/api/v1/settings/sales",
        Some(json!({ "return_days": 30, "receipt_widths": [{ "branch_id": new_id(), "receipt_width": 32 }] })),
    )
    .await
    .assert_problem(404, "not_found");
    let r = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/sales",
            Some(body.clone()),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["return_days"], 0);
    let stored = r.body["receipt_widths"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["branch_id"] == jkt.as_str())
        .unwrap()
        .clone();
    assert_eq!(stored["receipt_width"], 48);
    assert_eq!(h.audit_count("sales_settings.put").await, 1);
    let r = h
        .call(&owner, Method::PUT, "/api/v1/settings/sales", Some(body))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        h.audit_count("sales_settings.put").await,
        1,
        "a replay writes nothing"
    );

    // The branch width is the receipts' default.
    let teh = item(h, &owner, "TEH", 10_000).await;
    open(h, at, 0).await;
    let s = sell(h, at, sale(vec![line(&teh, 3)], vec![tender(CASH, 30_000)])).await;
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/sales/{}/receipt", s["id"].as_str().unwrap()),
            &[],
            None,
        )
        .await;
    assert!(
        r.body["lines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|l| l == &json!("-".repeat(48))),
        "{}",
        r.body
    );
    // `0`: no window at all, even a year later.
    h.clock.advance(365 * DAY_MS);
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let owner = h.owner_token().await;
    let r = put_return(h, at, &new_id(), &returned(&s, &teh, 1)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    // 400 days: a year is inside; 10 days: outside, and the problem says 10.
    for (days, inside) in [(400, true), (10, false)] {
        let r = h
            .call(
                &owner,
                Method::PUT,
                "/api/v1/settings/sales",
                Some(json!({ "return_days": days })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        let r = put_return(h, at, &new_id(), &returned(&s, &teh, 1)).await;
        if inside {
            assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        } else {
            r.assert_problem(403, "return_window");
            assert_eq!(r.body["days"], days);
        }
    }
}
