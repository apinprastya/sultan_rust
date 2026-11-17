//! Sales over HTTP (spec 2.6 §5, §7, §9, §12; phase S3): tender types, posting with the re-quote
//! and the permission checks, replay, payment rules, post-void, receipts and lists. Spec 2.6 §15
//! cases 4–10 and 12. Returns, held carts and the close report are in `sales_returns.rs`.

// The tests are scripts that read top to bottom with short names (`h`, `r`, `t`), as elsewhere in
// this suite; the body helpers take JSON values by value so call sites stay short.
#![allow(
    clippy::many_single_char_names,
    clippy::too_many_lines,
    clippy::needless_pass_by_value
)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::ids::{BranchId, VariantId};
use sultan_db::sales::stock_on_hand;
use sultan_server::{NodeRole, router};

use crate::catalog_fixtures::{STANDARD, put_product};
use crate::common::{ANI_PIN, BUDI_PIN, Harness, Response, api_tests, new_id};
use crate::pricing::{DEFAULT_LIST, fixed, new_list, price};

api_tests!(
    tender_types_are_hq_master_data,
    sale_needs_an_open_session_and_answers_stale_carts,
    replay_returns_the_stored_sale,
    split_payment_and_tender_rules,
    cash_rounding_applies_to_cash_only,
    saved_change_and_balance_tender,
    line_discount_above_the_limit_needs_an_override,
    post_void_pays_back_and_reverses_stock,
    receipts_reprints_and_lookups,
);

pub const CASH: &str = "00000000-0000-7000-8000-000000000301";
pub const CARD: &str = "00000000-0000-7000-8000-000000000302";
pub const QRIS: &str = "00000000-0000-7000-8000-000000000303";
pub const BALANCE: &str = "00000000-0000-7000-8000-000000000304";

/// A product with one variant and unit `pcs`, priced `amount` in the default list.
pub struct Item {
    pub variant: String,
    pub pcs: String,
}

pub async fn item(h: &Harness, owner: &str, code: &str, amount: i64) -> Item {
    let (id, variant, pcs) = (new_id(), new_id(), new_id());
    let body = json!({
        "code": code, "name": format!("Produk {code}"), "type": "stock", "tax_class_id": STANDARD,
        "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": pcs,
        "variants": [{ "id": variant }],
    });
    let r = put_product(h, owner, &id, &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    price(
        h,
        owner,
        DEFAULT_LIST,
        &variant,
        &pcs,
        json!([fixed(1, amount)]),
    )
    .await;
    Item { variant, pcs }
}

/// A new price at JKT01 (a branch price list, once per test).
pub async fn reprice(h: &Harness, owner: &str, item: &Item, amount: i64) {
    let list = new_list(h, owner, "branch", &h.jkt.id.to_string(), Value::Null).await;
    price(
        h,
        owner,
        &list,
        &item.variant,
        &item.pcs,
        json!([fixed(1, amount)]),
    )
    .await;
}

/// A sale line of `qty` units.
pub fn line(item: &Item, qty: i64) -> Value {
    json!({ "variant_id": item.variant, "unit_id": item.pcs, "qty_thousandths": qty * 1000 })
}

/// A tender.
pub fn tender(kind: &str, amount: i64) -> Value {
    json!({ "tender_type_id": kind, "amount": amount })
}

/// A sale body with zero totals (fixed by [`fresh_totals`]).
pub fn sale(lines: Vec<Value>, tenders: Vec<Value>) -> Value {
    json!({
        "lines": lines,
        "tenders": tenders,
        "totals": { "subtotal": 0, "line_discount": 0, "cart_discount": 0, "tax": 0, "total": 0 },
    })
}

/// The ani PIN session at JKT01 R1.
pub async fn ani(h: &Harness) -> String {
    h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await
}

/// Opens a cashier session at the register; returns its ID.
pub async fn open(h: &Harness, at: (&str, &str), cash: i64) -> String {
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/cashier-sessions",
            &[],
            Some(json!({ "opening_cash": cash })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body["id"].as_str().unwrap().to_owned()
}

/// `PUT /sales/{id}` with optional `X-Override`.
pub async fn put_sale(
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
        &format!("/api/v1/sales/{id}"),
        &headers,
        Some(body.clone()),
    )
    .await
}

/// Puts the fresh quote's totals into `body` (asks the server with stale totals first:
/// spec 2.6 §15 case 9).
pub async fn fresh_totals(h: &Harness, at: (&str, &str), body: &mut Value) {
    let r = put_sale(h, at, &new_id(), body, None).await;
    if r.status == StatusCode::CONFLICT && r.body["code"] == "cart_stale" {
        body["totals"] = r.body["quote"]["totals"].clone();
    }
}

/// Posts `body` with fresh totals; asserts `201`.
pub async fn sell(h: &Harness, at: (&str, &str), mut body: Value) -> Value {
    fresh_totals(h, at, &mut body).await;
    let r = put_sale(h, at, &new_id(), &body, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body
}

/// A manager override token for `action` with `value` on `context`, approved by budi.
pub async fn approve(
    h: &Harness,
    at: (&str, &str),
    action: &str,
    value: Value,
    context: &str,
) -> String {
    let r = h
        .at_register(
            at,
            Method::POST,
            "/api/v1/overrides",
            &[],
            Some(json!({
                "action": action, "value": value, "context_ref": context,
                "approver_id": h.budi.id.to_string(), "approver_pin": BUDI_PIN,
            })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body["token"].as_str().unwrap().to_owned()
}

/// Sets the pricing settings' cash rounding.
pub async fn rounding(h: &Harness, owner: &str, value: &str) {
    let r = h
        .call(
            owner,
            Method::PUT,
            "/api/v1/settings/pricing",
            Some(json!({ "tax_inclusive": true, "tax_registered": true, "cash_rounding": value })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

/// A customer created by `owner`; returns its ID.
pub async fn customer(h: &Harness, owner: &str, name: &str) -> String {
    let created = crate::customers::create(h, owner, crate::customers::body(name, None)).await;
    crate::customers::id(&created)
}

pub async fn balance_of(h: &Harness, owner: &str, customer: &str) -> Option<i64> {
    let uri = format!("/api/v1/customers/{customer}/balance");
    h.call(owner, Method::GET, &uri, None).await.body["balance"].as_i64()
}

pub async fn on_hand(h: &Harness, variant: &str) -> i64 {
    let variant: VariantId = variant.parse().unwrap();
    let branch: BranchId = h.jkt.id;
    stock_on_hand(&h.db, branch, variant)
        .await
        .unwrap()
        .thousandths()
}

async fn tender_types_are_hq_master_data(h: &Harness) {
    let owner = h.owner_token().await;
    let r = h
        .send(
            Method::GET,
            "/api/v1/tender-types",
            &[("x-register-token", &h.jkt_device)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let names: Vec<&str> = r
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Tunai", "Kartu debit", "QRIS", "Saldo pelanggan"]);

    let id = new_id();
    let uri = format!("/api/v1/tender-types/{id}");
    let body = json!({ "name": " Debit BCA ", "kind": "card", "needs_reference": true, "sort": 9 });
    let r = h.call(&owner, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["name"], "Debit BCA");
    assert_eq!(r.body["active"], true);
    let r = h.call(&owner, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!(r.status, StatusCode::OK);
    // The kind is fixed; exchange is no kind; unknown kinds are refused.
    for kind in ["qris", "exchange", "cheque"] {
        let mut b = body.clone();
        b["kind"] = kind.into();
        h.call(&owner, Method::PUT, &uri, Some(b))
            .await
            .assert_problem(422, "invalid_field");
    }
    // A cashier may not; a branch node refuses master data.
    let ani_pw = h.manager_token().await;
    h.call(&ani_pw, Method::PUT, &uri, Some(body.clone()))
        .await
        .assert_problem(403, "permission_denied");
    let bearer = format!("Bearer {owner}");
    let r = Harness::send_to(
        router(h.state(NodeRole::Branch)),
        Method::PUT,
        &uri,
        &[("authorization", &bearer)],
        Some(body.clone()),
    )
    .await;
    r.assert_problem(403, "read_only_replica");
    // Archive: twice is fine, then no edit; the register's list leaves it out.
    for _ in 0..2 {
        let r = h.call(&owner, Method::DELETE, &uri, None).await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.body["archived"], true);
    }
    h.call(&owner, Method::PUT, &uri, Some(body))
        .await
        .assert_problem(409, "archived");
    let r = h
        .call(&owner, Method::GET, "/api/v1/tender-types", None)
        .await;
    assert_eq!(r.body.as_array().unwrap().len(), 4);
    let r = h
        .call(&owner, Method::GET, "/api/v1/tender-types?all=true", None)
        .await;
    assert_eq!(r.body.as_array().unwrap().len(), 5);
    h.call(
        &owner,
        Method::DELETE,
        &format!("/api/v1/tender-types/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

/// Spec 2.6 §15 case 9 and the open-session rule.
async fn sale_needs_an_open_session_and_answers_stale_carts(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let body = sale(vec![line(&teh, 2)], vec![tender(CASH, 20_000)]);
    put_sale(h, at, &new_id(), &body, None)
        .await
        .assert_problem(409, "no_open_session");
    // A back-office session is not at a register.
    h.call(
        &owner,
        Method::PUT,
        &format!("/api/v1/sales/{}", new_id()),
        Some(body.clone()),
    )
    .await
    .assert_problem(403, "register_session_required");
    open(h, at, 100_000).await;
    let r = put_sale(h, at, &new_id(), &body, None).await;
    r.assert_problem(409, "cart_stale");
    assert_eq!(r.body["quote"]["totals"]["total"], 20_000);
    assert_eq!(r.body["quote"]["lines"][0]["unit_price"], 10_000);
    // The price changes between quote and post.
    let mut fresh = body.clone();
    fresh["totals"] = r.body["quote"]["totals"].clone();
    reprice(h, &owner, &teh, 11_000).await;
    let r = put_sale(h, at, &new_id(), &fresh, None).await;
    r.assert_problem(409, "cart_stale");
    assert_eq!(r.body["quote"]["totals"]["total"], 22_000);
    // Rule errors.
    let mut bad = fresh.clone();
    bad["lines"][0]["qty_thousandths"] = 1_500.into();
    put_sale(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_qty");
    bad = fresh.clone();
    bad["lines"] = json!([]);
    put_sale(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_field");
    bad["lines"] = Value::Array(vec![line(&teh, 1); 301]);
    put_sale(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "cart_too_large");
    bad = fresh.clone();
    bad["lines"][0]["variant_id"] = new_id().into();
    put_sale(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_reference");
    bad = fresh.clone();
    bad["tax_invoice"] = true.into();
    put_sale(h, at, &new_id(), &bad, None)
        .await
        .assert_problem(422, "invalid_field");
    put_sale(h, at, "not-a-uuid", &fresh, None)
        .await
        .assert_problem(400, "invalid_body");
}

/// Spec 2.6 §15 case 10.
async fn replay_returns_the_stored_sale(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let mut body = sale(vec![line(&teh, 1)], vec![tender(CASH, 10_000)]);
    fresh_totals(h, at, &mut body).await;
    let id = new_id();
    let mut rx = h.events.subscribe();
    let first = put_sale(h, at, &id, &body, None).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    let number = first.body["number"].as_str().unwrap().to_owned();
    assert!(number.starts_with("JKT01-R1-"), "{number}");
    assert!(number.ends_with("-0001"), "{number}");
    let event = rx.try_recv().unwrap();
    assert_eq!(event.kind, "sale.posted");
    // The same body later, even after a price change, answers the stored sale.
    reprice(h, &owner, &teh, 12_000).await;
    let outbox = h.outbox_count().await;
    let again = put_sale(h, at, &id, &body, None).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, first.body);
    assert_eq!(h.outbox_count().await, outbox);
    assert_eq!(h.audit_count("sale.post").await, 1);
    assert_eq!(on_hand(h, &teh.variant).await, -1_000);
    // Another body on the same ID.
    let mut other = body.clone();
    other["tenders"][0]["amount"] = 20_000.into();
    put_sale(h, at, &id, &other, None)
        .await
        .assert_problem(409, "conflict");
    // GET answers the same.
    let r = h
        .at_register(at, Method::GET, &format!("/api/v1/sales/{id}"), &[], None)
        .await;
    assert_eq!(r.body, first.body);
}

/// Spec 2.6 §15 case 5.
async fn split_payment_and_tender_rules(h: &Harness) {
    let owner = h.owner_token().await;
    let beras = item(h, &owner, "BERAS", 63_500).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let lines = vec![line(&beras, 1)];
    let mut qris = tender(QRIS, 50_000);
    qris["reference"] = "  A1B2C3  ".into();
    let r = sell(
        h,
        at,
        sale(lines.clone(), vec![qris.clone(), tender(CASH, 20_000)]),
    )
    .await;
    assert_eq!(r["total"], 63_500);
    assert_eq!(r["change_given"], 6_500);
    assert_eq!(r["tenders"][0]["reference"], "A1B2C3");
    assert_eq!(r["tenders"][1]["cash_received"], 20_000);
    assert_eq!(r["tenders"][1]["amount"], 13_500);
    let mut body = sale(lines.clone(), vec![qris.clone()]);
    fresh_totals(h, at, &mut body).await;
    let refuse = |tenders: Value| {
        let mut b = body.clone();
        b["tenders"] = tenders;
        b
    };
    let mut over = qris.clone();
    over["amount"] = 70_000.into();
    let r = put_sale(h, at, &new_id(), &refuse(json!([over])), None).await;
    r.assert_problem(422, "tender_over_rest");
    assert_eq!(r.body["rest"], 63_500);
    put_sale(
        h,
        at,
        &new_id(),
        &refuse(json!([tender(QRIS, 50_000)])),
        None,
    )
    .await
    .assert_problem(422, "tender_reference_required");
    put_sale(h, at, &new_id(), &refuse(json!([qris])), None)
        .await
        .assert_problem(422, "invalid_field");
    put_sale(
        h,
        at,
        &new_id(),
        &refuse(json!([tender(&new_id(), 1)])),
        None,
    )
    .await
    .assert_problem(422, "invalid_reference");
    put_sale(
        h,
        at,
        &new_id(),
        &refuse(json!([tender(BALANCE, 1_000)])),
        None,
    )
    .await
    .assert_problem(422, "invalid_field");
    // An archived tender type is refused.
    h.call(
        &owner,
        Method::DELETE,
        &format!("/api/v1/tender-types/{CARD}"),
        None,
    )
    .await;
    let mut card = tender(CARD, 63_500);
    card["reference"] = "1234".into();
    put_sale(h, at, &new_id(), &refuse(json!([card])), None)
        .await
        .assert_problem(422, "invalid_field");
}

/// Spec 2.6 §15 case 6.
async fn cash_rounding_applies_to_cash_only(h: &Harness) {
    let owner = h.owner_token().await;
    rounding(h, &owner, "nearest_100").await;
    let beras = item(h, &owner, "BERAS", 63_450).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let r = sell(
        h,
        at,
        sale(vec![line(&beras, 1)], vec![tender(CASH, 70_000)]),
    )
    .await;
    assert_eq!(
        (r["total"].as_i64(), r["cash_rounding"].as_i64()),
        (Some(63_450), Some(50))
    );
    assert_eq!(
        (r["paid"].as_i64(), r["change_given"].as_i64()),
        (Some(63_500), Some(6_500))
    );
    let mut qris = tender(QRIS, 63_450);
    qris["reference"] = "9999".into();
    let r = sell(h, at, sale(vec![line(&beras, 1)], vec![qris])).await;
    assert_eq!(
        (r["paid"].as_i64(), r["cash_rounding"].as_i64()),
        (Some(63_450), Some(0))
    );
    // Cash between the exact and the rounded rest is refused.
    let mut body = sale(vec![line(&beras, 1)], vec![tender(CASH, 63_460)]);
    fresh_totals(h, at, &mut body).await;
    put_sale(h, at, &new_id(), &body, None)
        .await
        .assert_problem(422, "invalid_field");
}

/// Spec 2.6 §15 cases 7 and 8.
async fn saved_change_and_balance_tender(h: &Harness) {
    let owner = h.owner_token().await;
    let gula = item(h, &owner, "GULA", 46_300).await;
    let cust = customer(h, &owner, "Bu Sari").await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 10_000).await;
    let mut body = sale(vec![line(&gula, 1)], vec![tender(CASH, 50_000)]);
    body["customer_id"] = cust.clone().into();
    body["save_change"] = 700.into();
    let r = sell(h, at, body.clone()).await;
    assert_eq!(
        (r["change_given"].as_i64(), r["change_saved"].as_i64()),
        (Some(3_000), Some(700))
    );
    assert_eq!(balance_of(h, &owner, &cust).await, Some(700));
    // Above the per-sale limit (Rp 5 000 by default).
    let mut big = body.clone();
    big["tenders"] = json!([tender(CASH, 100_000)]);
    big["save_change"] = 6_000.into();
    let mut b = big.clone();
    fresh_totals(h, at, &mut b).await;
    put_sale(h, at, &new_id(), &b, None)
        .await
        .assert_problem(422, "balance_limit");
    // Case 8: the balance as a tender.
    let mut pay = sale(
        vec![line(&gula, 1)],
        vec![tender(BALANCE, 1_000), tender(CASH, 50_000)],
    );
    pay["customer_id"] = cust.clone().into();
    let mut p = pay.clone();
    fresh_totals(h, at, &mut p).await;
    put_sale(h, at, &new_id(), &p, None)
        .await
        .assert_problem(422, "insufficient_balance");
    p["tenders"][0]["amount"] = 700.into();
    let r = put_sale(h, at, &new_id(), &p, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(balance_of(h, &owner, &cust).await, Some(0));
    // The open session's expected cash includes the saved 700 (the manager sees it).
    let budi = h.manager_token().await;
    let r = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}"),
            None,
        )
        .await;
    assert_eq!(
        r.body["session"]["expected_cash"],
        10_000 + 50_000 - 3_000 + 50_000 - 4_400
    );
}

/// Spec 2.6 §15 case 4.
async fn line_discount_above_the_limit_needs_an_override(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let mut l = line(&teh, 1);
    l["discount"] = json!({ "pct_bp": 2_000 });
    let mut body = sale(vec![l], vec![tender(CASH, 8_000)]);
    body["cart_discount"] = json!({ "amount": 800 });
    fresh_totals(h, at, &mut body).await;
    assert_eq!(body["totals"]["total"], 7_200);
    let id = new_id();
    let r = put_sale(h, at, &id, &body, None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "sale.discount");
    assert_eq!(r.body["limit"], 1_000);
    let discount = approve(h, at, "sale.discount", json!(2_000), &id).await;
    let r = put_sale(h, at, &id, &body, Some(&discount)).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "sale.cart_discount");
    let cart = approve(h, at, "sale.cart_discount", json!(1_000), &id).await;
    put_sale(h, at, &id, &body, Some("nope"))
        .await
        .assert_problem(403, "invalid_override");
    let both = format!("{discount}, {cart}");
    let r = put_sale(h, at, &id, &body, Some(&both)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(h.audit_count("override.use").await, 2);
    // A price override on the same line: the approver is stored.
    let mut l = line(&teh, 1);
    l["typed_price"] = 9_000.into();
    let mut body = sale(vec![l], vec![tender(CASH, 9_000)]);
    fresh_totals(h, at, &mut body).await;
    let id = new_id();
    let r = put_sale(h, at, &id, &body, None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "price.override");
    let token = approve(h, at, "price.override", json!(1_000), &id).await;
    let r = put_sale(h, at, &id, &body, Some(&token)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["lines"][0]["price_mode"], "overridden");
    assert_eq!(r.body["lines"][0]["resolved_price"], 10_000);
    assert_eq!(r.body["lines"][0]["override_by"], h.budi.id.to_string());
}

/// Spec 2.6 §15 case 12.
async fn post_void_pays_back_and_reverses_stock(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let cust = customer(h, &owner, "Pak Budi").await;
    crate::customers::add_balance(
        h,
        &cust,
        sultan_db::customers::BalanceEntryKind::ChangeSaved,
        3_000,
    )
    .await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let first = open(h, at, 0).await;
    let mut body = sale(
        vec![line(&teh, 2)],
        vec![tender(BALANCE, 3_000), tender(CASH, 20_000)],
    );
    body["customer_id"] = cust.clone().into();
    let s = sell(h, at, body).await;
    let id = s["id"].as_str().unwrap().to_owned();
    assert_eq!(on_hand(h, &teh.variant).await, -2_000);
    let void_uri = format!("/api/v1/sales/{id}/void");
    let void_body = json!({ "reason": "customer_changed_mind", "refund": "cash" });
    let r = h
        .at_register(at, Method::POST, &void_uri, &[], Some(void_body.clone()))
        .await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "sale.post_void");
    let bad = json!({ "reason": "because", "refund": "cash" });
    h.at_register(at, Method::POST, &void_uri, &[], Some(bad))
        .await
        .assert_problem(422, "invalid_field");
    let bad = json!({ "reason": "wrong_item", "refund": "card" });
    h.at_register(at, Method::POST, &void_uri, &[], Some(bad))
        .await
        .assert_problem(422, "invalid_field");
    let t = approve(h, at, "sale.post_void", Value::Null, &id).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &void_uri,
            &[("x-override", &t), ("idempotency-key", "v1")],
            Some(void_body.clone()),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["status"], "voided");
    assert_eq!(r.body["void"]["payment_reversed"], 3_000);
    assert_eq!(r.body["void"]["cash"], 17_000);
    assert_eq!(on_hand(h, &teh.variant).await, 0);
    // The same key replays; a second void is a conflict.
    let again = h
        .at_register(
            at,
            Method::POST,
            &void_uri,
            &[("x-override", &t), ("idempotency-key", "v1")],
            Some(void_body.clone()),
        )
        .await;
    assert_eq!(again.body, r.body);
    let t2 = approve(h, at, "sale.post_void", Value::Null, &id).await;
    h.at_register(
        at,
        Method::POST,
        &void_uri,
        &[("x-override", &t2)],
        Some(void_body.clone()),
    )
    .await
    .assert_problem(409, "conflict");
    let balance = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/customers/{cust}/balance"),
            None,
        )
        .await;
    assert_eq!(balance.body["balance"], 3_000);

    // A card sale of the first session, voided to the balance after that session closed.
    let mut card = tender(CARD, 10_000);
    card["reference"] = "4321".into();
    let mut body = sale(vec![line(&teh, 1)], vec![card]);
    body["customer_id"] = cust.clone().into();
    let s = sell(h, at, body).await;
    let id = s["id"].as_str().unwrap().to_owned();
    let close = |session: String| async move {
        let r = h
            .at_register(
                at,
                Method::POST,
                &format!("/api/v1/cashier-sessions/{session}/close"),
                &[],
                Some(json!({ "counted_cash": 0 })),
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.body
    };
    let closed = close(first.clone()).await;
    assert_eq!(closed["sales"]["post_voids"]["count"], 1);
    assert_eq!(closed["sales"]["cash_refunds"], 17_000);
    assert_eq!(closed["expected_cash"], 0);
    assert_eq!(closed["sales"]["expected_cash"], 0);
    let second = open(h, at, 0).await;
    let t = approve(h, at, "sale.post_void", Value::Null, &id).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/sales/{id}/void"),
            &[("x-override", &t)],
            Some(json!({ "reason": "other", "reason_text": "salah", "refund": "balance" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["void"]["to_balance"], 10_000);
    assert_eq!(r.body["void"]["cashier_session_id"], second);
    assert_eq!(r.body["void"]["reason_text"], "salah");
    let budi = h.manager_token().await;
    let old = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{first}"),
            None,
        )
        .await;
    assert_eq!(old.body["close_report"]["expected_cash"], 0);
    assert_eq!(old.body["close_report"]["sales"]["post_voids"]["count"], 1);
    let now = close(second).await;
    assert_eq!(now["sales"]["post_voids"]["count"], 1);
    assert_eq!(now["sales"]["post_voids"]["amount"], 10_000);
}

async fn receipts_reprints_and_lookups(h: &Harness) {
    let owner = h.owner_token().await;
    let teh = item(h, &owner, "TEH", 10_000).await;
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 0).await;
    let older = sell(h, at, sale(vec![line(&teh, 1)], vec![tender(CASH, 10_000)])).await;
    let last = sell(h, at, sale(vec![line(&teh, 2)], vec![tender(CASH, 50_000)])).await;
    let receipt =
        |id: &Value, query: &str| format!("/api/v1/sales/{}/receipt{query}", id.as_str().unwrap());
    let r = h
        .at_register(at, Method::GET, &receipt(&last["id"], ""), &[], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let text = r.body["text"].as_str().unwrap();
    assert!(text.contains(last["number"].as_str().unwrap()), "{text}");
    assert!(
        text.contains("TOTAL") && text.contains("Tunai") && !text.contains("COPY"),
        "{text}"
    );
    assert!(
        r.body["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l.as_str().unwrap().chars().count() <= 32)
    );
    let r = h
        .at_register(
            at,
            Method::GET,
            &receipt(&last["id"], "?width=48&copy=true"),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["text"].as_str().unwrap().contains("COPY"));
    assert_eq!(h.audit_count("sale.reprint").await, 1);
    h.at_register(
        at,
        Method::GET,
        &receipt(&last["id"], "?width=40"),
        &[],
        None,
    )
    .await
    .assert_problem(422, "invalid_field");
    // Another sale needs `sale.reprint` (overridable).
    let r = h
        .at_register(
            at,
            Method::GET,
            &receipt(&older["id"], "?copy=true"),
            &[],
            None,
        )
        .await;
    r.assert_problem(403, "needs_override");
    let t = approve(
        h,
        at,
        "sale.reprint",
        Value::Null,
        older["id"].as_str().unwrap(),
    )
    .await;
    let r = h
        .at_register(
            at,
            Method::GET,
            &receipt(&older["id"], "?copy=true"),
            &[("x-override", &t)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // The back office reprints with `sale.reprint` (the owner has it).
    let r = h
        .call(
            &owner,
            Method::GET,
            &receipt(&older["id"], "?copy=true"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(h.audit_count("sale.reprint").await, 3);
    // Lookups.
    let number = last["number"].as_str().unwrap();
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/sales/by-number/{number}"),
            &[],
            None,
        )
        .await;
    assert_eq!(r.body["id"], last["id"]);
    h.at_register(at, Method::GET, "/api/v1/sales/by-number/NOPE", &[], None)
        .await
        .assert_problem(404, "not_found");
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/sales?cashier_session_id={session}"),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["items"].as_array().unwrap().len(), 2);
    assert_eq!(r.body["items"][0]["id"], last["id"]);
    h.at_register(at, Method::GET, "/api/v1/sales", &[], None)
        .await
        .assert_problem(403, "permission_denied");
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/sales?limit=1&number={number}"),
            None,
        )
        .await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1);
    assert!(r.body["next_before_id"].is_string());
    let budi = h.manager_token().await;
    let jkt = h.jkt.id;
    let r = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/sales?branch_id={jkt}&limit=1"),
            None,
        )
        .await;
    let next = r.body["next_before_id"].as_str().unwrap().to_owned();
    let r = h
        .call(
            &budi,
            Method::GET,
            &format!("/api/v1/sales?branch_id={jkt}&before_id={next}"),
            None,
        )
        .await;
    assert_eq!(r.body["items"][0]["id"], older["id"]);
    h.call(&budi, Method::GET, "/api/v1/sales?limit=0", None)
        .await
        .assert_problem(422, "invalid_field");
    // A cashier of another branch may not read it.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    h.at_register(
        (&cici, &h.bdg_device),
        Method::GET,
        &format!("/api/v1/sales/{}", last["id"].as_str().unwrap()),
        &[],
        None,
    )
    .await
    .assert_problem(403, "permission_denied");
}
