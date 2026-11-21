//! Cash management over HTTP (spec 2.7 §2, §3, §4, §5, §11; phase K3): cash in / out / drop with
//! permissions, overrides, replays and reversals; cash reasons; the close report's cash and float
//! blocks; the X report; slips. Spec 2.7 §12 server cases 15–21, 27 (cash reasons), 28 (cash
//! movements), 29 and 30 (`cash.moved`). The Z report is in `cash_z.rs`.

// The tests are scripts that read top to bottom with short names (`h`, `r`, `t`), as elsewhere in
// this suite.
#![allow(clippy::many_single_char_names, clippy::too_many_lines)]

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_db::customers::BalanceEntryKind;
use sultan_server::events::Audience;
use sultan_server::{NodeRole, router};

use crate::common::{ANI_PIN, BUDI_PIN, Harness, Response, api_tests, new_id};
use crate::sales::{approve, open};

api_tests!(
    cash_movements_post_replay_and_permissions,
    reversals_and_movement_reads,
    cash_reasons_are_hq_master_data,
    close_report_counts_cash_and_keeps_the_float,
    x_report_rules,
    slips_and_reprints,
);

/// The seeded reasons (`sultan_db::cash::SEED_REASON_IDS`).
pub const TOP_UP: &str = "00000000-0000-7000-8000-000000000401";
pub const OTHER_IN: &str = "00000000-0000-7000-8000-000000000402";
pub const SHOP: &str = "00000000-0000-7000-8000-000000000403";
pub const PARKING: &str = "00000000-0000-7000-8000-000000000404";
pub const OTHER_OUT: &str = "00000000-0000-7000-8000-000000000406";

/// `PUT /cash-movements/{id}` at the register, with an override token.
pub async fn put_cash(
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
        &format!("/api/v1/cash-movements/{id}"),
        &headers,
        Some(body.clone()),
    )
    .await
}

/// Posts a movement that needs no override; asserts `201`.
pub async fn move_cash(h: &Harness, at: (&str, &str), body: Value) -> Value {
    let r = put_cash(h, at, &new_id(), &body, None).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body
}

/// Posts a movement with budi's approval of `permission` for `value`; asserts `201`.
pub async fn approved_cash(
    h: &Harness,
    at: (&str, &str),
    permission: &str,
    value: Value,
    body: Value,
) -> Value {
    let id = new_id();
    let t = approve(h, at, permission, value, &id).await;
    let r = put_cash(h, at, &id, &body, Some(&t)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    r.body
}

pub fn cash(kind: &str, amount: i64, reason: Option<&str>) -> Value {
    json!({ "kind": kind, "amount": amount, "reason_id": reason })
}

/// A GET as `token`, sending `device` too: a PIN session needs its register's token, a back-office
/// session ignores it.
pub async fn get_as(h: &Harness, (token, device): (&str, &str), uri: &str) -> Response {
    h.at_register((token, device), Method::GET, uri, &[], None)
        .await
}

/// The ani PIN session at JKT01 R1.
async fn ani(h: &Harness) -> String {
    h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await
}

/// Closes `session` at the register with `body`; asserts `200`.
pub async fn close(h: &Harness, at: (&str, &str), session: &str, body: Value) -> Value {
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/cashier-sessions/{session}/close"),
            &[],
            Some(body),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    r.body
}

async fn cash_movements_post_replay_and_permissions(h: &Harness) {
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    // Case 15: no open session.
    put_cash(h, at, &new_id(), &cash("in", 50_000, Some(TOP_UP)), None)
        .await
        .assert_problem(409, "no_open_session");
    let session = open(h, at, 200_000).await;
    let mut events = h.events.subscribe();
    let (outbox, audits) = (
        h.outbox_count().await,
        h.audit_count("cash_movement.post").await,
    );

    // Case 16: a replay answers the stored movement; another body is a conflict.
    let id = new_id();
    let body = cash("in", 50_000, Some(TOP_UP));
    let first = put_cash(h, at, &id, &body, None).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert!(
        first.body["number"]
            .as_str()
            .unwrap()
            .starts_with("JKT01-R1-K-"),
        "{}",
        first.body
    );
    assert!(first.body["number"].as_str().unwrap().ends_with("-01"));
    assert_eq!(first.body["effect"], 50_000);
    assert_eq!(first.body["cashier_session_id"], session);
    assert_eq!(first.body["reason_name"], "Tambahan modal / uang kecil");
    assert_eq!(first.body["photo_ids"], json!([]));
    assert_eq!(first.body["missing_nota"], false);
    let again = put_cash(h, at, &id, &body, None).await;
    assert_eq!(again.status, StatusCode::OK);
    assert_eq!(again.body, first.body);
    put_cash(h, at, &id, &cash("in", 50_001, Some(TOP_UP)), None)
        .await
        .assert_problem(409, "conflict");
    // A replay answers only who may read the movement (like `GET`): another branch's cashier
    // with the same ID and body is refused; a manager with `report.sales` at JKT01 gets it.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    put_cash(h, (&cici, &h.bdg_device), &id, &body, None)
        .await
        .assert_problem(403, "permission_denied");
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let r = put_cash(h, (&budi, &h.jkt_device), &id, &body, None).await;
    assert_eq!((r.status, &r.body), (StatusCode::OK, &first.body));
    // Case 28: one audit row and one outbox row per movement, none for the replay.
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("cash_movement.post").await, audits + 1);
    // Case 30: `cash.moved` to the branch.
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, "cash.moved");
    assert_eq!(event.audience, Audience::Branch(h.jkt.id));
    assert_eq!(event.data["session_id"], session);
    assert!(events.try_recv().is_err(), "a replay publishes nothing");

    // A cash out within the cashier's limit (Rp 50 000) needs nobody.
    let out = move_cash(h, at, cash("out", 50_000, Some(PARKING))).await;
    assert_eq!(out["effect"], -50_000);
    assert_eq!(out["approved_by"], Value::Null);
    // Above it: `needs_override` with the limit; a token for another amount is refused (case 17).
    let id = new_id();
    let big = cash("out", 75_000, Some(SHOP));
    let r = put_cash(h, at, &id, &big, None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(
        (r.body["permission"].as_str(), r.body["limit"].as_i64()),
        (Some("cash.out"), Some(50_000))
    );
    let wrong = approve(h, at, "cash.out", json!(10_000), &id).await;
    put_cash(h, at, &id, &big, Some(&wrong))
        .await
        .assert_problem(403, "invalid_override");
    let right = approve(h, at, "cash.out", json!(75_000), &id).await;
    let r = put_cash(h, at, &id, &big, Some(&right)).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["approved_by"], h.budi.id.to_string());
    // A drop needs `cash.drop`, which a cashier lacks.
    let r = put_cash(h, at, &new_id(), &cash("drop", 100_000, None), None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "cash.drop");
    let drop = approved_cash(h, at, "cash.drop", Value::Null, cash("drop", 100_000, None)).await;
    assert_eq!(drop["kind"], "drop");

    // The rules of `core::cash` (cases 5–7).
    let note =
        |n: &str| json!({ "kind": "out", "amount": 1_000, "reason_id": OTHER_OUT, "note": n });
    for (body, field) in [
        (cash("in", 0, Some(TOP_UP)), "amount"),
        (cash("in", -5, Some(TOP_UP)), "amount"),
        (cash("in", 1_000, Some(SHOP)), "reason"),
        (cash("drop", 1_000, Some(SHOP)), "reason"),
        (cash("out", 1_000, None), "reason"),
        (cash("out", 1_000, Some(&new_id())), "reason"),
        (cash("sideways", 1_000, None), "kind"),
        (json!({ "amount": 1_000 }), "kind"),
        (json!({ "kind": "in", "reason_id": TOP_UP }), "amount"),
        (note("   "), "note"),
        (note(&"x".repeat(201)), "note"),
    ] {
        let r = put_cash(h, at, &new_id(), &body, None).await;
        r.assert_problem(422, "invalid_field");
        assert_eq!(r.body["field"], field, "{body}");
    }
    let other = move_cash(h, at, note("  galon air  ")).await;
    assert_eq!(other["note"], "galon air");
    put_cash(h, at, "not-a-uuid", &cash("in", 1, Some(OTHER_IN)), None)
        .await
        .assert_problem(400, "invalid_body");

    // Budi (Manager) at a register without his own session.
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    put_cash(
        h,
        (&budi, &h.jkt_device),
        &new_id(),
        &cash("drop", 1, None),
        None,
    )
    .await
    .assert_problem(409, "no_open_session");
    // A back-office session cannot post cash.
    let owner = h.owner_token().await;
    h.call(
        &owner,
        Method::PUT,
        &format!("/api/v1/cash-movements/{}", new_id()),
        Some(cash("in", 1, Some(TOP_UP))),
    )
    .await
    .assert_problem(403, "register_session_required");
}

async fn reversals_and_movement_reads(h: &Harness) {
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 100_000).await;
    let original = move_cash(h, at, cash("out", 25_000, Some(PARKING))).await;
    let oid = original["id"].as_str().unwrap();
    let reversal = |note: Option<&str>| json!({ "reverses": oid, "note": note });
    // A reversal needs a note, then takes the original's kind, amount and reason.
    let r = put_cash(h, at, &new_id(), &reversal(None), None).await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "note");
    let r = put_cash(
        h,
        at,
        &new_id(),
        &json!({ "reverses": oid, "note": "x", "amount": 1 }),
        None,
    )
    .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "reverses");
    let back = move_cash(h, at, reversal(Some("wrong amount"))).await;
    assert_eq!(
        (
            back["kind"].as_str(),
            back["amount"].as_i64(),
            back["effect"].as_i64()
        ),
        (Some("out"), Some(25_000), Some(25_000))
    );
    assert_eq!(back["reason_id"], PARKING);
    assert_eq!(back["reverses_number"], original["number"]);
    // Case 8: reversed once; a reversal is never reversed.
    let r = put_cash(h, at, &new_id(), &reversal(Some("again")), None).await;
    r.assert_problem(409, "already_reversed");
    assert_eq!(r.body["field"], "reverses");
    let r = put_cash(
        h,
        at,
        &new_id(),
        &json!({ "reverses": back["id"], "note": "x" }),
        None,
    )
    .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "reverses");
    put_cash(
        h,
        at,
        &new_id(),
        &json!({ "reverses": new_id(), "note": "x" }),
        None,
    )
    .await
    .assert_problem(422, "invalid_field");

    // Reads: the original now shows who reversed it; a big cash out is missing its nota.
    let get = |token: String, device: String, id: String| async move {
        get_as(
            h,
            (&token, &device),
            &format!("/api/v1/cash-movements/{id}"),
        )
        .await
    };
    let (jkt, bdg) = (h.jkt_device.clone(), h.bdg_device.clone());
    let r = get(token.clone(), jkt.clone(), oid.to_owned()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["reversed_by"], back["id"]);
    let big = approved_cash(
        h,
        at,
        "cash.out",
        json!(150_000),
        cash("out", 150_000, Some(SHOP)),
    )
    .await;
    assert_eq!(big["missing_nota"], true);
    // Another cashier may not read it; a manager of the branch may.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    let cici_token = cici.clone();
    let cici_token = &cici_token;
    get(cici.clone(), bdg.clone(), oid.to_owned())
        .await
        .assert_problem(403, "permission_denied");
    let manager = h.manager_token().await;
    assert_eq!(
        get(manager.clone(), jkt.clone(), oid.to_owned())
            .await
            .status,
        StatusCode::OK
    );
    get(manager.clone(), jkt.clone(), new_id())
        .await
        .assert_problem(404, "not_found");

    // Lists: the own session for the cashier; filters and paging for the back office.
    let list = |token: String, query: String| async move {
        let device = if token == *cici_token {
            &h.bdg_device
        } else {
            &h.jkt_device
        };
        get_as(
            h,
            (&token, device),
            &format!("/api/v1/cash-movements?{query}"),
        )
        .await
    };
    let r = list(token.clone(), format!("cashier_session_id={session}")).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["items"].as_array().unwrap().len(), 3);
    list(token.clone(), String::new())
        .await
        .assert_problem(403, "permission_denied");
    list(cici, format!("cashier_session_id={session}"))
        .await
        .assert_problem(403, "permission_denied");
    let branch = h.jkt.id;
    let r = list(
        manager.clone(),
        format!("branch_id={branch}&kind=out&limit=1"),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["items"][0]["id"], big["id"]);
    let next = r.body["next_before_id"].as_str().unwrap().to_owned();
    let r = list(
        manager.clone(),
        format!("branch_id={branch}&kind=out&before_id={next}"),
    )
    .await;
    let rest: Vec<&Value> = r.body["items"].as_array().unwrap().iter().collect();
    assert_eq!(rest.len(), 2, "{}", r.body);
    assert_eq!(r.body["next_before_id"], Value::Null);
    let r = list(
        manager.clone(),
        format!("branch_id={branch}&missing_nota=true"),
    )
    .await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 1, "{}", r.body);
    let r = list(
        manager.clone(),
        format!(
            "branch_id={branch}&reason_id={PARKING}&register_id={}",
            h.jkt_register.id
        ),
    )
    .await;
    assert_eq!(r.body["items"].as_array().unwrap().len(), 2, "{}", r.body);
    let r = list(
        manager.clone(),
        format!(
            "branch_id={branch}&from={}&until={}",
            h.now().unix_millis() + 1,
            h.now().unix_millis() + 2
        ),
    )
    .await;
    assert_eq!(r.body["items"], json!([]));
    list(manager.clone(), format!("branch_id={branch}&kind=sideways"))
        .await
        .assert_problem(422, "invalid_field");
    list(manager.clone(), format!("branch_id={branch}&limit=0"))
        .await
        .assert_problem(422, "invalid_field");
    // The manager's assignment is JKT01 only: no list without a branch.
    list(manager, String::new())
        .await
        .assert_problem(403, "permission_denied");
    let owner = h.owner_token().await;
    let r = list(owner, "kind=drop".to_owned()).await;
    assert_eq!(r.body["items"], json!([]));
}

async fn cash_reasons_are_hq_master_data(h: &Harness) {
    let owner = h.owner_token().await;
    // Registers read the active reasons with the device token alone.
    let r = h
        .send(
            Method::GET,
            "/api/v1/cash-reasons",
            &[("x-register-token", &h.jkt_device)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let names: Vec<&str> = r
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 6, "{names:?}");
    assert_eq!(r.body[0]["direction"], "in");
    assert_eq!(r.body[1]["note_required"], true);

    let id = new_id();
    let uri = format!("/api/v1/cash-reasons/{id}");
    let body = json!({ "name": "  Galon air ", "direction": "out", "sort": 40 });
    let r = h.call(&owner, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        (r.body["name"].as_str(), r.body["active"].as_bool()),
        (Some("Galon air"), Some(true))
    );
    let again = h.call(&owner, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!((again.status, &again.body), (StatusCode::OK, &r.body));
    for (bad, field) in [
        (
            json!({ "name": "Galon air", "direction": "in" }),
            "direction",
        ),
        (
            json!({ "name": "Galon air", "direction": "up" }),
            "direction",
        ),
        (json!({ "name": " ", "direction": "out" }), "name"),
    ] {
        let r = h.call(&owner, Method::PUT, &uri, Some(bad)).await;
        r.assert_problem(422, "invalid_field");
        assert_eq!(r.body["field"], field);
    }
    // A manager has no `tender.manage`; a cashier may not list archived reasons.
    let manager = h.manager_token().await;
    h.call(&manager, Method::PUT, &uri, Some(body.clone()))
        .await
        .assert_problem(403, "permission_denied");
    let token = ani(h).await;
    h.at_register(
        (&token, &h.jkt_device),
        Method::GET,
        "/api/v1/cash-reasons?archived=true",
        &[],
        None,
    )
    .await
    .assert_problem(403, "permission_denied");
    h.send(
        Method::GET,
        "/api/v1/cash-reasons?archived=true",
        &[("x-register-token", &h.jkt_device)],
        None,
    )
    .await
    .assert_problem(403, "permission_denied");

    // Case 18: a movement with the reason, then the reason is archived.
    let at = (token.as_str(), h.jkt_device.as_str());
    open(h, at, 0).await;
    let old = move_cash(h, at, cash("out", 8_000, Some(&id))).await;
    let r = h.call(&owner, Method::DELETE, &uri, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["archived"], true);
    assert_eq!(
        h.call(&owner, Method::DELETE, &uri, None).await.status,
        StatusCode::OK
    );
    h.call(&owner, Method::PUT, &uri, Some(body))
        .await
        .assert_problem(409, "archived");
    h.call(
        &owner,
        Method::DELETE,
        &format!("/api/v1/cash-reasons/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    let r = put_cash(h, at, &new_id(), &cash("out", 1_000, Some(&id)), None).await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "reason");
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/cash-movements/{}", old["id"].as_str().unwrap()),
            &[],
            None,
        )
        .await;
    assert_eq!(r.body["reason_name"], "Galon air");
    let slip = h
        .at_register(
            at,
            Method::GET,
            &format!(
                "/api/v1/cash-movements/{}/slip",
                old["id"].as_str().unwrap()
            ),
            &[],
            None,
        )
        .await;
    assert_eq!(slip.status, StatusCode::OK, "{}", slip.body);
    assert!(
        slip.body["text"].as_str().unwrap().contains("Galon air"),
        "{}",
        slip.body["text"]
    );
    // The active list leaves it out; `archived=true` shows it.
    let active = h
        .call(&owner, Method::GET, "/api/v1/cash-reasons", None)
        .await;
    assert_eq!(active.body.as_array().unwrap().len(), 6);
    let all = h
        .call(
            &owner,
            Method::GET,
            "/api/v1/cash-reasons?archived=true",
            None,
        )
        .await;
    assert_eq!(all.body.as_array().unwrap().len(), 7);

    // Case 27: a branch node holds a read-only replica.
    let branch_app = router(h.state(NodeRole::Branch));
    let bearer = format!("Bearer {owner}");
    for method in [Method::PUT, Method::DELETE] {
        let r = Harness::send_to(
            branch_app.clone(),
            method,
            &format!("/api/v1/cash-reasons/{}", new_id()),
            &[("authorization", &bearer)],
            Some(json!({ "name": "X", "direction": "in" })),
        )
        .await;
        r.assert_problem(403, "read_only_replica");
    }
}

async fn close_report_counts_cash_and_keeps_the_float(h: &Harness) {
    let owner = h.owner_token().await;
    let budi = h.pin_token(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let at = (budi.as_str(), h.jkt_device.as_str());
    let float = |token: String| async move {
        h.at_register(
            (&token, &h.jkt_device),
            Method::GET,
            "/api/v1/registers/me/float",
            &[],
            None,
        )
        .await
    };
    // A register's first session: no proposal, so no float difference.
    let r = float(budi.clone()).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        (
            r.body["proposed_float"].as_i64(),
            r.body["has_proposal"].as_bool()
        ),
        (Some(0), Some(false))
    );
    let session = open(h, at, 200_000).await;
    // Case 19: a scripted session.
    move_cash(h, at, cash("in", 400_000, Some(TOP_UP))).await;
    move_cash(h, at, cash("out", 30_000, Some(PARKING))).await;
    move_cash(h, at, cash("drop", 300_000, None)).await;
    let customer = crate::sales::customer(h, &owner, "Bu Siti").await;
    crate::customers::add_balance(h, &customer, BalanceEntryKind::ChangeSaved, 10_000).await;
    let r = h
        .at_register(
            at,
            Method::POST,
            &format!("/api/v1/customers/{customer}/balance/payout"),
            &[],
            Some(json!({ "amount": 10_000 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    for _ in 0..2 {
        let r = h
            .at_register(
                at,
                Method::POST,
                "/api/v1/drawer/open",
                &[],
                Some(json!({})),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    }
    let x = h
        .at_register(
            at,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}/x-report"),
            &[],
            None,
        )
        .await;
    assert_eq!(x.status, StatusCode::OK, "{}", x.body);
    assert_eq!(x.body["expected_cash"], 260_000);
    // The float kept is at most the count.
    let close_uri = format!("/api/v1/cashier-sessions/{session}/close");
    let r = h
        .at_register(
            at,
            Method::POST,
            &close_uri,
            &[],
            Some(json!({ "counted_cash": 250_000, "float_kept": 250_001 })),
        )
        .await;
    r.assert_problem(422, "invalid_field");
    assert_eq!(r.body["field"], "float_kept");
    let report = close(
        h,
        at,
        &session,
        json!({ "counted_cash": 250_000, "float_kept": 100_000 }),
    )
    .await;
    // Both blocks give the same expected cash; the 2.1 block's cash out has drops and payouts.
    assert_eq!(report["expected_cash"], 260_000);
    assert_eq!(report["sales"]["expected_cash"], 260_000);
    assert_eq!(report["cash_out"], 340_000);
    assert_eq!(report["over_short"], -10_000);
    let s = &report["sales"];
    assert_eq!(
        (
            s["cash_in"].as_i64(),
            s["cash_out"].as_i64(),
            s["drops"].as_i64(),
            s["payouts"].as_i64(),
            s["opening_cash"].as_i64()
        ),
        (
            Some(400_000),
            Some(30_000),
            Some(300_000),
            Some(10_000),
            Some(200_000)
        )
    );
    let c = &report["cash"];
    assert_eq!(c["cash_in"], json!({ "count": 1, "amount": 400_000 }));
    assert_eq!(c["cash_out"], json!({ "count": 1, "amount": 30_000 }));
    assert_eq!(c["drops"], json!({ "count": 1, "amount": 300_000 }));
    assert_eq!(c["reversals"], json!({ "count": 0, "amount": 0 }));
    assert_eq!(c["reasons"].as_array().unwrap().len(), 2);
    assert_eq!(c["reasons"][0]["reason_id"], TOP_UP);
    assert_eq!(c["reasons"][1]["name"], "Ongkos kirim / parkir");
    assert_eq!(
        (
            c["drawer_opens"].as_u64(),
            c["x_reports"].as_u64(),
            c["missing_nota"].as_u64()
        ),
        (Some(2), Some(1), Some(0))
    );
    assert_eq!(
        report["float"],
        json!({ "float_kept": 100_000, "to_safe": 150_000, "opening_proposed": null, "float_difference": 0 })
    );
    assert_eq!(report["session"]["float_kept"], 100_000);
    // The stored report reads back unchanged.
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}"),
            None,
        )
        .await;
    assert_eq!(r.body["close_report"], report);
    // Renaming or re-sorting a reason after the close changes neither the stored report nor its
    // reprint: the close stored the reasons as they were (§4).
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/cash-reasons/{PARKING}"),
            Some(json!({ "name": "Transport", "direction": "out", "sort": 1 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}"),
            None,
        )
        .await;
    assert_eq!(r.body["close_report"], report);
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}/report/text?width=42"),
            None,
        )
        .await;
    let text = r.body["text"].as_str().unwrap();
    assert!(
        text.contains("parkir") && !text.contains("Transport"),
        "{text}"
    );
    // Its text.
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}/report/text?copy=true&width=42"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let text = r.body["text"].as_str().unwrap();
    for part in [
        "CLOSE REPORT",
        "COPY",
        "Kept as float",
        "To the safe",
        "Safe drops (1)",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/cashier-sessions/{session}/report/text?lang=id"),
            None,
        )
        .await;
    assert!(
        r.body["text"]
            .as_str()
            .unwrap()
            .contains("LAPORAN TUTUP KAS")
    );

    // The next open on the register proposes the float kept; another amount is stored.
    let ani = ani(h).await;
    let r = float(ani.clone()).await;
    assert_eq!(
        (
            r.body["proposed_float"].as_i64(),
            r.body["has_proposal"].as_bool()
        ),
        (Some(100_000), Some(true))
    );
    let ani_at = (ani.as_str(), h.jkt_device.as_str());
    let next = open(h, ani_at, 150_000).await;
    let current = h
        .at_register(
            ani_at,
            Method::GET,
            "/api/v1/cashier-sessions/current",
            &[],
            None,
        )
        .await;
    assert_eq!(current.body["opening_proposed"], 100_000);
    let r = get_as(
        h,
        ani_at,
        &format!("/api/v1/cashier-sessions/{next}/report/text"),
    )
    .await;
    r.assert_problem(404, "not_found");
    let opened = h.audit_count("cashier_session.open").await;
    assert!(opened >= 2);
    // A force close takes the float too; the default is the opening cash.
    let manager = h.manager_token().await;
    let r = h
        .call(
            &manager,
            Method::POST,
            &format!("/api/v1/cashier-sessions/{next}/force-close"),
            Some(json!({ "counted_cash": 120_000 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.body["float"],
        json!({ "float_kept": 120_000, "to_safe": 0, "opening_proposed": 100_000, "float_difference": 50_000 })
    );
    // Another cashier may not read the text.
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    get_as(
        h,
        (&cici, &h.bdg_device),
        &format!("/api/v1/cashier-sessions/{session}/report/text"),
    )
    .await
    .assert_problem(403, "permission_denied");
    h.call(
        &owner,
        Method::GET,
        &format!("/api/v1/cashier-sessions/{}/report/text", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

async fn x_report_rules(h: &Harness) {
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 75_000).await;
    move_cash(h, at, cash("in", 5_000, Some(TOP_UP))).await;
    let uri = format!("/api/v1/cashier-sessions/{session}/x-report");
    let text_uri = format!("{uri}/text?width=32");
    // Case 20: a cashier needs a manager.
    let r = h.at_register(at, Method::GET, &uri, &[], None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "session.view_expected");
    assert_eq!(h.audit_count("session.x_report").await, 0);
    let t = approve(h, at, "session.view_expected", Value::Null, &session).await;
    let r = h
        .at_register(at, Method::GET, &uri, &[("x-override", &t)], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["expected_cash"], 80_000);
    assert_eq!(r.body["approved_by"], h.budi.id.to_string());
    assert_eq!(r.body["cash"]["x_reports"], 1);
    // Case 21: never a count or an over/short.
    let raw = r.body.to_string();
    assert!(
        !raw.contains("counted_cash") && !raw.contains("over_short"),
        "{raw}"
    );
    let owner = h.owner_token().await;
    let audit = h
        .call(
            &owner,
            Method::GET,
            "/api/v1/audit?action=session.x_report",
            None,
        )
        .await;
    let items = audit.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["target"], format!("cashier_session:{session}"));
    assert_eq!(items[0]["after"]["approved_by"], h.budi.id.to_string());
    // The text, with another token.
    let t = approve(h, at, "session.view_expected", Value::Null, &session).await;
    let r = h
        .at_register(at, Method::GET, &text_uri, &[("x-override", &t)], None)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(
        r.body["text"].as_str().unwrap().contains("X REPORT"),
        "{}",
        r.body["text"]
    );
    h.at_register(at, Method::GET, &format!("{uri}/text?width=40"), &[], None)
        .await
        .assert_problem(422, "invalid_field");
    // The back office: `report.sales` + `session.view_expected` at the branch.
    let r = h.call(&owner, Method::GET, &uri, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["cash"]["x_reports"], 3);
    let manager = h.manager_token().await;
    assert_eq!(
        h.call(&manager, Method::GET, &uri, None).await.status,
        StatusCode::OK
    );
    let cici = h.pin_token(&h.bdg_device, &h.cici, ANI_PIN).await;
    get_as(h, (&cici, &h.bdg_device), &uri)
        .await
        .assert_problem(403, "permission_denied");
    h.call(
        &owner,
        Method::GET,
        &format!("/api/v1/cashier-sessions/{}/x-report", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    // Closed: read the close report instead; it counts every X.
    let report = close(h, at, &session, json!({ "counted_cash": 80_000 })).await;
    assert_eq!(report["cash"]["x_reports"], 4);
    h.call(&owner, Method::GET, &uri, None)
        .await
        .assert_problem(409, "session_closed");
}

async fn slips_and_reprints(h: &Harness) {
    let token = ani(h).await;
    let at = (token.as_str(), h.jkt_device.as_str());
    let session = open(h, at, 0).await;
    let out = move_cash(
        h,
        at,
        json!({ "kind": "out", "amount": 20_000, "reason_id": PARKING, "note": "parkir truk" }),
    )
    .await;
    let id = out["id"].as_str().unwrap().to_owned();
    let slip = |token: String, query: &'static str, override_token: Option<String>| {
        let id = id.clone();
        async move {
            let headers: Vec<(&str, &str)> = override_token
                .as_deref()
                .map(|t| ("x-override", t))
                .into_iter()
                .collect();
            h.at_register(
                (&token, &h.jkt_device),
                Method::GET,
                &format!("/api/v1/cash-movements/{id}/slip{query}"),
                &headers,
                None,
            )
            .await
        }
    };
    let r = slip(token.clone(), "", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let text = r.body["text"].as_str().unwrap();
    for part in [
        "CASH OUT",
        out["number"].as_str().unwrap(),
        "parkir truk",
        "Received by",
        "Rp 20.000",
    ] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(r.body["copy"], false);
    assert_eq!(r.body["large"].as_array().unwrap().len(), 1);
    let r = slip(token.clone(), "?lang=id&copy=true", None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body["text"].as_str().unwrap().contains("SALINAN"));
    // Case 29: free in the same open session, audited.
    assert_eq!(h.audit_count("cash_movement.reprint").await, 1);
    // A reversal prints REVERSAL and the original's number.
    let back = move_cash(h, at, json!({ "reverses": id, "note": "salah" })).await;
    let r = h
        .at_register(
            at,
            Method::GET,
            &format!(
                "/api/v1/cash-movements/{}/slip",
                back["id"].as_str().unwrap()
            ),
            &[],
            None,
        )
        .await;
    assert!(
        r.body["text"].as_str().unwrap().contains("REVERSAL"),
        "{}",
        r.body["text"]
    );
    // From the next session the copy needs `sale.reprint`.
    close(h, at, &session, json!({ "counted_cash": 0 })).await;
    open(h, at, 0).await;
    let r = slip(token.clone(), "?copy=true", None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "sale.reprint");
    let t = approve(h, at, "sale.reprint", Value::Null, &id).await;
    let r = slip(token.clone(), "?copy=true", Some(t)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(h.audit_count("cash_movement.reprint").await, 2);
    let owner = h.owner_token().await;
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/cash-movements/{id}/slip?copy=true&width=48"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(
        r.body["lines"]
            .as_array()
            .unwrap()
            .iter()
            .all(|l| l.as_str().unwrap().chars().count() <= 48)
    );
    assert_eq!(h.audit_count("cash_movement.reprint").await, 3);

    // The payout slip.
    let customer = crate::sales::customer(h, &owner, "Pak Joko").await;
    crate::customers::add_balance(h, &customer, BalanceEntryKind::ChangeSaved, 7_000).await;
    // The register is taken by ani's session: budi pays out from the second register.
    let r2 = h.second_jkt_register().await;
    let budi2 = h.pin_token(&r2, &h.budi, BUDI_PIN).await;
    let budi2_at = (budi2.as_str(), r2.as_str());
    open(h, budi2_at, 0).await;
    let r = h
        .at_register(
            budi2_at,
            Method::POST,
            &format!("/api/v1/customers/{customer}/balance/payout"),
            &[],
            Some(json!({ "amount": 5_000 })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let entry = r.body["entry"]["id"].as_str().unwrap().to_owned();
    let uri = format!("/api/v1/customers/{customer}/balance/payouts/{entry}/slip");
    let r = h
        .at_register(
            budi2_at,
            Method::GET,
            &format!("{uri}?copy=true"),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let text = r.body["text"].as_str().unwrap();
    for part in ["BALANCE PAYOUT", "Pak Joko", "Rp 5.000", "Balance", "2.000"] {
        assert!(text.contains(part), "{part}: {text}");
    }
    assert_eq!(h.audit_count("customer.payout_reprint").await, 1);
    // From another session a cashier's copy needs `sale.reprint`.
    h.at_register(at, Method::GET, &format!("{uri}?copy=true"), &[], None)
        .await
        .assert_problem(403, "needs_override");
    assert_eq!(get_as(h, at, &uri).await.status, StatusCode::OK);
    h.call(
        &owner,
        Method::GET,
        &format!(
            "/api/v1/customers/{customer}/balance/payouts/{}/slip",
            new_id()
        ),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(
        &owner,
        Method::GET,
        &format!(
            "/api/v1/customers/{}/balance/payouts/{entry}/slip",
            new_id()
        ),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}
