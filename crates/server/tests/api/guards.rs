//! Node-role guard and routing fallbacks (spec 2.1 §1, §9 case 14).

use axum::http::{Method, StatusCode};
use serde_json::json;
use sultan_server::NodeRole;

use crate::common::{BUDI_PIN, Harness, OWNER_PASSWORD, api_tests, new_id};

api_tests!(
    branch_node_rejects_user_writes,
    branch_node_still_authenticates_and_unlocks,
    unknown_routes_and_methods_are_problems,
);

/// Spec 2.1 §9 case 14, on the real `PUT /users/{id}` and the other master-data writes. Reads
/// still work on a branch node (it serves its replica).
async fn branch_node_rejects_user_writes(h: &Harness) {
    let cases = [
        (NodeRole::Branch, StatusCode::FORBIDDEN),
        (NodeRole::Hq, StatusCode::CREATED),
        (NodeRole::Standalone, StatusCode::CREATED),
    ];
    for (role, status) in cases {
        let app = || sultan_server::router(h.state(role));
        let login = Harness::send_to(
            app(),
            Method::POST,
            "/api/v1/auth/password",
            &[],
            Some(json!({ "username": "owner", "password": OWNER_PASSWORD })),
        )
        .await;
        assert_eq!(login.status, StatusCode::OK, "{role:?}");
        let bearer = format!("Bearer {}", login.token());
        let auth = [("authorization", bearer.as_str())];
        let id = new_id();
        let r = Harness::send_to(
            app(),
            Method::PUT,
            &format!("/api/v1/users/{id}"),
            &auth,
            Some(json!({ "username": format!("u-{id}"), "display_name": "X" })),
        )
        .await;
        assert_eq!(r.status, status, "{role:?}: {}", r.body);
        if status == StatusCode::FORBIDDEN {
            r.assert_problem(403, "read_only_replica");
            let writes = [
                (Method::PUT, format!("/api/v1/branches/{}", new_id())),
                (Method::DELETE, format!("/api/v1/branches/{}", h.jkt.id)),
                (Method::PUT, format!("/api/v1/registers/{}", new_id())),
                (
                    Method::DELETE,
                    format!("/api/v1/registers/{}", h.jkt_register.id),
                ),
                (Method::PUT, format!("/api/v1/locations/{}", new_id())),
                (Method::DELETE, format!("/api/v1/locations/{}", new_id())),
                (Method::DELETE, format!("/api/v1/users/{}", h.ani.id)),
                (Method::PUT, format!("/api/v1/roles/{}", new_id())),
                (Method::DELETE, format!("/api/v1/roles/{}", new_id())),
                (
                    Method::POST,
                    format!("/api/v1/registers/{}/pairing-code", h.jkt_register.id),
                ),
                (
                    Method::POST,
                    format!("/api/v1/registers/{}/unpair", h.jkt_register.id),
                ),
            ];
            for (method, uri) in writes {
                let body = (method == Method::PUT).then(|| json!({}));
                Harness::send_to(app(), method.clone(), &uri, &auth, body)
                    .await
                    .assert_problem(403, "read_only_replica");
            }
            let read = Harness::send_to(app(), Method::GET, "/api/v1/users", &auth, None).await;
            assert_eq!(read.status, StatusCode::OK);
        } else {
            assert_eq!(r.body["id"], id);
        }
    }
}

/// Login and unlock are local to the node, so a branch node serves them from its replica.
async fn branch_node_still_authenticates_and_unlocks(h: &Harness) {
    let app = || sultan_server::router(h.state(NodeRole::Branch));
    let r = Harness::send_to(
        app(),
        Method::POST,
        "/api/v1/auth/pin",
        &[("x-register-token", &h.jkt_device)],
        Some(json!({ "user_id": h.budi.id.to_string(), "pin": BUDI_PIN })),
    )
    .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let bearer = format!("Bearer {}", r.token());
    let r = Harness::send_to(
        app(),
        Method::POST,
        &format!("/api/v1/users/{}/unlock", h.ani.id),
        &[
            ("authorization", &bearer),
            ("x-register-token", &h.jkt_device),
        ],
        None,
    )
    .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
}

async fn unknown_routes_and_methods_are_problems(h: &Harness) {
    h.send(Method::GET, "/api/v1/nope", &[], None)
        .await
        .assert_problem(404, "not_found");
    h.send(Method::GET, "/elsewhere", &[], None)
        .await
        .assert_problem(404, "not_found");
    h.send(Method::GET, "/api/v1/auth/pin", &[], None)
        .await
        .assert_problem(405, "method_not_allowed");
}
