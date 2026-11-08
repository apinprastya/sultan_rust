//! Roles and the permission catalog over HTTP (spec 2.1 §3, §8, §9 cases 6 and 17).

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::Permission;
use sultan_db::{roles, seed};

use crate::common::{Harness, api_tests, new_id};

api_tests!(
    role_crud_and_replay,
    role_validation,
    role_endpoints_need_role_manage,
    owner_role_cannot_be_emptied_or_archived,
    role_changes_apply_on_the_next_request,
    permission_catalog,
);

fn supervisor() -> Value {
    json!({
        "name": "Supervisor",
        "grants": [
            { "permission": "sale.void", "limit": null },
            { "permission": "sale.discount", "limit": 1500 },
            { "permission": "cash.out", "limit": 500_000 },
        ],
    })
}

async fn role_crud_and_replay(h: &Harness) {
    let token = h.owner_token().await;
    let id = new_id();
    let uri = format!("/api/v1/roles/{id}");
    let outbox = h.outbox_count().await;
    let created = h.call(&token, Method::PUT, &uri, Some(supervisor())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    // Grants come back sorted by key, with limits in their stored units.
    assert_eq!(
        created.body["grants"],
        json!([
            { "permission": "cash.out", "limit": 500_000 },
            { "permission": "sale.discount", "limit": 1500 },
            { "permission": "sale.void", "limit": null },
        ])
    );
    let replay = h.call(&token, Method::PUT, &uri, Some(supervisor())).await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &created.body)
    );
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("role.save").await, 1);
    assert_eq!(
        h.call(&token, Method::GET, &uri, None).await.body,
        created.body
    );

    let list = h.call(&token, Method::GET, "/api/v1/roles", None).await;
    let names: Vec<&str> = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "Cashier",
            "Manager",
            "Owner",
            "Stock clerk",
            "Supervisor",
            "Viewer"
        ]
    );

    // Editing a seeded role is allowed.
    let cashier = format!("/api/v1/roles/{}", seed::CASHIER_ROLE);
    let r = h
        .call(
            &token,
            Method::PUT,
            &cashier,
            Some(json!({ "name": "Cashier", "grants": [{ "permission": "sale.create" }] })),
        )
        .await;
    assert_eq!(
        (r.status, r.body["grants"].as_array().unwrap().len()),
        (StatusCode::OK, 1)
    );

    let archived = h.call(&token, Method::DELETE, &uri, None).await;
    assert_eq!(archived.status, StatusCode::NO_CONTENT);
    assert_eq!(h.audit_count("role.archive").await, 1);
    h.call(&token, Method::GET, &uri, None)
        .await
        .assert_problem(404, "not_found");
    h.call(&token, Method::DELETE, &uri, None)
        .await
        .assert_problem(404, "not_found");
    // A PUT restores it.
    let restored = h.call(&token, Method::PUT, &uri, Some(supervisor())).await;
    assert_eq!(restored.status, StatusCode::OK);
    assert!(
        roles::get_role(&h.db, id.parse().unwrap())
            .await
            .unwrap()
            .is_some()
    );
}

async fn role_validation(h: &Harness) {
    let token = h.owner_token().await;
    let outbox = h.outbox_count().await;
    let grants = |g: Value| json!({ "name": "X", "grants": g });
    #[rustfmt::skip]
    let cases = [
        (json!({ "name": "Manager", "grants": [] }), 409, "conflict"),
        (json!({ "name": " ", "grants": [] }), 422, "invalid_field"),
        (grants(json!([{ "permission": "sale.teleport" }])), 422, "invalid_field"),
        (grants(json!([{ "permission": "sale.void" }, { "permission": "sale.void" }])), 422, "invalid_field"),
        (grants(json!([{ "permission": "sale.discount", "limit": 20000 }])), 422, "invalid_grant"),
        (grants(json!([{ "permission": "sale.create", "limit": 1 }])), 422, "invalid_grant"),
        (grants(json!([{ "permission": "cash.out", "limit": -1 }])), 422, "invalid_grant"),
        (json!({ "name": "X" }), 422, "invalid_body"),
    ];
    for (body, status, code) in cases {
        h.call(
            &token,
            Method::PUT,
            &format!("/api/v1/roles/{}", new_id()),
            Some(body),
        )
        .await
        .assert_problem(status, code);
    }
    assert_eq!(h.outbox_count().await, outbox);
}

/// A Manager has neither `role.manage` nor `user.manage`.
async fn role_endpoints_need_role_manage(h: &Harness) {
    let token = h.manager_token().await;
    let cashier = format!("/api/v1/roles/{}", seed::CASHIER_ROLE);
    let cases = [
        (Method::GET, "/api/v1/roles".to_owned(), None, "role.manage"),
        (Method::GET, cashier.clone(), None, "role.manage"),
        (
            Method::PUT,
            cashier.clone(),
            Some(supervisor()),
            "role.manage",
        ),
        (Method::DELETE, cashier, None, "role.manage"),
    ];
    for (method, uri, body, permission) in cases {
        let r = h.call(&token, method, &uri, body).await;
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], permission);
    }
}

/// Spec 2.1 §3: the Owner role cannot be deleted or emptied (§9 case 6).
async fn owner_role_cannot_be_emptied_or_archived(h: &Harness) {
    let token = h.owner_token().await;
    let uri = format!("/api/v1/roles/{}", seed::OWNER_ROLE);
    let outbox = h.outbox_count().await;
    let all: Vec<Value> = Permission::ALL
        .iter()
        .map(|p| json!({ "permission": p.as_str() }))
        .collect();
    let mut limited = all.clone();
    limited[1] = json!({ "permission": "sale.discount", "limit": 500 });
    let cases = [
        json!({ "name": "Owner", "grants": [] }),
        json!({ "name": "Owner", "grants": all[1..] }),
        json!({ "name": "Owner", "grants": limited }),
    ];
    for body in cases {
        h.call(&token, Method::PUT, &uri, Some(body))
            .await
            .assert_problem(409, "last_owner");
    }
    h.call(&token, Method::DELETE, &uri, None)
        .await
        .assert_problem(409, "last_owner");
    assert_eq!(h.outbox_count().await, outbox);
    let owner = roles::get_role(&h.db, seed::OWNER_ROLE)
        .await
        .unwrap()
        .unwrap();
    assert!(owner.grants_everything_unlimited());
    // Renaming keeps it intact, so it is allowed.
    let r = h
        .call(
            &token,
            Method::PUT,
            &uri,
            Some(json!({ "name": "Pemilik", "grants": all })),
        )
        .await;
    assert_eq!(
        (r.status, &r.body["name"]),
        (StatusCode::OK, &json!("Pemilik"))
    );
}

/// Spec 2.1 §3–4: role changes apply on the next request of an open session.
async fn role_changes_apply_on_the_next_request(h: &Harness) {
    let owner = h.owner_token().await;
    let manager = h.manager_token().await;
    h.call(&manager, Method::GET, "/api/v1/audit", None).await;
    let manager_role = format!("/api/v1/roles/{}", seed::MANAGER_ROLE);
    let r = h
        .call(
            &owner,
            Method::PUT,
            &manager_role,
            Some(json!({ "name": "Manager", "grants": [{ "permission": "role.manage" }] })),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let roles = h.call(&manager, Method::GET, "/api/v1/roles", None).await;
    assert_eq!(roles.status, StatusCode::OK);
    h.call(&manager, Method::GET, "/api/v1/audit", None)
        .await
        .assert_problem(403, "permission_denied");
}

async fn permission_catalog(h: &Harness) {
    // Any logged-in user, even a cashier on a register.
    let login = h
        .pin_login(&h.jkt_device, &h.ani, crate::common::ANI_PIN)
        .await;
    let bearer = format!("Bearer {}", login.token());
    let r = h
        .send(
            Method::GET,
            "/api/v1/permissions",
            &[
                ("authorization", &bearer),
                ("x-register-token", &h.jkt_device),
            ],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    let items = r.body.as_array().unwrap();
    assert_eq!(items.len(), Permission::ALL.len());
    assert_eq!(
        items[0],
        json!({ "key": "sale.create", "limit_kind": null, "overridable": false })
    );
    assert_eq!(
        items[1],
        json!({ "key": "sale.discount", "limit_kind": "pct", "overridable": true })
    );
    let cash_out = items.iter().find(|p| p["key"] == "cash.out").unwrap();
    assert_eq!(cash_out["limit_kind"], "amount");
    h.send(Method::GET, "/api/v1/permissions", &[], None)
        .await
        .assert_problem(401, "invalid_session");
}
