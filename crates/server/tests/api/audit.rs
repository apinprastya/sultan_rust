//! The audit log viewer over HTTP (spec 2.1 §7, §8): filters, pagination, branch scoping and
//! the `audit.view` permission.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_db::audit::{self, AuditEntry};

use crate::common::{BUDI_PIN, Harness, api_tests, new_id};

api_tests!(
    audit_filters,
    audit_pagination,
    audit_is_scoped_to_assigned_branches,
    audit_needs_audit_view,
);

fn actions(page: &Value) -> Vec<&str> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["action"].as_str().unwrap())
        .collect()
}

/// Writes one branch, then one location a minute later.
async fn two_writes(h: &Harness, token: &str) -> (String, String) {
    let branch = new_id();
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/branches/{branch}"),
        Some(json!({ "code": "MLG01", "name": "Malang" })),
    )
    .await;
    h.clock.advance(60_000);
    let location = new_id();
    h.call(
        token,
        Method::PUT,
        &format!("/api/v1/locations/{location}"),
        Some(json!({ "branch_id": branch, "name": "Toko" })),
    )
    .await;
    (branch, location)
}

async fn audit_filters(h: &Harness) {
    let token = h.owner_token().await;
    let start = h.now().unix_millis();
    let (branch, location) = two_writes(h, &token).await;
    let get = |query: String| {
        let token = token.clone();
        async move {
            let r = h
                .call(&token, Method::GET, &format!("/api/v1/audit{query}"), None)
                .await;
            assert_eq!(r.status, StatusCode::OK, "{query}: {}", r.body);
            r.body
        }
    };
    let all = get(String::new()).await;
    assert_eq!(actions(&all), ["location.save", "branch.save"]);
    assert_eq!(all["next_before_id"], Value::Null);
    let first = &all["items"][1];
    assert_eq!(first["user_id"], h.owner.id.to_string());
    assert_eq!(
        first["branch_id"],
        h.bdg.id.to_string(),
        "the owner's session branch"
    );
    assert_eq!(first["target"], format!("branch:{branch}"));
    assert_eq!(first["before"], Value::Null);
    assert_eq!(first["after"]["code"], "MLG01");

    let owner = h.owner.id;
    let bdg = h.bdg.id;
    let cases = [
        ("?action=branch.save".to_owned(), vec!["branch.save"]),
        (
            format!("?target=location:{location}"),
            vec!["location.save"],
        ),
        (
            format!("?user_id={owner}"),
            vec!["location.save", "branch.save"],
        ),
        (format!("?user_id={}", h.ani.id), vec![]),
        (
            format!("?branch_id={bdg}"),
            vec!["location.save", "branch.save"],
        ),
        (format!("?branch_id={}", h.jkt.id), vec![]),
        (format!("?register_id={}", h.jkt_register.id), vec![]),
        (format!("?from={}", start + 60_000), vec!["location.save"]),
        (format!("?until={}", start + 60_000), vec!["branch.save"]),
        (
            format!("?from={start}&until={}&action=location.save", start + 1),
            vec![],
        ),
    ];
    for (query, expected) in cases {
        assert_eq!(actions(&get(query.clone()).await), expected, "{query}");
    }
    let bad = [
        ("?user_id=nope", 400, "invalid_body"),
        ("?before_id=nope", 400, "invalid_body"),
        ("?register_id=nope", 400, "invalid_body"),
        ("?from=yesterday", 400, "invalid_query"),
        ("?limit=0", 422, "invalid_field"),
        ("?limit=501", 422, "invalid_field"),
    ];
    for (query, status, code) in bad {
        h.call(&token, Method::GET, &format!("/api/v1/audit{query}"), None)
            .await
            .assert_problem(status, code);
    }
}

async fn audit_pagination(h: &Harness) {
    let token = h.owner_token().await;
    for i in 0..5 {
        let mut entry = AuditEntry::new(h.now(), format!("test.{i}"));
        entry.branch = Some(h.bdg.id);
        audit::append(&h.db, &entry).await.unwrap();
    }
    let mut seen = Vec::new();
    let mut cursor = String::new();
    let mut pages = 0;
    loop {
        let r = h
            .call(
                &token,
                Method::GET,
                &format!("/api/v1/audit?limit=2{cursor}"),
                None,
            )
            .await;
        pages += 1;
        seen.extend(actions(&r.body).into_iter().map(str::to_owned));
        match r.body["next_before_id"].as_str() {
            Some(id) => cursor = format!("&before_id={id}"),
            None => break,
        }
    }
    assert_eq!(seen, ["test.4", "test.3", "test.2", "test.1", "test.0"]);
    // 2 + 2 + 1: the last page is short, so it has no cursor.
    assert_eq!(pages, 3);
}

/// Budi holds `audit.view` only through Manager@JKT01: he sees JKT01 entries only.
async fn audit_is_scoped_to_assigned_branches(h: &Harness) {
    for (branch, action) in [(h.jkt.id, "test.jkt"), (h.bdg.id, "test.bdg")] {
        let mut entry = AuditEntry::new(h.now(), action);
        entry.branch = Some(branch);
        audit::append(&h.db, &entry).await.unwrap();
    }
    let manager = h.manager_token().await;
    let r = h.call(&manager, Method::GET, "/api/v1/audit", None).await;
    assert_eq!(actions(&r.body), ["test.jkt"]);
    let r = h
        .call(
            &manager,
            Method::GET,
            &format!("/api/v1/audit?branch_id={}", h.jkt.id),
            None,
        )
        .await;
    assert_eq!(actions(&r.body), ["test.jkt"]);
    let r = h
        .call(
            &manager,
            Method::GET,
            &format!("/api/v1/audit?branch_id={}", h.bdg.id),
            None,
        )
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "audit.view");

    // The owner (all branches) sees both.
    let owner = h.owner_token().await;
    let r = h
        .call(&owner, Method::GET, "/api/v1/audit?limit=10", None)
        .await;
    assert_eq!(actions(&r.body), ["test.bdg", "test.jkt"]);
}

async fn audit_needs_audit_view(h: &Harness) {
    // Ani is a Cashier: no `audit.view`.
    let login = h
        .pin_login(&h.jkt_device, &h.ani, crate::common::ANI_PIN)
        .await;
    let bearer = format!("Bearer {}", login.token());
    let r = h
        .send(
            Method::GET,
            "/api/v1/audit",
            &[
                ("authorization", &bearer),
                ("x-register-token", &h.jkt_device),
            ],
            None,
        )
        .await;
    r.assert_problem(403, "permission_denied");
    assert_eq!(r.body["permission"], "audit.view");
    // A manager at the register may view it.
    let login = h.pin_login(&h.jkt_device, &h.budi, BUDI_PIN).await;
    let bearer = format!("Bearer {}", login.token());
    let r = h
        .send(
            Method::GET,
            "/api/v1/audit",
            &[
                ("authorization", &bearer),
                ("x-register-token", &h.jkt_device),
            ],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}
