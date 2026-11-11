//! Delegation over HTTP (spec 2.1 §3; plan Phase 6 privilege-escalation fix): nobody assigns,
//! unassigns, creates or edits a role that grants more than they hold at that scope.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_core::ids::RoleId;
use sultan_db::seed;

use crate::common::{BUDI_PASSWORD, Harness, Response, api_tests, new_id};

api_tests!(
    user_writes_cannot_hand_out_more_than_the_caller_holds,
    user_writes_need_to_cover_the_target_user,
    role_writes_cannot_grant_more_than_the_caller_holds,
    branch_scoped_admin_assigns_only_at_their_branch,
);

/// "HR": manages users and roles, and holds the Cashier role's grants with higher limits — but not
/// the Manager's or the Owner's.
fn hr_role() -> Value {
    json!({
        "name": "HR",
        "grants": [
            { "permission": "user.manage" },
            { "permission": "role.manage" },
            { "permission": "sale.create" },
            { "permission": "sale.discount", "limit": 2000 },
            { "permission": "sale.cart_discount", "limit": 2000 },
            { "permission": "sale.void_line" },
            { "permission": "refund.with_receipt" },
            { "permission": "cash.in" },
            { "permission": "session.open_close" },
            { "permission": "customer.create" },
        ],
    })
}

/// Creates the HR role (as the Owner) and a user holding it at `scope`; returns the user's token.
async fn hr_admin(h: &Harness, scope: BranchScope) -> String {
    let owner = h.owner_token().await;
    let id: RoleId = new_id().parse().unwrap();
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/roles/{id}"),
            Some(hr_role()),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let hr = h.add_user("hr", id, scope).await;
    h.password_token(&hr.username, BUDI_PASSWORD).await
}

fn user_body(username: &str, assignments: &Value) -> Value {
    json!({ "username": username, "display_name": username, "assignments": assignments })
}

fn at(role: RoleId, branch: Option<&str>) -> Value {
    json!({ "role_id": role.to_string(), "branch_id": branch })
}

#[track_caller]
fn assert_escalation(r: &Response, permission: &str) {
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    assert_eq!(r.body["permission"], permission, "{}", r.body);
}

async fn user_writes_cannot_hand_out_more_than_the_caller_holds(h: &Harness) {
    let hr = hr_admin(h, BranchScope::All).await;
    let jkt_id = h.jkt.id.to_string();
    let jkt = Some(jkt_id.as_str());
    let put = |id: String, body: Value| {
        let hr = hr.clone();
        async move {
            h.call(&hr, Method::PUT, &format!("/api/v1/users/{id}"), Some(body))
                .await
        }
    };

    // Covered: the Cashier role, at a branch or everywhere.
    let dewi = new_id();
    let r = put(
        dewi.clone(),
        user_body("dewi", &json!([at(seed::CASHIER_ROLE, jkt)])),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = put(
        dewi.clone(),
        user_body("dewi", &json!([at(seed::CASHIER_ROLE, None)])),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);

    #[rustfmt::skip]
    let cases = [
        ("Owner for all branches", user_body("x1", &json!([at(seed::OWNER_ROLE, None)]))),
        ("Owner at one branch", user_body("x2", &json!([at(seed::OWNER_ROLE, jkt)]))),
        ("Manager at one branch", user_body("x3", &json!([at(seed::MANAGER_ROLE, jkt)]))),
    ];
    for (name, body) in cases {
        let r = put(new_id(), body).await;
        assert_eq!(
            (r.status.as_u16(), r.body["reason"].as_str()),
            (403, Some("grant_exceeds_own")),
            "{name}: {}",
            r.body
        );
    }

    // HR cannot promote themselves either.
    let hr_user = sultan_db::users::find_by_username(&h.db, "hr")
        .await
        .unwrap()
        .unwrap();
    let r = put(
        hr_user.id.to_string(),
        json!({ "username": "hr", "display_name": "hr", "assignments": [at(seed::OWNER_ROLE, None)] }),
    )
    .await;
    r.assert_problem(403, "permission_denied");
}

/// Writing an existing user (or unassigning a role) needs the caller to cover the user's roles.
async fn user_writes_need_to_cover_the_target_user(h: &Harness) {
    let hr = hr_admin(h, BranchScope::All).await;
    let jkt_id = h.jkt.id.to_string();
    let jkt = Some(jkt_id.as_str());
    let put = |id: String, body: Value| {
        let hr = hr.clone();
        async move {
            h.call(&hr, Method::PUT, &format!("/api/v1/users/{id}"), Some(body))
                .await
        }
    };

    // Nobody can take over or lock out the Owner: password reset, deactivation, archive.
    let owner_uri = format!("/api/v1/users/{}", h.owner.id);
    let r = h
        .call(
            &hr,
            Method::PUT,
            &owner_uri,
            Some(
                json!({ "username": "owner", "display_name": "OWNER", "password": "taken-over-1" }),
            ),
        )
        .await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    let r = h.call(&hr, Method::DELETE, &owner_uri, None).await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);

    // Nor unassign a role they do not cover: the Owner creates Dewi with the Manager role at JKT01 …
    let owner = h.owner_token().await;
    let dewi = new_id();
    let both = json!([at(seed::CASHIER_ROLE, None), at(seed::MANAGER_ROLE, jkt)]);
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/users/{dewi}"),
            Some(user_body("dewi", &both)),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    // … then HR may neither remove it nor edit Dewi any more.
    let r = put(
        dewi.clone(),
        user_body("dewi", &json!([at(seed::CASHIER_ROLE, None)])),
    )
    .await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    let r = put(
        dewi.clone(),
        json!({ "username": "dewi", "display_name": "Dewi S" }),
    )
    .await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    // A replay of the current state changes nothing, so it is still fine.
    let r = put(
        dewi.clone(),
        json!({ "username": "dewi", "display_name": "dewi" }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // The Owner may do anything.
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/users/{dewi}"),
            Some(user_body("dewi", &json!([at(seed::OWNER_ROLE, None)]))),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(h.audit_count("user.save").await, 2);
}

async fn role_writes_cannot_grant_more_than_the_caller_holds(h: &Harness) {
    let hr = hr_admin(h, BranchScope::All).await;
    let put = |id: String, body: Value| {
        let hr = hr.clone();
        async move {
            h.call(&hr, Method::PUT, &format!("/api/v1/roles/{id}"), Some(body))
                .await
        }
    };
    let role = |grants: Value| json!({ "name": "Senior cashier", "grants": grants });

    let senior = new_id();
    let r = put(
        senior.clone(),
        role(json!([{ "permission": "sale.discount", "limit": 1500 }, { "permission": "sale.create" }])),
    )
    .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);

    let r = put(
        senior.clone(),
        role(json!([{ "permission": "sale.discount", "limit": 2500 }])),
    )
    .await;
    assert_escalation(&r, "sale.discount");
    assert_eq!(r.body["limit"], 2500);
    // Unlimited is more than any limit.
    let r = put(
        senior.clone(),
        role(json!([{ "permission": "sale.discount" }])),
    )
    .await;
    assert_escalation(&r, "sale.discount");
    let r = put(senior.clone(), role(json!([{ "permission": "sale.void" }]))).await;
    assert_escalation(&r, "sale.void");
    let r = put(new_id(), role(json!([{ "permission": "org.manage" }]))).await;
    assert_escalation(&r, "org.manage");

    // Editing or archiving a role that already grants more is refused too, even to shrink it.
    let manager = seed::MANAGER_ROLE.to_string();
    let r = put(manager.clone(), json!({ "name": "Manager", "grants": [] })).await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    let r = put(
        seed::OWNER_ROLE.to_string(),
        json!({ "name": "Boss", "grants": [] }),
    )
    .await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    let r = h
        .call(
            &hr,
            Method::DELETE,
            &format!("/api/v1/roles/{manager}"),
            None,
        )
        .await;
    assert_eq!(r.body["reason"], "grant_exceeds_own", "{}", r.body);
    // Their own covered role can be archived.
    let r = h
        .call(
            &hr,
            Method::DELETE,
            &format!("/api/v1/roles/{senior}"),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(h.audit_count("role.save").await, 2);
}

async fn branch_scoped_admin_assigns_only_at_their_branch(h: &Harness) {
    let hr = hr_admin(h, BranchScope::Branch(h.jkt.id)).await;
    let put = |body: Value| {
        let hr = hr.clone();
        async move {
            h.call(
                &hr,
                Method::PUT,
                &format!("/api/v1/users/{}", new_id()),
                Some(body),
            )
            .await
        }
    };
    let jkt_id = h.jkt.id.to_string();
    let jkt = Some(jkt_id.as_str());
    let bdg_id = h.bdg.id.to_string();
    let bdg = Some(bdg_id.as_str());
    let r = put(user_body("j1", &json!([at(seed::CASHIER_ROLE, jkt)]))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = put(user_body("b1", &json!([at(seed::CASHIER_ROLE, bdg)]))).await;
    assert_escalation(&r, "cash.in");
    let r = put(user_body("a1", &json!([at(seed::CASHIER_ROLE, None)]))).await;
    assert_escalation(&r, "cash.in");
    // Roles apply wherever they are assigned, so a branch-scoped admin cannot write roles that
    // grant anything.
    let r = h
        .call(
            &hr,
            Method::PUT,
            &format!("/api/v1/roles/{}", new_id()),
            Some(json!({ "name": "Tiny", "grants": [{ "permission": "sale.create" }] })),
        )
        .await;
    assert_escalation(&r, "sale.create");
    let r = h
        .call(
            &hr,
            Method::PUT,
            &format!("/api/v1/roles/{}", new_id()),
            Some(json!({ "name": "Empty", "grants": [] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
}
