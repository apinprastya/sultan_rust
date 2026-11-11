//! `DELETE /products/{id}`, `/categories/{id}`, `/brands/{id}` (spec 2.2 §9, §17 item 1, C3b):
//! a never-used row is hard-deleted (`204`, audited, outbox, `catalog.changed`); a used one is
//! `409 in_use`. Permissions and `read_only_replica` are in `catalog_access`.

use axum::http::{Method, StatusCode};
use serde_json::json;
use sultan_core::ids::VariantId;

use crate::catalog_fixtures::{create, put_brand, put_category, simple};
use crate::common::{Harness, api_tests, new_id};

api_tests!(
    never_used_products_are_deleted,
    categories_and_brands_are_deleted_when_unused,
);

async fn delete(h: &Harness, token: &str, uri: &str) -> crate::common::Response {
    h.call(token, Method::DELETE, uri, None).await
}

async fn never_used_products_are_deleted(h: &Harness) {
    let owner = h.owner_token().await;
    let typo = simple("TYPO1", "TYPO-1");
    create(h, &owner, &typo).await;
    let uri = format!("/api/v1/products/{}", typo.id);
    let mut events = h.events.subscribe();
    let outbox = h.outbox_count().await;
    let r = delete(h, &owner, &uri).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    assert_eq!(h.audit_count("catalog.product.delete").await, 1);
    assert_eq!(h.outbox_count().await, outbox + 1);
    let event = events.try_recv().unwrap();
    assert_eq!(event.kind, "catalog.changed");
    assert_eq!(
        event.data,
        json!({ "scope": "product", "product_id": typo.id })
    );
    h.call(&owner, Method::GET, &uri, None)
        .await
        .assert_problem(404, "not_found");
    // The barcode is free again.
    create(h, &owner, &simple("TYPO2", "TYPO-1")).await;
    // Deleting again, or an unknown ID: `404`; a bad ID: `400`.
    delete(h, &owner, &uri)
        .await
        .assert_problem(404, "not_found");
    delete(h, &owner, "/api/v1/products/nope")
        .await
        .assert_problem(400, "invalid_body");

    // A product whose variant was sold (marked used) is `409 in_use`; archive it instead.
    let sold = simple("SOLD1", "SOLD-1");
    create(h, &owner, &sold).await;
    let variant: VariantId = sold.variants[0].parse().unwrap();
    sultan_db::catalog::mark_used(&h.db, &[variant], h.now())
        .await
        .unwrap();
    let uri = format!("/api/v1/products/{}", sold.id);
    let outbox = h.outbox_count().await;
    let r = delete(h, &owner, &uri).await;
    r.assert_problem(409, "in_use");
    assert_eq!(r.body["reason"], "used");
    assert_eq!(h.outbox_count().await, outbox);
    let r = h.call(&owner, Method::GET, &uri, None).await;
    assert_eq!(r.body["used"], true);
}

async fn categories_and_brands_are_deleted_when_unused(h: &Harness) {
    let owner = h.owner_token().await;
    let parent = new_id();
    let child = new_id();
    let brand = new_id();
    put_category(h, &owner, &parent, json!({ "name": "Makanan" })).await;
    put_category(
        h,
        &owner,
        &child,
        json!({ "name": "Snack", "parent_id": parent }),
    )
    .await;
    put_brand(h, &owner, &brand, "Aqua").await;
    let mut doc = simple("CAT1", "CAT-1");
    doc.body["category_id"] = child.clone().into();
    doc.body["brand_id"] = brand.clone().into();
    create(h, &owner, &doc).await;

    let cases = [
        (format!("/api/v1/categories/{parent}"), "has_children"),
        (format!("/api/v1/categories/{child}"), "has_products"),
        (format!("/api/v1/brands/{brand}"), "has_products"),
    ];
    for (uri, reason) in &cases {
        let r = delete(h, &owner, uri).await;
        r.assert_problem(409, "in_use");
        assert_eq!(r.body["reason"], *reason, "{uri}");
    }

    // Once the product is gone, all three may go: child first, then the parent.
    let r = delete(h, &owner, &format!("/api/v1/products/{}", doc.id)).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "{}", r.body);
    let mut events = h.events.subscribe();
    for (uri, _) in [&cases[1], &cases[0], &cases[2]] {
        let r = delete(h, &owner, uri).await;
        assert_eq!(r.status, StatusCode::NO_CONTENT, "{uri}: {}", r.body);
        h.call(&owner, Method::GET, uri, None)
            .await
            .assert_problem(404, "not_found");
    }
    assert_eq!(h.audit_count("catalog.category.delete").await, 2);
    assert_eq!(h.audit_count("catalog.brand.delete").await, 1);
    let scopes: Vec<_> = (0..3)
        .map(|_| events.try_recv().unwrap().data["scope"].clone())
        .collect();
    assert_eq!(scopes, ["category", "category", "brand"]);
    delete(h, &owner, &format!("/api/v1/brands/{brand}"))
        .await
        .assert_problem(404, "not_found");
}
