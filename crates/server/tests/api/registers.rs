//! Pairing, pairing codes, unpairing and the lock-screen user list (spec 2.1 §2, §9 case 11).

use axum::body::Body;
use axum::http::{HeaderValue, Method, Request, StatusCode};
use serde_json::json;
use sultan_core::ids::RegisterId;
use sultan_db::{devices, org};
use sultan_server::token;
use tower::ServiceExt;

use crate::common::{Harness, T0, api_tests, new_id};

api_tests!(
    pairing_happy_path,
    pairing_code_expires_after_ten_minutes,
    pairing_code_is_single_use,
    pairing_rejects_bad_requests,
    pairing_code_of_archived_register_is_rejected,
    lock_screen_lists_the_branch_users,
    device_token_is_required,
    issued_code_pairs_and_replaces_the_old_one,
    pairing_code_needs_an_active_register,
    unpair_revokes_the_device,
);

const TEN_MINUTES: i64 = 10 * 60 * 1_000;

async fn issue_code(h: &Harness, code: &str, register: RegisterId) {
    devices::create_pairing_code(
        &h.db,
        &token::hash(code),
        register,
        T0,
        T0.saturating_add_millis(TEN_MINUTES),
    )
    .await
    .unwrap();
}

async fn pair(h: &Harness, code: &str) -> crate::common::Response {
    h.send(
        Method::POST,
        "/api/v1/registers/pair",
        &[],
        Some(json!({ "code": code })),
    )
    .await
}

async fn pairing_happy_path(h: &Harness) {
    issue_code(h, "K7Q2-M9XP", h.jkt_register.id).await;
    h.clock.advance(TEN_MINUTES - 1);
    let r = pair(h, "K7Q2-M9XP").await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(r.body["register_id"], h.jkt_register.id.to_string());
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    assert_eq!(r.body["register_code"], "R1");
    assert_eq!(r.body["register_name"], "Front");

    // Only the hash is stored, and it identifies the register.
    let device_token = r.body["device_token"].as_str().unwrap();
    assert_eq!(device_token.len(), 43);
    let identity = devices::find_device(&h.db, &token::hash(device_token))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(identity.register, h.jkt_register.id);
    assert_eq!(h.audit_count("register.pair").await, 1);

    // The new token works.
    let users = h
        .send(
            Method::GET,
            "/api/v1/registers/me/users",
            &[("x-register-token", device_token)],
            None,
        )
        .await;
    assert_eq!(users.status, StatusCode::OK);
}

async fn pairing_code_expires_after_ten_minutes(h: &Harness) {
    issue_code(h, "OLD-CODE", h.jkt_register.id).await;
    h.clock.advance(TEN_MINUTES);
    pair(h, "OLD-CODE")
        .await
        .assert_problem(400, "invalid_pairing_code");
    assert_eq!(h.audit_count("register.pair").await, 0);
}

async fn pairing_code_is_single_use(h: &Harness) {
    issue_code(h, "ONCE", h.jkt_register.id).await;
    assert_eq!(pair(h, "ONCE").await.status, StatusCode::CREATED);
    pair(h, "ONCE")
        .await
        .assert_problem(400, "invalid_pairing_code");
    assert_eq!(h.audit_count("register.pair").await, 1);
}

async fn pairing_rejects_bad_requests(h: &Harness) {
    pair(h, "NEVER-ISSUED")
        .await
        .assert_problem(400, "invalid_pairing_code");
    // Missing field.
    let r = h
        .send(Method::POST, "/api/v1/registers/pair", &[], Some(json!({})))
        .await;
    r.assert_problem(422, "invalid_body");
    // Not JSON at all.
    let r = h
        .send(Method::POST, "/api/v1/registers/pair", &[], None)
        .await;
    r.assert_problem(415, "invalid_body");
}

async fn pairing_code_of_archived_register_is_rejected(h: &Harness) {
    issue_code(h, "ARCHIVED", h.bdg_register.id).await;
    let mut archived = h.bdg_register.clone();
    archived.archived = true;
    let mut tx = h.db.begin().await.unwrap();
    org::save_register(&mut tx, &archived, T0).await.unwrap();
    tx.commit().await.unwrap();
    pair(h, "ARCHIVED")
        .await
        .assert_problem(400, "invalid_pairing_code");
    // The archived register's existing device token stops working too.
    let r = h
        .send(
            Method::GET,
            "/api/v1/registers/me/users",
            &[("x-register-token", &h.bdg_device)],
            None,
        )
        .await;
    r.assert_problem(401, "invalid_device_token");
}

async fn lock_screen_lists_the_branch_users(h: &Harness) {
    let names = |r: &crate::common::Response| -> Vec<String> {
        r.body
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["display_name"].as_str().unwrap().to_owned())
            .collect()
    };
    // JKT01: ani and budi; owner (all branches) has no PIN; cici is BDG01 only.
    let r = h
        .send(
            Method::GET,
            "/api/v1/registers/me/users",
            &[("x-register-token", &h.jkt_device)],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(names(&r), ["ANI", "BUDI"]);
    assert_eq!(r.body[0]["id"], h.ani.id.to_string());

    // A branch_id query/body cannot change the branch: it comes from the token.
    let r = h
        .send(
            Method::GET,
            &format!("/api/v1/registers/me/users?branch_id={}", h.jkt.id),
            &[("x-register-token", &h.bdg_device)],
            Some(json!({ "branch_id": h.jkt.id.to_string() })),
        )
        .await;
    assert_eq!(names(&r), ["CICI"]);
}

async fn device_token_is_required(h: &Harness) {
    let cases: [&[(&str, &str)]; 2] = [&[], &[("x-register-token", "unknown-token")]];
    for headers in cases {
        h.send(Method::GET, "/api/v1/registers/me/users", headers, None)
            .await
            .assert_problem(401, "invalid_device_token");
    }
    // A header value that is not text.
    let request = Request::get("/api/v1/registers/me/users")
        .header(
            "x-register-token",
            HeaderValue::from_bytes(b"\xff").unwrap(),
        )
        .body(Body::empty())
        .unwrap();
    let response = h.app().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    // Revoked → 401.
    devices::revoke_register_tokens(&h.db, h.jkt_register.id, T0)
        .await
        .unwrap();
    h.send(
        Method::GET,
        "/api/v1/registers/me/users",
        &[("x-register-token", &h.jkt_device)],
        None,
    )
    .await
    .assert_problem(401, "invalid_device_token");
}

/// `POST /registers/{id}/pairing-code` → pair; issuing again expires the unused old code.
async fn issued_code_pairs_and_replaces_the_old_one(h: &Harness) {
    let token = h.owner_token().await;
    let uri = format!("/api/v1/registers/{}/pairing-code", h.jkt_register.id);
    let first = h.call(&token, Method::POST, &uri, None).await;
    assert_eq!(first.status, StatusCode::CREATED, "{}", first.body);
    assert_eq!(first.body["register_id"], h.jkt_register.id.to_string());
    assert_eq!(
        first.body["expires_at"],
        h.now().unix_millis() + TEN_MINUTES
    );
    let old = first.body["code"].as_str().unwrap().to_owned();
    assert_eq!(old.len(), 9);

    let second = h.call(&token, Method::POST, &uri, None).await;
    let new = second.body["code"].as_str().unwrap().to_owned();
    assert_ne!(old, new);
    pair(h, &old)
        .await
        .assert_problem(400, "invalid_pairing_code");
    let paired = pair(h, &new).await;
    assert_eq!(paired.status, StatusCode::CREATED, "{}", paired.body);
    assert_eq!(paired.body["register_id"], h.jkt_register.id.to_string());
    pair(h, &new)
        .await
        .assert_problem(400, "invalid_pairing_code");

    // Audited without the code itself.
    assert_eq!(h.audit_count("register.pairing_code").await, 2);
    let entries = sultan_db::audit::query(&h.db, &sultan_db::audit::AuditFilter::default())
        .await
        .unwrap();
    assert!(
        !entries
            .iter()
            .any(|e| format!("{:?}", e.after).contains(&new))
    );

    // A fresh code also expires after 10 minutes.
    let third = h.call(&token, Method::POST, &uri, None).await;
    h.clock.advance(TEN_MINUTES);
    pair(h, third.body["code"].as_str().unwrap())
        .await
        .assert_problem(400, "invalid_pairing_code");
}

async fn pairing_code_needs_an_active_register(h: &Harness) {
    let token = h.owner_token().await;
    for id in [new_id(), h.bdg_register.id.to_string()] {
        if id == h.bdg_register.id.to_string() {
            h.call(
                &token,
                Method::DELETE,
                &format!("/api/v1/registers/{id}"),
                None,
            )
            .await;
        }
        for action in ["pairing-code", "unpair"] {
            h.call(
                &token,
                Method::POST,
                &format!("/api/v1/registers/{id}/{action}"),
                None,
            )
            .await
            .assert_problem(404, "not_found");
        }
    }
    h.call(&token, Method::POST, "/api/v1/registers/nope/unpair", None)
        .await
        .assert_problem(400, "invalid_body");
}

/// Spec 2.1 §2: unpairing revokes the token; the register's sessions die with it.
async fn unpair_revokes_the_device(h: &Harness) {
    let login = h
        .pin_login(&h.jkt_device, &h.ani, crate::common::ANI_PIN)
        .await;
    let session = login.token();
    let token = h.owner_token().await;
    let code = h
        .call(
            &token,
            Method::POST,
            &format!("/api/v1/registers/{}/pairing-code", h.jkt_register.id),
            None,
        )
        .await;
    let r = h
        .call(
            &token,
            Method::POST,
            &format!("/api/v1/registers/{}/unpair", h.jkt_register.id),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["revoked_tokens"], 1);
    assert_eq!(h.audit_count("register.unpair").await, 1);
    h.logout(&session, Some(&h.jkt_device))
        .await
        .assert_problem(401, "invalid_device_token");
    // The unused code issued before unpairing is dead too.
    pair(h, code.body["code"].as_str().unwrap())
        .await
        .assert_problem(400, "invalid_pairing_code");
    // Unpairing again revokes nothing.
    let r = h
        .call(
            &token,
            Method::POST,
            &format!("/api/v1/registers/{}/unpair", h.jkt_register.id),
            None,
        )
        .await;
    assert_eq!(r.body["revoked_tokens"], 0);
}
