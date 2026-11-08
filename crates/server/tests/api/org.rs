//! Branches, registers and stock locations over HTTP (spec 2.1 §1, §8, §9 case 17; plan
//! Phase 5): CRUD, idempotent `PUT` replay, archive, validation, branch visibility, permission
//! denied, and one audit entry plus one outbox event per write.

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_db::org;

use crate::common::{Harness, api_tests, new_id};

api_tests!(
    branch_crud_and_replay,
    branch_validation_and_conflicts,
    register_crud,
    register_validation,
    location_crud,
    org_writes_need_org_manage,
    reads_show_only_assigned_branches,
);

fn branch_body(code: &str) -> Value {
    json!({
        "code": code,
        "name": format!("Toko {code}"),
        "address": "Jl. Sudirman 1",
        "npwp": "01.234.567.8-901.000",
        "receipt_header": "Selamat datang",
    })
}

/// Spec 2.1 §9 case 17 for branches: replaying a `PUT` neither duplicates the row nor writes a
/// second audit entry or outbox event, and answers the same body.
async fn branch_crud_and_replay(h: &Harness) {
    let token = h.owner_token().await;
    let id = new_id();
    let uri = format!("/api/v1/branches/{id}");
    let outbox = h.outbox_count().await;

    let created = h
        .call(&token, Method::PUT, &uri, Some(branch_body("SBY01")))
        .await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["code"], "SBY01");
    assert_eq!(created.body["company_id"], h.jkt.company.to_string());
    assert_eq!(created.body["archived"], false);
    assert_eq!(h.outbox_count().await, outbox + 1);
    assert_eq!(h.audit_count("branch.save").await, 1);

    let replay = h
        .call(&token, Method::PUT, &uri, Some(branch_body("SBY01")))
        .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body, created.body);
    assert_eq!(h.outbox_count().await, outbox + 1, "replay writes nothing");
    assert_eq!(h.audit_count("branch.save").await, 1);
    assert_eq!(org::list_branches(&h.db).await.unwrap().len(), 3);

    let mut changed = branch_body("SBY01");
    changed["name"] = json!("Toko Surabaya");
    let updated = h.call(&token, Method::PUT, &uri, Some(changed)).await;
    assert_eq!(updated.status, StatusCode::OK);
    assert_eq!(updated.body["name"], "Toko Surabaya");
    assert_eq!(h.outbox_count().await, outbox + 2);

    let got = h.call(&token, Method::GET, &uri, None).await;
    assert_eq!(got.body, updated.body);
    let list = h.call(&token, Method::GET, "/api/v1/branches", None).await;
    let codes: Vec<&str> = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["BDG01", "JKT01", "SBY01"]);

    // Archive, twice: the second is a no-op.
    let archived = h.call(&token, Method::DELETE, &uri, None).await;
    assert_eq!(archived.status, StatusCode::OK);
    assert_eq!(archived.body["archived"], true);
    let again = h.call(&token, Method::DELETE, &uri, None).await;
    assert_eq!(again.body, archived.body);
    assert_eq!(h.audit_count("branch.archive").await, 1);
    assert_eq!(h.outbox_count().await, outbox + 3);
    assert!(
        org::get_branch(&h.db, id.parse().unwrap())
            .await
            .unwrap()
            .unwrap()
            .archived
    );

    // The audit entry records who, where, what and before/after.
    let entries = sultan_db::audit::query(
        &h.db,
        &sultan_db::audit::AuditFilter {
            action: Some("branch.archive".into()),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries[0].user, Some(h.owner.id));
    assert_eq!(
        entries[0].target.as_deref(),
        Some(format!("branch:{id}").as_str())
    );
    assert_eq!(entries[0].before.as_ref().unwrap()["archived"], false);
    assert_eq!(entries[0].after.as_ref().unwrap()["archived"], true);

    h.call(
        &token,
        Method::DELETE,
        &format!("/api/v1/branches/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(
        &token,
        Method::GET,
        &format!("/api/v1/branches/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
}

async fn branch_validation_and_conflicts(h: &Harness) {
    let token = h.owner_token().await;
    let outbox = h.outbox_count().await;
    let put = |id: String, body: Value| {
        let token = token.clone();
        async move {
            h.call(
                &token,
                Method::PUT,
                &format!("/api/v1/branches/{id}"),
                Some(body),
            )
            .await
        }
    };
    // Duplicate code: the unique key, not an overwrite of JKT01.
    put(new_id(), branch_body("JKT01"))
        .await
        .assert_problem(409, "conflict");
    let cases = [
        (json!({ "code": "JKT 02", "name": "x" }), "code"),
        (json!({ "code": "", "name": "x" }), "code"),
        (json!({ "code": "JKT02", "name": "  " }), "name"),
    ];
    for (body, field) in cases {
        let r = put(new_id(), body).await;
        r.assert_problem(422, "invalid_field");
        assert_eq!(r.body["field"], field);
    }
    put(new_id(), json!({ "name": "no code" }))
        .await
        .assert_problem(422, "invalid_body");
    let v4 = "550e8400-e29b-41d4-a716-446655440000".to_owned();
    put(v4, branch_body("JKT02"))
        .await
        .assert_problem(400, "invalid_body");
    put("nope".into(), branch_body("JKT02"))
        .await
        .assert_problem(400, "invalid_body");
    h.call(&token, Method::GET, "/api/v1/branches/nope", None)
        .await
        .assert_problem(400, "invalid_body");
    assert_eq!(
        h.outbox_count().await,
        outbox,
        "failed writes leave no outbox event"
    );
}

fn register_body(branch: &str, code: &str, location: Option<&str>) -> Value {
    json!({
        "branch_id": branch,
        "code": code,
        "name": format!("Kasir {code}"),
        "default_location_id": location,
    })
}

async fn register_crud(h: &Harness) {
    let token = h.owner_token().await;
    let jkt = h.jkt.id.to_string();
    let location = new_id();
    let r = h
        .call(
            &token,
            Method::PUT,
            &format!("/api/v1/locations/{location}"),
            Some(json!({ "branch_id": jkt, "name": "Toko", "receiving_default": true })),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);

    let id = new_id();
    let uri = format!("/api/v1/registers/{id}");
    let body = register_body(&jkt, "R2", Some(&location));
    let created = h.call(&token, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["default_location_id"], location);
    let replay = h.call(&token, Method::PUT, &uri, Some(body)).await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &created.body)
    );
    assert_eq!(h.audit_count("register.save").await, 1);

    let list = h
        .call(
            &token,
            Method::GET,
            &format!("/api/v1/registers?branch_id={jkt}"),
            None,
        )
        .await;
    let codes: Vec<&str> = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["R1", "R2"]);
    let all = h.call(&token, Method::GET, "/api/v1/registers", None).await;
    assert_eq!(all.body.as_array().unwrap().len(), 3);
    assert_eq!(
        h.call(&token, Method::GET, &uri, None).await.body,
        created.body
    );

    // Archiving a paired register cuts its device off.
    let jkt_r1 = format!("/api/v1/registers/{}", h.jkt_register.id);
    let archived = h.call(&token, Method::DELETE, &jkt_r1, None).await;
    assert_eq!(archived.body["archived"], true);
    h.send(
        Method::GET,
        "/api/v1/registers/me/users",
        &[("x-register-token", &h.jkt_device)],
        None,
    )
    .await
    .assert_problem(401, "invalid_device_token");
    // A PUT without `archived` restores it.
    let restored = h
        .call(
            &token,
            Method::PUT,
            &jkt_r1,
            Some(json!({ "branch_id": jkt, "code": "R1", "name": "Front" })),
        )
        .await;
    assert_eq!(
        (restored.status, &restored.body["archived"]),
        (StatusCode::OK, &json!(false))
    );
    assert_eq!(h.audit_count("register.archive").await, 1);
}

async fn register_validation(h: &Harness) {
    let token = h.owner_token().await;
    let jkt = h.jkt.id.to_string();
    let bdg = h.bdg.id.to_string();
    let bdg_location = new_id();
    h.call(
        &token,
        Method::PUT,
        &format!("/api/v1/locations/{bdg_location}"),
        Some(json!({ "branch_id": bdg, "name": "Gudang" })),
    )
    .await;
    let outbox = h.outbox_count().await;
    let existing = h.jkt_register.id.to_string();
    let cases = [
        (new_id(), register_body(&new_id(), "R5", None), "branch_id"),
        (
            existing.clone(),
            register_body(&bdg, "R1", None),
            "branch_id",
        ),
        (
            new_id(),
            register_body(&jkt, "R5", Some(&bdg_location)),
            "default_location_id",
        ),
        (
            new_id(),
            register_body(&jkt, "R5", Some(&new_id())),
            "default_location_id",
        ),
        (new_id(), register_body(&jkt, "R-5", None), "code"),
    ];
    for (id, body, field) in cases {
        let r = h
            .call(
                &token,
                Method::PUT,
                &format!("/api/v1/registers/{id}"),
                Some(body),
            )
            .await;
        r.assert_problem(422, "invalid_field");
        assert_eq!(r.body["field"], field, "{}", r.body);
    }
    // Same code at another branch is fine; at the same branch it is a conflict.
    let ok = h
        .call(
            &token,
            Method::PUT,
            &format!("/api/v1/registers/{}", new_id()),
            Some(register_body(&bdg, "R2", None)),
        )
        .await;
    assert_eq!(ok.status, StatusCode::CREATED);
    h.call(
        &token,
        Method::PUT,
        &format!("/api/v1/registers/{}", new_id()),
        Some(register_body(&jkt, "R1", None)),
    )
    .await
    .assert_problem(409, "conflict");
    h.call(
        &token,
        Method::PUT,
        &format!("/api/v1/registers/{}", new_id()),
        Some(register_body(&jkt, "R9", Some("x"))),
    )
    .await
    .assert_problem(400, "invalid_body");
    h.call(
        &token,
        Method::GET,
        "/api/v1/registers?branch_id=nope",
        None,
    )
    .await
    .assert_problem(400, "invalid_body");
    h.call(
        &token,
        Method::GET,
        &format!("/api/v1/registers/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(
        &token,
        Method::DELETE,
        &format!("/api/v1/registers/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    assert_eq!(h.outbox_count().await, outbox + 1);
}

async fn location_crud(h: &Harness) {
    let token = h.owner_token().await;
    let jkt = h.jkt.id.to_string();
    let id = new_id();
    let uri = format!("/api/v1/locations/{id}");
    let body = json!({ "branch_id": jkt, "name": "Gudang" });
    let created = h.call(&token, Method::PUT, &uri, Some(body.clone())).await;
    assert_eq!(created.status, StatusCode::CREATED, "{}", created.body);
    assert_eq!(created.body["receiving_default"], false);
    let replay = h.call(&token, Method::PUT, &uri, Some(body)).await;
    assert_eq!(
        (replay.status, &replay.body),
        (StatusCode::OK, &created.body)
    );
    assert_eq!(h.audit_count("location.save").await, 1);
    assert_eq!(
        h.call(&token, Method::GET, &uri, None).await.body,
        created.body
    );

    let list = h
        .call(
            &token,
            Method::GET,
            &format!("/api/v1/locations?branch_id={jkt}"),
            None,
        )
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    let archived = h.call(&token, Method::DELETE, &uri, None).await;
    assert_eq!(archived.body["archived"], true);
    assert_eq!(h.audit_count("location.archive").await, 1);

    h.call(
        &token,
        Method::PUT,
        &uri,
        Some(json!({ "branch_id": h.bdg.id.to_string(), "name": "Gudang" })),
    )
    .await
    .assert_problem(422, "invalid_field");
    h.call(
        &token,
        Method::PUT,
        &format!("/api/v1/locations/{}", new_id()),
        Some(json!({ "branch_id": jkt, "name": "Gudang" })),
    )
    .await
    .assert_problem(409, "conflict");
    h.call(
        &token,
        Method::GET,
        &format!("/api/v1/locations/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(
        &token,
        Method::DELETE,
        &format!("/api/v1/locations/{}", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(&token, Method::GET, "/api/v1/locations?branch_id=x", None)
        .await
        .assert_problem(400, "invalid_body");
}

/// A Manager (no `org.manage`) is denied every org write; nothing is written or audited.
async fn org_writes_need_org_manage(h: &Harness) {
    let token = h.manager_token().await;
    let outbox = h.outbox_count().await;
    let jkt = h.jkt.id.to_string();
    let reg = h.jkt_register.id;
    let cases = [
        (
            Method::PUT,
            format!("/api/v1/branches/{}", new_id()),
            Some(branch_body("X1")),
        ),
        (Method::DELETE, format!("/api/v1/branches/{jkt}"), None),
        (
            Method::PUT,
            format!("/api/v1/registers/{}", new_id()),
            Some(register_body(&jkt, "R7", None)),
        ),
        (Method::DELETE, format!("/api/v1/registers/{reg}"), None),
        (
            Method::PUT,
            format!("/api/v1/locations/{}", new_id()),
            Some(json!({ "branch_id": jkt, "name": "x" })),
        ),
        (
            Method::DELETE,
            format!("/api/v1/locations/{}", new_id()),
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/registers/{reg}/pairing-code"),
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/registers/{reg}/unpair"),
            None,
        ),
    ];
    for (method, uri, body) in cases {
        let r = h.call(&token, method, &uri, body).await;
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], "org.manage", "{uri}");
    }
    assert_eq!(h.outbox_count().await, outbox);
    // Without a session at all: 401.
    h.send(
        Method::PUT,
        &format!("/api/v1/branches/{}", new_id()),
        &[],
        Some(branch_body("X1")),
    )
    .await
    .assert_problem(401, "invalid_session");
}

/// Spec 2.1 §3: lookups only show branches the user is assigned to.
async fn reads_show_only_assigned_branches(h: &Harness) {
    let manager = h.manager_token().await;
    let list = h
        .call(&manager, Method::GET, "/api/v1/branches", None)
        .await;
    let codes: Vec<&str> = list
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["JKT01"]);
    h.call(
        &manager,
        Method::GET,
        &format!("/api/v1/branches/{}", h.bdg.id),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    let ok = h
        .call(
            &manager,
            Method::GET,
            &format!("/api/v1/branches/{}", h.jkt.id),
            None,
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK);

    let registers = h
        .call(&manager, Method::GET, "/api/v1/registers", None)
        .await;
    assert_eq!(registers.body.as_array().unwrap().len(), 1);
    assert_eq!(registers.body[0]["branch_id"], h.jkt.id.to_string());
    h.call(
        &manager,
        Method::GET,
        &format!("/api/v1/registers/{}", h.bdg_register.id),
        None,
    )
    .await
    .assert_problem(404, "not_found");

    // A location at BDG01 is invisible to the JKT01 manager.
    let owner = h.owner_token().await;
    let bdg_location = new_id();
    h.call(
        &owner,
        Method::PUT,
        &format!("/api/v1/locations/{bdg_location}"),
        Some(json!({ "branch_id": h.bdg.id.to_string(), "name": "Gudang" })),
    )
    .await;
    let locations = h
        .call(&manager, Method::GET, "/api/v1/locations", None)
        .await;
    assert_eq!(locations.body, json!([]));
    h.call(
        &manager,
        Method::GET,
        &format!("/api/v1/locations/{bdg_location}"),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    let all = h.call(&owner, Method::GET, "/api/v1/locations", None).await;
    assert_eq!(all.body.as_array().unwrap().len(), 1);
}
