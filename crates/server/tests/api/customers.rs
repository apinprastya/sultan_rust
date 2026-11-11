//! Customers over HTTP (spec 2.5 §15 server cases 10–17 and 20, phase M3): sign-up and replay,
//! permissions, phone uniqueness, lookup, name search, branch nodes, erase and merge, groups and
//! tiers. The balance (cases 18–19) is in `customer_balance`.

// Customers are named a, b, c, … after the spec's merge examples (A absorbs B).
#![allow(
    clippy::many_single_char_names,
    reason = "a, b, c follow the spec's examples"
)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::money::Money;
use sultan_db::customers::{self as cust, BalanceEntry, BalanceEntryKind};
use sultan_server::{NodeRole, router};
use uuid::Uuid;

use crate::common::{ANI_PIN, BUDI_PIN, Harness, Response, api_tests, new_id};

api_tests!(
    sign_up_replay_and_edit_permissions,
    input_rules,
    phone_is_unique_among_active_customers,
    lookup_by_phone_and_code,
    name_search_and_list_need_customer_search,
    tier_and_group_need_customer_manage,
    branch_node_allows_sign_up_only,
    erase_blanks_and_is_final,
    merge_moves_the_balance,
    groups_and_tiers,
    review_lists_and_history,
);

/// `PUT /customers/{id}` URI.
pub fn uri(id: &str) -> String {
    format!("/api/v1/customers/{id}")
}

/// A sign-up body.
pub fn body(name: &str, phone: Option<&str>) -> Value {
    json!({ "name": name, "phone": phone })
}

/// Creates a customer as `token` (a back-office session); returns the answer (`201`).
pub async fn create(h: &Harness, token: &str, body: Value) -> Value {
    let r = h
        .call(token, Method::PUT, &uri(&new_id()), Some(body))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body
}

/// The `id` of an answer.
pub fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

/// Appends a balance entry directly (there are no sales yet).
pub async fn add_balance(h: &Harness, customer: &str, kind: BalanceEntryKind, amount: i64) {
    let customer: Uuid = customer.parse().unwrap();
    let entry = BalanceEntry::new(customer, kind, Money::from_minor(amount), h.now());
    let mut tx = h.db.begin().await.unwrap();
    cust::append_balance_entry(&mut tx, &entry, h.now())
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

/// A POST with an `Idempotency-Key` as a back-office session.
async fn action(h: &Harness, token: &str, uri: &str, key: &str, body: Option<Value>) -> Response {
    let bearer = format!("Bearer {token}");
    h.send(
        Method::POST,
        uri,
        &[("authorization", &bearer), ("idempotency-key", key)],
        body,
    )
    .await
}

/// Cases 10 and 20: a cashier signs up at the till, a replay changes nothing, a cashier may not
/// edit, a manager may; every write is one audit row, one outbox row and one `customer.changed`
/// event without personal data.
async fn sign_up_replay_and_edit_permissions(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let budi_at = (budi.as_str(), h.jkt_device.as_str());
    let id = new_id();
    let sign_up = body(" Budi Santoso ", Some("0812-3456-789"));
    let mut events = h.events.subscribe();
    let (outbox, audit) = (h.outbox_count().await, h.audit_count("customer.put").await);

    let r = h
        .at_register(ani_at, Method::PUT, &uri(&id), &[], Some(sign_up.clone()))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let created = r.body.clone();
    assert_eq!(created["name"], "Budi Santoso");
    assert_eq!(created["phone"], "+628123456789");
    assert_eq!(created["masked_phone"], "0812****789");
    assert_eq!(created["balance"], 0);
    assert_eq!(created["archived"], false);
    assert_eq!(created["signed_up_branch_id"], h.jkt.id.to_string());
    assert_eq!(created["tier_applies"], Value::Null);
    let code = created["code"].as_str().unwrap();
    assert_eq!(code.len(), 8);
    assert_eq!(
        created["display_code"],
        format!("{}-{}", &code[..4], &code[4..])
    );
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("customer.put").await, audit + 1);
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, "customer.changed");
    assert_eq!(event.data, json!({ "customer_id": id }));

    // A replay of the same body (the till retries): `200`, nothing written.
    let r = h
        .at_register(ani_at, Method::PUT, &uri(&id), &[], Some(sign_up.clone()))
        .await;
    assert_eq!((r.status, &r.body), (StatusCode::OK, &created));
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("customer.put").await, audit + 1);
    assert!(events.try_recv().is_err());

    // The cashier may not edit; the manager may (the code never changes).
    let edit = json!({ "name": "Budi S.", "phone": "0812-3456-789", "code": "K7QH29XM" });
    let r = h
        .at_register(ani_at, Method::PUT, &uri(&id), &[], Some(edit.clone()))
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.edit");
    let r = h
        .at_register(budi_at, Method::PUT, &uri(&id), &[], Some(edit))
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        (r.body["name"].as_str(), r.body["code"].as_str()),
        (Some("Budi S."), Some(code))
    );
    assert_eq!(h.outbox_count().await, outbox + 2);
    assert_eq!(h.audit_count("customer.put").await, audit + 2);
    assert_eq!(events.try_recv().unwrap().kind, "customer.changed");

    // Reads: any session; a device token alone sees no customers.
    let r = h
        .at_register(ani_at, Method::GET, &uri(&id), &[], None)
        .await;
    assert_eq!(r.body["name"], "Budi S.");
    h.send(
        Method::GET,
        &uri(&id),
        &[("x-register-token", h.jkt_device.as_str())],
        None,
    )
    .await
    .assert_problem(401, "invalid_session");
    h.at_register(ani_at, Method::GET, &uri(&new_id()), &[], None)
        .await
        .assert_problem(404, "not_found");
    h.at_register(ani_at, Method::GET, &uri("nope"), &[], None)
        .await
        .assert_problem(400, "invalid_body");
}

/// Input rules: `core::customer` checks every field; a sent code is kept and must be free.
#[allow(clippy::too_many_lines)] // one table row per rule
async fn input_rules(h: &Harness) {
    let owner = h.owner_token().await;
    let owner = owner.as_str();
    let put = |body: Value| async move {
        h.call(owner, Method::PUT, &uri(&new_id()), Some(body))
            .await
    };
    let cases = [
        (
            json!({ "name": "A", "phone": "12345" }),
            422,
            "invalid_phone",
            "phone",
        ),
        (
            json!({ "name": "A", "tax_id": "1234" }),
            422,
            "invalid_tax_id",
            "tax_id",
        ),
        (
            json!({ "name": "A", "code": "K7QH-29X0" }),
            422,
            "invalid_code",
            "code",
        ),
        (json!({ "name": "  " }), 422, "invalid_field", "name"),
        (
            json!({ "name": "A", "email": "a@b@c" }),
            422,
            "invalid_field",
            "email",
        ),
        (
            json!({ "name": "A", "birth_month": 2 }),
            422,
            "invalid_field",
            "birthday",
        ),
        (
            json!({ "name": "A", "birth_month": 2, "birth_day": 30 }),
            422,
            "invalid_field",
            "birthday",
        ),
        (
            json!({ "name": "A", "birth_month": 1, "birth_day": 1, "birth_year": 0 }),
            422,
            "invalid_field",
            "birth_year",
        ),
        (
            json!({ "name": "A", "tier_valid_until": "2027-01-01" }),
            422,
            "invalid_field",
            "tier_valid_until",
        ),
        (
            json!({ "name": "A", "tier_id": new_id(), "tier_valid_until": "2027-02-30" }),
            422,
            "invalid_field",
            "tier_valid_until",
        ),
        (
            json!({ "name": "A", "group_id": new_id() }),
            422,
            "invalid_reference",
            "group_id",
        ),
        (
            json!({ "name": "A", "tier_id": new_id() }),
            422,
            "invalid_reference",
            "tier_id",
        ),
        (
            json!({ "name": "A", "group_id": "x" }),
            400,
            "invalid_body",
            "",
        ),
    ];
    for (body, status, code, field) in cases {
        let r = put(body.clone()).await;
        r.assert_problem(status, code);
        if !field.is_empty() {
            assert_eq!(r.body["field"], field, "{body}");
        }
    }
    h.call(
        owner,
        Method::PUT,
        &uri("550e8400-e29b-41d4-a716-446655440000"),
        Some(body("A", None)),
    )
    .await
    .assert_problem(400, "invalid_body");

    // Everything normalized: code, phone, email, tax id; blank text is "none"; 29 February.
    let r = put(json!({
        "code": " k7qh-29xm ", "name": "Rina", "phone": "+65 9123 4567", "email": " Rina@Example.COM ",
        "tax_id": "01.234.567.8-901.000", "birth_month": 2, "birth_day": 29, "address": " ",
        "company_name": "PT Rina", "notes": "VIP",
    }))
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let b = &r.body;
    assert_eq!(
        (b["code"].as_str(), b["display_code"].as_str()),
        (Some("K7QH29XM"), Some("K7QH-29XM"))
    );
    assert_eq!(b["phone"], "+6591234567");
    assert_eq!(b["masked_phone"], "+659***567");
    assert_eq!(b["email"], "rina@example.com");
    assert_eq!(b["tax_id"], "012345678901000");
    assert_eq!(
        (b["birth_month"].as_u64(), b["birth_day"].as_u64()),
        (Some(2), Some(29))
    );
    assert_eq!(b["birth_year"], Value::Null);
    assert_eq!(b["address"], Value::Null);
    // A code the client sent that is taken: `409 conflict`, the client draws again.
    let r = put(json!({ "code": "K7QH29XM", "name": "Other" })).await;
    r.assert_problem(409, "conflict");
    assert_eq!(r.body["field"], "code");
}

/// Case 11: a phone used by another active customer is `409 phone_taken` with that customer's
/// code and masked phone; an archived holder frees it; unarchiving then clashes.
async fn phone_is_unique_among_active_customers(h: &Harness) {
    let owner = h.owner_token().await;
    let a = create(h, &owner, body("Ani", Some("0812-3456-789"))).await;
    let r = h
        .call(
            &owner,
            Method::PUT,
            &uri(&new_id()),
            Some(body("Budi", Some("62812 3456 789"))),
        )
        .await;
    r.assert_problem(409, "phone_taken");
    assert_eq!(r.body["customer_id"], a["id"]);
    assert_eq!(r.body["customer_code"], a["display_code"]);
    assert_eq!(r.body["masked_phone"], "0812****789");
    assert_eq!(r.body["field"], "phone");

    let r = action(
        h,
        &owner,
        &format!("{}/archive", uri(&id(&a))),
        "arch-1",
        None,
    )
    .await;
    assert_eq!(
        (r.status, &r.body["archived"]),
        (StatusCode::OK, &json!(true))
    );
    let b = create(h, &owner, body("Budi", Some("+62 812 3456 789"))).await;
    assert_eq!(b["phone"], "+628123456789");
    // Archiving again changes nothing; unarchiving clashes with B.
    let outbox = h.outbox_count().await;
    let r = action(
        h,
        &owner,
        &format!("{}/archive", uri(&id(&a))),
        "arch-2",
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.outbox_count().await, outbox);
    let r = action(
        h,
        &owner,
        &format!("{}/unarchive", uri(&id(&a))),
        "unarch-1",
        None,
    )
    .await;
    r.assert_problem(409, "phone_taken");
    assert_eq!(r.body["customer_code"], b["display_code"]);
    // An archived customer cannot be edited.
    h.call(
        &owner,
        Method::PUT,
        &uri(&id(&a)),
        Some(body("Ani 2", None)),
    )
    .await
    .assert_problem(409, "archived");
}

/// Case 12: lookup by phone and code; a merged customer's code finds the survivor; archived and
/// erased customers are `404`; a device token alone is `401`.
async fn lookup_by_phone_and_code(h: &Harness) {
    let owner = h.owner_token().await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let a = create(h, &owner, body("Ani", Some("0812-1111-222"))).await;
    let b = create(h, &owner, body("Budi", Some("0812-3333-444"))).await;
    let lookup = |query: String| async move {
        h.at_register(
            at,
            Method::GET,
            &format!("/api/v1/customers/lookup?{query}"),
            &[],
            None,
        )
        .await
    };
    let r = lookup("phone=%2B62%20812%201111%20222".into()).await;
    assert_eq!(
        (r.status, r.body["id"].clone()),
        (StatusCode::OK, a["id"].clone())
    );
    let code = a["display_code"].as_str().unwrap().to_lowercase();
    assert_eq!(lookup(format!("code={code}")).await.body["id"], a["id"]);

    let r = action(
        h,
        &owner,
        &format!("{}/merge", uri(&id(&a))),
        "m-1",
        Some(json!({ "from": id(&b) })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let b_code = b["code"].as_str().unwrap();
    assert_eq!(lookup(format!("code={b_code}")).await.body["id"], a["id"]);
    // A has a phone of its own, so B keeps its phone, which finds A through the merge.
    assert_eq!(
        lookup("phone=0812-3333-444".into()).await.body["id"],
        a["id"]
    );

    let c = create(h, &owner, body("Citra", Some("0812-5555-666"))).await;
    action(h, &owner, &format!("{}/archive", uri(&id(&c))), "a-c", None).await;
    lookup("phone=0812-5555-666".into())
        .await
        .assert_problem(404, "not_found");
    let d = create(h, &owner, body("Dedi", Some("0812-7777-888"))).await;
    let r = action(h, &owner, &format!("{}/erase", uri(&id(&d))), "e-d", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    lookup(format!("code={}", d["code"].as_str().unwrap()))
        .await
        .assert_problem(404, "not_found");

    lookup("phone=12".into())
        .await
        .assert_problem(422, "invalid_phone");
    lookup("code=K7QH-29X0".into())
        .await
        .assert_problem(422, "invalid_code");
    lookup(String::new())
        .await
        .assert_problem(422, "invalid_field");
    lookup(format!("phone=0812-1111-222&code={code}"))
        .await
        .assert_problem(422, "invalid_field");
    h.send(
        Method::GET,
        "/api/v1/customers/lookup?phone=081211112222",
        &[("x-register-token", h.jkt_device.as_str())],
        None,
    )
    .await
    .assert_problem(401, "invalid_session");
}

/// Case 13: name search needs `customer.search`; a device token alone is `401`. Without `q` the
/// list (archived ones with `include_archived` and `customer.manage`).
#[allow(clippy::too_many_lines)] // one flow: search, page, list, permissions
async fn name_search_and_list_need_customer_search(h: &Harness) {
    let owner = h.owner_token().await;
    for i in 0..22 {
        create(h, &owner, body(&format!("Budi {i:02}"), None)).await;
    }
    let rina = create(
        h,
        &owner,
        json!({ "name": "Rina", "company_name": "Toko Budiman" }),
    )
    .await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let budi_at = (budi.as_str(), h.jkt_device.as_str());
    let r = h
        .at_register(ani_at, Method::GET, "/api/v1/customers?q=budi", &[], None)
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.search");
    h.send(
        Method::GET,
        "/api/v1/customers?q=budi",
        &[("x-register-token", h.jkt_device.as_str())],
        None,
    )
    .await
    .assert_problem(401, "invalid_session");

    let page = |query: &'static str| async move {
        let r = h
            .at_register(
                budi_at,
                Method::GET,
                &format!("/api/v1/customers?{query}"),
                &[],
                None,
            )
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        r.body
    };
    let first = page("q=BUDI").await;
    assert_eq!(first["items"].as_array().unwrap().len(), 20);
    assert_eq!(first["items"][0]["name"], "Budi 00");
    assert_eq!(first["next_page"], 2);
    let second = page("q=budi&page=2").await;
    assert_eq!(
        second["items"].as_array().unwrap().len(),
        3,
        "the company name matches too"
    );
    assert_eq!(second["items"][2]["id"], rina["id"]);
    assert_eq!(second["next_page"], Value::Null);
    assert_eq!(
        page("q=budi&limit=5").await["items"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    // The list: every active customer by name.
    action(
        h,
        &owner,
        &format!("{}/archive", uri(&id(&rina))),
        "a-r",
        None,
    )
    .await;
    let list = page("page=2").await;
    assert_eq!(list["items"].as_array().unwrap().len(), 2);
    let all = page("page=2&include_archived=true").await;
    assert_eq!(all["items"].as_array().unwrap().len(), 3);
    assert_eq!(all["items"][2]["archived"], true);

    for bad in ["page=0", "limit=0", "limit=21"] {
        h.at_register(
            budi_at,
            Method::GET,
            &format!("/api/v1/customers?{bad}"),
            &[],
            None,
        )
        .await
        .assert_problem(422, "invalid_field");
    }
    // `include_archived` needs `customer.manage`: a role with `customer.search` only.
    let mut tx = h.db.begin().await.unwrap();
    let role = sultan_core::ids::RoleId::generate();
    sultan_db::roles::save_role(
        &mut tx,
        &sultan_core::access::Role {
            id: role,
            name: "Searcher".into(),
            grants: vec![sultan_core::access::Grant::unlimited(
                sultan_core::access::Permission::CustomerSearch,
            )],
        },
        h.now(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    h.add_user(
        "cari",
        role,
        sultan_core::access::BranchScope::Branch(h.jkt.id),
    )
    .await;
    let cari = h.password_token("cari", crate::common::BUDI_PASSWORD).await;
    assert_eq!(
        h.call(&cari, Method::GET, "/api/v1/customers?q=budi", None)
            .await
            .status,
        StatusCode::OK
    );
    let r = h
        .call(
            &cari,
            Method::GET,
            "/api/v1/customers?include_archived=true",
            None,
        )
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.manage");
    // An archived customer's page needs `customer.manage` too.
    h.call(&cari, Method::GET, &uri(&id(&rina)), None)
        .await
        .assert_problem(403, "permission_denied");
    assert_eq!(
        h.call(&owner, Method::GET, &uri(&id(&rina)), None)
            .await
            .status,
        StatusCode::OK
    );
}

/// Case 14: group, tier, valid-until and manual flag need `customer.manage`, on create too; the
/// tier applies until its last day at the session's branch.
async fn tier_and_group_need_customer_manage(h: &Harness) {
    let owner = h.owner_token().await;
    let gold = new_id();
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/membership-tiers/{gold}"),
            Some(json!({ "name": "Gold", "rank": 2 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let budi_at = (budi.as_str(), h.jkt_device.as_str());

    let id = new_id();
    let with_tier = json!({ "name": "Siti", "tier_id": gold });
    let r = h
        .at_register(ani_at, Method::PUT, &uri(&id), &[], Some(with_tier.clone()))
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.manage");
    let r = h
        .at_register(
            ani_at,
            Method::PUT,
            &uri(&id),
            &[],
            Some(json!({ "name": "Siti" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    // A cashier changing the tier: denied even with `customer.create`.
    let r = h
        .at_register(ani_at, Method::PUT, &uri(&id), &[], Some(with_tier.clone()))
        .await;
    r.assert_problem(403, "permission_denied");

    // T0 is 2027-01-15 in Jakarta: valid until the 15th still applies, the 14th does not.
    for (until, applies) in [("2027-01-15", true), ("2027-01-14", false)] {
        let mut b = with_tier.clone();
        b["tier_valid_until"] = until.into();
        let r = h
            .at_register(budi_at, Method::PUT, &uri(&id), &[], Some(b))
            .await;
        assert_eq!(r.status, StatusCode::OK, "{}", r.body);
        assert_eq!(r.body["tier_name"], "Gold");
        assert_eq!(r.body["tier_valid_until"], until);
        assert_eq!(r.body["tier_applies"], applies, "{until}");
    }
    // An archived tier does not apply.
    h.call(
        &owner,
        Method::PUT,
        &format!("/api/v1/membership-tiers/{gold}"),
        Some(json!({ "name": "Gold", "rank": 2, "archived": true })),
    )
    .await;
    let r = h
        .at_register(budi_at, Method::GET, &uri(&id), &[], None)
        .await;
    assert_eq!(r.body["tier_applies"], false);
}

/// Case 15: on a branch node only sign-up works; every other write is `read_only_replica`.
async fn branch_node_allows_sign_up_only(h: &Harness) {
    let owner = h.owner_token().await;
    let existing = create(h, &owner, body("Ani", None)).await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let send = |token: &str, method: Method, uri: String, body: Option<Value>| {
        let bearer = format!("Bearer {token}");
        let device = h.jkt_device.clone();
        async move {
            Harness::send_to(
                router(h.state(NodeRole::Branch)),
                method,
                &uri,
                &[("authorization", &bearer), ("x-register-token", &device)],
                body,
            )
            .await
        }
    };
    let fresh = new_id();
    let r = send(
        &ani,
        Method::PUT,
        uri(&fresh),
        Some(body("Baru", Some("0812-9999-000"))),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = send(
        &ani,
        Method::PUT,
        uri(&fresh),
        Some(body("Baru", Some("0812-9999-000"))),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "a replay is still fine");
    let lookup = send(
        &ani,
        Method::GET,
        "/api/v1/customers/lookup?phone=08129999000".into(),
        None,
    )
    .await;
    assert_eq!(lookup.body["id"], fresh);

    let writes = [
        (Method::PUT, uri(&fresh), Some(body("Edited", None))),
        (
            Method::POST,
            format!("{}/archive", uri(&id(&existing))),
            None,
        ),
        (
            Method::POST,
            format!("{}/unarchive", uri(&id(&existing))),
            None,
        ),
        (
            Method::POST,
            format!("{}/merge", uri(&id(&existing))),
            Some(json!({ "from": fresh })),
        ),
        (Method::POST, format!("{}/erase", uri(&id(&existing))), None),
        (
            Method::PUT,
            format!("/api/v1/customer-groups/{}", new_id()),
            Some(json!({ "name": "G" })),
        ),
        (
            Method::PUT,
            format!("/api/v1/membership-tiers/{}", new_id()),
            Some(json!({ "name": "T", "rank": 1 })),
        ),
        (
            Method::PUT,
            "/api/v1/settings/customer-balance".into(),
            Some(json!({ "max_saved_per_sale": 1, "max_balance": 2 })),
        ),
    ];
    for (method, uri, body) in writes {
        send(&budi, method.clone(), uri.clone(), body)
            .await
            .assert_problem(403, "read_only_replica");
    }
}

/// Case 16: erase blanks the personal data, archives, audits without the values and cannot be
/// repeated; a balance that is not 0 refuses it.
async fn erase_blanks_and_is_final(h: &Harness) {
    let owner = h.owner_token().await;
    let full = json!({
        "name": "Budi Santoso", "phone": "0812-3456-789", "email": "budi@example.com",
        "birth_month": 5, "birth_day": 17, "birth_year": 1990, "address": "Jl. Mawar 1",
        "company_name": "PT Budi", "tax_id": "0123456789012345", "tax_address": "Jl. Pajak",
        "notes": "secret note",
    });
    let c = create(h, &owner, full).await;
    let erase_uri = format!("{}/erase", uri(&id(&c)));
    // A manager may not erase (Owner only).
    let budi = h.manager_token().await;
    let r = action(h, &budi, &erase_uri, "e-0", None).await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "customer.erase");

    add_balance(h, &id(&c), BalanceEntryKind::ChangeSaved, 700).await;
    let r = action(h, &owner, &erase_uri, "e-1", None).await;
    r.assert_problem(409, "balance_not_zero");
    assert_eq!(r.body["balance"], 700);
    add_balance(h, &id(&c), BalanceEntryKind::Payment, -700).await;

    let puts = h.audit_count("customer.put").await;
    let mut events = h.events.subscribe();
    let r = action(h, &owner, &erase_uri, "e-2", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let e = &r.body;
    assert_eq!(e["name"], "Erased customer");
    for field in [
        "phone",
        "masked_phone",
        "email",
        "birth_month",
        "birth_day",
        "birth_year",
        "address",
        "company_name",
        "tax_id",
        "tax_address",
        "notes",
    ] {
        assert_eq!(e[field], Value::Null, "{field}");
    }
    assert_eq!(e["archived"], true);
    assert_eq!(e["erased_at"], h.now().unix_millis());
    assert_eq!(e["code"], c["code"], "the code stays");
    assert_eq!(
        events.try_recv().unwrap().data,
        json!({ "customer_id": id(&c) })
    );
    let filter = sultan_db::audit::AuditFilter {
        action: Some("customer.erase".into()),
        ..Default::default()
    };
    let entries = sultan_db::audit::query(&h.db, &filter).await.unwrap();
    assert_eq!(entries.len(), 1);
    let after = entries[0].after.as_ref().unwrap().to_string();
    assert!(
        !after.contains("Budi") && !after.contains("secret"),
        "{after}"
    );
    assert_eq!(
        h.audit_count("customer.put").await,
        puts,
        "older entries untouched"
    );
    // A second erase: `404`; the phone is free again.
    action(h, &owner, &erase_uri, "e-3", None)
        .await
        .assert_problem(404, "not_found");
    create(h, &owner, body("Baru", Some("0812-3456-789"))).await;
    // Unarchiving an erased customer: `404`.
    action(
        h,
        &owner,
        &format!("{}/unarchive", uri(&id(&c))),
        "u-1",
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

/// Case 17: merge archives B with `merged_into`, moves B's balance in one transaction, refuses
/// chains (`409 merged`) and A = B (`422`); a retry with the same key is replayed.
#[allow(clippy::too_many_lines)] // one flow: merge, ledger, replay, refusals
async fn merge_moves_the_balance(h: &Harness) {
    let owner = h.owner_token().await;
    let a = create(h, &owner, body("Ani", None)).await;
    let b = create(
        h,
        &owner,
        json!({ "name": "Ani W.", "phone": "0812-1212-121", "email": "ani@x.id" }),
    )
    .await;
    add_balance(h, &id(&b), BalanceEntryKind::ChangeSaved, 1_500).await;
    let merge_uri = format!("{}/merge", uri(&id(&a)));
    let from_b = Some(json!({ "from": id(&b) }));
    let mut events = h.events.subscribe();
    let outbox = h.outbox_count().await;
    let r = action(h, &owner, &merge_uri, "merge-1", from_b.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["balance"], 1_500);
    assert_eq!(
        r.body["phone"], "+628121212121",
        "empty fields filled from B"
    );
    assert_eq!(r.body["email"], "ani@x.id");
    assert_eq!(
        h.outbox_count().await,
        outbox + 2,
        "one customer.put for each"
    );
    assert_eq!(h.audit_count("customer.merge").await, 1);
    assert_eq!(h.audit_count("customer.merged").await, 1);
    let ids: Vec<Value> = (0..2)
        .map(|_| events.try_recv().unwrap().data["customer_id"].clone())
        .collect();
    assert_eq!(ids, [a["id"].clone(), b["id"].clone()]);

    let stored_b = h.call(&owner, Method::GET, &uri(&id(&b)), None).await.body;
    assert_eq!(
        (
            stored_b["archived"].clone(),
            stored_b["merged_into_id"].clone(),
            stored_b["balance"].clone()
        ),
        (json!(true), a["id"].clone(), json!(0))
    );
    let owner = owner.as_str();
    let kinds = |who: &Value| {
        let who = id(who);
        async move {
            let r = h
                .call(owner, Method::GET, &format!("{}/balance", uri(&who)), None)
                .await;
            r.body["entries"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e["kind"].as_str().unwrap().to_owned(),
                        e["amount"].as_i64().unwrap(),
                    )
                })
                .collect::<Vec<_>>()
        }
    };
    assert_eq!(kinds(&a).await, [("merge_in".to_owned(), 1_500)]);
    assert_eq!(kinds(&b).await[0], ("merge_out".to_owned(), -1_500));

    // The same key replays the answer; a new key hits the rule.
    let replay = action(h, owner, &merge_uri, "merge-1", from_b.clone()).await;
    assert_eq!(replay.headers["idempotency-replayed"], "true");
    action(h, owner, &merge_uri, "merge-2", from_b)
        .await
        .assert_problem(409, "merged");
    let c = create(h, owner, body("Citra", None)).await;
    action(
        h,
        owner,
        &format!("{}/merge", uri(&id(&b))),
        "merge-3",
        Some(json!({ "from": id(&c) })),
    )
    .await
    .assert_problem(409, "merged");
    let r = action(
        h,
        owner,
        &merge_uri,
        "merge-4",
        Some(json!({ "from": id(&a) })),
    )
    .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "from");
    action(
        h,
        owner,
        &merge_uri,
        "merge-5",
        Some(json!({ "from": new_id() })),
    )
    .await
    .assert_problem(404, "not_found");
    action(
        h,
        owner,
        &merge_uri,
        "merge-6",
        Some(json!({ "from": "x" })),
    )
    .await
    .assert_problem(400, "invalid_body");
    let budi = h.manager_token().await;
    assert_eq!(
        action(
            h,
            &budi,
            &format!("{}/merge", uri(&id(&a))),
            "merge-7",
            Some(json!({ "from": id(&c) }))
        )
        .await
        .status,
        StatusCode::OK,
        "a manager holds customer.manage"
    );
    // A cashier does not.
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    h.at_register(
        (&ani, &h.jkt_device),
        Method::POST,
        &format!("{}/archive", uri(&id(&a))),
        &[],
        None,
    )
    .await
    .assert_problem(403, "permission_denied");
}

/// Groups and tiers: create, replay, archive, duplicates, permissions.
#[allow(clippy::too_many_lines)] // one flow: groups, then tiers, then permissions
async fn groups_and_tiers(h: &Harness) {
    let owner = h.owner_token().await;
    let g = new_id();
    let guri = format!("/api/v1/customer-groups/{g}");
    let r = h
        .call(
            &owner,
            Method::PUT,
            &guri,
            Some(json!({ "name": " Wholesale " })),
        )
        .await;
    assert_eq!(
        (r.status, r.body["name"].as_str()),
        (StatusCode::CREATED, Some("Wholesale"))
    );
    let outbox = h.outbox_count().await;
    let r = h
        .call(
            &owner,
            Method::PUT,
            &guri,
            Some(json!({ "name": "Wholesale" })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(h.outbox_count().await, outbox, "a replay writes nothing");
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/customer-groups/{}", new_id()),
            Some(json!({ "name": "Wholesale" })),
        )
        .await;
    r.assert_problem(409, "conflict");
    assert_eq!(r.body["field"], "name");
    h.call(
        &owner,
        Method::PUT,
        &guri,
        Some(json!({ "name": "Wholesale", "archived": true })),
    )
    .await;
    let owner = owner.as_str();
    let list = |query: &'static str| async move {
        h.call(
            owner,
            Method::GET,
            &format!("/api/v1/customer-groups{query}"),
            None,
        )
        .await
        .body
    };
    assert_eq!(list("").await, json!([]));
    assert_eq!(list("?include_archived=true").await[0]["archived"], true);
    assert_eq!(
        h.call(owner, Method::GET, &guri, None).await.body["name"],
        "Wholesale"
    );
    h.call(
        owner,
        Method::GET,
        &format!("/api/v1/customer-groups/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");

    let (silver, gold) = (new_id(), new_id());
    let turi = |id: &str| format!("/api/v1/membership-tiers/{id}");
    h.call(
        owner,
        Method::PUT,
        &turi(&silver),
        Some(json!({ "name": "Silver", "rank": 1 })),
    )
    .await;
    h.call(
        owner,
        Method::PUT,
        &turi(&gold),
        Some(json!({ "name": "Gold", "rank": 2 })),
    )
    .await;
    let r = h
        .call(
            owner,
            Method::PUT,
            &turi(&new_id()),
            Some(json!({ "name": "Platinum", "rank": 2 })),
        )
        .await;
    r.assert_problem(409, "conflict");
    assert_eq!(r.body["field"], "rank");
    let tiers = h
        .call(owner, Method::GET, "/api/v1/membership-tiers", None)
        .await
        .body;
    let names: Vec<&str> = tiers
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Gold", "Silver"], "best first");
    assert_eq!(
        h.call(owner, Method::GET, &turi(&gold), None).await.body["rank"],
        2
    );
    h.call(owner, Method::GET, &turi(&new_id()), None)
        .await
        .assert_problem(404, "not_found");
    h.call(
        owner,
        Method::PUT,
        &turi("x"),
        Some(json!({ "name": "X", "rank": 9 })),
    )
    .await
    .assert_problem(400, "invalid_body");

    // A cashier with a back-office session reads but does not write.
    h.add_user(
        "kasir",
        sultan_db::seed::CASHIER_ROLE,
        sultan_core::access::BranchScope::Branch(h.jkt.id),
    )
    .await;
    let kasir = h
        .password_token("kasir", crate::common::BUDI_PASSWORD)
        .await;
    assert_eq!(
        h.call(
            &kasir,
            Method::GET,
            "/api/v1/membership-tiers?include_archived=true",
            None
        )
        .await
        .status,
        StatusCode::OK
    );
    for uri in [guri, turi(&gold)] {
        let r = h
            .call(
                &kasir,
                Method::PUT,
                &uri,
                Some(json!({ "name": "X", "rank": 7 })),
            )
            .await;
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], "customer.manage");
    }
}

/// "Possible duplicates", "Negative balances" (`customer.manage`) and the history (empty until
/// sales exist).
#[allow(clippy::too_many_lines)] // the synced fixture is long
async fn review_lists_and_history(h: &Harness) {
    let owner = h.owner_token().await;
    let a = create(h, &owner, body("Ani", Some("0812-1111-222"))).await;
    // A branch's offline sign-up with the same phone, synced to HQ.
    let synced = sultan_db::customers::Customer {
        id: Uuid::now_v7(),
        code: "SYNCED22".into(),
        name: "Ani (BDG)".into(),
        phone: Some("+628121111222".into()),
        email: None,
        birthday: None,
        address: None,
        company_name: None,
        tax_id: None,
        tax_address: None,
        notes: None,
        group: None,
        tier: None,
        tier_valid_until: None,
        manual_tier: false,
        signed_up_branch: Some(h.bdg.id),
        signed_up_at: h.now(),
    };
    let mut tx = h.db.begin().await.unwrap();
    cust::put_synced_customer(
        &mut tx,
        &synced,
        &sultan_db::customers::Actor {
            user: None,
            branch: Some(h.bdg.id),
            register: None,
        },
        h.now(),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    add_balance(h, &id(&a), BalanceEntryKind::Payment, -300).await;

    let budi = h.manager_token().await;
    let dups = h
        .call(&budi, Method::GET, "/api/v1/customers/duplicates", None)
        .await
        .body;
    assert_eq!(dups["items"][0]["phone_conflict"], "+628121111222");
    assert_eq!(dups["next_page"], Value::Null);
    let neg = h
        .call(
            &budi,
            Method::GET,
            "/api/v1/customers/negative-balances?page=1",
            None,
        )
        .await
        .body;
    assert_eq!(
        (
            neg["items"][0]["id"].clone(),
            neg["items"][0]["balance"].clone()
        ),
        (a["id"].clone(), json!(-300))
    );
    h.call(
        &budi,
        Method::GET,
        "/api/v1/customers/duplicates?page=0",
        None,
    )
    .await
    .assert_problem(422, "invalid_field");
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    for list in ["duplicates", "negative-balances"] {
        h.at_register(
            at,
            Method::GET,
            &format!("/api/v1/customers/{list}"),
            &[],
            None,
        )
        .await
        .assert_problem(403, "permission_denied");
    }
    let history = h
        .at_register(
            at,
            Method::GET,
            &format!("{}/history", uri(&id(&a))),
            &[],
            None,
        )
        .await;
    assert_eq!(history.body, json!({ "items": [], "next_before_id": null }));
    h.at_register(
        at,
        Method::GET,
        &format!("{}/history", uri(&new_id())),
        &[],
        None,
    )
    .await
    .assert_problem(404, "not_found");
}
