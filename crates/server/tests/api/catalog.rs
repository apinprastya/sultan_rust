//! Catalog products, categories, brands, settings and generators over HTTP (spec 2.2 §12, §14
//! server cases 15, 16, 20; phase C3).

use axum::http::{Method, StatusCode};
use serde_json::{Value, json};
use sultan_core::ids::VariantId;
use sultan_db::Db;

use crate::catalog_fixtures::{
    STANDARD, as_device, create, ean13, kaos, put_brand, put_category, put_product, simple,
};
use crate::common::{Harness, api_tests, new_id};

api_tests!(
    product_create_replay_update,
    duplicates_are_409_with_the_conflicting_variant,
    rule_failures_are_422_with_every_error,
    frozen_after_use_and_bundles,
    list_products_filters_and_pages,
    categories,
    brands,
    settings_and_generators,
    racing_catalog_writes,
);

/// §14 item 15 + 20: create → replay → no duplicates; the update diff is applied atomically,
/// with one audit row and one outbox row per write.
async fn product_create_replay_update(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = kaos("KAOS01", "899100");
    let outbox = h.outbox_count().await;
    let created = create(h, &owner, &doc).await;
    assert_eq!(created["id"], doc.id);
    assert_eq!(created["code"], "KAOS01");
    assert_eq!(created["receipt_name"], "Kaos Polos");
    assert_eq!(created["tags"], json!(["promo"]));
    assert_eq!(created["purchasable"], true);
    assert_eq!(created["used"], false);
    assert_eq!(created["archived_at"], Value::Null);
    assert_eq!(created["default_sale_unit_id"], doc.base_unit);
    assert_eq!(created["units"][1]["factor_thousandths"], 24_000);
    let skus: Vec<&str> = created["variants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["sku"].as_str().unwrap())
        .collect();
    assert_eq!(
        skus,
        [
            "KAOS01-S-RED",
            "KAOS01-S-BLUE",
            "KAOS01-M-RED",
            "KAOS01-M-BLUE"
        ]
    );
    assert_eq!(created["barcodes"].as_array().unwrap().len(), 5);
    assert_eq!(h.audit_count("catalog.product.put").await, 1);
    assert_eq!(h.outbox_count().await, outbox + 1);

    // Replay: 200 with the same body, nothing written.
    let replay = put_product(h, &owner, &doc.id, &doc.body).await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(replay.body, created);
    assert_eq!(h.audit_count("catalog.product.put").await, 1);
    assert_eq!(h.outbox_count().await, outbox + 1);

    let uri = format!("/api/v1/products/{}", doc.id);
    let got = h.call(&owner, Method::GET, &uri, None).await;
    assert_eq!(got.body, created);
    // A register reads it with its device token alone.
    assert_eq!(as_device(h, &h.jkt_device, &uri).await.body, created);

    // Update: rename, drop the last variant (never used → deleted) and its barcode.
    let mut body = doc.body.clone();
    body["name"] = "Kaos Polos Premium".into();
    body["variants"].as_array_mut().unwrap().pop();
    body["barcodes"].as_array_mut().unwrap().remove(3);
    let updated = put_product(h, &owner, &doc.id, &body).await;
    assert_eq!(updated.status, StatusCode::OK, "{}", updated.body);
    assert_eq!(updated.body["name"], "Kaos Polos Premium");
    assert_eq!(updated.body["variants"].as_array().unwrap().len(), 3);
    assert_eq!(updated.body["barcodes"].as_array().unwrap().len(), 4);
    assert_eq!(h.audit_count("catalog.product.put").await, 2);
    assert_eq!(h.outbox_count().await, outbox + 2);

    // The product ID must be a UUIDv7; unknown products are 404.
    let r = put_product(h, &owner, "550e8400-e29b-41d4-a716-446655440000", &body).await;
    r.assert_problem(400, "invalid_body");
    let r = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/products/{}", new_id()),
            None,
        )
        .await;
    r.assert_problem(404, "not_found");
    let r = h
        .call(&owner, Method::GET, "/api/v1/products/nope", None)
        .await;
    r.assert_problem(400, "invalid_body");
    // Without any credentials a read is 401.
    h.send(Method::GET, &uri, &[], None)
        .await
        .assert_problem(401, "invalid_session");
}

/// §14 item 16.
async fn duplicates_are_409_with_the_conflicting_variant(h: &Harness) {
    let owner = h.owner_token().await;
    let (taken, free) = (ean13("899123456789"), ean13("899123456790"));
    let a = simple("AQUA600", &taken);
    let created = create(h, &owner, &a).await;
    let a_variant = created["variants"][0]["id"].clone();

    let b = simple("AQUA1500", &taken);
    let r = put_product(h, &owner, &b.id, &b.body).await;
    r.assert_problem(409, "duplicate_barcode");
    assert_eq!(r.body["product_id"], a.id);
    assert_eq!(r.body["variant_id"], a_variant);
    assert_eq!(r.body["value"], taken);
    assert_eq!(r.body["field"], "barcodes.code");

    let mut b = simple("AQUA1500", &free);
    b.body["variants"][0]["sku"] = "aqua600".into();
    let r = put_product(h, &owner, &b.id, &b.body).await;
    r.assert_problem(409, "duplicate_sku");
    assert_eq!(r.body["variant_id"], a_variant);
    assert_eq!(r.body["value"], "AQUA600");

    let b = simple("AQUA600", &free);
    let r = put_product(h, &owner, &b.id, &b.body).await;
    r.assert_problem(409, "duplicate_code");
    assert_eq!(r.body["product_id"], a.id);
    assert_eq!(r.body["variant_id"], Value::Null);
    // Nothing of the rejected writes was kept.
    assert_eq!(h.audit_count("catalog.product.put").await, 1);
}

/// 422 with an `errors` list of every failure; body-level parse errors.
async fn rule_failures_are_422_with_every_error(h: &Harness) {
    let owner = h.owner_token().await;
    let valid = ean13("899123456789");
    let last = valid.as_bytes()[12] - b'0';
    let wrong = format!("{}{}", &valid[..12], (last + 1) % 10);
    let mut doc = simple("BAD1", &wrong);
    doc.body["name"] = "  ".into();
    doc.body["units"][0]["factor_thousandths"] = 1_500.into();
    let r = put_product(h, &owner, &doc.id, &doc.body).await;
    assert_eq!(r.status.as_u16(), 422, "{}", r.body);
    let errors = r.body["errors"].as_array().unwrap();
    let codes: Vec<&str> = errors.iter().map(|e| e["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"invalid_field"), "{codes:?}");
    assert!(codes.contains(&"invalid_unit_factor"), "{codes:?}");
    assert!(codes.contains(&"invalid_check_digit"), "{codes:?}");
    assert_eq!(r.body["code"], codes[0]);
    assert!(errors.iter().all(|e| e["message"].is_string()));
    let check = errors
        .iter()
        .find(|e| e["code"] == "invalid_check_digit")
        .unwrap();
    assert_eq!(check["field"], "barcodes.code");

    // Enum values and IDs in the body.
    let cases: [(&str, Value, u16, &str); 6] = [
        ("type", json!("perishable"), 422, "invalid_field"),
        ("tracking", json!("lot"), 422, "invalid_field"),
        ("tax_class_id", json!("nope"), 400, "invalid_body"),
        ("category_id", json!(new_id()), 422, "invalid_field"),
        ("tracking", json!("serial"), 422, "tracking_not_supported"),
        ("default_sale_unit_id", json!("x"), 400, "invalid_body"),
    ];
    for (field, value, status, code) in cases {
        let mut doc = simple("BAD2", "BAD2");
        doc.body[field] = value;
        put_product(h, &owner, &doc.id, &doc.body)
            .await
            .assert_problem(status, code);
    }
    let mut doc = simple("BAD3", "BAD3");
    doc.body["units"][0]["kind"] = "liquid".into();
    put_product(h, &owner, &doc.id, &doc.body)
        .await
        .assert_problem(422, "invalid_field");
    let mut doc = simple("BAD4", "BAD4");
    doc.body["variants"][0]["plu"] = 1_000_000.into();
    put_product(h, &owner, &doc.id, &doc.body)
        .await
        .assert_problem(422, "invalid_field");
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/products/{}", new_id()),
            Some(json!({})),
        )
        .await;
    assert_eq!(r.body["code"], "invalid_body");
}

/// "Frozen after use" through the API, bundle rules, and a weighed product with a PLU.
async fn frozen_after_use_and_bundles(h: &Harness) {
    let owner = h.owner_token().await;
    let doc = simple("TEH01", "TEH01");
    let created = create(h, &owner, &doc).await;
    let variant: VariantId = doc.variants[0].parse().unwrap();
    sultan_db::catalog::mark_used(&h.db, &[variant], h.now())
        .await
        .unwrap();
    let got = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/products/{}", doc.id),
            None,
        )
        .await;
    assert_eq!(got.body["used"], true);
    let mut body = doc.body.clone();
    body["type"] = "service".into();
    let r = put_product(h, &owner, &doc.id, &body).await;
    r.assert_problem(422, "frozen_after_use");
    assert_eq!(r.body["field"], "type");
    assert_eq!(r.body["errors"][0]["field"], "type");
    // Renaming is still allowed.
    body["type"] = "stock".into();
    body["name"] = "Teh Botol".into();
    let r = put_product(h, &owner, &doc.id, &body).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_ne!(r.body, created);

    // A bundle of the tea; a bundle inside a bundle is `bundle_nesting`.
    let bundle_id = new_id();
    let bundle_body = |component_variant: &str, component_unit: &str| {
        let pcs = new_id();
        json!({
            "code": "", "name": "Paket", "type": "bundle", "tax_class_id": STANDARD,
            "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
            "base_unit_id": pcs,
            "variants": [{ "id": new_id() }],
            "components": [{ "variant_id": component_variant, "unit_id": component_unit,
                             "qty_thousandths": 2000 }],
        })
    };
    let r = put_product(
        h,
        &owner,
        &bundle_id,
        &bundle_body(&doc.variants[0], &doc.base_unit),
    )
    .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    assert_eq!(r.body["code"], "P000001", "generated code");
    assert_eq!(r.body["purchasable"], false);
    assert_eq!(r.body["components"][0]["qty_thousandths"], 2000);
    let inner_variant = r.body["variants"][0]["id"].as_str().unwrap().to_owned();
    let inner_unit = r.body["base_unit_id"].as_str().unwrap().to_owned();
    let nested = put_product(
        h,
        &owner,
        &new_id(),
        &bundle_body(&inner_variant, &inner_unit),
    )
    .await;
    nested.assert_problem(422, "bundle_nesting");
    assert_eq!(nested.body["field"], "components");

    // Weighed with a PLU.
    let (kg, v) = (new_id(), new_id());
    let cheese = json!({
        "code": "KEJU", "name": "Keju", "type": "weighed", "tax_class_id": STANDARD,
        "units": [{ "id": kg, "name": "kg", "kind": "decimal", "factor_thousandths": 1000 }],
        "base_unit_id": kg,
        "variants": [{ "id": v, "plu": 123 }],
    });
    let r = put_product(h, &owner, &new_id(), &cheese).await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    assert_eq!(r.body["variants"][0]["plu"], 123);
}

async fn list_products_filters_and_pages(h: &Harness) {
    let owner = h.owner_token().await;
    let category = new_id();
    put_category(h, &owner, &category, json!({ "name": "Minuman" })).await;
    let brand = new_id();
    put_brand(h, &owner, &brand, "Aqua").await;
    let mut ids = Vec::new();
    for (i, code) in ["LA1", "LA2", "LA3"].into_iter().enumerate() {
        let mut doc = simple(code, &format!("LIST-{code}"));
        if i == 0 {
            doc.body["category_id"] = category.clone().into();
            doc.body["brand_id"] = brand.clone().into();
            doc.body["name"] = "Air Mineral".into();
        }
        create(h, &owner, &doc).await;
        ids.push(doc.id);
    }
    let page = h
        .call(&owner, Method::GET, "/api/v1/products?limit=2", None)
        .await;
    assert_eq!(page.status, StatusCode::OK);
    let items = page.body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], ids[2], "newest first");
    assert_eq!(items[0]["type"], "stock");
    let next = page.body["next_before_id"].as_str().unwrap();
    let rest = h
        .call(
            &owner,
            Method::GET,
            &format!("/api/v1/products?limit=2&before_id={next}"),
            None,
        )
        .await;
    assert_eq!(rest.body["items"].as_array().unwrap().len(), 1);
    assert_eq!(rest.body["next_before_id"], Value::Null);

    for query in [
        format!("category_id={category}"),
        format!("brand_id={brand}"),
        "q=mineral".to_owned(),
        "q=aqua".to_owned(),
    ] {
        let r = as_device(h, &h.jkt_device, &format!("/api/v1/products?{query}")).await;
        let items = r.body["items"].as_array().unwrap();
        assert_eq!(items.len(), 1, "{query}: {}", r.body);
        assert_eq!(items[0]["id"], ids[0]);
        assert_eq!(items[0]["category_id"], category);
    }
    let r = h
        .call(&owner, Method::GET, "/api/v1/products?type=service", None)
        .await;
    assert_eq!(r.body["items"], json!([]));
    for (query, status, code) in [
        ("type=x", 422, "invalid_field"),
        ("limit=0", 422, "invalid_field"),
        ("limit=51", 422, "invalid_field"),
        ("before_id=x", 400, "invalid_body"),
        ("limit=x", 400, "invalid_query"),
    ] {
        h.call(
            &owner,
            Method::GET,
            &format!("/api/v1/products?{query}"),
            None,
        )
        .await
        .assert_problem(status, code);
    }

    let r = as_device(h, &h.jkt_device, "/api/v1/tax-classes").await;
    let names: Vec<&str> = r
        .body
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["Exempt", "Standard"]);
    assert_eq!(r.body[1]["id"], STANDARD);
}

async fn categories(h: &Harness) {
    let owner = h.owner_token().await;
    let (root, child) = (new_id(), new_id());
    let r = put_category(
        h,
        &owner,
        &root,
        json!({ "name": " Makanan ", "code": "mkn" }),
    )
    .await;
    assert_eq!(r.status.as_u16(), 201, "{}", r.body);
    assert_eq!(
        r.body,
        json!({ "id": root, "parent_id": null, "name": "Makanan", "code": "MKN", "archived": false })
    );
    let r = put_category(
        h,
        &owner,
        &child,
        json!({ "name": "Snack", "parent_id": root }),
    )
    .await;
    assert_eq!(r.status.as_u16(), 201);
    let replay = put_category(
        h,
        &owner,
        &child,
        json!({ "name": "Snack", "parent_id": root }),
    )
    .await;
    assert_eq!(replay.status, StatusCode::OK);
    assert_eq!(h.audit_count("catalog.category.put").await, 2);
    // Moving the root under its child is a cycle.
    put_category(
        h,
        &owner,
        &root,
        json!({ "name": "Makanan", "parent_id": child }),
    )
    .await
    .assert_problem(422, "category_cycle");
    put_category(
        h,
        &owner,
        &new_id(),
        json!({ "name": "Snack", "parent_id": root }),
    )
    .await
    .assert_problem(422, "duplicate_category_name");
    put_category(
        h,
        &owner,
        &new_id(),
        json!({ "name": "X", "parent_id": new_id() }),
    )
    .await
    .assert_problem(422, "unknown_category");

    let tree = as_device(h, &h.jkt_device, "/api/v1/categories").await;
    assert_eq!(tree.body[0]["id"], root);
    assert_eq!(tree.body[0]["children"][0]["id"], child);
    assert_eq!(tree.body[0]["children"][0]["children"], json!([]));
    let got = as_device(h, &h.jkt_device, &format!("/api/v1/categories/{child}")).await;
    assert_eq!(got.body["parent_id"], root);
    as_device(
        h,
        &h.jkt_device,
        &format!("/api/v1/categories/{}", new_id()),
    )
    .await
    .assert_problem(404, "not_found");

    // Archived: hidden from the tree; `include_archived` needs `catalog.view_archived`.
    put_category(
        h,
        &owner,
        &child,
        json!({ "name": "Snack", "parent_id": root, "archived": true }),
    )
    .await;
    let tree = h
        .call(&owner, Method::GET, "/api/v1/categories", None)
        .await;
    assert_eq!(tree.body[0]["children"], json!([]));
    let all = h
        .call(
            &owner,
            Method::GET,
            "/api/v1/categories?include_archived=true",
            None,
        )
        .await;
    assert_eq!(all.body[0]["children"][0]["archived"], true);
    as_device(h, &h.jkt_device, "/api/v1/categories?include_archived=true")
        .await
        .assert_problem(403, "permission_denied");
}

async fn brands(h: &Harness) {
    let owner = h.owner_token().await;
    let brand = new_id();
    let r = put_brand(h, &owner, &brand, "Indomie").await;
    assert_eq!(r.status.as_u16(), 201);
    assert_eq!(
        r.body,
        json!({ "id": brand, "name": "Indomie", "archived": false })
    );
    assert_eq!(
        put_brand(h, &owner, &brand, "Indomie").await.status,
        StatusCode::OK
    );
    put_brand(h, &owner, &new_id(), "INDOMIE")
        .await
        .assert_problem(422, "duplicate_brand_name");
    let list = as_device(h, &h.bdg_device, "/api/v1/brands").await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    let got = as_device(h, &h.bdg_device, &format!("/api/v1/brands/{brand}")).await;
    assert_eq!(got.body["name"], "Indomie");
    as_device(h, &h.bdg_device, &format!("/api/v1/brands/{}", new_id()))
        .await
        .assert_problem(404, "not_found");
    let r = h
        .call(
            &owner,
            Method::PUT,
            &format!("/api/v1/brands/{brand}"),
            Some(json!({ "name": "Indomie", "archived": true })),
        )
        .await;
    assert_eq!(r.body["archived"], true);
    let list = h.call(&owner, Method::GET, "/api/v1/brands", None).await;
    assert_eq!(list.body, json!([]));
    let list = h
        .call(
            &owner,
            Method::GET,
            "/api/v1/brands?include_archived=true",
            None,
        )
        .await;
    assert_eq!(list.body.as_array().unwrap().len(), 1);
    as_device(h, &h.bdg_device, "/api/v1/brands?include_archived=true")
        .await
        .assert_problem(403, "permission_denied");
    assert_eq!(h.audit_count("catalog.brand.put").await, 2);
}

async fn settings_and_generators(h: &Harness) {
    let owner = h.owner_token().await;
    let r = h
        .call(&owner, Method::GET, "/api/v1/settings/catalog", None)
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body["product_code_prefix"], "P");
    assert_eq!(r.body["internal_barcode_prefix"], "29");
    assert_eq!(
        r.body["scale_layouts"][0],
        json!({ "prefix": 21, "plu_digits": 5, "value_digits": 5, "kind": "weight" })
    );
    // Settings need a session; a device token alone is not enough.
    as_device(h, &h.jkt_device, "/api/v1/settings/catalog")
        .await
        .assert_problem(401, "invalid_session");

    let mut body = r.body.clone();
    body["internal_barcode_prefix"] = "21".into();
    let bad = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/catalog",
            Some(body.clone()),
        )
        .await;
    bad.assert_problem(422, "scale_prefix_overlap");
    assert_eq!(bad.body["field"], "scale_layouts");
    body["internal_barcode_prefix"] = "28".into();
    body["product_code_prefix"] = "sku".into();
    body["unit_names"] = json!(["pcs", "kg"]);
    let ok = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/catalog",
            Some(body.clone()),
        )
        .await;
    assert_eq!(ok.status, StatusCode::OK, "{}", ok.body);
    assert_eq!(ok.body["product_code_prefix"], "SKU");
    let replay = h
        .call(
            &owner,
            Method::PUT,
            "/api/v1/settings/catalog",
            Some(ok.body.clone()),
        )
        .await;
    assert_eq!(replay.body, ok.body);
    assert_eq!(h.audit_count("catalog.settings.put").await, 1);
    body["scale_layouts"][0]["kind"] = "volume".into();
    h.call(&owner, Method::PUT, "/api/v1/settings/catalog", Some(body))
        .await
        .assert_problem(422, "invalid_field");

    // Generators: an Idempotency-Key replays the reserved value; without one each call reserves
    // the next.
    let bearer = format!("Bearer {owner}");
    let generate = |key: Option<&'static str>, what: &'static str| {
        let bearer = bearer.clone();
        async move {
            let mut headers = vec![("authorization", bearer.as_str())];
            if let Some(k) = key {
                headers.push(("idempotency-key", k));
            }
            h.send(
                Method::POST,
                &format!("/api/v1/catalog/generate/{what}"),
                &headers,
                None,
            )
            .await
        }
    };
    let first = generate(Some("gen-1"), "product-code").await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.body);
    assert_eq!(first.body["value"], "SKU000001");
    let again = generate(Some("gen-1"), "product-code").await;
    assert_eq!(again.body, first.body);
    assert_eq!(again.headers["idempotency-replayed"], "true");
    let next = generate(None, "product-code").await;
    assert_eq!(next.body["value"], "SKU000002");
    let barcode = generate(None, "barcode").await;
    let value = barcode.body["value"].as_str().unwrap();
    assert_eq!(value.len(), 13);
    assert!(value.starts_with("28"), "{value}");
    assert_eq!(value, ean13(&value[..12]));
    assert_eq!(h.audit_count("catalog.generate.product_code").await, 2);
}

/// How many writers race in [`racing_catalog_writes`].
const RACERS: usize = 8;

/// Concurrent writes that collide: every request gets its proper answer (the server reruns a
/// transaction that lost a race, with backoff), never a `500`.
async fn racing_catalog_writes(h: &Harness) {
    race_same_sku(h).await;
    race_generators(h).await;
}

/// Sends `RACERS` requests at once, built by `request(n)`, as the owner.
async fn concurrently(
    h: &Harness,
    request: impl Fn(usize) -> (Method, String, Option<Value>),
) -> Vec<crate::common::Response> {
    let bearer = format!("Bearer {}", h.owner_token().await);
    let tasks: Vec<_> = (0..RACERS)
        .map(|n| {
            let (app, bearer) = (h.app(), bearer.clone());
            let (method, uri, body) = request(n);
            tokio::spawn(async move {
                Harness::send_to(app, method, &uri, &[("authorization", &bearer)], body).await
            })
        })
        .collect();
    let mut responses = Vec::new();
    for task in tasks {
        responses.push(task.await.unwrap());
    }
    responses
}

/// Different products with the same SKU: exactly one wins, the others are `409 duplicate_sku`.
async fn race_same_sku(h: &Harness) {
    let docs: Vec<_> = (0..RACERS)
        .map(|n| {
            let barcode = ean13(&format!("8991000000{n:02}"));
            let mut doc = simple(&format!("RACE{n}"), &barcode);
            doc.body["variants"][0]["sku"] = "RACE-SKU".into();
            doc
        })
        .collect();
    let responses = concurrently(h, |n| {
        let doc = &docs[n];
        (
            Method::PUT,
            format!("/api/v1/products/{}", doc.id),
            Some(doc.body.clone()),
        )
    })
    .await;
    let mut statuses = Vec::new();
    for r in &responses {
        if r.status != StatusCode::CREATED {
            r.assert_problem(409, "duplicate_sku");
        }
        statuses.push(r.status.as_u16());
    }
    statuses.sort_unstable();
    let mut expected = vec![409; RACERS];
    expected[0] = 201;
    assert_eq!(statuses, expected);
}

/// Concurrent generators: every request gets its own value.
async fn race_generators(h: &Harness) {
    for path in ["product-code", "barcode"] {
        let responses = concurrently(h, |_| {
            (
                Method::POST,
                format!("/api/v1/catalog/generate/{path}"),
                None,
            )
        })
        .await;
        let mut values: Vec<String> = responses
            .iter()
            .map(|r| {
                assert_eq!(r.status, StatusCode::OK, "{path}: {}", r.body);
                r.body["value"].as_str().unwrap().to_owned()
            })
            .collect();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), RACERS, "{path}: {values:?}");
    }
}

/// The same races on a `SQLite` file with a real connection pool (multi-threaded runtime).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_catalog_writes_on_a_sqlite_file() {
    let dir = std::env::temp_dir().join(format!("sultan-catalog-race-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = Db::connect(&format!("sqlite://{}", dir.join("race.db").display()))
        .await
        .unwrap();
    let h = Harness::with_db(db).await;
    racing_catalog_writes(&h).await;
    h.db.close().await;
    std::fs::remove_dir_all(dir).unwrap();
}
