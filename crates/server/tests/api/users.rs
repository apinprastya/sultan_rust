//! Users over HTTP (spec 2.1 §3, §4, §8, §9 cases 6 and 17; plan Phase 5): CRUD with role
//! assignments, credentials hashed and never returned, idempotent replay, the Owner invariant,
//! and that a rejected write leaves no row, audit entry or outbox event.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_db::{seed, users};
use sultan_server::credentials;

use crate::common::{Harness, api_tests, new_id};

api_tests!(
    user_crud_with_pin_and_assignments,
    user_replay_with_secrets_is_a_no_op,
    user_archive_deactivates,
    user_validation,
    user_endpoints_need_user_manage,
    last_owner_cannot_be_removed,
    second_owner_makes_the_first_removable,
);

fn cashier_at(branch: &str) -> Value {
    json!([{ "role_id": seed::CASHIER_ROLE.to_string(), "branch_id": branch }])
}

async fn user_crud_with_pin_and_assignments(h: &Harness) {
    let token = h.owner_token().await;
    let jkt = h.jkt.id.to_string();
    let id = new_id();
    let uri = format!("/api/v1/users/{id}");
    let outbox = h.outbox_count().await;
    let body = json!({
        "username": "dewi",
        "display_name": "Dewi",
        "pin": "4321",
        "assignments": cashier_at(&jkt),
    });
    let created = h.call(&token, Method::PUT, &uri, Some(body)).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(
        created.body,
        json!({
            "id": id,
            "username": "dewi",
            "display_name": "Dewi",
            "active": true,
            "has_pin": true,
            "has_password": false,
            "assignments": cashier_at(&jkt),
        })
    );
    // user.saved + user.assignments_saved, one audit entry.
    assert_eq!(h.outbox_count().await, outbox + 2);
    assert_eq!(h.audit_count("user.save").await, 1);

    // The PIN is stored as argon2id and works at the register.
    let stored = users::get_user(&h.db, id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(credentials::verify_secret(
        stored.pin_hash.as_deref().unwrap(),
        "4321"
    ));
    let login = h.pin_login(&h.jkt_device, &stored, "4321").await;
    assert_eq!(login.status, StatusCode::OK, "{}", login.body);

    // GET and list never expose hashes.
    let got = h.call(&token, Method::GET, &uri, None).await;
    assert_eq!(got.body, created.body);
    let list = h.call(&token, Method::GET, "/api/v1/users", None).await;
    let text = list.body.to_string();
    assert!(
        !text.contains("argon2") && !text.contains("pin_hash"),
        "{text}"
    );
    let names: Vec<&str> = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["username"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["ani", "budi", "cici", "dewi", "owner"]);
    let audit = sultan_db::audit::query(&h.db, &sultan_db::audit::AuditFilter::default())
        .await
        .unwrap();
    assert!(!audit.iter().any(|e| {
        e.after
            .as_ref()
            .is_some_and(|a| a.to_string().contains("argon2"))
    }));

    // Change only the display name: assignments are kept when omitted.
    let renamed = h
        .call(
            &token,
            Method::PUT,
            &uri,
            Some(json!({ "username": "dewi", "display_name": "Dewi S" })),
        )
        .await;
    assert_eq!(renamed.status, StatusCode::OK);
    assert_eq!(renamed.body["assignments"], cashier_at(&jkt));
    assert_eq!(renamed.body["has_pin"], true);
    assert_eq!(h.outbox_count().await, outbox + 3, "only user.saved");

    // Reassign to all branches: only the assignments change.
    let all = json!([{ "role_id": seed::VIEWER_ROLE.to_string(), "branch_id": null }]);
    let reassigned = h
        .call(
            &token,
            Method::PUT,
            &uri,
            Some(json!({ "username": "dewi", "display_name": "Dewi S", "assignments": all })),
        )
        .await;
    assert_eq!(reassigned.body["assignments"], all);
    assert_eq!(
        h.outbox_count().await,
        outbox + 4,
        "only user.assignments_saved"
    );
}

/// Spec 2.1 §9 case 17 with secrets in the body: the same PIN and password are recognised, so
/// the stored hashes (salted) are kept and nothing is written.
async fn user_replay_with_secrets_is_a_no_op(h: &Harness) {
    let token = h.owner_token().await;
    let id = new_id();
    let uri = format!("/api/v1/users/{id}");
    let body = json!({
        "username": "eko",
        "display_name": "Eko",
        "pin": "112233",
        "password": "eko-password",
        "assignments": cashier_at(&h.bdg.id.to_string()),
    });
    let created = h.call(&token, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED);
    let stored = users::get_user(&h.db, id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    let outbox = h.outbox_count().await;

    let replay = h.call(&token, Method::PUT, &uri, Some(body)).await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &created.body)
    );
    assert_eq!(h.outbox_count().await, outbox);
    assert_eq!(h.audit_count("user.save").await, 1);
    assert_eq!(
        users::get_user(&h.db, id.parse().unwrap())
            .await
            .unwrap()
            .unwrap(),
        stored
    );
    assert_eq!(users::list_users(&h.db).await.unwrap().len(), 5);

    // A new PIN is a change and is audited as such (without the hash).
    let new_pin = json!({ "username": "eko", "display_name": "Eko", "pin": "445566" });
    let r = h.call(&token, Method::PUT, &uri, Some(new_pin)).await;
    assert_eq!(r.status, StatusCode::OK);
    let entries = sultan_db::audit::query(
        &h.db,
        &sultan_db::audit::AuditFilter {
            action: Some("user.save".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let after = entries[0].after.as_ref().unwrap();
    assert_eq!(
        (&after["pin_changed"], &after["password_changed"]),
        (&json!(true), &json!(false))
    );
    let changed = users::get_user(&h.db, id.parse().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(credentials::verify_secret(
        changed.pin_hash.as_deref().unwrap(),
        "445566"
    ));
    assert_eq!(changed.password_hash, stored.password_hash);
}

async fn user_validation(h: &Harness) {
    let token = h.owner_token().await;
    let outbox = h.outbox_count().await;
    let put = |body: Value| {
        let token = token.clone();
        async move {
            h.call(
                &token,
                Method::PUT,
                &format!("/api/v1/users/{}", new_id()),
                Some(body),
            )
            .await
        }
    };
    let role = seed::CASHIER_ROLE.to_string();
    #[rustfmt::skip]
    let cases = [
        (json!({ "username": "ani", "display_name": "x" }), 409, "conflict"),
        (json!({ "username": " ", "display_name": "x" }), 422, "invalid_field"),
        (json!({ "username": "x1", "display_name": "" }), 422, "invalid_field"),
        (json!({ "username": "x1", "display_name": "x", "pin": "12" }), 422, "invalid_pin"),
        (json!({ "username": "x1", "display_name": "x", "password": "short" }), 422, "weak_password"),
        (json!({ "username": "x1", "display_name": "x",
            "assignments": [{ "role_id": new_id(), "branch_id": null }] }), 422, "invalid_field"),
        (json!({ "username": "x1", "display_name": "x",
            "assignments": [{ "role_id": role, "branch_id": new_id() }] }), 422, "invalid_field"),
        (json!({ "username": "x1", "display_name": "x",
            "assignments": [{ "role_id": "nope" }] }), 400, "invalid_body"),
        (json!({ "username": "x1" }), 422, "invalid_body"),
    ];
    for (body, status, code) in cases {
        put(body).await.assert_problem(status, code);
    }
    assert_eq!(h.outbox_count().await, outbox);
    assert_eq!(h.audit_count("user.save").await, 0);
}

/// Spec 2.1 §8: `/users` reads and writes need `user.manage`.
async fn user_endpoints_need_user_manage(h: &Harness) {
    let token = h.manager_token().await;
    let ani = format!("/api/v1/users/{}", h.ani.id);
    let cases = [
        (Method::GET, "/api/v1/users".to_owned(), None),
        (Method::GET, ani.clone(), None),
        (
            Method::PUT,
            format!("/api/v1/users/{}", new_id()),
            Some(json!({ "username": "z", "display_name": "z" })),
        ),
        (Method::DELETE, ani, None),
    ];
    for (method, uri, body) in cases {
        let r = h.call(&token, method, &uri, body).await;
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], "user.manage");
    }
    assert!(
        users::get_user(&h.db, h.ani.id)
            .await
            .unwrap()
            .unwrap()
            .active
    );
}

/// Spec 2.1 §9 case 6 over HTTP: deactivating, archiving, unassigning or narrowing the last
/// Owner is `409 last_owner`, and the rolled-back transaction leaves no outbox event (the user
/// row and the assignment were already written inside it).
async fn last_owner_cannot_be_removed(h: &Harness) {
    let token = h.owner_token().await;
    let uri = format!("/api/v1/users/{}", h.owner.id);
    let outbox = h.outbox_count().await;
    let owner_all = json!([{ "role_id": seed::OWNER_ROLE.to_string(), "branch_id": null }]);
    let cases = [
        json!({ "username": "owner", "display_name": "OWNER", "active": false }),
        json!({ "username": "owner", "display_name": "OWNER", "assignments": [] }),
        json!({ "username": "owner", "display_name": "OWNER", "assignments": cashier_at(&h.jkt.id.to_string()) }),
        json!({ "username": "owner", "display_name": "OWNER",
            "assignments": [{ "role_id": seed::OWNER_ROLE.to_string(), "branch_id": h.jkt.id.to_string() }] }),
        // Renaming in the same request does not sneak through.
        json!({ "username": "boss", "display_name": "Boss", "active": false, "assignments": owner_all }),
    ];
    for body in cases {
        h.call(&token, Method::PUT, &uri, Some(body))
            .await
            .assert_problem(409, "last_owner");
    }
    h.call(&token, Method::DELETE, &uri, None)
        .await
        .assert_problem(409, "last_owner");
    assert_eq!(
        h.outbox_count().await,
        outbox,
        "rolled back with the rejected change"
    );
    assert_eq!(
        h.audit_count("user.save").await + h.audit_count("user.archive").await,
        0
    );
    let owner = users::get_user(&h.db, h.owner.id).await.unwrap().unwrap();
    assert_eq!(owner, h.owner);
    // Still an Owner: the session keeps working.
    let r = h.call(&token, Method::GET, &uri, None).await;
    assert_eq!(r.body["assignments"], owner_all);
}

async fn second_owner_makes_the_first_removable(h: &Harness) {
    let token = h.owner_token().await;
    let id = new_id();
    let owner_all = json!([{ "role_id": seed::OWNER_ROLE.to_string(), "branch_id": null }]);
    let r = h
        .call(
            &token,
            Method::PUT,
            &format!("/api/v1/users/{id}"),
            Some(json!({ "username": "owner2", "display_name": "Owner 2", "password": "owner-two-pass", "assignments": owner_all })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let r = h
        .call(
            &token,
            Method::DELETE,
            &format!("/api/v1/users/{}", h.owner.id),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    // The archived owner's session is gone; the new owner cannot remove themself now.
    h.call(&token, Method::GET, "/api/v1/users", None)
        .await
        .assert_problem(401, "invalid_session");
    let token2 = h.password_token("owner2", "owner-two-pass").await;
    h.call(
        &token2,
        Method::DELETE,
        &format!("/api/v1/users/{id}"),
        None,
    )
    .await
    .assert_problem(409, "last_owner");
}

/// `DELETE` archives by deactivating: the user's open session stops working at once.
async fn user_archive_deactivates(h: &Harness) {
    let token = h.owner_token().await;
    let ani = h
        .pin_login(&h.jkt_device, &h.ani, crate::common::ANI_PIN)
        .await
        .token();
    let uri = format!("/api/v1/users/{}", h.ani.id);
    let archived = h.call(&token, Method::DELETE, &uri, None).await;
    assert_eq!(
        (archived.status, &archived.body["active"]),
        (StatusCode::OK, &json!(false))
    );
    assert_eq!(
        archived.body["assignments"],
        cashier_at(&h.jkt.id.to_string())
    );
    assert_eq!(h.audit_count("user.archive").await, 1);
    let again = h.call(&token, Method::DELETE, &uri, None).await;
    assert_eq!(again.body, archived.body);
    assert_eq!(h.audit_count("user.archive").await, 1);
    h.logout(&ani, Some(&h.jkt_device))
        .await
        .assert_problem(401, "invalid_session");
    for method in [Method::DELETE, Method::GET] {
        h.call(&token, method, &format!("/api/v1/users/{}", new_id()), None)
            .await
            .assert_problem(404, "not_found");
    }
}
