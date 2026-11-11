//! `POST /pricing/quote` and the lookup price over HTTP (spec 2.3 §4–§7, §10, §12 server case 23,
//! cases 3–9, 12, 14–17 end to end; phase R3).
//!
//! The fixture: Teh Botol (pcs, pack(6), box(24)) priced 10 000 / 9 000 from 12 in the default
//! list, 11 000 in JKT01's branch list; a Gold tier list −5%; a Wholesale group list "8 500 from
//! 12"; a contract list at 7 000 for one customer.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::catalog_fixtures::{STANDARD, ean13, put_product};
use crate::common::{Harness, Response, api_tests, new_id};
use crate::pricing::{DEFAULT_LIST, EXEMPT, customer, fixed, group, new_list, price, tier};

api_tests!(
    quote_follows_the_chain,
    quote_lines_tax_and_errors,
    lookup_shows_the_store_price,
);

struct Teh {
    owner: String,
    variant: String,
    pcs: String,
    pack: String,
    box_: String,
    gold: Uuid,
    expired: Uuid,
    wholesale: Uuid,
    contract: Uuid,
}

/// Teh Botol with its prices and the four customers (see the module docs).
async fn teh(h: &Harness) -> Teh {
    let owner = h.owner_token().await;
    let (id, variant, pcs, pack, box_) = (new_id(), new_id(), new_id(), new_id(), new_id());
    let body = json!({
        "code": "TEH", "name": "Teh Botol", "type": "stock", "tax_class_id": STANDARD,
        "units": [
            { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": pack, "name": "pack", "kind": "integer", "factor_thousandths": 6000 },
            { "id": box_, "name": "box", "kind": "integer", "factor_thousandths": 24000 },
        ],
        "base_unit_id": pcs,
        "variants": [{ "id": variant }],
        "barcodes": [
            { "id": new_id(), "variant_id": variant, "unit_id": pcs, "code": ean13("899100000002") },
            { "id": new_id(), "variant_id": variant, "unit_id": box_, "code": "TEH-BOX" },
        ],
    });
    let r = put_product(h, &owner, &id, &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    price(
        h,
        &owner,
        DEFAULT_LIST,
        &variant,
        &pcs,
        json!([fixed(1, 10_000), fixed(12, 9_000)]),
    )
    .await;
    let jkt = new_list(h, &owner, "branch", &h.jkt.id.to_string(), Value::Null).await;
    price(h, &owner, &jkt, &variant, &pcs, json!([fixed(1, 11_000)])).await;
    let gold_tier = tier(h, "Gold", 1).await;
    new_list(
        h,
        &owner,
        "tier",
        &gold_tier.to_string(),
        json!({ "pct_bp": -500 }),
    )
    .await;
    let wholesale_group = group(h, "Wholesale").await;
    let wholesale_list = new_list(
        h,
        &owner,
        "group",
        &wholesale_group.to_string(),
        Value::Null,
    )
    .await;
    price(
        h,
        &owner,
        &wholesale_list,
        &variant,
        &pcs,
        json!([fixed(12, 8_500)]),
    )
    .await;
    let gold = customer(h, "C1", None, Some((gold_tier, Some(20_271_231)))).await;
    let expired = customer(h, "C2", None, Some((gold_tier, Some(20_270_114)))).await;
    let wholesale = customer(h, "C3", Some(wholesale_group), None).await;
    let contract = customer(h, "C4", Some(wholesale_group), Some((gold_tier, None))).await;
    let contract_list = new_list(h, &owner, "customer", &contract.to_string(), Value::Null).await;
    price(
        h,
        &owner,
        &contract_list,
        &variant,
        &pcs,
        json!([fixed(1, 7_000)]),
    )
    .await;
    Teh {
        owner,
        variant,
        pcs,
        pack,
        box_,
        gold,
        expired,
        wholesale,
        contract,
    }
}

/// `POST /pricing/quote` with a device token only.
async fn quote(h: &Harness, device: &str, customer: Option<Uuid>, lines: Value) -> Response {
    h.send(
        Method::POST,
        "/api/v1/pricing/quote",
        &[("x-register-token", device)],
        Some(json!({ "customer_id": customer.map(|c| c.to_string()), "lines": lines })),
    )
    .await
}

fn line(variant: &str, unit: &str, qty: i64) -> Value {
    json!({ "variant_id": variant, "unit_id": unit, "qty_thousandths": qty * 1_000 })
}

/// `(unit_price, amount, source kind, unit_fallback)` of each line.
fn priced(r: &Response) -> Vec<(Value, Value, Value, Value)> {
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| {
            (
                l["unit_price"].clone(),
                l["amount"].clone(),
                l["source"]["kind"].clone(),
                l["source"]["unit_fallback"].clone(),
            )
        })
        .collect()
}

fn p(unit: i64, amount: i64, kind: &str, fallback: bool) -> (Value, Value, Value, Value) {
    (unit.into(), amount.into(), kind.into(), fallback.into())
}

/// Case 23: a Gold customer at a branch gets the prices of cases 3–9 and 12.
#[allow(clippy::too_many_lines)] // one row per case
async fn quote_follows_the_chain(h: &Harness) {
    let t = teh(h).await;
    let (jkt, bdg) = (h.jkt_device.as_str(), h.bdg_device.as_str());
    let pcs = |q| line(&t.variant, &t.pcs, q);

    // Walk-in at JKT01: the branch list. The whole answer once.
    let r = quote(h, jkt, None, json!([pcs(5)])).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let branch_list = r.body["lines"][0]["source"]["list_id"].clone();
    assert_eq!(
        r.body,
        json!({
            "branch_id": h.jkt.id.to_string(), "date": "2027-01-15", "customer_id": null,
            "tax_inclusive": true, "tax_registered": true, "cash_rounding": "none",
            "lines": [{
                "variant_id": t.variant, "unit_id": t.pcs, "qty_thousandths": 5000,
                "mode": "listed", "unit_price": 11_000, "amount": 55_000, "resolved_price": 11_000,
                "source": { "list_id": branch_list, "kind": "branch",
                            "tier_min_qty_thousandths": 1000, "unit_fallback": false },
                "tax": { "tax_class_id": STANDARD, "rate_bp": 1100, "tax": 5450, "net": 49_550,
                         "total": 55_000 },
                "error": null,
            }],
            "totals": { "amount": 55_000, "tax": 5450, "net": 49_550, "total": 55_000 },
        })
    );

    // Case 5: Gold −5% on the next price: 11 000 at JKT01 → 10 450; elsewhere 10 000 → 9 500.
    let r = quote(h, jkt, Some(t.gold), json!([pcs(5)])).await;
    assert_eq!(priced(&r), vec![p(10_450, 52_250, "tier", false)]);
    assert_eq!(
        r.body["lines"][0]["source"]["tier_min_qty_thousandths"],
        Value::Null
    );
    assert_eq!(r.body["customer_id"], t.gold.to_string());
    let r = quote(h, bdg, Some(t.gold), json!([pcs(5)])).await;
    assert_eq!(priced(&r), vec![p(9_500, 47_500, "tier", false)]);
    assert_eq!(r.body["branch_id"], h.bdg.id.to_string());

    // Case 2 + 9: 10 pcs + 1 pack(6) count 16 → the default tier from 12 (9 000), Gold −5% →
    // 8 550; the pack has no price of its own → 6 × 8 550.
    let r = quote(
        h,
        bdg,
        Some(t.gold),
        json!([pcs(10), line(&t.variant, &t.pack, 1)]),
    )
    .await;
    assert_eq!(
        priced(&r),
        vec![
            p(8_550, 85_500, "tier", false),
            p(51_300, 51_300, "tier", true)
        ]
    );
    assert_eq!(r.body["totals"]["amount"], 136_800);
    // Remove the pack: back to 9 500.
    let r = quote(h, bdg, Some(t.gold), json!([pcs(10)])).await;
    assert_eq!(priced(&r), vec![p(9_500, 95_000, "tier", false)]);

    // Case 9: a box without its own price at JKT01 → 24 × (11 000 − 5%).
    let r = quote(h, jkt, Some(t.gold), json!([line(&t.variant, &t.box_, 1)])).await;
    assert_eq!(priced(&r), vec![p(250_800, 250_800, "tier", true)]);

    // Case 12: an expired tier falls to the next list.
    let r = quote(h, jkt, Some(t.expired), json!([pcs(5)])).await;
    assert_eq!(priced(&r), vec![p(11_000, 55_000, "branch", false)]);

    // Case 4: Wholesale has only "8 500 from 12": 5 pcs → default, 12 pcs → 8 500.
    let r = quote(h, bdg, Some(t.wholesale), json!([pcs(5)])).await;
    assert_eq!(priced(&r), vec![p(10_000, 50_000, "default", false)]);
    let r = quote(h, bdg, Some(t.wholesale), json!([pcs(12)])).await;
    assert_eq!(priced(&r), vec![p(8_500, 102_000, "group", false)]);

    // Case 3: the contract beats tier, group, branch and default.
    let r = quote(h, jkt, Some(t.contract), json!([pcs(12)])).await;
    assert_eq!(priced(&r), vec![p(7_000, 84_000, "customer", false)]);

    // An unknown customer.
    let r = quote(h, jkt, Some(Uuid::now_v7()), json!([pcs(1)])).await;
    r.assert_problem(422, "invalid_reference");
    assert_eq!(r.body["field"], "customer_id");

    // A back-office session prices at its own branch.
    let r = h
        .call(
            &t.owner,
            Method::POST,
            "/api/v1/pricing/quote",
            Some(json!({ "lines": [pcs(1)] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["branch_id"].is_string());
}

/// Product with one unit; returns `(variant, unit)`.
async fn product(h: &Harness, owner: &str, code: &str, extra: Value) -> (String, String) {
    let (unit, variant) = (new_id(), new_id());
    let weighed = extra["type"] == "weighed";
    let mut body = json!({
        "code": code, "name": code, "type": "stock", "tax_class_id": STANDARD,
        "units": [{ "id": unit, "name": if weighed { "kg" } else { "pcs" },
                    "kind": if weighed { "decimal" } else { "integer" },
                    "factor_thousandths": 1000 }],
        "base_unit_id": unit,
        "variants": [{ "id": variant, "plu": extra.get("plu") }],
    });
    for (k, v) in extra.as_object().unwrap() {
        if k != "plu" {
            body[k] = v.clone();
        }
    }
    let r = put_product(h, owner, &new_id(), &body).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    (variant, unit)
}

/// Special lines, per-line errors, tax inclusive / exclusive / not registered, rate history,
/// line rounding (cases 10, 14–17).
#[allow(clippy::too_many_lines)] // one story
async fn quote_lines_tax_and_errors(h: &Harness) {
    let t = teh(h).await;
    let owner = t.owner.clone();
    let bdg = h.bdg_device.as_str();
    let (gula, gula_pcs) = product(h, &owner, "GULA", json!({})).await;
    let (jasa, jasa_pcs) = product(h, &owner, "JASA", json!({ "open_price": true })).await;
    price(
        h,
        &owner,
        DEFAULT_LIST,
        &jasa,
        &jasa_pcs,
        json!([fixed(1, 4_000)]),
    )
    .await;
    let (free, free_pcs) = product(h, &owner, "BEBAS", json!({ "tax_class_id": EXEMPT })).await;
    price(
        h,
        &owner,
        DEFAULT_LIST,
        &free,
        &free_pcs,
        json!([fixed(1, 10_000)]),
    )
    .await;
    let (keju, kg) = product(h, &owner, "KEJU", json!({ "type": "weighed", "plu": 123 })).await;
    price(
        h,
        &owner,
        DEFAULT_LIST,
        &keju,
        &kg,
        json!([fixed(1, 12_990)]),
    )
    .await;

    let typed = |variant: &str, unit: &str, price: i64| {
        json!({ "variant_id": variant, "unit_id": unit, "qty_thousandths": 1000,
                "typed_price": price })
    };
    let r = quote(
        h,
        bdg,
        None,
        json!([
            line(&t.variant, &t.pcs, 1),
            line(&gula, &gula_pcs, 1),
            typed(&jasa, &jasa_pcs, 5_000),
            line(&jasa, &jasa_pcs, 1),
            typed(&t.variant, &t.pcs, 9_000),
            line(&new_id(), &t.pcs, 1),
            line(&t.variant, &gula_pcs, 1),
            typed(&t.variant, &t.pcs, -1),
            line(&free, &free_pcs, 1),
            { "variant_id": keju, "unit_id": kg, "qty_thousandths": 1255 },
        ]),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let lines = r.body["lines"].as_array().unwrap();
    let get = |i: usize, k: &str| lines[i][k].clone();
    // Case 14: 10 000 inclusive 11% → tax 991.
    assert_eq!(get(0, "tax")["tax"], 991);
    assert_eq!(get(0, "tax")["net"], 9_009);
    // Case 10: no price anywhere.
    assert_eq!(get(1, "error")["code"], "no_price");
    assert_eq!(get(1, "unit_price"), Value::Null);
    assert_eq!(get(1, "tax"), Value::Null);
    // Open price: the typed price; without one, `no_price` with the suggestion.
    assert_eq!(
        (get(2, "mode"), get(2, "unit_price")),
        (json!("open_price"), json!(5_000))
    );
    assert_eq!(get(2, "resolved_price"), 4_000);
    assert_eq!(get(3, "error")["code"], "no_price");
    assert_eq!(get(3, "error")["suggestion"], 4_000);
    assert_eq!(get(3, "source")["kind"], "default");
    // A manual override keeps the resolved price beside it.
    assert_eq!(
        (
            get(4, "mode"),
            get(4, "unit_price"),
            get(4, "resolved_price")
        ),
        (json!("overridden"), json!(9_000), json!(10_000))
    );
    // Unknown variant, a unit of another product, a negative typed price.
    assert_eq!(get(5, "error")["code"], "invalid_reference");
    assert_eq!(get(6, "error")["code"], "invalid_reference");
    assert_eq!(get(7, "error")["code"], "invalid_field");
    // Exempt: rate 0.
    assert_eq!(get(8, "tax")["rate_bp"], 0);
    assert_eq!(get(8, "tax")["tax"], 0);
    assert_eq!(get(8, "tax")["tax_class_id"], EXEMPT);
    // Case 17: 1.255 kg × 12 990 = 16 302.45 → 16 302.
    assert_eq!(get(9, "amount"), 16_302);
    // Totals add the priced lines only.
    let amount: i64 = [0, 2, 4, 8, 9]
        .iter()
        .map(|&i| get(i, "amount").as_i64().unwrap())
        .sum();
    assert_eq!(r.body["totals"]["amount"], amount);
    assert_eq!(amount, 10_000 + 5_000 + 9_000 + 10_000 + 16_302);

    // Case 16: a rate from an earlier date applies; a future one does not.
    let rates = format!("/api/v1/tax-classes/{STANDARD}/rates");
    for (rate, from) in [(1200, "2027-01-01"), (1500, "2027-01-16")] {
        let r = h
            .call(
                &owner,
                Method::PUT,
                &rates,
                Some(json!({ "rate_bp": rate, "valid_from": from })),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    }
    let r = quote(h, bdg, None, json!([line(&t.variant, &t.pcs, 1)])).await;
    assert_eq!(r.body["lines"][0]["tax"]["rate_bp"], 1200);
    assert_eq!(r.body["lines"][0]["tax"]["tax"], 1071);

    // Case 15: exclusive adds tax on top; not registered computes none.
    let settings = |inclusive: bool, registered: bool| {
        json!({ "tax_inclusive": inclusive, "tax_registered": registered,
                "cash_rounding": "nearest_100" })
    };
    h.call(
        &owner,
        Method::PUT,
        "/api/v1/settings/pricing",
        Some(settings(false, true)),
    )
    .await;
    let r = quote(h, bdg, None, json!([line(&t.variant, &t.pcs, 1)])).await;
    assert_eq!(
        r.body["totals"],
        json!({ "amount": 10_000, "tax": 1_200, "net": 10_000, "total": 11_200 })
    );
    assert_eq!(
        (&r.body["tax_inclusive"], &r.body["cash_rounding"]),
        (&json!(false), &json!("nearest_100"))
    );
    h.call(
        &owner,
        Method::PUT,
        "/api/v1/settings/pricing",
        Some(settings(true, false)),
    )
    .await;
    let r = quote(h, bdg, None, json!([line(&t.variant, &t.pcs, 1)])).await;
    assert_eq!(r.body["lines"][0]["tax"]["rate_bp"], 0);
    assert_eq!(
        r.body["totals"],
        json!({ "amount": 10_000, "tax": 0, "net": 10_000, "total": 10_000 })
    );

    // Request errors.
    quote(
        h,
        bdg,
        None,
        json!([{ "variant_id": t.variant, "unit_id": t.pcs, "qty_thousandths": 0 }]),
    )
    .await
    .assert_problem(422, "invalid_field");
    let many: Vec<Value> = (0..501).map(|_| line(&t.variant, &t.pcs, 1)).collect();
    quote(h, bdg, None, json!(many))
        .await
        .assert_problem(422, "invalid_field");
    quote(h, bdg, None, json!([line("nope", &t.pcs, 1)]))
        .await
        .assert_problem(400, "invalid_body");
    let r = quote(h, bdg, None, json!([])).await;
    assert_eq!(r.body["totals"]["amount"], 0);
    h.send(
        Method::POST,
        "/api/v1/pricing/quote",
        &[],
        Some(json!({ "lines": [] })),
    )
    .await
    .assert_problem(401, "invalid_session");
}

/// `GET /catalog/lookup` answers the store price for one unit (branch → default, no customer)
/// and the quantity of a price-embedded label.
async fn lookup_shows_the_store_price(h: &Harness) {
    let t = teh(h).await;
    let lookup = |device: &str, code: String| {
        let device = device.to_owned();
        async move {
            let r = h
                .send(
                    Method::GET,
                    &format!("/api/v1/catalog/lookup?code={code}"),
                    &[("x-register-token", &device)],
                    None,
                )
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.body);
            r.body
        }
    };
    let pcs_code = ean13("899100000002");
    let r = lookup(&h.jkt_device, pcs_code.clone()).await;
    assert_eq!(
        (&r["unit_price"], &r["qty_thousandths"]),
        (&json!(11_000), &json!(1000))
    );
    assert_eq!(
        lookup(&h.jkt_device, "TEH-BOX".into()).await["unit_price"],
        264_000
    );
    assert_eq!(
        lookup(&h.bdg_device, pcs_code.clone()).await["unit_price"],
        10_000
    );
    // One box counts 24 pcs, so the default tier from 12 applies: 24 × 9 000.
    assert_eq!(
        lookup(&h.bdg_device, "TEH-BOX".into()).await["unit_price"],
        216_000
    );
    // Back office: the requested branch.
    let r = h
        .call(
            &t.owner,
            Method::GET,
            &format!(
                "/api/v1/catalog/lookup?code={pcs_code}&branch_id={}",
                h.jkt.id
            ),
            None,
        )
        .await;
    assert_eq!(r.body["unit_price"], 11_000);

    // A weighed product: a weight label keeps its quantity; a price label's quantity follows
    // from the store price (15 000 ÷ 12 000 = 1.250).
    let (keju, kg) = product(
        h,
        &t.owner,
        "KEJU",
        json!({ "type": "weighed", "plu": 123 }),
    )
    .await;
    let price_label = ean13("220012315000");
    let r = lookup(&h.jkt_device, price_label.clone()).await;
    assert_eq!(
        (&r["unit_price"], &r["qty_thousandths"], &r["amount"]),
        (&Value::Null, &Value::Null, &json!(15_000))
    );
    price(
        h,
        &t.owner,
        DEFAULT_LIST,
        &keju,
        &kg,
        json!([fixed(1, 12_000)]),
    )
    .await;
    let r = lookup(&h.jkt_device, price_label).await;
    assert_eq!(
        (&r["unit_price"], &r["qty_thousandths"], &r["amount"]),
        (&json!(12_000), &json!(1250), &json!(15_000))
    );
    let r = lookup(&h.jkt_device, ean13("210012301250")).await;
    assert_eq!(
        (&r["unit_price"], &r["qty_thousandths"]),
        (&json!(12_000), &json!(1250))
    );
}
