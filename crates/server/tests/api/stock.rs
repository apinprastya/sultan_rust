//! Inventory over HTTP (spec 2.8 §16; phase N3): spec 2.8 §17 cases 13–22 end to end, with the
//! permission cases, the HQ data on a branch node, the on-hand filters and the events.
// The tests are scripts that read top to bottom with short names (`h`, `r`, `s`), as elsewhere in
// this suite.
#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_db::seed;
use sultan_server::{NodeRole, router};

use crate::catalog_fixtures::{STANDARD, put_product};
use crate::common::{ANI_PIN, BUDI_PASSWORD, Harness, Response, api_tests, new_id};
use crate::sales::{CASH, Item, approve, item, line, open, sale, sell, tender};

api_tests!(
    ledger_and_balance_after_every_posting_kind,
    receipt_posts_and_replays,
    receipt_void_before_and_after_use,
    purchase_order_lifecycle,
    supplier_return_against_a_receipt_line,
    transfer_between_branches,
    opname_counts_and_permissions,
    costs_hidden_without_report_profit,
    sales_values_reverse_exactly,
    till_lookup_with_the_register_token,
    hq_data_suppliers_reasons_settings,
    on_hand_filters_and_lots,
);

/// What every test starts with: locations at both branches, a priced stock product `tea`, a
/// tracked product `milk` and a supplier.
struct Shop {
    owner: String,
    tea: Item,
    milk: Item,
    jkt_loc: String,
    bdg_loc: String,
    supplier: String,
}

async fn location(h: &Harness, owner: &str, branch: &str, name: &str) -> String {
    let id = new_id();
    let r = h
        .call(
            owner,
            Method::PUT,
            &format!("/api/v1/locations/{id}"),
            Some(json!({ "branch_id": branch, "name": name, "receiving_default": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    id
}

async fn tracked(h: &Harness, owner: &str, code: &str) -> Item {
    let (id, variant, pcs) = (new_id(), new_id(), new_id());
    let body = json!({
        "code": code, "name": format!("Susu {code}"), "type": "stock", "tax_class_id": STANDARD,
        "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": pcs,
        "variants": [{ "id": variant }],
        "track_expiry": true,
        "expiry_warn_days": 10,
    });
    let r = put_product(h, owner, &id, &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        (&r.body["track_expiry"], &r.body["expiry_warn_days"]),
        (&json!(true), &json!(10))
    );
    // A replay changes nothing.
    let again = put_product(h, owner, &id, &body).await;
    assert_eq!(again.status, StatusCode::OK);
    Item { variant, pcs }
}

/// The owner's back-office session at JKT01 (without `branch_id` the first branch by code,
/// BDG01, would be the session's).
async fn owner_at_jkt(h: &Harness) -> String {
    let r = h
        .send(
            Method::POST,
            "/api/v1/auth/password",
            &[],
            Some(json!({
                "username": "owner", "password": crate::common::OWNER_PASSWORD,
                "branch_id": h.jkt.id.to_string(),
            })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.token()
}

async fn shop(h: &Harness) -> Shop {
    let owner = owner_at_jkt(h).await;
    let jkt_loc = location(h, &owner, &h.jkt.id.to_string(), "Toko").await;
    let bdg_loc = location(h, &owner, &h.bdg.id.to_string(), "Gudang").await;
    let tea = item(h, &owner, "TEH", 2_000).await;
    let milk = tracked(h, &owner, "SUSU").await;
    let supplier = new_id();
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/suppliers/{supplier}"),
            Some(json!({ "code": "sup1", "name": "PT Pemasok", "terms_days": 30 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["code"], "SUP1");
    Shop {
        owner,
        tea,
        milk,
        jkt_loc,
        bdg_loc,
        supplier,
    }
}

fn gr_line(item: &Item, qty: i64, price: i64) -> Value {
    json!({
        "variant_id": item.variant, "unit_id": item.pcs, "qty_thousandths": qty * 1000,
        "price": price,
    })
}

fn receipt(s: &Shop, invoice: &str, lines: Vec<Value>) -> Value {
    json!({
        "supplier_id": s.supplier, "invoice_no": invoice, "invoice_date": "2027-01-15",
        "lines": lines,
    })
}

async fn put_doc(h: &Harness, token: &str, path: &str, id: &str, body: &Value) -> Response {
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/{path}/{id}"),
        Some(body.clone()),
    )
    .await
}

async fn action(h: &Harness, token: &str, uri: &str, body: Option<Value>) -> Response {
    h.call(token, Method::POST, uri, body).await
}

/// Writes and posts a receipt; returns the post answer (asserted `200`).
async fn receive(h: &Harness, token: &str, body: &Value) -> Value {
    let id = new_id();
    let r = put_doc(h, token, "goods-receipts", &id, body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["status"], "draft");
    let r = action(h, token, &format!("/api/v1/goods-receipts/{id}/post"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body
}

/// The on-hand row of `variant` at JKT01 as `token` sees it.
async fn row(h: &Harness, token: &str, variant: &str) -> Value {
    let r = h
        .call(
            token,
            Method::GET,
            &format!("/api/v1/stock/on-hand?variant_id={variant}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body["rows"][0].clone()
}

async fn qty_of(h: &Harness, token: &str, variant: &str) -> i64 {
    row(h, token, variant).await["qty_thousandths"]
        .as_i64()
        .unwrap_or(0)
}

/// A Stock clerk at JKT01 (password session): receive, transfer, count; no adjust, no
/// `report.profit`.
async fn clerk(h: &Harness) -> (sultan_db::users::User, String) {
    let u = h
        .add_user("sri", seed::STOCK_CLERK_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let token = h.password_token("sri", BUDI_PASSWORD).await;
    (u, token)
}

/// A user at Jakarta whose only key is `report.profit` (a role the Owner creates).
async fn profit_reader(h: &Harness, owner: &str) -> String {
    let id: sultan_core::ids::RoleId = new_id().parse().unwrap();
    let r = h
        .call(
            owner,
            Method::PUT,
            &format!("/api/v1/roles/{id}"),
            Some(json!({ "name": "Auditor", "grants": [{ "permission": "report.profit" }] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    h.add_user("audit", id, BranchScope::Branch(h.jkt.id)).await;
    h.password_token("audit", BUDI_PASSWORD).await
}

fn adjustment(item: &Item, qty: i64, reason: &str) -> Value {
    json!({
        "lines": [{
            "variant_id": item.variant, "unit_id": item.pcs, "qty_thousandths": qty * 1000,
            "reason_id": reason, "note": "cek",
        }],
    })
}

/// …0506 Koreksi (both ways, note required); …0501 Rusak (out).
const KOREKSI: &str = "00000000-0000-7000-8000-000000000506";
const RUSAK: &str = "00000000-0000-7000-8000-000000000501";

/// Spec §17 case 13: after every posting kind the cached on hand equals the ledger (the stock
/// card's running balance) and the expected sum.
async fn ledger_and_balance_after_every_posting_kind(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let tea = &s.tea.variant;
    let check = |expected: i64| async move {
        let on_hand = qty_of(h, o, tea).await;
        let card = h
            .call(o, Method::GET, &format!("/api/v1/stock/card/{tea}"), None)
            .await;
        assert_eq!(card.status, StatusCode::OK, "{}", card.body);
        let rows = card.body["rows"].as_array().unwrap();
        let last = rows.last().unwrap()["balance_thousandths"]
            .as_i64()
            .unwrap();
        assert_eq!((on_hand, last), (expected, expected));
    };
    // Opening stock.
    let r = put_doc(
        h,
        o,
        "opening-stock",
        &new_id(),
        &json!({ "lines": [{ "variant_id": tea, "unit_id": s.tea.pcs, "qty_thousandths": 5_000, "unit_cost": 1_000 }] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert!(r.body["number"].as_str().unwrap().contains("-OS-"));
    check(5_000).await;
    // Receipt.
    receive(
        h,
        o,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_000)]),
    )
    .await;
    check(15_000).await;
    // Adjustment out.
    let r = put_doc(
        h,
        o,
        "adjustments",
        &new_id(),
        &adjustment(&s.tea, -2, RUSAK),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["movements"][0]["kind"], "adjust");
    check(13_000).await;
    // Internal transfer to a second location (branch on hand unchanged).
    let back = location(h, o, &h.jkt.id.to_string(), "Belakang").await;
    let r = put_doc(
        h,
        o,
        "internal-transfers",
        &new_id(),
        &json!({
            "from_location_id": s.jkt_loc, "to_location_id": back,
            "lines": [{ "variant_id": tea, "unit_id": s.tea.pcs, "qty_thousandths": 3_000 }],
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/on-hand?variant_id={tea}&location_id={back}"),
            None,
        )
        .await;
    assert_eq!(r.body["rows"][0]["qty_thousandths"], 3_000);
    // A sale.
    let token = crate::sales::ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    sell(
        h,
        at,
        sale(vec![line(&s.tea, 1)], vec![tender(CASH, 2_000)]),
    )
    .await;
    let till = h
        .send(
            Method::GET,
            &format!("/api/v1/registers/me/stock/{tea}"),
            &[("x-register-token", &h.jkt_device)],
            None,
        )
        .await;
    assert_eq!(till.body["qty_thousandths"], 12_000);
    // Transfer out to BDG01, received in full.
    let id = new_id();
    let r = put_doc(
        h,
        o,
        "transfers",
        &id,
        &json!({
            "from_branch_id": h.jkt.id.to_string(), "to_branch_id": h.bdg.id.to_string(),
            "ship_from_location_id": s.jkt_loc,
            "lines": [{ "variant_id": tea, "unit_id": s.tea.pcs, "qty_thousandths": 2_000 }],
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["status"], "shipped");
    let r = action(h, o, &format!("/api/v1/transfers/{id}/receive"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = h
        .call(
            o,
            Method::GET,
            &format!(
                "/api/v1/stock/on-hand?branch_id={}&variant_id={tea}",
                h.bdg.id
            ),
            None,
        )
        .await;
    assert_eq!(r.body["rows"][0]["qty_thousandths"], 2_000);
    // JKT01's two locations: 12 − 2 = 10 in all.
    let r = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/on-hand?variant_id={tea}"),
            None,
        )
        .await;
    let total: i64 = r.body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["qty_thousandths"].as_i64().unwrap())
        .sum();
    assert_eq!(total, 10_000);
    // The documents are audited with their outbox events.
    for action in [
        "opening_stock.post",
        "goods_receipt.post",
        "adjustment.post",
        "internal_transfer.post",
        "transfer.create",
        "transfer.ship",
        "transfer.receive",
    ] {
        assert_eq!(h.audit_count(action).await, 1, "{action}");
    }
}

/// Spec §17 case 14: a posted receipt brings stock, cost, lots, the last price, the PO's
/// received quantity and a payable; replaying the post is harmless; a second receipt with the
/// same invoice number of the supplier is a conflict.
async fn receipt_posts_and_replays(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let mut milk = gr_line(&s.milk, 4, 3_000);
    milk["batch"] = "B1".into();
    // A tracked line without an expiry cannot be posted.
    let id = new_id();
    let r = put_doc(
        h,
        o,
        "goods-receipts",
        &id,
        &receipt(&s, "INV-9", vec![milk.clone()]),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    action(h, o, &format!("/api/v1/goods-receipts/{id}/post"), None)
        .await
        .assert_problem(422, "invalid_field");
    milk["expiry"] = "2027-03-01".into();
    let mut body = receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_100), milk]);
    body["tax_mode"] = "added".into();
    body["tax_rate_bp"] = 1_100.into();
    body["header_discount"] = 1_000.into();
    body["other_costs"] = 2_200.into();
    let id = new_id();
    let r = put_doc(h, o, "goods-receipts", &id, &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    // The same draft again changes nothing; a bad body is refused.
    let again = put_doc(h, o, "goods-receipts", &id, &body).await;
    assert_eq!(again.status, StatusCode::OK);
    let mut bad = body.clone();
    bad["payment"] = "barter".into();
    put_doc(h, o, "goods-receipts", &id, &bad)
        .await
        .assert_problem(422, "invalid_field");
    let uri = format!("/api/v1/goods-receipts/{id}/post");
    let bearer = format!("Bearer {o}");
    let headers = [
        ("authorization", bearer.as_str()),
        ("idempotency-key", "gr-1"),
    ];
    let r = h.send(Method::POST, &uri, &headers, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "posted");
    assert!(r.body["number"].as_str().unwrap().contains("-GR-"));
    // Net 11 000 + 12 000; less the 1 000 header discount = 22 000; PPN 11% = 2 420;
    // total 24 420.
    assert_eq!(
        (&r.body["net"], &r.body["tax"], &r.body["total"]),
        (&json!(23_000), &json!(2_420), &json!(24_420))
    );
    assert_eq!(r.body["due_date"], "2027-02-14", "invoice date + 30 days");
    // Non-PKP: PPN in the landed cost, plus the other costs.
    assert_eq!(r.body["landed"], 26_620);
    assert_eq!(r.body["movements"].as_array().unwrap().len(), 2);
    assert!(r.body["lines"][1]["lot_id"].is_string());
    let replay = h.send(Method::POST, &uri, &headers, None).await;
    assert_eq!(replay.headers["idempotency-replayed"], "true");
    assert_eq!(replay.body, r.body);
    // Without the key the post is unchanged too.
    let r2 = action(h, o, &uri, None).await;
    assert_eq!(r2.status, StatusCode::OK);
    assert_eq!(r2.body["number"], r.body["number"]);
    assert_eq!(h.audit_count("goods_receipt.post").await, 1);
    // Stock and cost.
    let tea = row(h, o, &s.tea.variant).await;
    assert_eq!(tea["qty_thousandths"], 10_000);
    assert!(tea["unit_cost"].as_i64().unwrap() > 1_100);
    // Lots.
    let lots = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/lots?variant_id={}", s.milk.variant),
            None,
        )
        .await;
    assert_eq!(lots.body[0]["batch"], "B1");
    assert_eq!(lots.body[0]["expiry"], "2027-03-01");
    assert_eq!(lots.body[0]["qty_thousandths"], 4_000);
    // The supplier item's last price per base unit.
    let item = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/suppliers/{}/items/{}", s.supplier, s.tea.variant),
            None,
        )
        .await;
    assert_eq!(item.body["last_price"], 1_100);
    let items = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/suppliers/{}/items", s.supplier),
            None,
        )
        .await;
    assert_eq!(items.body.as_array().unwrap().len(), 2);
    // The payable.
    let p = h.call(o, Method::GET, "/api/v1/payables", None).await;
    assert_eq!(p.body[0]["amount"], 24_420);
    assert_eq!(p.body[0]["paid"], false);
    h.call(o, Method::GET, "/api/v1/payables?status=x", None)
        .await
        .assert_problem(422, "invalid_field");
    // A duplicate invoice number of the supplier.
    let id2 = new_id();
    put_doc(
        h,
        o,
        "goods-receipts",
        &id2,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 1, 1_000)]),
    )
    .await;
    action(h, o, &format!("/api/v1/goods-receipts/{id2}/post"), None)
        .await
        .assert_problem(409, "conflict");
    // A posted receipt is not edited.
    put_doc(h, o, "goods-receipts", &id, &body)
        .await
        .assert_problem(409, "conflict");
    // Lists and reads.
    let list = h
        .call(o, Method::GET, "/api/v1/goods-receipts?status=posted", None)
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    let one = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/goods-receipts/{id}"),
            None,
        )
        .await;
    assert_eq!(one.body["movements"].as_array().unwrap().len(), 2);
    h.call(
        o,
        Method::GET,
        &format!("/api/v1/goods-receipts/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

/// Spec §17 case 15: a receipt is voided before its stock was used, with a manager's approval
/// (the manager's own `stock.adjust`, or an override at the register for a Stock clerk); after a
/// sale of one of its variants it is `409 receipt_in_use`.
async fn receipt_void_before_and_after_use(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let first = receive(
        h,
        o,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_000)]),
    )
    .await;
    let id = first["id"].as_str().unwrap();
    // A Stock clerk in the back office needs a manager.
    let (sri, sri_token) = clerk(h).await;
    action(
        h,
        &sri_token,
        &format!("/api/v1/goods-receipts/{id}/void"),
        None,
    )
    .await
    .assert_problem(403, "needs_override");
    // At the register the manager approves it (`stock.adjust`, context = the receipt).
    let pin = h.pin_token(&h.jkt_device, &sri, ANI_PIN).await;
    let at = (pin.as_str(), h.jkt_device.as_str());
    let t = approve(h, at, "stock.adjust", Value::Null, id).await;
    let uri = format!("/api/v1/goods-receipts/{id}/void");
    let r = h
        .at_register(at, Method::POST, &uri, &[("x-override", &t)], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "voided");
    assert_eq!(r.body["approved_by"], h.budi.id.to_string());
    assert_eq!(qty_of(h, o, &s.tea.variant).await, 0);
    // The invoice number is free again; the manager voids under their own login.
    let second = receive(
        h,
        o,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_000)]),
    )
    .await;
    let budi = h.manager_token().await;
    let id2 = second["id"].as_str().unwrap();
    // A sale of the variant uses its cost.
    let token = crate::sales::ani(h).await;
    let till = (token.as_str(), h.jkt_device.as_str());
    open(h, till, 0).await;
    sell(
        h,
        till,
        sale(vec![line(&s.tea, 1)], vec![tender(CASH, 2_000)]),
    )
    .await;
    action(
        h,
        &budi,
        &format!("/api/v1/goods-receipts/{id2}/void"),
        None,
    )
    .await
    .assert_problem(409, "receipt_in_use");
    // A fresh receipt the manager voids at once; voiding twice is harmless.
    let mut milk = gr_line(&s.milk, 1, 1_000);
    milk["expiry"] = "2027-06-01".into();
    let fourth = receive(h, o, &receipt(&s, "INV-4", vec![milk])).await;
    let id4 = fourth["id"].as_str().unwrap();
    let r = action(
        h,
        &budi,
        &format!("/api/v1/goods-receipts/{id4}/void"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["approved_by"], Value::Null);
    let r = action(
        h,
        &budi,
        &format!("/api/v1/goods-receipts/{id4}/void"),
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.audit_count("goods_receipt.void").await, 2);
}

/// Spec §17 case 16: partial receipt, over-receipt warning, closing by itself, "close the rest",
/// cancel only while nothing was received.
async fn purchase_order_lifecycle(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let po = |qty: i64| {
        json!({
            "supplier_id": s.supplier, "order_date": "2027-01-10", "expected_date": "2027-01-17",
            "lines": [{ "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": qty * 1000, "price": 1_000 }],
        })
    };
    let id = new_id();
    let r = put_doc(h, o, "purchase-orders", &id, &po(10)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["status"], "draft");
    let r = action(h, o, &format!("/api/v1/purchase-orders/{id}/order"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["number"].as_str().unwrap().contains("-PO-"));
    // An ordered PO is not edited.
    put_doc(h, o, "purchase-orders", &id, &po(5))
        .await
        .assert_problem(409, "conflict");
    // On order shows in the on-hand list once stock exists.
    let from_po = |qty: i64| {
        let mut l = gr_line(&s.tea, qty, 1_000);
        l["order_id"] = id.clone().into();
        l["order_line_no"] = 1.into();
        l
    };
    receive(h, o, &receipt(&s, "INV-1", vec![from_po(4)])).await;
    let r = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/purchase-orders/{id}"),
            None,
        )
        .await;
    assert_eq!(
        (
            &r.body["status"],
            &r.body["lines"][0]["received_qty_thousandths"]
        ),
        (&json!("ordered"), &json!(4_000))
    );
    assert_eq!(
        row(h, o, &s.tea.variant).await["on_order_thousandths"],
        6_000
    );
    // It cannot be cancelled once something came.
    action(h, o, &format!("/api/v1/purchase-orders/{id}/cancel"), None)
        .await
        .assert_problem(409, "conflict");
    // Over-received: a warning, and the PO closes by itself.
    let r = receive(h, o, &receipt(&s, "INV-2", vec![from_po(7)])).await;
    assert_eq!(r["over_received"][0]["order_id"], id.as_str());
    let r = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/purchase-orders/{id}"),
            None,
        )
        .await;
    assert_eq!(r.body["status"], "closed");
    // A second PO: "close the rest".
    let id2 = new_id();
    put_doc(h, o, "purchase-orders", &id2, &po(3)).await;
    action(h, o, &format!("/api/v1/purchase-orders/{id2}/close"), None)
        .await
        .assert_problem(409, "conflict");
    action(h, o, &format!("/api/v1/purchase-orders/{id2}/order"), None).await;
    let r = action(h, o, &format!("/api/v1/purchase-orders/{id2}/close"), None).await;
    assert_eq!(r.body["status"], "closed");
    // A third: cancelled with nothing received.
    let id3 = new_id();
    put_doc(h, o, "purchase-orders", &id3, &po(2)).await;
    action(h, o, &format!("/api/v1/purchase-orders/{id3}/order"), None).await;
    let r = action(h, o, &format!("/api/v1/purchase-orders/{id3}/cancel"), None).await;
    assert_eq!(r.body["status"], "cancelled");
    let list = h
        .call(
            o,
            Method::GET,
            "/api/v1/purchase-orders?status=closed",
            None,
        )
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 2);
    h.call(o, Method::GET, "/api/v1/purchase-orders?status=nope", None)
        .await
        .assert_problem(422, "invalid_field");
    // A Stock clerk cannot order.
    let (_, sri) = clerk(h).await;
    put_doc(h, &sri, "purchase-orders", &new_id(), &po(1))
        .await
        .assert_problem(403, "permission_denied");
    // Reorder: a rule and the suggestion for the supplier.
    let r = h
        .call(
            o,
            Method::PUT,
            &format!("/api/v1/reorder-rules/{}/{}", h.jkt.id, s.tea.variant),
            Some(json!({ "min_thousandths": 20_000, "max_thousandths": 50_000 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/reorder/suggestions?supplier_id={}", s.supplier),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body[0]["base_qty_thousandths"], 39_000, "50 − 11");
    assert_eq!(r.body[0]["price"], 1_000);
    let low = h
        .call(o, Method::GET, "/api/v1/reorder/low-stock", None)
        .await;
    assert_eq!(low.body[0]["variant_id"], s.tea.variant.as_str());
    let rules = h.call(o, Method::GET, "/api/v1/reorder-rules", None).await;
    assert_eq!(rules.body[0]["max_thousandths"], 50_000);
    let filter = h
        .call(o, Method::GET, "/api/v1/stock/on-hand?filter=low", None)
        .await;
    assert_eq!(filter.body["rows"][0]["variant_id"], s.tea.variant.as_str());
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/reorder-rules/{}/{}", h.jkt.id, s.tea.variant),
        Some(json!({ "min_thousandths": 5_000, "max_thousandths": 1_000 })),
    )
    .await
    .assert_problem(422, "invalid_field");
    // No rule for a variant the catalog does not have.
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/reorder-rules/{}/{}", h.jkt.id, new_id()),
        Some(json!({ "min_thousandths": 1_000, "max_thousandths": 2_000 })),
    )
    .await
    .assert_problem(404, "not_found");
}

/// Spec §17 case 17: a supplier return against a receipt line returns at most what it
/// received minus earlier returns.
async fn supplier_return_against_a_receipt_line(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let gr = receive(h, o, &receipt(&s, "INV-1", vec![gr_line(&s.tea, 5, 1_000)])).await;
    let ret = |qty: i64| {
        json!({
            "supplier_id": s.supplier,
            "lines": [{
                "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": qty * 1000,
                "price": 1_000, "receipt_id": gr["id"], "receipt_line_no": 1,
            }],
        })
    };
    let id = new_id();
    let r = put_doc(h, o, "supplier-returns", &id, &ret(3)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = action(h, o, &format!("/api/v1/supplier-returns/{id}/post"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "posted");
    assert_eq!(r.body["credit"], 3_000);
    assert!(r.body["number"].as_str().unwrap().contains("-RS-"));
    assert_eq!(qty_of(h, o, &s.tea.variant).await, 2_000);
    let id2 = new_id();
    put_doc(h, o, "supplier-returns", &id2, &ret(3)).await;
    action(h, o, &format!("/api/v1/supplier-returns/{id2}/post"), None)
        .await
        .assert_problem(422, "invalid_field");
    let mut half = ret(1);
    half["lines"][0]["receipt_line_no"] = Value::Null;
    put_doc(h, o, "supplier-returns", &new_id(), &half)
        .await
        .assert_problem(422, "invalid_field");
    let list = h
        .call(o, Method::GET, "/api/v1/supplier-returns", None)
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 2);
    let one = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/supplier-returns/{id}"),
            None,
        )
        .await;
    assert_eq!(one.body["movements"][0]["kind"], "supplier_return");
}

/// Spec §17 case 18: a request at the receiver, shipped by the sender, received with a
/// shortage (a transit loss at the sender); a shipped transfer cannot be cancelled; a branch
/// node answers `other_node` for a branch it does not hold. Events go to both branches.
async fn transfer_between_branches(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    receive(
        h,
        o,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_000)]),
    )
    .await;
    let mut events = h.events.subscribe();
    let body = json!({
        "from_branch_id": h.jkt.id.to_string(), "to_branch_id": h.bdg.id.to_string(),
        "lines": [{ "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": 5_000 }],
    });
    // A Stock clerk of JKT01 cannot request for BDG01.
    let (_, sri) = clerk(h).await;
    put_doc(h, &sri, "transfers", &new_id(), &body)
        .await
        .assert_problem(403, "permission_denied");
    let id = new_id();
    let r = put_doc(h, o, "transfers", &id, &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["status"], "requested");
    assert!(r.body["number"].as_str().unwrap().starts_with("BDG01-TR-"));
    let first = events.recv().await.unwrap();
    assert_eq!(first.kind, "transfer.changed");
    // The clerk ships from JKT01 (the sender), 4 of 5.
    let r = action(
        h,
        &sri,
        &format!("/api/v1/transfers/{id}/ship"),
        Some(json!({ "lines": [{ "line_no": 1, "qty_thousandths": 4_000 }] })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "shipped");
    assert!(
        r.body["lines"][0].get("shipped_value").is_none(),
        "no report.profit"
    );
    assert_eq!(
        row(h, o, &s.tea.variant).await["in_transit_thousandths"],
        4_000
    );
    action(h, o, &format!("/api/v1/transfers/{id}/cancel"), None)
        .await
        .assert_problem(409, "conflict");
    // The clerk cannot receive at BDG01.
    action(h, &sri, &format!("/api/v1/transfers/{id}/receive"), None)
        .await
        .assert_problem(403, "permission_denied");
    // More than shipped is refused; 3 of 4 is a shortage of 1.
    action(
        h,
        o,
        &format!("/api/v1/transfers/{id}/receive"),
        Some(json!({ "lines": [{ "line_no": 1, "qty_thousandths": 5_000 }] })),
    )
    .await
    .assert_problem(422, "invalid_field");
    let r = action(
        h,
        o,
        &format!("/api/v1/transfers/{id}/receive"),
        Some(json!({ "location_id": s.bdg_loc, "lines": [{ "line_no": 1, "qty_thousandths": 3_000 }] })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "received");
    assert_eq!(r.body["lines"][0]["lost_qty_thousandths"], 1_000);
    assert_eq!(r.body["lines"][0]["lost_value"], 1_000);
    let kinds: Vec<&str> = r.body["movements"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"transit_loss"), "{kinds:?}");
    assert_eq!(qty_of(h, o, &s.tea.variant).await, 6_000);
    let mut seen = Vec::new();
    while let Ok(e) = events.try_recv() {
        seen.push((e.kind, e.data["branch_id"].clone()));
    }
    assert!(seen.iter().any(|(k, _)| *k == "stock.changed"), "{seen:?}");
    assert!(
        seen.iter()
            .filter(|(k, _)| *k == "transfer.changed")
            .count()
            >= 4,
        "{seen:?}"
    );
    // A request can be cancelled.
    let id2 = new_id();
    put_doc(h, o, "transfers", &id2, &body).await;
    let r = action(h, o, &format!("/api/v1/transfers/{id2}/cancel"), None).await;
    assert_eq!(r.body["status"], "cancelled");
    let list = h
        .call(o, Method::GET, "/api/v1/transfers?status=received", None)
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    let one = h
        .call(o, Method::GET, &format!("/api/v1/transfers/{id}"), None)
        .await;
    assert_eq!(one.body["lines"][0]["received_qty_thousandths"], 3_000);
    // A branch node does not hold the other branch.
    let branch_node = router(h.state(NodeRole::Branch));
    let bearer = format!("Bearer {o}");
    let mut unknown = body.clone();
    unknown["to_branch_id"] = new_id().into();
    unknown["ship_from_location_id"] = s.jkt_loc.clone().into();
    let r = Harness::send_to(
        branch_node,
        Method::PUT,
        &format!("/api/v1/transfers/{}", new_id()),
        &[("authorization", &bearer)],
        Some(unknown),
    )
    .await;
    r.assert_problem(409, "other_node");
}

/// Spec §17 case 19: two counters add up; a second open opname on the same variant and
/// location is `409 opname_open`; posting needs `stock.adjust`.
async fn opname_counts_and_permissions(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    receive(
        h,
        o,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_000)]),
    )
    .await;
    let (_, sri) = clerk(h).await;
    let id = new_id();
    let cycle = json!({ "scope": "cycle", "variant_ids": [s.tea.variant] });
    let r = put_doc(h, &sri, "opnames", &id, &cycle).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert!(r.body["number"].as_str().unwrap().contains("-OP-"));
    assert_eq!(r.body["lines"][0]["snapshot_thousandths"], 10_000);
    let other = put_doc(h, o, "opnames", &new_id(), &cycle).await;
    other.assert_problem(409, "opname_open");
    assert_eq!(other.body["opname"], r.body["number"]);
    let count = |n: i64, replace: bool| {
        json!({ "entries": [{
            "id": new_id(), "variant_id": s.tea.variant, "unit_id": s.tea.pcs,
            "qty_thousandths": n * 1000, "replace": replace,
        }] })
    };
    let uri = format!("/api/v1/opnames/{id}/counts");
    let r = action(h, &sri, &uri, Some(count(4, false))).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = action(h, o, &uri, Some(count(4, false))).await;
    assert_eq!(r.body["lines"][0]["counted_thousandths"], 8_000);
    assert_eq!(r.body["lines"][0]["variance_thousandths"], -2_000);
    // Same entry ID twice counts once.
    let single = count(1, false);
    action(h, o, &uri, Some(single.clone())).await;
    let r = action(h, o, &uri, Some(single)).await;
    assert_eq!(r.body["lines"][0]["counted_thousandths"], 9_000);
    // The clerk cannot post; the owner can.
    action(h, &sri, &format!("/api/v1/opnames/{id}/post"), None)
        .await
        .assert_problem(403, "permission_denied");
    action(
        h,
        o,
        &format!("/api/v1/opnames/{id}/post"),
        Some(json!({ "uncounted": "maybe" })),
    )
    .await
    .assert_problem(422, "invalid_field");
    let r = action(
        h,
        o,
        &format!("/api/v1/opnames/{id}/post"),
        Some(json!({ "uncounted": "leave" })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "posted");
    assert_eq!(r.body["movements"][0]["kind"], "count");
    assert_eq!(qty_of(h, o, &s.tea.variant).await, 9_000);
    action(h, o, &uri, Some(count(1, false)))
        .await
        .assert_problem(409, "conflict");
    // Free again: a full count, cancelled.
    let id2 = new_id();
    let r = put_doc(h, &sri, "opnames", &id2, &json!({ "scope": "full" })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = action(h, &sri, &format!("/api/v1/opnames/{id2}/cancel"), None).await;
    assert_eq!(r.body["status"], "cancelled");
    put_doc(h, o, "opnames", &new_id(), &json!({ "scope": "half" }))
        .await
        .assert_problem(422, "invalid_field");
    let list = h.call(o, Method::GET, "/api/v1/opnames", None).await;
    assert_eq!(list.body.as_array().unwrap().len(), 2);
    let one = h
        .call(o, Method::GET, &format!("/api/v1/opnames/{id}"), None)
        .await;
    assert_eq!(one.body["uncounted"], "leave");
    assert_eq!(h.audit_count("opname.count").await, 3);
}

/// Spec §17 case 20: without `report.profit` cost and value fields are left out (not zeroed);
/// a Stock clerk receives, transfers and counts but neither adjusts nor posts an opname.
async fn costs_hidden_without_report_profit(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let (_, sri) = clerk(h).await;
    // The clerk receives.
    let gr = receive(
        h,
        &sri,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 10, 1_000)]),
    )
    .await;
    assert!(gr.get("landed").is_none());
    assert!(gr["lines"][0].get("landed").is_none());
    assert!(gr["lines"][0].get("unit_cost").is_none());
    assert_eq!(
        gr["lines"][0]["price"], 1_000,
        "the invoice's own price is shown"
    );
    assert!(gr["movements"][0].get("value").is_none());
    let owner_view = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/goods-receipts/{}", gr["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(owner_view.body["landed"], 10_000);
    assert_eq!(owner_view.body["lines"][0]["unit_cost"], 1_000);
    assert_eq!(owner_view.body["movements"][0]["value"], 10_000);
    // On hand: no unit cost or value for the clerk.
    let clerk_row = row(h, &sri, &s.tea.variant).await;
    assert!(clerk_row.get("unit_cost").is_none() && clerk_row.get("value").is_none());
    let owner_row = row(h, o, &s.tea.variant).await;
    assert_eq!(
        (&owner_row["unit_cost"], &owner_row["value"]),
        (&json!(1_000), &json!(10_000))
    );
    // "No cost yet" is null, not absent, for the owner.
    let opening = row(h, o, &s.milk.variant).await;
    assert!(opening.is_null(), "milk never moved");
    // Stock card values.
    let card = h
        .call(
            &sri,
            Method::GET,
            &format!("/api/v1/stock/card/{}", s.tea.variant),
            None,
        )
        .await;
    assert!(card.body["rows"][0]["movement"].get("value").is_none());
    // Supplier item last price.
    let item = h
        .call(
            &sri,
            Method::GET,
            &format!("/api/v1/suppliers/{}/items/{}", s.supplier, s.tea.variant),
            None,
        )
        .await;
    assert!(item.body.get("last_price").is_none());
    // The clerk transfers inside the branch and counts.
    let back = location(h, o, &h.jkt.id.to_string(), "Belakang").await;
    let r = put_doc(
        h,
        &sri,
        "internal-transfers",
        &new_id(),
        &json!({
            "from_location_id": s.jkt_loc, "to_location_id": back,
            "lines": [{ "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": 1_000 }],
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert!(r.body["lines"][0].get("value").is_none());
    let r = put_doc(
        h,
        &sri,
        "opnames",
        &new_id(),
        &json!({ "scope": "cycle", "variant_ids": [s.milk.variant] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    // The clerk cannot adjust or post opening stock.
    put_doc(
        h,
        &sri,
        "adjustments",
        &new_id(),
        &adjustment(&s.tea, -1, RUSAK),
    )
    .await
    .assert_problem(403, "permission_denied");
    put_doc(
        h,
        &sri,
        "opening-stock",
        &new_id(),
        &json!({ "lines": [{ "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": 1_000, "unit_cost": 1 }] }),
    )
    .await
    .assert_problem(403, "permission_denied");
    // Lists of the owner carry values; the clerk's lists do not.
    let r = put_doc(
        h,
        o,
        "adjustments",
        &new_id(),
        &adjustment(&s.tea, 1, KOREKSI),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["lines"][0]["value"], 1_000);
    let clerk_list = h.call(&sri, Method::GET, "/api/v1/adjustments", None).await;
    assert!(clerk_list.body[0]["lines"][0].get("value").is_none());
    let owner_list = h.call(o, Method::GET, "/api/v1/adjustments", None).await;
    assert_eq!(owner_list.body[0]["lines"][0]["value"], 1_000);
    let one = h
        .call(
            &sri,
            Method::GET,
            &format!("/api/v1/adjustments/{}", r.body["id"].as_str().unwrap()),
            None,
        )
        .await;
    assert_eq!(one.body["lines"][0]["reason_id"], KOREKSI);
    // An increase without a cost yet needs a typed unit cost.
    put_doc(
        h,
        o,
        "adjustments",
        &new_id(),
        &adjustment(&s.milk, 1, KOREKSI),
    )
    .await
    .assert_problem(422, "invalid_field");
    // A reader without any inventory key (a Cashier) is refused.
    let cashier = crate::sales::ani(h).await;
    let r = h
        .at_register(
            (&cashier, &h.jkt_device),
            Method::GET,
            "/api/v1/stock/on-hand",
            &[],
            None,
        )
        .await;
    r.assert_problem(403, "permission_denied");
    // `report.profit` alone is an inventory read key (§12), and it shows the cost.
    let auditor = profit_reader(h, o).await;
    let r = row(h, &auditor, &s.tea.variant).await;
    assert!(r.get("unit_cost").is_some(), "{r}");
    let list = h
        .call(&sri, Method::GET, "/api/v1/opening-stock", None)
        .await;
    assert_eq!(list.status, StatusCode::OK);
    let list = h
        .call(&sri, Method::GET, "/api/v1/internal-transfers", None)
        .await;
    let it = list.body[0]["id"].as_str().unwrap().to_owned();
    let one = h
        .call(
            &sri,
            Method::GET,
            &format!("/api/v1/internal-transfers/{it}"),
            None,
        )
        .await;
    assert_eq!(one.body["to_location_id"], back.as_str());
}

/// Spec §17 case 21: a sale after 2.8 has a value at the average; a post-void and a restocked
/// return reverse it exactly. Every posting publishes `stock.changed`.
async fn sales_values_reverse_exactly(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    // 3 @ total 1 000: the average is a fraction.
    let r = put_doc(
        h,
        o,
        "opening-stock",
        &new_id(),
        &json!({ "lines": [{ "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": 3_000, "unit_cost": 333 }] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let mut events = h.events.subscribe();
    let token = crate::sales::ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let sold = sell(
        h,
        at,
        sale(vec![line(&s.tea, 2)], vec![tender(CASH, 4_000)]),
    )
    .await;
    let event = loop {
        let e = events.recv().await.unwrap();
        if e.kind == "stock.changed" {
            break e;
        }
    };
    assert_eq!(event.data["variant_ids"][0], s.tea.variant.as_str());
    let moves = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/card/{}", s.tea.variant),
            None,
        )
        .await;
    let rows = moves.body["rows"].as_array().unwrap();
    let sale_row = &rows[1]["movement"];
    assert_eq!(
        (&sale_row["kind"], &sale_row["value"]),
        (&json!("sale"), &json!(-666))
    );
    // A return of 1 restocked at the sale line's value.
    let ret = json!({
        "sale_id": sold["id"], "refund": "cash",
        "lines": [{ "sale_line_no": 1, "variant_id": s.tea.variant, "unit_id": s.tea.pcs, "qty_thousandths": 1_000 }],
    });
    let r = h
        .at_register(
            at,
            Method::PUT,
            &format!("/api/v1/returns/{}", new_id()),
            &[],
            Some(ret),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let moves = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/card/{}", s.tea.variant),
            None,
        )
        .await;
    let last = moves.body["rows"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_eq!(
        (&last["movement"]["kind"], &last["movement"]["value"]),
        (&json!("return"), &json!(333))
    );
    // A second sale, post-voided: the void puts back exactly what it took.
    let second = sell(
        h,
        at,
        sale(vec![line(&s.tea, 1)], vec![tender(CASH, 2_000)]),
    )
    .await;
    let sid = second["id"].as_str().unwrap();
    let t = approve(h, at, "sale.post_void", Value::Null, sid).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/sales/{sid}/void"),
            &[("x-override", &t)],
            Some(json!({ "reason": "wrong_item", "refund": "cash" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let moves = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/card/{}", s.tea.variant),
            None,
        )
        .await;
    let rows = moves.body["rows"].as_array().unwrap();
    let n = rows.len();
    let (sold_row, void_row) = (&rows[n - 2]["movement"], &rows[n - 1]["movement"]);
    assert_eq!(void_row["kind"], "sale_void");
    assert_eq!(
        sold_row["value"].as_i64().unwrap(),
        -void_row["value"].as_i64().unwrap()
    );
    assert_eq!(rows[n - 1]["balance_thousandths"], 2_000);
    // The card pages by cursor.
    let page = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/card/{}?limit=2", s.tea.variant),
            None,
        )
        .await;
    let cursor = page.body["next_cursor"].as_str().unwrap().to_owned();
    let next = h
        .call(
            o,
            Method::GET,
            &format!(
                "/api/v1/stock/card/{}?limit=2&cursor={cursor}",
                s.tea.variant
            ),
            None,
        )
        .await;
    assert_eq!(next.body["rows"][0]["balance_thousandths"], 2_000);
}

/// Spec §17 case 22: the till reads this branch's on hand with only its device token, no
/// cost; a user without a register needs `sale.create`.
async fn till_lookup_with_the_register_token(h: &Harness) {
    let s = shop(h).await;
    receive(
        h,
        &s.owner,
        &receipt(&s, "INV-1", vec![gr_line(&s.tea, 12, 1_000)]),
    )
    .await;
    let uri = format!("/api/v1/registers/me/stock/{}", s.tea.variant);
    let r = h
        .send(
            Method::GET,
            &uri,
            &[("x-register-token", &h.jkt_device)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["qty_thousandths"], 12_000);
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    assert!(r.body.get("unit_cost").is_none() && r.body.get("value").is_none());
    // BDG01's register sees its own branch.
    let r = h
        .send(
            Method::GET,
            &uri,
            &[("x-register-token", &h.bdg_device)],
            None,
        )
        .await;
    assert_eq!(r.body["qty_thousandths"], 0);
    // No credentials, or a bad token.
    h.send(Method::GET, &uri, &[], None)
        .await
        .assert_problem(401, "invalid_session");
    h.send(Method::GET, &uri, &[("x-register-token", "nope")], None)
        .await
        .assert_problem(401, "invalid_device_token");
    // A back-office session: the Manager sells, the Stock clerk does not.
    let budi = h.manager_token().await;
    let r = h.call(&budi, Method::GET, &uri, None).await;
    assert_eq!(r.body["qty_thousandths"], 12_000);
    let (_, sri) = clerk(h).await;
    h.call(&sri, Method::GET, &uri, None)
        .await
        .assert_problem(403, "permission_denied");
}

/// Suppliers, supplier items, stock reasons and the settings are HQ data: `read_only_replica`
/// on a branch node, their keys, and audited.
async fn hq_data_suppliers_reasons_settings(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    let branch_node = router(h.state(NodeRole::Branch));
    let bearer = format!("Bearer {o}");
    let r = Harness::send_to(
        branch_node.clone(),
        Method::PUT,
        &format!("/api/v1/suppliers/{}", new_id()),
        &[("authorization", &bearer)],
        Some(json!({ "code": "X", "name": "X" })),
    )
    .await;
    r.assert_problem(403, "read_only_replica");
    for uri in [
        format!("/api/v1/stock-reasons/{}", new_id()),
        "/api/v1/settings/inventory".to_owned(),
        format!("/api/v1/suppliers/{}/items/{}", s.supplier, s.tea.variant),
    ] {
        Harness::send_to(
            branch_node.clone(),
            Method::PUT,
            &uri,
            &[("authorization", &bearer)],
            Some(json!({})),
        )
        .await
        .assert_problem(403, "read_only_replica");
    }
    // Suppliers: replay, duplicate code, archive.
    let r = h
        .call(
            o,
            Method::PUT,
            &format!("/api/v1/suppliers/{}", s.supplier),
            Some(json!({ "code": "sup1", "name": "PT Pemasok", "terms_days": 30 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.audit_count("supplier.put").await, 1);
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/suppliers/{}", new_id()),
        Some(json!({ "code": "SUP1", "name": "Other" })),
    )
    .await
    .assert_problem(409, "conflict");
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/suppliers/{}", new_id()),
        Some(json!({ "code": "", "name": "Other" })),
    )
    .await
    .assert_problem(422, "invalid_field");
    let (_, sri) = clerk(h).await;
    h.call(
        &sri,
        Method::PUT,
        &format!("/api/v1/suppliers/{}", new_id()),
        Some(json!({ "code": "S2", "name": "Other" })),
    )
    .await
    .assert_problem(403, "permission_denied");
    let list = h.call(&sri, Method::GET, "/api/v1/suppliers", None).await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    let r = h
        .call(
            o,
            Method::DELETE,
            &format!("/api/v1/suppliers/{}", s.supplier),
            None,
        )
        .await;
    assert_eq!(r.body["archived"], true);
    let list = h.call(o, Method::GET, "/api/v1/suppliers", None).await;
    assert!(list.body.as_array().unwrap().is_empty());
    let list = h
        .call(o, Method::GET, "/api/v1/suppliers?archived=true", None)
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    let one = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/suppliers/{}", s.supplier),
            None,
        )
        .await;
    assert_eq!(one.body["archived"], true);
    // Supplier items: preferred.
    let other = new_id();
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/suppliers/{other}"),
        Some(json!({ "code": "S2", "name": "Dua" })),
    )
    .await;
    let r = h
        .call(
            o,
            Method::PUT,
            &format!("/api/v1/suppliers/{other}/items/{}", s.tea.variant),
            Some(json!({ "supplier_code": " T-01 ", "preferred": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["supplier_code"], "T-01");
    assert_eq!(
        r.body["last_price"],
        Value::Null,
        "never received: null, not absent"
    );
    h.call(
        o,
        Method::GET,
        &format!("/api/v1/suppliers/{other}/items/{}", s.milk.variant),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    // Stock reasons.
    let reasons = h
        .call(&sri, Method::GET, "/api/v1/stock-reasons", None)
        .await;
    assert_eq!(reasons.body.as_array().unwrap().len(), 6);
    let rid = new_id();
    let r = h
        .call(
            o,
            Method::PUT,
            &format!("/api/v1/stock-reasons/{rid}"),
            Some(json!({ "name": "Bonus", "direction": "in", "sort": 9 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/stock-reasons/{rid}"),
        Some(json!({ "name": "Bonus", "direction": "sideways" })),
    )
    .await
    .assert_problem(422, "invalid_field");
    h.call(
        &sri,
        Method::PUT,
        &format!("/api/v1/stock-reasons/{rid}"),
        Some(json!({ "name": "Bonus", "direction": "in" })),
    )
    .await
    .assert_problem(403, "permission_denied");
    let r = h
        .call(
            o,
            Method::DELETE,
            &format!("/api/v1/stock-reasons/{rid}"),
            None,
        )
        .await;
    assert_eq!(r.body["archived"], true);
    h.call(
        o,
        Method::PUT,
        &format!("/api/v1/stock-reasons/{rid}"),
        Some(json!({ "name": "B", "direction": "in" })),
    )
    .await
    .assert_problem(409, "archived");
    let all = h
        .call(o, Method::GET, "/api/v1/stock-reasons?archived=true", None)
        .await;
    assert_eq!(all.body.as_array().unwrap().len(), 7);
    // Settings.
    let r = h
        .call(&sri, Method::GET, "/api/v1/settings/inventory", None)
        .await;
    assert_eq!(
        (
            &r.body["pkp"],
            &r.body["standard_ppn_rate_bp"],
            &r.body["expiry_warn_days"]
        ),
        (&json!(false), &json!(1_100), &json!(30))
    );
    let mut settings = r.body.clone();
    settings["pkp"] = true.into();
    h.call(
        &sri,
        Method::PUT,
        "/api/v1/settings/inventory",
        Some(settings.clone()),
    )
    .await
    .assert_problem(403, "permission_denied");
    let r = h
        .call(
            o,
            Method::PUT,
            "/api/v1/settings/inventory",
            Some(settings.clone()),
        )
        .await;
    assert_eq!(r.body["pkp"], true);
    settings["velocity_days"] = 0.into();
    h.call(o, Method::PUT, "/api/v1/settings/inventory", Some(settings))
        .await
        .assert_problem(422, "invalid_field");
    assert_eq!(h.audit_count("inventory_settings.put").await, 1);
}

/// The on-hand list's filters (category, brand, text, negative, low, expiring) and paging, and
/// the lots' Expiring list.
async fn on_hand_filters_and_lots(h: &Harness) {
    let s = shop(h).await;
    let o = &s.owner;
    // A categorised, branded product.
    let (cat, brand) = (new_id(), new_id());
    let r = h
        .call(
            o,
            Method::PUT,
            &format!("/api/v1/categories/{cat}"),
            Some(json!({ "name": "Minuman" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = h
        .call(
            o,
            Method::PUT,
            &format!("/api/v1/brands/{brand}"),
            Some(json!({ "name": "Sariwangi" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let (pid, variant, pcs) = (new_id(), new_id(), new_id());
    let r = put_product(
        h,
        o,
        &pid,
        &json!({
            "code": "KOPI", "name": "Kopi Bubuk", "type": "stock", "tax_class_id": STANDARD,
            "category_id": cat, "brand_id": brand,
            "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
            "base_unit_id": pcs, "variants": [{ "id": variant }],
        }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let kopi = Item {
        variant: variant.clone(),
        pcs,
    };
    let mut milk = gr_line(&s.milk, 2, 5_000);
    milk["expiry"] = "2027-01-05".into();
    milk["batch"] = "M1".into();
    receive(
        h,
        o,
        &receipt(
            &s,
            "INV-1",
            vec![gr_line(&s.tea, 3, 1_000), gr_line(&kopi, 4, 2_000), milk],
        ),
    )
    .await;
    let r = put_doc(
        h,
        o,
        "adjustments",
        &new_id(),
        &adjustment(&s.tea, -5, RUSAK),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        r.body["negative_after"][0]["qty_thousandths"], -2_000,
        "a warning, never a refusal"
    );
    let list = |q: &'static str| async move {
        let r = h
            .call(o, Method::GET, &format!("/api/v1/stock/on-hand?{q}"), None)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{q}: {}", r.body);
        r.body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x["product_code"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let all = list("").await;
    assert_eq!(all.len(), 3);
    assert_eq!(list("filter=negative").await, ["TEH"]);
    assert_eq!(list("q=kopi").await, ["KOPI"]);
    assert_eq!(list("q=teh").await, ["TEH"]);
    let by_cat = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/on-hand?category_id={cat}"),
            None,
        )
        .await;
    assert_eq!(by_cat.body["rows"][0]["product_name"], "Kopi Bubuk");
    let by_brand = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/on-hand?brand_id={brand}&q=sariwangi"),
            None,
        )
        .await;
    assert_eq!(by_brand.body["rows"].as_array().unwrap().len(), 1);
    // The test clock is 2027-01-15: milk (expired 2027-01-05) is expiring.
    assert_eq!(list("filter=expiring").await, ["SUSU"]);
    h.call(o, Method::GET, "/api/v1/stock/on-hand?filter=odd", None)
        .await
        .assert_problem(422, "invalid_field");
    // Paging.
    let page = h
        .call(o, Method::GET, "/api/v1/stock/on-hand?limit=2", None)
        .await;
    assert_eq!(page.body["rows"].as_array().unwrap().len(), 2);
    let cursor = page.body["next_cursor"].as_str().unwrap().to_owned();
    let rest = h
        .call(
            o,
            Method::GET,
            &format!("/api/v1/stock/on-hand?limit=2&cursor={cursor}"),
            None,
        )
        .await;
    assert_eq!(rest.body["rows"].as_array().unwrap().len(), 1);
    assert_eq!(rest.body["next_cursor"], Value::Null);
    h.call(o, Method::GET, "/api/v1/stock/on-hand?cursor=junk", None)
        .await
        .assert_problem(422, "invalid_field");
    // Lots: the Expiring list with its value at the average.
    let lots = h
        .call(o, Method::GET, "/api/v1/stock/lots?expiring=true", None)
        .await;
    assert_eq!(lots.body[0]["batch"], "M1");
    assert_eq!(lots.body[0]["expired"], true);
    assert_eq!(lots.body[0]["value"], 10_000);
    let (_, sri) = clerk(h).await;
    let lots = h
        .call(&sri, Method::GET, "/api/v1/stock/lots?expiring=true", None)
        .await;
    assert!(lots.body[0].get("value").is_none());
}
