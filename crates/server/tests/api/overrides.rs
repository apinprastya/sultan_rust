//! Manager override over HTTP (spec 2.1 §6, §9 case 15; plan Phase 6): `POST /overrides` and the
//! `X-Override` guard behind the demo endpoint.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_db::audit::{self, AuditFilter};
use sultan_db::seed;
use sultan_server::overrides::OVERRIDE_TTL_MS;

use crate::common::{ANI_PIN, BUDI_PIN, Harness, Response, api_tests};

api_tests!(
    override_happy_path_and_single_use,
    token_is_bound_to_action_value_context_register_and_time,
    approval_rules,
    approver_pin_failures_count_towards_lockout,
);

const DEMO: &str = "/api/v1/demo/override-protected";
const OVERRIDES: &str = "/api/v1/overrides";

fn discount(bp: i64, context: &str) -> Value {
    json!({ "action": "sale.discount", "value": bp, "context_ref": context })
}

fn approval(action: Value, approver: &str, pin: &str) -> Value {
    let mut body = action;
    body["approver_id"] = approver.into();
    body["approver_pin"] = pin.into();
    body
}

async fn approve(
    h: &Harness,
    at: (&str, &str),
    action: Value,
    approver: &str,
    pin: &str,
) -> Response {
    h.at_register(
        at,
        Method::POST,
        OVERRIDES,
        &[],
        Some(approval(action, approver, pin)),
    )
    .await
}

async fn demo(h: &Harness, at: (&str, &str), action: Value, token: Option<&str>) -> Response {
    let headers: Vec<(&str, &str)> = token.map(|t| ("x-override", t)).into_iter().collect();
    h.at_register(at, Method::POST, DEMO, &headers, Some(action))
        .await
}

async fn override_happy_path_and_single_use(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let budi_id = h.budi.id.to_string();

    // Within the cashier's own 10% limit: no override needed.
    let r = demo(h, at, discount(1_000, "cart-1"), None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["approved_by"], Value::Null);

    // Case 1: 15% needs an override; the problem tells the register the cashier's limit.
    let r = demo(h, at, discount(1_500, "cart-1"), None).await;
    r.assert_problem(403, "needs_override");
    assert_eq!(r.body["permission"], "sale.discount");
    assert_eq!(r.body["limit"], 1_000);

    // The manager (20%) approves at the same register.
    let issued = approve(h, at, discount(1_500, "cart-1"), &budi_id, BUDI_PIN).await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.body);
    let token = issued.body["token"].as_str().unwrap().to_owned();
    assert_eq!(token.len(), 43);
    assert_eq!(issued.body["action"], "sale.discount");
    assert_eq!(issued.body["value"], 1_500);
    assert_eq!(issued.body["context_ref"], "cart-1");
    assert_eq!(issued.body["requester_id"], h.ani.id.to_string());
    assert_eq!(issued.body["approver_id"], budi_id);
    assert_eq!(
        issued.body["expires_at"],
        h.now().unix_millis() + OVERRIDE_TTL_MS
    );

    let r = demo(h, at, discount(1_500, "cart-1"), Some(&token)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["approved_by"], budi_id);

    // Single use.
    demo(h, at, discount(1_500, "cart-1"), Some(&token))
        .await
        .assert_problem(403, "invalid_override");

    // A plain (unlimited) action the cashier lacks: `sale.void`.
    let void = json!({ "action": "sale.void", "context_ref": "sale-9" });
    demo(h, at, void.clone(), None)
        .await
        .assert_problem(403, "needs_override");
    let issued = approve(h, at, void.clone(), &budi_id, BUDI_PIN).await;
    assert_eq!(issued.status, StatusCode::CREATED, "{}", issued.body);
    assert_eq!(issued.body["value"], Value::Null);
    let token = issued.body["token"].as_str().unwrap();
    assert_eq!(demo(h, at, void, Some(token)).await.status, StatusCode::OK);

    // Audit: both approvals and both uses, with requester, approver, action, value, register.
    assert_eq!(h.audit_count("override.approve").await, 2);
    assert_eq!(h.audit_count("override.use").await, 2);
    let uses = audit::query(
        &h.db,
        &AuditFilter {
            action: Some("override.use".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let first = uses.last().unwrap();
    assert_eq!(first.user, Some(h.ani.id));
    assert_eq!(first.register, Some(h.jkt_register.id));
    assert_eq!(first.target.as_deref(), Some("context:cart-1"));
    let after = first.after.as_ref().unwrap();
    assert_eq!(after["approver_id"], budi_id);
    assert_eq!(after["requester_id"], h.ani.id.to_string());
    assert_eq!(
        (after["action"].as_str(), after["value"].as_i64()),
        (Some("sale.discount"), Some(1_500))
    );
    let approvals = audit::query(
        &h.db,
        &AuditFilter {
            action: Some("override.approve".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(approvals.last().unwrap().user, Some(h.budi.id));
}

/// Spec 2.1 §9 case 15.
async fn token_is_bound_to_action_value_context_register_and_time(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let budi_id = h.budi.id.to_string();
    let issued = approve(h, at, discount(1_500, "cart-1"), &budi_id, BUDI_PIN).await;
    let token = issued.body["token"].as_str().unwrap().to_owned();

    let r2 = h.second_jkt_register().await;
    let ani_r2 = h.pin_token(&r2, &h.ani, ANI_PIN).await;
    let other_user = h
        .add_user("eka", seed::CASHIER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let eka = h.pin_token(&h.jkt_device, &other_user, ANI_PIN).await;
    #[rustfmt::skip]
    let cases = [
        ("wrong value", at, discount(1_400, "cart-1")),
        ("wrong context", at, discount(1_500, "cart-2")),
        ("wrong action", at,
            json!({ "action": "sale.cart_discount", "value": 1_500, "context_ref": "cart-1" })),
        ("wrong register", (ani_r2.as_str(), r2.as_str()), discount(1_500, "cart-1")),
        ("garbage token", at, discount(1_500, "cart-1")),
    ];
    for (name, at, action) in cases {
        let presented = if name == "garbage token" {
            "nope"
        } else {
            token.as_str()
        };
        let r = demo(h, at, action, Some(presented)).await;
        assert_eq!(
            (r.status.as_u16(), r.body["code"].as_str()),
            (403, Some("invalid_override")),
            "{name}: {}",
            r.body
        );
    }
    // A mismatch does not use the token up: the right request still works.
    let r = demo(h, at, discount(1_500, "cart-1"), Some(&token)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    // Another cashier presenting someone else's token is refused; like any mismatch, this does not
    // use the token up (the guard's transaction rolls back), so its requester can still use it.
    let issued = approve(h, at, discount(1_500, "cart-3"), &budi_id, BUDI_PIN).await;
    let token = issued.body["token"].as_str().unwrap().to_owned();
    demo(
        h,
        (&eka, &h.jkt_device),
        discount(1_500, "cart-3"),
        Some(&token),
    )
    .await
    .assert_problem(403, "invalid_override");
    let r = demo(h, at, discount(1_500, "cart-3"), Some(&token)).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    // Expiry after 2 minutes.
    let issued = approve(h, at, discount(1_500, "cart-4"), &budi_id, BUDI_PIN).await;
    let token = issued.body["token"].as_str().unwrap().to_owned();
    h.clock.advance(OVERRIDE_TTL_MS);
    // The cashier's session is still fresh (idle expiry is 30 minutes).
    demo(h, at, discount(1_500, "cart-4"), Some(&token))
        .await
        .assert_problem(403, "invalid_override");
    assert_eq!(h.audit_count("override.use").await, 2);

    // An X-Override on an action the caller may do anyway is ignored and not used up.
    let issued = approve(h, at, discount(500, "cart-5"), &budi_id, BUDI_PIN).await;
    let token = issued.body["token"].as_str().unwrap().to_owned();
    let r = demo(h, at, discount(500, "cart-5"), Some(&token)).await;
    assert_eq!(r.body["approved_by"], Value::Null);
}

async fn approval_rules(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let budi_id = h.budi.id.to_string();
    let eka = h
        .add_user("eka", seed::CASHIER_ROLE, BranchScope::Branch(h.jkt.id))
        .await
        .id
        .to_string();
    let ani_id = h.ani.id.to_string();
    let cici_id = h.cici.id.to_string();
    let owner_id = h.owner.id.to_string();
    let void = || json!({ "action": "sale.void", "context_ref": "s" });
    #[rustfmt::skip]
    let cases: [(&str, Value, &str, &str, u16, &str); 11] = [
        // Case 3.
        ("self-approval", discount(1_500, "c"), &ani_id, ANI_PIN, 403, "self_approval"),
        // Case 2: the manager's 20% does not cover 25%.
        ("approver limit too low", discount(2_500, "c"), &budi_id, BUDI_PIN, 403, "override_limit_exceeded"),
        ("approver lacks the permission", void(), &eka, ANI_PIN, 403, "permission_denied"),
        ("not overridable", json!({ "action": "user.manage", "context_ref": "c" }), &budi_id, BUDI_PIN,
            403, "permission_denied"),
        ("approver not at this branch", discount(1_500, "c"), &cici_id, ANI_PIN, 401, "invalid_credentials"),
        ("approver without PIN", discount(1_500, "c"), &owner_id, "1234", 401, "invalid_credentials"),
        ("unknown approver", discount(1_500, "c"), &crate::common::new_id(), "1234", 401, "invalid_credentials"),
        ("wrong PIN", discount(1_500, "c"), &budi_id, "0000", 401, "invalid_credentials"),
        ("unknown action", json!({ "action": "sale.nope", "context_ref": "c" }), &budi_id, BUDI_PIN,
            422, "invalid_field"),
        ("empty context", discount(1_500, " "), &budi_id, BUDI_PIN, 422, "invalid_field"),
        ("bad approver id", discount(1_500, "c"), "nope", BUDI_PIN, 400, "invalid_body"),
    ];
    for (name, action, approver, pin, status, code) in cases {
        let r = approve(h, at, action, approver, pin).await;
        assert_eq!(
            (r.status.as_u16(), r.body["code"].as_str()),
            (status, Some(code)),
            "{name}: {}",
            r.body
        );
    }
    assert_eq!(h.audit_count("override.approve").await, 0);
    assert_eq!(h.audit_count("override.failed").await, 1);

    // The requester must be at a register.
    let back_office = h.manager_token().await;
    h.call(
        &back_office,
        Method::POST,
        OVERRIDES,
        Some(approval(discount(1_500, "c"), &budi_id, BUDI_PIN)),
    )
    .await
    .assert_problem(403, "register_session_required");
    h.call(&back_office, Method::POST, DEMO, Some(discount(1, "c")))
        .await
        .assert_problem(403, "register_session_required");
    // The demo endpoint refuses non-overridable actions the caller lacks.
    demo(
        h,
        at,
        json!({ "action": "user.manage", "context_ref": "c" }),
        None,
    )
    .await
    .assert_problem(403, "permission_denied");
    demo(
        h,
        at,
        json!({ "action": "sale.void", "value": 1, "context_ref": "c" }),
        None,
    )
    .await
    .assert_problem(422, "invalid_field");
}

/// Spec 2.1 §6: a wrong approver PIN counts towards the approver's lockout.
async fn approver_pin_failures_count_towards_lockout(h: &Harness) {
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let at = (ani.as_str(), h.jkt_device.as_str());
    let budi_id = h.budi.id.to_string();
    for _ in 0..4 {
        approve(h, at, discount(1_500, "c"), &budi_id, "0000")
            .await
            .assert_problem(401, "invalid_credentials");
    }
    approve(h, at, discount(1_500, "c"), &budi_id, "0000")
        .await
        .assert_problem(423, "locked_out");
    // Locked: even the right PIN is refused, and so is the manager's own login.
    approve(h, at, discount(1_500, "c"), &budi_id, BUDI_PIN)
        .await
        .assert_problem(423, "locked_out");
    h.pin_login(&h.jkt_device, &h.budi, BUDI_PIN)
        .await
        .assert_problem(423, "locked_out");
    assert_eq!(h.audit_count("override.failed").await, 4);
    assert_eq!(h.audit_count("override.locked_out").await, 1);
}
