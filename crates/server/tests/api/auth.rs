//! Login, lockout, logout, idle expiry and unlock (spec 2.1 §4, §9 cases 7, 12, 13).

use axum::http::{Method, StatusCode};
use serde_json::json;
use sultan_core::access::{LOCKOUT_DURATION_MS, LoginAttempts};
use sultan_db::{auth, devices, users};
use sultan_server::token;

use crate::common::{
    ANI_PIN, BUDI_PASSWORD, BUDI_PIN, Harness, IDLE_MS, OWNER_PASSWORD, Response, T0, api_tests,
};

api_tests!(
    pin_login_binds_session_to_register,
    branch_id_in_body_is_ignored,
    pin_login_rejects_unknown_or_unassigned_users,
    wrong_pin_locks_out_until_timeout,
    successful_login_resets_failures,
    revoked_device_rejects_valid_session,
    password_login,
    password_login_failures,
    logout_invalidates_session,
    session_expires_when_idle,
    inactive_user_session_is_rejected,
    unlock_needs_permission,
);

async fn password_login_as(h: &Harness, body: serde_json::Value) -> Response {
    h.send(Method::POST, "/api/v1/auth/password", &[], Some(body))
        .await
}

/// Spec 2.1 §9 case 12 (first half).
async fn pin_login_binds_session_to_register(h: &Harness) {
    let r = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["user_id"], h.ani.id.to_string());
    assert_eq!(r.body["display_name"], "ANI");
    assert_eq!(r.body["register_id"], h.jkt_register.id.to_string());
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    assert_eq!(
        r.body["expires_at"],
        T0.saturating_add_millis(IDLE_MS).unix_millis()
    );

    let token = r.token();
    let stored = auth::find_session(&h.db, &token::hash(&token))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.register, Some(h.jkt_register.id));
    assert_eq!(stored.user, h.ani.id);

    // The session only works together with its own register's device token.
    h.logout(&token, None)
        .await
        .assert_problem(401, "invalid_device_token");
    h.logout(&token, Some(&h.bdg_device))
        .await
        .assert_problem(401, "invalid_device_token");
    assert_eq!(
        h.logout(&token, Some(&h.jkt_device)).await.status,
        StatusCode::NO_CONTENT
    );
}

/// Spec 2.1 §9 case 13.
async fn branch_id_in_body_is_ignored(h: &Harness) {
    let body = json!({
        "user_id": h.ani.id.to_string(),
        "pin": ANI_PIN,
        "branch_id": h.bdg.id.to_string(),
        "register_id": h.bdg_register.id.to_string(),
    });
    let r = h
        .send(
            Method::POST,
            "/api/v1/auth/pin",
            &[("x-register-token", &h.jkt_device)],
            Some(body),
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
    assert_eq!(r.body["register_id"], h.jkt_register.id.to_string());

    // cici works at BDG01 only; claiming BDG01 in the body from a JKT01 register does not help.
    let body = json!({
        "user_id": h.cici.id.to_string(),
        "pin": ANI_PIN,
        "branch_id": h.bdg.id.to_string(),
    });
    h.send(
        Method::POST,
        "/api/v1/auth/pin",
        &[("x-register-token", &h.jkt_device)],
        Some(body),
    )
    .await
    .assert_problem(401, "invalid_credentials");
    // At her own register she gets in.
    let r = h.pin_login(&h.bdg_device, &h.cici, ANI_PIN).await;
    assert_eq!(r.body["branch_id"], h.bdg.id.to_string());
}

async fn pin_login_rejects_unknown_or_unassigned_users(h: &Harness) {
    let unknown = crate::common::stranger();
    let cases = [
        (&unknown, ANI_PIN, "unknown user"),
        (&h.owner, "1234", "owner has no PIN"),
        (&h.cici, ANI_PIN, "not assigned to JKT01"),
    ];
    for (user, pin, why) in cases {
        let r = h.pin_login(&h.jkt_device, user, pin).await;
        assert_eq!(r.body["code"], "invalid_credentials", "{why}");
    }
    // None of these count as failures.
    let attempts = auth::load_login_attempts(&h.db, h.cici.id, h.jkt.id)
        .await
        .unwrap();
    assert_eq!(attempts, LoginAttempts::default());

    // Bad user_id and missing device token.
    h.send(
        Method::POST,
        "/api/v1/auth/pin",
        &[("x-register-token", &h.jkt_device)],
        Some(json!({ "user_id": "nope", "pin": "1234" })),
    )
    .await
    .assert_problem(400, "invalid_body");
    h.pin_login("", &h.ani, ANI_PIN)
        .await
        .assert_problem(401, "invalid_device_token");
}

/// Spec 2.1 §9 case 7 over HTTP: the 5th wrong PIN locks, the 6th attempt is refused even with
/// the right PIN, and the lockout ends after 5 minutes.
async fn wrong_pin_locks_out_until_timeout(h: &Harness) {
    for attempt in 1..=4 {
        h.clock.advance(1_000);
        let r = h.pin_login(&h.jkt_device, &h.ani, "0000").await;
        assert_eq!(r.body["code"], "invalid_credentials", "attempt {attempt}");
    }
    h.clock.advance(1_000);
    let fifth = h.pin_login(&h.jkt_device, &h.ani, "0000").await;
    fifth.assert_problem(423, "locked_out");
    let until = h.now().unix_millis() + LOCKOUT_DURATION_MS;
    assert_eq!(fifth.body["locked_until"], until);

    h.clock.advance(1_000);
    let sixth = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await;
    sixth.assert_problem(423, "locked_out");
    assert_eq!(sixth.body["locked_until"], until);

    // Locked at JKT01 only (lockout is per branch); budi is unaffected.
    let r = h.pin_login(&h.jkt_device, &h.budi, BUDI_PIN).await;
    assert_eq!(r.status, StatusCode::OK);

    assert_eq!(h.audit_count("login.failed").await, 4);
    assert_eq!(h.audit_count("login.locked_out").await, 1);

    h.clock
        .set(sultan_core::time::Timestamp::from_unix_millis(until - 1));
    h.pin_login(&h.jkt_device, &h.ani, ANI_PIN)
        .await
        .assert_problem(423, "locked_out");
    h.clock
        .set(sultan_core::time::Timestamp::from_unix_millis(until));
    let r = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}

async fn successful_login_resets_failures(h: &Harness) {
    for _ in 0..4 {
        h.pin_login(&h.jkt_device, &h.ani, "0000").await;
    }
    assert_eq!(
        auth::load_login_attempts(&h.db, h.ani.id, h.jkt.id)
            .await
            .unwrap()
            .failures,
        4
    );
    assert_eq!(
        h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await.status,
        StatusCode::OK
    );
    assert_eq!(
        auth::load_login_attempts(&h.db, h.ani.id, h.jkt.id)
            .await
            .unwrap(),
        LoginAttempts::default()
    );
    // Four more failures do not lock.
    for _ in 0..4 {
        let r = h.pin_login(&h.jkt_device, &h.ani, "0000").await;
        assert_eq!(r.body["code"], "invalid_credentials");
    }
}

/// Spec 2.1 §9 case 12 (second half).
async fn revoked_device_rejects_valid_session(h: &Harness) {
    let token = h.pin_login(&h.jkt_device, &h.budi, BUDI_PIN).await.token();
    devices::revoke_register_tokens(&h.db, h.jkt_register.id, h.now())
        .await
        .unwrap();
    h.logout(&token, Some(&h.jkt_device))
        .await
        .assert_problem(401, "invalid_device_token");
    h.unlock(&token, &h.jkt_device, &h.ani.id.to_string())
        .await
        .assert_problem(401, "invalid_device_token");
    // The session itself is still valid; only the device is refused.
    let session = auth::find_session(&h.db, &token::hash(&token))
        .await
        .unwrap()
        .unwrap();
    assert!(session.is_active(h.now()));
}

async fn password_login(h: &Harness) {
    // Owner holds all branches; without branch_id the first branch by code is used.
    let r = password_login_as(
        h,
        json!({ "username": "owner", "password": OWNER_PASSWORD }),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["branch_id"], h.bdg.id.to_string());
    assert_eq!(r.body["register_id"], serde_json::Value::Null);
    let token = r.token();
    let stored = auth::find_session(&h.db, &token::hash(&token))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.register, None);
    // A back-office session needs no device token.
    assert_eq!(h.logout(&token, None).await.status, StatusCode::NO_CONTENT);

    let r = password_login_as(
        h,
        json!({ "username": "owner", "password": OWNER_PASSWORD,
                "branch_id": h.jkt.id.to_string() }),
    )
    .await;
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());

    // budi is assigned to JKT01 only.
    let r = password_login_as(h, json!({ "username": "budi", "password": BUDI_PASSWORD })).await;
    assert_eq!(r.body["branch_id"], h.jkt.id.to_string());
}

async fn password_login_failures(h: &Harness) {
    let cases = [
        (
            json!({ "username": "nobody", "password": "whatever-1" }),
            401,
            "invalid_credentials",
        ),
        (
            json!({ "username": "ani", "password": "whatever-1" }),
            401,
            "invalid_credentials",
        ),
        (
            json!({ "username": "owner", "password": "wrong-pass" }),
            401,
            "invalid_credentials",
        ),
        (
            json!({ "username": "budi", "password": BUDI_PASSWORD, "branch_id": h.bdg.id.to_string() }),
            401,
            "invalid_credentials",
        ),
        (
            json!({ "username": "budi", "password": BUDI_PASSWORD, "branch_id": "x" }),
            400,
            "invalid_body",
        ),
        (json!({ "username": "budi" }), 422, "invalid_body"),
    ];
    for (body, status, code) in cases {
        password_login_as(h, body)
            .await
            .assert_problem(status, code);
    }
    // Only the wrong owner password counted, at the owner's default branch.
    let attempts = auth::load_login_attempts(&h.db, h.owner.id, h.bdg.id)
        .await
        .unwrap();
    assert_eq!(attempts.failures, 1);
}

async fn logout_invalidates_session(h: &Harness) {
    let token = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await.token();
    let r = h.logout(&token, Some(&h.jkt_device)).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let again = h.logout(&token, Some(&h.jkt_device)).await;
    again.assert_problem(401, "invalid_session");
    assert_eq!(again.headers["www-authenticate"], "Bearer");

    // No or malformed Authorization header.
    h.send(Method::POST, "/api/v1/auth/logout", &[], None)
        .await
        .assert_problem(401, "invalid_session");
    h.send(
        Method::POST,
        "/api/v1/auth/logout",
        &[("authorization", "Basic abc")],
        None,
    )
    .await
    .assert_problem(401, "invalid_session");
}

/// Idle expiry: every request moves the expiry forward; a session idle for `IDLE_MS` is gone.
async fn session_expires_when_idle(h: &Harness) {
    let token = h.pin_login(&h.jkt_device, &h.budi, BUDI_PIN).await.token();
    let target = h.ani.id.to_string();
    for _ in 0..2 {
        h.clock.advance(IDLE_MS - 1);
        let r = h.unlock(&token, &h.jkt_device, &target).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    }
    h.clock.advance(IDLE_MS);
    h.unlock(&token, &h.jkt_device, &target)
        .await
        .assert_problem(401, "invalid_session");
}

async fn inactive_user_session_is_rejected(h: &Harness) {
    let token = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await.token();
    let mut ani = h.ani.clone();
    ani.active = false;
    let mut tx = h.db.begin().await.unwrap();
    users::save_user(&mut tx, &ani, T0).await.unwrap();
    tx.commit().await.unwrap();
    h.logout(&token, Some(&h.jkt_device))
        .await
        .assert_problem(401, "invalid_session");
    h.pin_login(&h.jkt_device, &h.ani, ANI_PIN)
        .await
        .assert_problem(401, "invalid_credentials");
}

/// `POST /users/{id}/unlock` needs `user.unlock` (spec 2.1 §4, §8).
async fn unlock_needs_permission(h: &Harness) {
    let ani_token = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await.token();
    for _ in 0..5 {
        h.pin_login(&h.jkt_device, &h.ani, "0000").await;
    }
    h.pin_login(&h.jkt_device, &h.ani, ANI_PIN)
        .await
        .assert_problem(423, "locked_out");

    // A cashier cannot unlock, not even herself.
    let denied = h
        .unlock(&ani_token, &h.jkt_device, &h.ani.id.to_string())
        .await;
    denied.assert_problem(403, "permission_denied");
    assert_eq!(denied.body["permission"], "user.unlock");

    // A manager can; the permission check comes before the target lookup.
    let budi = h.pin_login(&h.jkt_device, &h.budi, BUDI_PIN).await.token();
    h.unlock(&budi, &h.jkt_device, "not-a-uuid")
        .await
        .assert_problem(400, "invalid_body");
    h.unlock(
        &budi,
        &h.jkt_device,
        &sultan_core::ids::UserId::generate().to_string(),
    )
    .await
    .assert_problem(404, "not_found");
    let r = h.unlock(&budi, &h.jkt_device, &h.ani.id.to_string()).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    assert_eq!(h.audit_count("user.unlock").await, 1);

    let r = h.pin_login(&h.jkt_device, &h.ani, ANI_PIN).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
}
