//! Pricing master data over HTTP (spec 2.3 §9–§12 server cases 20, 21, 22 and 24; phase R3):
//! price lists, entries, product prices, tax rates and the pricing settings, with their audit
//! rows, outbox rows and `price.changed` events. The fixtures are shared with
//! [`crate::pricing_quote`].

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_db::catalog::Actor;
use sultan_db::customers::{self, Customer, CustomerGroup, MembershipTier};
use sultan_server::NodeRole;
use sultan_server::events::Event;
use tokio::sync::broadcast::Receiver;
use uuid::Uuid;

use crate::catalog_fixtures::{STANDARD, create, ean13, kaos, simple};
use crate::common::{ANI_PIN, Harness, Response, T0, api_tests, new_id};

api_tests!(
    price_list_put_replay_and_conflict,
    price_list_rules_and_references,
    entries_overlap_validation_and_pages,
    branch_node_and_permissions,
    writes_audit_outbox_and_events,
    tax_rates_and_settings,
);

/// The seeded `default` list.
pub const DEFAULT_LIST: &str = "00000000-0000-7000-8000-000000000201";
/// The seeded `Exempt` tax class.
pub const EXEMPT: &str = "00000000-0000-7000-8000-000000000102";

/// `PUT /price-lists/{id}`.
pub async fn put_list(h: &Harness, token: &str, id: &str, body: Value) -> Response {
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/price-lists/{id}"),
        Some(body),
    )
    .await
}

/// Creates a live list; returns its ID.
pub async fn new_list(h: &Harness, token: &str, kind: &str, target: &str, rule: Value) -> String {
    let id = new_id();
    let r = put_list(
        h,
        token,
        &id,
        json!({ "kind": kind, "target_id": target, "name": format!("{kind} list"), "rule": rule }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    id
}

/// `PUT /price-lists/{list}/entries/{id}`.
pub async fn put_entry(h: &Harness, token: &str, list: &str, id: &str, body: Value) -> Response {
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/price-lists/{list}/entries/{id}"),
        Some(body),
    )
    .await
}

/// Adds an open-dated entry with `tiers`; asserts `201`.
pub async fn price(h: &Harness, token: &str, list: &str, variant: &str, unit: &str, tiers: Value) {
    let r = put_entry(
        h,
        token,
        list,
        &new_id(),
        json!({ "variant_id": variant, "unit_id": unit, "tiers": tiers }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
}

/// A fixed-price tier from `min_qty` whole units.
pub fn fixed(min_qty: i64, price: i64) -> Value {
    json!({ "min_qty_thousandths": min_qty * 1_000, "price": price })
}

fn actor() -> Actor {
    Actor {
        user: None,
        branch: None,
        register: None,
    }
}

/// A membership tier; returns its ID.
pub async fn tier(h: &Harness, name: &str, rank: i64) -> Uuid {
    let t = MembershipTier {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        rank,
        archived_at: None,
    };
    let mut tx = h.db.begin().await.unwrap();
    customers::put_tier(&mut tx, &t, &actor(), T0)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    t.id
}

/// A customer group; returns its ID.
pub async fn group(h: &Harness, name: &str) -> Uuid {
    let g = CustomerGroup {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        archived_at: None,
    };
    let mut tx = h.db.begin().await.unwrap();
    customers::put_group(&mut tx, &g, &actor(), T0)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    g.id
}

/// A customer with an optional group and tier (valid until `YYYYMMDD`); returns its ID.
pub async fn customer(
    h: &Harness,
    code: &str,
    group: Option<Uuid>,
    tier: Option<(Uuid, Option<u32>)>,
) -> Uuid {
    let c = Customer {
        id: Uuid::now_v7(),
        code: code.to_owned(),
        name: format!("Pelanggan {code}"),
        phone: None,
        email: None,
        birthday: None,
        address: None,
        company_name: None,
        tax_id: None,
        tax_address: None,
        notes: None,
        group,
        tier: tier.map(|(t, _)| t),
        tier_valid_until: tier.and_then(|(_, until)| until),
        manual_tier: false,
        signed_up_branch: None,
        signed_up_at: T0,
    };
    let mut tx = h.db.begin().await.unwrap();
    customers::put_customer(&mut tx, &c, &actor(), T0)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    c.id
}

/// The `price.changed` events received so far.
pub fn price_events(rx: &mut Receiver<Event>) -> Vec<Value> {
    let mut events = Vec::new();
    while let Ok(e) = rx.try_recv() {
        if e.kind == "price.changed" {
            events.push(e.data);
        }
    }
    events
}

/// Case 20: create → replay → one list; a second active list for the branch → conflict.
#[allow(clippy::too_many_lines)] // one story
async fn price_list_put_replay_and_conflict(h: &Harness) {
    let owner = h.owner_token().await;
    let id = new_id();
    let body = json!({
        "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "  Jakarta  ",
        "rule": { "pct_bp": 500 },
    });
    let r = put_list(h, &owner, &id, body.clone()).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        r.body,
        json!({
            "id": id, "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "Jakarta",
            "active": true, "rule": { "pct_bp": 500 }, "archived": false, "archived_at": null,
        })
    );
    let replay = put_list(h, &owner, &id, body).await;
    assert_eq!((replay.status, &replay.body), (StatusCode::OK, &r.body));
    let lists = h
        .call(&owner, Method::GET, "/api/v1/price-lists?kind=branch", None)
        .await;
    assert_eq!(lists.body.as_array().unwrap().len(), 1);
    assert_eq!(h.audit_count("price_list.put").await, 1);

    // A second active list for JKT01 → 409, naming the live one; an inactive one is fine.
    let second = new_id();
    let other = json!({ "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "Promo" });
    let r = put_list(h, &owner, &second, other.clone()).await;
    r.assert_problem(409, "price_list_conflict");
    assert_eq!(r.body["price_list_id"], id);
    let mut off = other.clone();
    off["active"] = false.into();
    assert_eq!(
        put_list(h, &owner, &second, off).await.status,
        StatusCode::CREATED
    );
    put_list(h, &owner, &second, other.clone())
        .await
        .assert_problem(409, "price_list_conflict");

    // Archiving the first frees the target (and is idempotent).
    let uri = format!("/api/v1/price-lists/{id}/archive");
    let r = h.call(&owner, Method::POST, &uri, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["archived"], true);
    let archived_at = r.body["archived_at"].clone();
    let again = h.call(&owner, Method::POST, &uri, None).await;
    assert_eq!(again.body["archived_at"], archived_at);
    assert_eq!(
        put_list(h, &owner, &second, other).await.status,
        StatusCode::OK
    );
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/price-lists/{id}"),
            None,
        )
        .await;
    assert_eq!(r.body["archived"], true);
    let live = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/price-lists?target_id={}", h.jkt.id),
            None,
        )
        .await;
    assert_eq!(live.body.as_array().unwrap().len(), 1);
    let all = h
        .call(
            &owner,
            Method::GET,
            "/api/v1/price-lists?include_archived=true",
            None,
        )
        .await;
    assert_eq!(all.body.as_array().unwrap().len(), 3, "{}", all.body);
    // Unarchive with `archived: false`: now it conflicts again.
    let r = put_list(
        h,
        &owner,
        &id,
        json!({ "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "Jakarta" }),
    )
    .await;
    r.assert_problem(409, "price_list_conflict");
    h.call(&owner, Method::GET, "/api/v1/price-lists/nope", None)
        .await
        .assert_problem(400, "invalid_body");
    h.call(
        &owner,
        Method::GET,
        &format!("/api/v1/price-lists/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(
        &owner,
        Method::POST,
        &format!("/api/v1/price-lists/{}/archive", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

/// Case 21 (second half) and the list's field rules.
#[allow(clippy::too_many_lines)] // one row per rule
async fn price_list_rules_and_references(h: &Harness) {
    let owner = h.owner_token().await;
    // The default list: no rule, cannot be switched off or archived, but may be renamed.
    let base = json!({ "kind": "default", "name": "Harga umum" });
    let mut ruled = base.clone();
    ruled["rule"] = json!({ "pct_bp": -500 });
    put_list(h, &owner, DEFAULT_LIST, ruled)
        .await
        .assert_problem(422, "invalid_rule");
    let mut off = base.clone();
    off["active"] = false.into();
    let r = put_list(h, &owner, DEFAULT_LIST, off).await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "active");
    h.call(
        &owner,
        Method::POST,
        &format!("/api/v1/price-lists/{DEFAULT_LIST}/archive"),
        None,
    )
    .await
    .assert_problem(422, "invalid_field");
    let r = put_list(h, &owner, DEFAULT_LIST, base).await;
    assert_eq!(
        (r.status, &r.body["name"]),
        (StatusCode::OK, &json!("Harga umum"))
    );

    let cases = [
        (
            json!({ "kind": "default", "name": "Second" }),
            422,
            "invalid_field",
        ),
        (json!({ "kind": "gold", "name": "X" }), 422, "invalid_field"),
        (json!({ "kind": "tier", "name": "X" }), 422, "invalid_field"),
        (
            json!({ "kind": "tier", "target_id": new_id(), "name": "X" }),
            422,
            "invalid_reference",
        ),
        (
            json!({ "kind": "branch", "target_id": h.bdg.id.to_string(), "name": " " }),
            422,
            "invalid_field",
        ),
        (
            json!({ "kind": "branch", "target_id": h.bdg.id.to_string(), "name": "X",
                    "rule": { "pct_bp": -10000 } }),
            422,
            "invalid_rule",
        ),
        (
            json!({ "kind": "branch", "target_id": h.bdg.id.to_string(), "name": "X",
                    "rule": { "pct_bp": -1, "amount": -1 } }),
            422,
            "invalid_field",
        ),
        (
            json!({ "kind": "branch", "target_id": "nope", "name": "X" }),
            400,
            "invalid_body",
        ),
        (json!({ "name": "X" }), 422, "invalid_body"),
    ];
    for (body, status, code) in cases {
        let r = put_list(h, &owner, &new_id(), body.clone()).await;
        assert_eq!(
            (r.status.as_u16(), r.body["code"].as_str()),
            (status, Some(code)),
            "{body}: {}",
            r.body
        );
    }
    let r = put_list(
        h,
        &owner,
        &new_id(),
        json!({ "kind": "tier", "target_id": new_id(), "name": "X" }),
    )
    .await;
    assert_eq!(r.body["field"], "target_id");
    // Not a UUIDv7.
    put_list(
        h,
        &owner,
        "550e8400-e29b-41d4-a716-446655440000",
        json!({ "kind": "branch", "target_id": h.bdg.id.to_string(), "name": "X" }),
    )
    .await
    .assert_problem(400, "invalid_body");
    // Kind and target are fixed once created.
    let id = new_list(h, &owner, "branch", &h.bdg.id.to_string(), Value::Null).await;
    let r = put_list(
        h,
        &owner,
        &id,
        json!({ "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "X" }),
    )
    .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "target_id");
    let gold = tier(h, "Gold", 1).await;
    let gold_list = new_list(
        h,
        &owner,
        "tier",
        &gold.to_string(),
        json!({ "amount": -500 }),
    )
    .await;
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/price-lists/{gold_list}"),
            None,
        )
        .await;
    assert_eq!(r.body["rule"], json!({ "amount": -500 }));
}

/// Case 21 (first half): overlapping dates, adjacent dates, tier rules, the entry pages and
/// `GET /products/{id}/prices`.
#[allow(clippy::too_many_lines)] // one story
async fn entries_overlap_validation_and_pages(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = kaos("KAOS10", "899310");
    let product = create(h, &owner, &doc).await;
    let (pcs, box_) = (
        doc.base_unit.clone(),
        product["default_purchase_unit_id"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    let variant = doc.variants[0].clone();
    let entry = new_id();
    let body = json!({
        "variant_id": variant, "unit_id": pcs, "valid_to": "2026-11-30",
        "tiers": [fixed(12, 9_000), fixed(1, 10_000)],
    });
    let r = put_entry(h, &owner, DEFAULT_LIST, &entry, body.clone()).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        r.body,
        json!({
            "id": entry, "list_id": DEFAULT_LIST, "variant_id": variant, "unit_id": pcs,
            "valid_from": null, "valid_to": "2026-11-30",
            "tiers": [fixed(1, 10_000), fixed(12, 9_000)],
        })
    );
    let replay = put_entry(h, &owner, DEFAULT_LIST, &entry, body).await;
    assert_eq!((replay.status, &replay.body), (StatusCode::OK, &r.body));
    // Pricing marks the product used (spec 2.2 §2).
    let p = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/products/{}", doc.id),
            None,
        )
        .await;
    assert_eq!(p.body["used"], true);

    // An entry that shares a day → 409 naming the other; the next day on is fine.
    let r = put_entry(
        h,
        &owner,
        DEFAULT_LIST,
        &new_id(),
        json!({ "variant_id": variant, "unit_id": pcs, "valid_from": "2026-11-30",
                "tiers": [fixed(1, 11_000)] }),
    )
    .await;
    r.assert_problem(409, "price_overlap");
    assert_eq!(r.body["entry_id"], entry);
    let next = new_id();
    let r = put_entry(
        h,
        &owner,
        DEFAULT_LIST,
        &next,
        json!({ "variant_id": variant, "unit_id": pcs, "valid_from": "2026-12-01",
                "tiers": [fixed(1, 11_000)] }),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);

    // Validation (spec 2.3 §3, §15).
    let cases = [
        (json!([]), 422, "invalid_field", "tiers"),
        (
            json!([{ "min_qty_thousandths": 1000 }]),
            422,
            "invalid_field",
            "tiers",
        ),
        (
            json!([fixed(0, 1)]),
            422,
            "invalid_field",
            "tiers.min_qty_thousandths",
        ),
        (
            json!([fixed(1, 1), fixed(1, 2)]),
            422,
            "invalid_field",
            "tiers.min_qty_thousandths",
        ),
        (json!([fixed(1, -1)]), 422, "invalid_field", "tiers.price"),
        (
            json!([{ "min_qty_thousandths": 1000, "rule": { "pct_bp": -500 } }]),
            422,
            "invalid_rule",
            "rule",
        ),
    ];
    for (tiers, status, code, field) in cases {
        let r = put_entry(
            h,
            &owner,
            DEFAULT_LIST,
            &new_id(),
            json!({ "variant_id": doc.variants[1], "unit_id": pcs, "tiers": tiers }),
        )
        .await;
        r.assert_problem(status, code);
        assert_eq!(r.body["field"], field, "{tiers}");
    }
    let refs = [
        (
            json!({ "variant_id": new_id(), "unit_id": pcs }),
            "variant_id",
        ),
        (
            json!({ "variant_id": variant, "unit_id": new_id() }),
            "unit_id",
        ),
    ];
    for (mut body, field) in refs {
        body["tiers"] = json!([fixed(1, 1)]);
        let r = put_entry(h, &owner, DEFAULT_LIST, &new_id(), body).await;
        r.assert_problem(422, "invalid_reference");
        assert_eq!(r.body["field"], field);
    }
    let r = put_entry(
        h,
        &owner,
        DEFAULT_LIST,
        &new_id(),
        json!({ "variant_id": variant, "unit_id": box_, "valid_from": "2027-01-02",
                "valid_to": "2027-01-01", "tiers": [fixed(1, 1)] }),
    )
    .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "valid_to");
    put_entry(
        h,
        &owner,
        DEFAULT_LIST,
        &new_id(),
        json!({ "variant_id": variant, "unit_id": box_, "valid_from": "1/1/2027",
                "tiers": [fixed(1, 1)] }),
    )
    .await
    .assert_problem(422, "invalid_field");
    put_entry(
        h,
        &owner,
        &new_id(),
        &new_id(),
        json!({ "variant_id": variant, "unit_id": box_, "tiers": [fixed(1, 1)] }),
    )
    .await
    .assert_problem(404, "not_found");
    // An entry ID of another list.
    let branch = new_list(h, &owner, "branch", &h.jkt.id.to_string(), Value::Null).await;
    let r = put_entry(
        h,
        &owner,
        &branch,
        &entry,
        json!({ "variant_id": variant, "unit_id": pcs, "tiers": [fixed(1, 1)] }),
    )
    .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "list_id");
    // A rule in a non-default list is fine; box entry in the branch list.
    price(
        h,
        &owner,
        &branch,
        &variant,
        &pcs,
        json!([{ "min_qty_thousandths": 1000, "rule": { "amount": 1000 } }]),
    )
    .await;
    price(
        h,
        &owner,
        &branch,
        &variant,
        &box_,
        json!([fixed(1, 230_000)]),
    )
    .await;

    // Reads.
    let owner_ref = owner.as_str();
    let get = |uri: String| async move { h.call(owner_ref, Method::GET, &uri, None).await };
    let r = get(format!(
        "/api/v1/price-lists/{DEFAULT_LIST}/entries/{entry}"
    ))
    .await;
    assert_eq!(r.body["tiers"][1], fixed(12, 9_000));
    get(format!("/api/v1/price-lists/{branch}/entries/{entry}"))
        .await
        .assert_problem(404, "not_found");
    let r = get(format!(
        "/api/v1/price-lists/{DEFAULT_LIST}/entries?variant={variant}"
    ))
    .await;
    assert_eq!(r.body["entries"].as_array().unwrap().len(), 2);
    assert_eq!(r.body["next_after_id"], Value::Null);
    let r = get(format!(
        "/api/v1/price-lists/{DEFAULT_LIST}/entries?limit=1"
    ))
    .await;
    assert_eq!(r.body["entries"][0]["id"], entry);
    assert_eq!(r.body["next_after_id"], entry);
    let r = get(format!(
        "/api/v1/price-lists/{DEFAULT_LIST}/entries?limit=1&after_id={entry}"
    ))
    .await;
    assert_eq!(r.body["entries"][0]["id"], next);
    get(format!("/api/v1/price-lists/{}/entries", new_id()))
        .await
        .assert_problem(404, "not_found");
    let r = get(format!("/api/v1/products/{}/prices", doc.id)).await;
    assert_eq!(r.body["product_id"], doc.id);
    assert_eq!(r.body["entries"].as_array().unwrap().len(), 4, "{}", r.body);
    assert_eq!(r.body["lists"].as_array().unwrap().len(), 2);
    get(format!("/api/v1/products/{}/prices", new_id()))
        .await
        .assert_problem(404, "not_found");
    // A cashier may read entries too.
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let r = h
        .at_register(
            (&ani, &h.jkt_device),
            Method::GET,
            &format!("/api/v1/products/{}/prices", doc.id),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);

    // Delete → 204, then gone; again → 404.
    let uri = format!("/api/v1/price-lists/{DEFAULT_LIST}/entries/{next}");
    let r = h.call(&owner, Method::DELETE, &uri, None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    get(uri.clone()).await.assert_problem(404, "not_found");
    h.call(&owner, Method::DELETE, &uri, None)
        .await
        .assert_problem(404, "not_found");
    assert_eq!(h.audit_count("price_entry.delete").await, 1);
}

/// Case 22: a branch node refuses every pricing write; a user without `price.manage` too.
async fn branch_node_and_permissions(h: &Harness) {
    let owner = h.owner_token().await;
    let branch_node = || sultan_server::router(h.state(NodeRole::Branch));
    let bearer = format!("Bearer {owner}");
    let list = format!("/api/v1/price-lists/{}", new_id());
    let writes = [
        (
            Method::PUT,
            list.clone(),
            Some(json!({ "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "X" })),
        ),
        (Method::POST, format!("{list}/archive"), None),
        (
            Method::PUT,
            format!("/api/v1/price-lists/{DEFAULT_LIST}/entries/{}", new_id()),
            Some(json!({ "variant_id": new_id(), "unit_id": new_id(), "tiers": [] })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/price-lists/{DEFAULT_LIST}/entries/{}", new_id()),
            None,
        ),
        (
            Method::PUT,
            format!("/api/v1/tax-classes/{STANDARD}/rates"),
            Some(json!({ "rate_bp": 1200, "valid_from": "2027-01-01" })),
        ),
        (
            Method::PUT,
            "/api/v1/settings/pricing".to_owned(),
            Some(json!({ "tax_inclusive": true, "tax_registered": true, "cash_rounding": "none" })),
        ),
    ];
    for (method, uri, body) in &writes {
        let r = Harness::send_to(
            branch_node(),
            method.clone(),
            uri,
            &[("authorization", &bearer)],
            body.clone(),
        )
        .await;
        r.assert_problem(403, "read_only_replica");
    }
    // Reads work on a branch node.
    let r = Harness::send_to(
        branch_node(),
        Method::GET,
        "/api/v1/price-lists",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);

    // A cashier (no `price.manage`) is denied every write but may read.
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    for (method, uri, body) in writes {
        let r = h
            .at_register((&ani, &h.jkt_device), method, &uri, &[], body)
            .await;
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], "price.manage");
    }
    for uri in [
        "/api/v1/price-lists".to_owned(),
        "/api/v1/settings/pricing".to_owned(),
        format!("/api/v1/tax-classes/{STANDARD}/rates"),
    ] {
        let r = h
            .at_register((&ani, &h.jkt_device), Method::GET, &uri, &[], None)
            .await;
        assert_eq!(r.status, StatusCode::OK, "{uri}");
    }
    // A device token alone cannot read the lists (session only).
    let r = h
        .send(
            Method::GET,
            "/api/v1/price-lists",
            &[("x-register-token", &h.jkt_device)],
            None,
        )
        .await;
    r.assert_problem(401, "invalid_session");
    // The Manager holds `price.manage` (seed).
    let budi = h.manager_token().await;
    new_list(h, &budi, "branch", &h.jkt.id.to_string(), Value::Null).await;
}

/// Case 24: every write is one audit row and one outbox row, and sends `price.changed`; a replay
/// sends nothing.
#[allow(clippy::too_many_lines)] // one row per write
async fn writes_audit_outbox_and_events(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = simple("TEH01", &ean13("899100000001"));
    create(h, &owner, &doc).await;
    let (variant, pcs) = (doc.variants[0].clone(), doc.base_unit.clone());
    let mut rx = h.events.subscribe();
    let outbox = h.outbox_count().await;

    let list = new_id();
    let list_body = json!({ "kind": "branch", "target_id": h.jkt.id.to_string(), "name": "JKT" });
    put_list(h, &owner, &list, list_body.clone()).await;
    put_list(h, &owner, &list, list_body).await;
    let entry = new_id();
    let entry_body = json!({ "variant_id": variant, "unit_id": pcs, "tiers": [fixed(1, 5_000)] });
    put_entry(h, &owner, &list, &entry, entry_body.clone()).await;
    put_entry(h, &owner, &list, &entry, entry_body).await;
    let rate = json!({ "rate_bp": 1200, "valid_from": "2027-02-01" });
    let uri = format!("/api/v1/tax-classes/{STANDARD}/rates");
    h.call(&owner, Method::PUT, &uri, Some(rate.clone())).await;
    h.call(&owner, Method::PUT, &uri, Some(rate)).await;
    let settings =
        json!({ "tax_inclusive": false, "tax_registered": true, "cash_rounding": "none" });
    h.call(
        &owner,
        Method::PUT,
        "/api/v1/settings/pricing",
        Some(settings.clone()),
    )
    .await;
    h.call(
        &owner,
        Method::PUT,
        "/api/v1/settings/pricing",
        Some(settings),
    )
    .await;
    h.call(
        &owner,
        Method::DELETE,
        &format!("/api/v1/price-lists/{list}/entries/{entry}"),
        None,
    )
    .await;
    h.call(
        &owner,
        Method::POST,
        &format!("/api/v1/price-lists/{list}/archive"),
        None,
    )
    .await;
    h.call(
        &owner,
        Method::POST,
        &format!("/api/v1/price-lists/{list}/archive"),
        None,
    )
    .await;

    for (action, n) in [
        ("price_list.put", 2),
        ("price_entry.put", 1),
        ("price_entry.delete", 1),
        ("tax_rate.put", 1),
        ("pricing.settings.put", 1),
    ] {
        assert_eq!(h.audit_count(action).await, n, "{action}");
    }
    assert_eq!(h.outbox_count().await, outbox + 6);
    let events = price_events(&mut rx);
    let v = json!([variant]);
    assert_eq!(
        events,
        vec![
            json!({ "scope": "price_list", "variant_ids": null }),
            json!({ "scope": "price_entry", "variant_ids": v }),
            json!({ "scope": "tax_rate", "variant_ids": null }),
            json!({ "scope": "settings", "variant_ids": null }),
            json!({ "scope": "price_entry", "variant_ids": v }),
            json!({ "scope": "price_list", "variant_ids": null }),
        ]
    );
    // A failed write leaves nothing behind.
    let audit = h.audit_count("price_list.put").await;
    put_list(
        h,
        &owner,
        &new_id(),
        json!({ "kind": "default", "name": "Second" }),
    )
    .await
    .assert_problem(422, "invalid_field");
    assert_eq!(h.audit_count("price_list.put").await, audit);
    assert!(price_events(&mut rx).is_empty());
}

/// Tax rates per class (history, future dates) and the pricing settings.
async fn tax_rates_and_settings(h: &Harness) {
    let owner = h.owner_token().await;
    let uri = format!("/api/v1/tax-classes/{STANDARD}/rates");
    let r = h.call(&owner, Method::GET, &uri, None).await;
    assert_eq!(
        r.body,
        json!([{ "rate_bp": 1100, "valid_from": "2022-04-01" }])
    );
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/tax-classes/{EXEMPT}/rates"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let future = json!({ "rate_bp": 1200, "valid_from": "2027-06-01" });
    let r = h
        .call(&owner, Method::PUT, &uri, Some(future.clone()))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        r.body,
        json!([{ "rate_bp": 1100, "valid_from": "2022-04-01" }, future])
    );
    let r = h.call(&owner, Method::PUT, &uri, Some(future)).await;
    assert_eq!(r.status, StatusCode::OK);
    let r = h
        .call(
            &owner,
            Method::PUT,
            &uri,
            Some(json!({ "rate_bp": 1250, "valid_from": "2027-06-01" })),
        )
        .await;
    assert_eq!(
        (r.status, &r.body[1]["rate_bp"]),
        (StatusCode::OK, &json!(1250))
    );
    h.call(
        &owner,
        Method::PUT,
        &uri,
        Some(json!({ "rate_bp": 1250, "valid_from": "June" })),
    )
    .await
    .assert_problem(422, "invalid_field");
    h.call(
        &owner,
        Method::PUT,
        &uri,
        Some(json!({ "rate_bp": -1, "valid_from": "2027-06-01" })),
    )
    .await
    .assert_problem(422, "invalid_body");
    let unknown = format!("/api/v1/tax-classes/{}/rates", new_id());
    h.call(&owner, Method::GET, &unknown, None)
        .await
        .assert_problem(404, "not_found");
    h.call(
        &owner,
        Method::PUT,
        &unknown,
        Some(json!({ "rate_bp": 0, "valid_from": "2027-06-01" })),
    )
    .await
    .assert_problem(404, "not_found");

    let r = h
        .call(&owner, Method::GET, "/api/v1/settings/pricing", None)
        .await;
    assert_eq!(
        r.body,
        json!({ "tax_inclusive": true, "tax_registered": true, "cash_rounding": "none" })
    );
    let new =
        json!({ "tax_inclusive": false, "tax_registered": false, "cash_rounding": "down_500" });
    let r = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/pricing",
            Some(new.clone()),
        )
        .await;
    assert_eq!((r.status, &r.body), (StatusCode::OK, &new));
    let r = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/pricing",
            Some(json!({ "tax_inclusive": true, "tax_registered": true, "cash_rounding": "up" })),
        )
        .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "cash_rounding");
}
