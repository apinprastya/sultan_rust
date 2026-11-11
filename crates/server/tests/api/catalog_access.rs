//! Who may write and read the catalog (spec 2.2 §11, §14 server cases 17, 19, 20; phase C3).

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::access::BranchScope;
use sultan_db::seed;
use sultan_server::{NodeRole, router};

use crate::catalog_fixtures::{as_device, create, simple};
use crate::common::{ANI_PIN, Harness, api_tests, new_id};

api_tests!(
    every_write_needs_hq_and_catalog_manage,
    archive_hides_and_archived_reads_need_view_archived,
);

/// Every catalog write route with a body that would succeed for the Owner.
fn writes(product: &str) -> Vec<(Method, String, Option<Value>)> {
    let doc = simple("PERM1", "PERM1");
    vec![
        (
            Method::PUT,
            format!("/api/v1/products/{}", new_id()),
            Some(doc.body),
        ),
        (
            Method::POST,
            format!("/api/v1/products/{product}/archive"),
            None,
        ),
        (
            Method::POST,
            format!("/api/v1/products/{product}/unarchive"),
            None,
        ),
        (
            Method::PUT,
            format!("/api/v1/products/{product}/branch-blocks"),
            Some(json!({ "branch_ids": [] })),
        ),
        (
            Method::PUT,
            format!("/api/v1/categories/{}", new_id()),
            Some(json!({ "name": "C" })),
        ),
        (
            Method::PUT,
            format!("/api/v1/brands/{}", new_id()),
            Some(json!({ "name": "B" })),
        ),
        (
            Method::PUT,
            format!("/api/v1/products/{product}/image"),
            None,
        ),
        (
            Method::DELETE,
            format!("/api/v1/products/{product}/image"),
            None,
        ),
        (Method::DELETE, format!("/api/v1/products/{product}"), None),
        (
            Method::DELETE,
            format!("/api/v1/categories/{}", new_id()),
            None,
        ),
        (Method::DELETE, format!("/api/v1/brands/{}", new_id()), None),
        (
            Method::PUT,
            "/api/v1/settings/catalog".to_owned(),
            Some(json!({
                "product_code_prefix": "P", "internal_barcode_prefix": "29",
                "scale_layouts": [], "unit_names": [],
            })),
        ),
        (
            Method::POST,
            "/api/v1/catalog/generate/product-code".to_owned(),
            None,
        ),
        (
            Method::POST,
            "/api/v1/catalog/generate/barcode".to_owned(),
            None,
        ),
    ]
}

/// §14 item 17: a branch node refuses every catalog write (`read_only_replica`); a user without
/// `catalog.manage` gets `permission_denied`. Since C3b (§11, decided 2026-11-11) any role
/// assignment counts: the Manager or Stock clerk at JKT01 may write, like one for all branches.
async fn every_write_needs_hq_and_catalog_manage(h: &Harness) {
    let owner = h.owner_token().await;
    let product = simple("BASE1", "BASE1");
    create(h, &owner, &product).await;
    // A Cashier with a password (back office), assigned at JKT01: no `catalog.manage`.
    h.add_user("kasir", seed::CASHIER_ROLE, BranchScope::Branch(h.jkt.id))
        .await;
    let kasir = h
        .password_token("kasir", crate::common::BUDI_PASSWORD)
        .await;
    let ani = h.pin_token(&h.jkt_device, &h.ani, ANI_PIN).await;
    let branch_node = || router(h.state(NodeRole::Branch));
    let before = h.outbox_count().await;
    for (method, uri, body) in writes(&product.id) {
        let bearer = format!("Bearer {owner}");
        let r = Harness::send_to(
            branch_node(),
            method.clone(),
            &uri,
            &[("authorization", &bearer)],
            body.clone(),
        )
        .await;
        r.assert_problem(403, "read_only_replica");
        let r = h.call(&kasir, method.clone(), &uri, body.clone()).await;
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], "catalog.manage", "{method} {uri}");
        let r = h
            .at_register(
                (&ani, &h.jkt_device),
                method.clone(),
                &uri,
                &[],
                body.clone(),
            )
            .await;
        r.assert_problem(403, "permission_denied");
        // A device token alone never writes.
        let r = h
            .send(
                method.clone(),
                &uri,
                &[("x-register-token", &h.jkt_device)],
                body,
            )
            .await;
        r.assert_problem(401, "invalid_session");
    }
    assert_eq!(
        h.outbox_count().await,
        before,
        "no refused write left a trace"
    );

    // A Manager for all branches, Budi (Manager at JKT01) and a Stock clerk at BDG01 may write.
    h.add_user("mgr-all", seed::MANAGER_ROLE, BranchScope::All)
        .await;
    h.add_user(
        "gudang",
        seed::STOCK_CLERK_ROLE,
        BranchScope::Branch(h.bdg.id),
    )
    .await;
    let mut doc = simple("MGR1", "MGR1");
    for (i, user) in ["mgr-all", "budi", "gudang"].into_iter().enumerate() {
        let token = h.password_token(user, crate::common::BUDI_PASSWORD).await;
        doc = simple(&format!("MGR{i}"), &format!("MGR{i}"));
        let r = crate::catalog_fixtures::put_product(h, &token, &doc.id, &doc.body).await;
        assert_eq!(r.status.as_u16(), 201, "{user}: {}", r.body);
    }
    // Reads need no permission: the Cashier reads the product it may not write.
    let r = h
        .at_register(
            (&ani, &h.jkt_device),
            Method::GET,
            &format!("/api/v1/products/{}", doc.id),
            &[],
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
}

/// §14 items 19 + 20: archive → lookup `archived`, search hides it, the list shows it only with
/// `archived=true`; archived reads need `catalog.view_archived` (any role assignment).
async fn archive_hides_and_archived_reads_need_view_archived(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = simple("ARSIP", "ARSIP-1");
    create(h, &owner, &doc).await;
    let archive = format!("/api/v1/products/{}/archive", doc.id);
    let outbox = h.outbox_count().await;
    let r = h.call(&owner, Method::POST, &archive, None).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(r.body["archived"], true);
    assert_eq!(r.body["archived_at"], h.now().unix_millis());
    assert_eq!(h.audit_count("catalog.product.archive").await, 1);
    assert_eq!(h.outbox_count().await, outbox + 1);
    // Archiving twice is a no-op.
    let again = h.call(&owner, Method::POST, &archive, None).await;
    assert_eq!(again.body, r.body);
    assert_eq!(h.audit_count("catalog.product.archive").await, 1);

    as_device(h, &h.jkt_device, "/api/v1/catalog/lookup?code=ARSIP-1")
        .await
        .assert_problem(422, "archived");
    let found = as_device(h, &h.jkt_device, "/api/v1/catalog/search?q=arsip").await;
    assert_eq!(found.body["items"], json!([]));
    let active = h.call(&owner, Method::GET, "/api/v1/products", None).await;
    assert_eq!(active.body["items"], json!([]));
    let archived = h
        .call(&owner, Method::GET, "/api/v1/products?archived=true", None)
        .await;
    assert_eq!(archived.body["items"][0]["id"], doc.id);
    assert!(archived.body["items"][0]["archived_at"].is_i64());

    // `catalog.view_archived`: the Owner has it everywhere, Budi at JKT01 (enough since C3b);
    // the Stock clerk has `catalog.manage` but not this; a device never.
    let budi = h.manager_token().await;
    let uri = format!("/api/v1/products/{}", doc.id);
    for token in [&owner, &budi] {
        assert_eq!(
            h.call(token, Method::GET, &uri, None).await.status,
            StatusCode::OK
        );
    }
    let r = h
        .call(&budi, Method::GET, "/api/v1/products?archived=true", None)
        .await;
    assert_eq!(r.body["items"][0]["id"], doc.id);
    h.add_user(
        "gudang",
        seed::STOCK_CLERK_ROLE,
        BranchScope::Branch(h.jkt.id),
    )
    .await;
    let clerk = h
        .password_token("gudang", crate::common::BUDI_PASSWORD)
        .await;
    for r in [
        as_device(h, &h.jkt_device, &uri).await,
        as_device(h, &h.jkt_device, "/api/v1/products?archived=true").await,
        h.call(&clerk, Method::GET, &uri, None).await,
        h.call(&clerk, Method::GET, "/api/v1/products?archived=true", None)
            .await,
    ] {
        r.assert_problem(403, "permission_denied");
        assert_eq!(r.body["permission"], "catalog.view_archived");
    }

    let r = h
        .call(
            &owner,
            Method::POST,
            &format!("/api/v1/products/{}/unarchive", doc.id),
            None,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["archived_at"], Value::Null);
    assert_eq!(h.audit_count("catalog.product.unarchive").await, 1);
    let found = as_device(h, &h.jkt_device, "/api/v1/catalog/lookup?code=ARSIP-1").await;
    assert_eq!(found.status, StatusCode::OK, "{}", found.body);
    h.call(
        &owner,
        Method::POST,
        &format!("/api/v1/products/{}/archive", new_id()),
        None,
    )
    .await
    .assert_problem(404, "not_found");
    h.call(&owner, Method::POST, "/api/v1/products/nope/archive", None)
        .await
        .assert_problem(400, "invalid_body");
}
