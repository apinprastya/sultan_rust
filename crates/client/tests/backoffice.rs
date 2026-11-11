//! [`BackOffice`] (phase C4): one contract run against [`HttpService`] on the real router
//! (in-memory `SQLite`, `127.0.0.1:0`) and against [`FakeService`], so the fake answers like the
//! server: password login and `GET /auth/me`, branches, tax classes, settings, brands, the
//! category tree, product documents (create, replay, list, `422 errors[]`, `409 duplicate_*`,
//! `403 permission_denied`), generators, branch blocks, images, search, bundle labels and archive.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::json;
use sultan_client::catalog::{CATALOG_MANAGE, CATALOG_VIEW_ARCHIVED};
use sultan_client::fake::{FakeService, OTHER_BRANCH_ID};
use sultan_client::{
    BackOffice, BrandBody, CategoryBody, ClientError, EventSink, Generate, HttpService, Product,
    ProductQuery, ServerEvent, Service,
};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, ProductId};
use sultan_db::org::{Branch, Company};
use sultan_db::users::User;
use sultan_db::{Db, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router};

const STANDARD: &str = "00000000-0000-7000-8000-000000000101";

/// A 1×1 PNG.
const PNG: [u8; 70] = [
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0xf8, 0xcf, 0x50,
    0x0f, 0x00, 0x03, 0x86, 0x01, 0x80, 0x5a, 0x34, 0x7d, 0x6b, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45,
    0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
];

struct Server {
    root: String,
    branches: [String; 2],
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn user(username: &str, name: &str, password: &str) -> User {
    User {
        id: sultan_core::ids::UserId::generate(),
        username: username.to_owned(),
        display_name: name.to_owned(),
        pin_hash: None,
        password_hash: Some(credentials::hash_password(password).unwrap()),
        active: true,
    }
}

fn branch(company: CompanyId, code: &str, name: &str) -> Branch {
    Branch {
        id: BranchId::generate(),
        company,
        code: code.to_owned(),
        name: name.to_owned(),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    }
}

/// Owner `owner` (all branches), Manager `budi` (JKT01 only) and Cashier `kasir` (JKT01, no
/// catalog permission), as in the fake.
async fn serve() -> Server {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let jkt = branch(company.id, "JKT01", "Toko Jakarta");
    let bdg = branch(company.id, "BDG01", "Toko Bandung");
    let owner = user("owner", "Owner", "owner-pass-1");
    let budi = user("budi", "Budi", "budi-pass-1");
    let kasir = user("kasir", "Kasir", "kasir-pass-1");
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    for b in [&jkt, &bdg] {
        org::save_branch(&mut tx, b, now).await.unwrap();
    }
    for (u, role, scope) in [
        (&owner, seed::OWNER_ROLE, BranchScope::All),
        (&budi, seed::MANAGER_ROLE, BranchScope::Branch(jkt.id)),
        (&kasir, seed::CASHIER_ROLE, BranchScope::Branch(jkt.id)),
    ] {
        users::save_user(&mut tx, u, now).await.unwrap();
        roles::replace_assignments(&mut tx, u.id, &[(role, scope)], now)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    let state = AppState::new(db, NodeRole::Standalone, Arc::new(SystemClock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    Server {
        root: format!("http://{addr}"),
        branches: [jkt.id.to_string(), bdg.id.to_string()],
        task,
    }
}

fn new_id() -> String {
    ProductId::generate().to_string()
}

#[track_caller]
fn code_of<T: std::fmt::Debug>(result: Result<T, ClientError>) -> (u16, String) {
    let err = result.unwrap_err();
    let p = err.problem().unwrap_or_else(|| panic!("{err:?}"));
    (p.status, p.code.clone())
}

/// `Kaos`: Size (S, M) × Color (RED, BLUE), units pcs + box(24), a barcode per variant.
fn kaos(code: &str, barcode_prefix: &str) -> Product {
    let (pcs, box_) = (new_id(), new_id());
    let sizes = [(new_id(), "Small", "S"), (new_id(), "Medium", "M")];
    let colors = [(new_id(), "Red", "red"), (new_id(), "Blue", "blue")];
    let mut variants = Vec::new();
    for s in &sizes {
        for c in &colors {
            variants.push(json!({ "id": new_id(), "option_value_ids": [s.0, c.0] }));
        }
    }
    let barcodes: Vec<_> = variants
        .iter()
        .enumerate()
        .map(|(i, v)| {
            json!({ "id": new_id(), "variant_id": v["id"], "unit_id": pcs,
                    "code": format!("{barcode_prefix}-{i}") })
        })
        .collect();
    let values = |list: &[(String, &str, &str)]| -> Vec<serde_json::Value> {
        list.iter()
            .map(|(id, name, code)| json!({ "id": id, "name": name, "code": code }))
            .collect()
    };
    serde_json::from_value(json!({
        "code": code, "name": "Kaos Polos", "type": "stock", "tax_class_id": STANDARD,
        "tags": ["Promo"],
        "options": [
            { "id": new_id(), "name": "Size", "values": values(&sizes) },
            { "id": new_id(), "name": "Color", "values": values(&colors) },
        ],
        "units": [
            { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": box_, "name": "box", "kind": "integer", "factor_thousandths": 24000 },
        ],
        "base_unit_id": pcs,
        "default_purchase_unit_id": box_,
        "variants": variants,
        "barcodes": barcodes,
    }))
    .unwrap()
}

fn bundle_of(variant: &str, unit: &str) -> Product {
    let pcs = new_id();
    serde_json::from_value(json!({
        "code": "PAKET", "name": "Paket Kaos", "type": "bundle", "tax_class_id": STANDARD,
        "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": pcs,
        "variants": [{ "id": new_id() }],
        "components": [{ "variant_id": variant, "unit_id": unit, "qty_thousandths": 2000 }],
    }))
    .unwrap()
}

#[allow(clippy::too_many_lines)] // one contract, step by step
async fn contract(c: &dyn BackOffice, branches: [&str; 2]) {
    // Login and permissions.
    assert_eq!(
        code_of(c.password_login("owner", "wrong").await),
        (401, "invalid_credentials".into())
    );
    let owner = c.password_login("owner", "owner-pass-1").await.unwrap();
    assert_eq!(owner.register_id, None);
    let owner = owner.token;
    let me = c.me(&owner).await.unwrap();
    assert_eq!(me.username, "owner");
    assert!(me.holds_everywhere(CATALOG_MANAGE) && me.holds_everywhere(CATALOG_VIEW_ARCHIVED));
    let budi = c.password_login("budi", "budi-pass-1").await.unwrap().token;
    let me = c.me(&budi).await.unwrap();
    assert!(me.holds_here(CATALOG_MANAGE) && !me.holds_everywhere(CATALOG_MANAGE));
    // Since C3b any assignment counts (spec 2.2 §11): Budi may manage the catalog.
    assert!(me.holds_anywhere(CATALOG_MANAGE) && me.holds_anywhere(CATALOG_VIEW_ARCHIVED));
    assert!(me.permissions_anywhere.contains(&CATALOG_MANAGE.to_owned()));
    let kasir = c
        .password_login("kasir", "kasir-pass-1")
        .await
        .unwrap()
        .token;
    let me = c.me(&kasir).await.unwrap();
    assert!(!me.holds_anywhere(CATALOG_MANAGE) && !me.holds_anywhere(CATALOG_VIEW_ARCHIVED));

    let ids: Vec<String> = c
        .branches(&owner)
        .await
        .unwrap()
        .into_iter()
        .map(|b| b.id)
        .collect();
    assert!(
        branches.iter().all(|b| ids.iter().any(|i| i == b)),
        "{ids:?}"
    );
    let tax: Vec<String> = c
        .tax_classes(&owner)
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(tax.contains(&"Standard".into()) && tax.contains(&"Exempt".into()));
    let settings = c.catalog_settings(&owner).await.unwrap();
    assert_eq!(settings.product_code_prefix, "P");
    assert!(settings.unit_names.contains(&"pcs".to_owned()));

    // Brands.
    let aqua = new_id();
    let body = BrandBody {
        name: " Aqua ".into(),
        archived: false,
    };
    assert_eq!(
        c.put_brand(&owner, &aqua, &body).await.unwrap().name,
        "Aqua"
    );
    assert_eq!(
        c.put_brand(&owner, &aqua, &body).await.unwrap().name,
        "Aqua"
    );
    let twin = c.put_brand(&owner, &new_id(), &body).await;
    let err = twin.unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!(
        (p.status, p.code.as_str(), p.field()),
        (422, "duplicate_brand_name", Some("name"))
    );
    assert_eq!(p.errors()[0].field.as_deref(), Some("name"));
    assert_eq!(
        code_of(c.put_brand(&kasir, &new_id(), &body).await),
        (403, "permission_denied".into())
    );
    let archived = BrandBody {
        name: "Lama".into(),
        archived: true,
    };
    c.put_brand(&owner, &new_id(), &archived).await.unwrap();
    assert_eq!(c.brands(&owner, false).await.unwrap().len(), 1);
    assert_eq!(c.brands(&owner, true).await.unwrap().len(), 2);
    assert_eq!(
        code_of(c.brands(&kasir, true).await),
        (403, "permission_denied".into())
    );

    // Categories.
    let (drinks, water) = (new_id(), new_id());
    let root = CategoryBody {
        name: "Minuman".into(),
        ..CategoryBody::default()
    };
    c.put_category(&owner, &drinks, &root).await.unwrap();
    let child = CategoryBody {
        parent_id: Some(drinks.clone()),
        name: "Air".into(),
        code: "air".into(),
        archived: false,
    };
    let stored = c.put_category(&owner, &water, &child).await.unwrap();
    assert_eq!(stored.code, "AIR");
    let tree = c.categories(&owner, false).await.unwrap();
    assert_eq!(tree.len(), 1);
    assert_eq!(tree[0].children[0].name, "Air");
    let cycle = CategoryBody {
        parent_id: Some(water.clone()),
        ..root.clone()
    };
    assert_eq!(
        code_of(c.put_category(&owner, &drinks, &cycle).await),
        (422, "category_cycle".into())
    );
    let gone = CategoryBody {
        archived: true,
        ..child.clone()
    };
    c.put_category(&owner, &water, &gone).await.unwrap();
    assert!(
        c.categories(&owner, false).await.unwrap()[0]
            .children
            .is_empty()
    );
    assert_eq!(
        c.categories(&owner, true).await.unwrap()[0].children.len(),
        1
    );
    assert_eq!(
        code_of(c.categories(&kasir, true).await),
        (403, "permission_denied".into())
    );
    assert_eq!(
        code_of(c.put_category(&kasir, &new_id(), &root).await),
        (403, "permission_denied".into())
    );

    // A product document: generated code, default SKUs, replay.
    let shirt_id = new_id();
    let mut shirt = kaos("", "KAOS-BC");
    shirt.category_id = Some(drinks.clone());
    shirt.brand_id = Some(aqua.clone());
    let created = c.put_product(&owner, &shirt_id, &shirt).await.unwrap();
    assert!(created.code.starts_with('P'), "{}", created.code);
    assert_eq!(created.variants[0].sku, format!("{}-S-RED", created.code));
    assert_eq!(created.options[1].values[0].code, "RED");
    assert_eq!(created.tags, ["promo"]);
    assert_eq!(created.receipt_name, "Kaos Polos");
    assert!(!created.used && created.created_at > 0);
    let replay = c.put_product(&owner, &shirt_id, &shirt).await.unwrap();
    assert_eq!(
        replay.updated_at, created.updated_at,
        "a replay writes nothing"
    );
    let got = c.product(&owner, &shirt_id).await.unwrap();
    assert_eq!(got.code, created.code);
    assert_eq!(got.default_purchase_unit_id, shirt.units[1].id);
    assert_eq!(
        code_of(c.product(&owner, &new_id()).await),
        (404, "not_found".into())
    );

    let query = |q: &str| ProductQuery {
        q: Some(q.into()),
        ..ProductQuery::default()
    };
    let page = c.products(&owner, &query("kaos")).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].variant_count, 4);
    assert_eq!(page.items[0].brand_id.as_deref(), Some(aqua.as_str()));
    let by_brand = c.products(&budi, &query("aqua")).await.unwrap();
    assert_eq!(
        by_brand.items.len(),
        1,
        "reading needs no catalog permission"
    );
    let by_category = ProductQuery {
        category_id: Some(drinks.clone()),
        product_type: Some("stock".into()),
        limit: Some(1),
        ..ProductQuery::default()
    };
    let page = c.products(&owner, &by_category).await.unwrap();
    assert_eq!(page.next_before_id.as_deref(), Some(shirt_id.as_str()));

    // 409: the same barcode on another product, with the conflicting product and variant.
    let other = kaos("KAOS2", "KAOS-BC");
    let err = c.put_product(&owner, &new_id(), &other).await.unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!((p.status, p.code.as_str()), (409, "duplicate_barcode"));
    assert_eq!(p.field(), Some("barcodes.code"));
    let dup = p.duplicate().unwrap();
    assert_eq!(dup.product_id, shirt_id);
    assert_eq!(
        dup.variant_id.as_deref(),
        Some(created.variants[0].id.as_str())
    );
    assert_eq!(dup.value, "KAOS-BC-0");
    let mut same_code = kaos(&created.code, "OTHER");
    for (i, v) in same_code.variants.iter_mut().enumerate() {
        v.sku = format!("OTHER-{i}");
    }
    let p = c
        .put_product(&owner, &new_id(), &same_code)
        .await
        .unwrap_err();
    let dup = p.problem().unwrap().duplicate().unwrap();
    assert_eq!(
        (dup.value.as_str(), dup.variant_id),
        (created.code.as_str(), None)
    );

    // 422: every rule failure with its field.
    let mut bad = kaos("BAD", "BAD");
    bad.name = String::new();
    bad.variants[0].sku = "a b".into();
    let err = c.put_product(&owner, &new_id(), &bad).await.unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!(p.status, 422);
    let fields: Vec<Option<String>> = p.errors().into_iter().map(|e| e.field).collect();
    assert!(fields.contains(&Some("name".into())), "{fields:?}");
    assert!(fields.contains(&Some("variants.sku".into())), "{fields:?}");
    assert_eq!(
        code_of(c.put_product(&kasir, &new_id(), &kaos("B", "B")).await),
        (403, "permission_denied".into())
    );

    // Generators.
    let code = c
        .generate(&owner, Generate::ProductCode, "g-1")
        .await
        .unwrap();
    assert!(code.starts_with('P') && code != created.code, "{code}");
    assert_eq!(
        c.generate(&owner, Generate::ProductCode, "g-1")
            .await
            .unwrap(),
        code,
        "a retried generate replays"
    );
    let barcode = c.generate(&owner, Generate::Barcode, "g-2").await.unwrap();
    assert!(
        barcode.len() == 13 && barcode.starts_with("29"),
        "{barcode}"
    );
    assert_eq!(
        code_of(c.generate(&kasir, Generate::Barcode, "g-3").await),
        (403, "permission_denied".into())
    );

    // Branch blocks.
    assert!(c.branch_blocks(&owner, &shirt_id).await.unwrap().is_empty());
    let set = vec![branches[1].to_owned()];
    assert_eq!(
        c.put_branch_blocks(&owner, &shirt_id, &set).await.unwrap(),
        set
    );
    assert_eq!(c.branch_blocks(&budi, &shirt_id).await.unwrap(), set);
    let unknown = vec![new_id()];
    assert_eq!(
        code_of(c.put_branch_blocks(&owner, &shirt_id, &unknown).await),
        (422, "invalid_field".into())
    );
    assert_eq!(
        code_of(c.branch_blocks(&owner, &new_id()).await),
        (404, "not_found".into())
    );

    // Image.
    assert_eq!(c.product_image(&owner, &shirt_id).await.unwrap(), None);
    let info = c
        .put_product_image(&owner, &shirt_id, PNG.to_vec())
        .await
        .unwrap();
    assert_eq!(info.product_id, shirt_id);
    assert!(c.product_image(&owner, &shirt_id).await.unwrap().is_some());
    assert_eq!(
        code_of(
            c.put_product_image(&owner, &shirt_id, b"not an image".to_vec())
                .await
        ),
        (422, "invalid_image".into())
    );
    c.delete_product_image(&owner, &shirt_id).await.unwrap();
    assert_eq!(c.product_image(&owner, &shirt_id).await.unwrap(), None);

    // Search and a bundle of a variant: its lines are labelled.
    let page = c.search(&owner, "kaos", 10, None).await.unwrap();
    let hits = page.items;
    assert_eq!(hits.len(), 4, "{hits:?}");
    assert!(hits.iter().all(|h| h.product_id == shirt_id));
    assert_eq!(page.next_after_variant_id, None);
    // Paged: two full pages, then an empty one.
    let first = c.search(&owner, "kaos", 2, None).await.unwrap();
    assert_eq!(first.items, hits[..2]);
    let cursor = first.next_after_variant_id.unwrap();
    let second = c.search(&owner, "kaos", 2, Some(&cursor)).await.unwrap();
    assert_eq!(second.items, hits[2..]);
    let cursor = second.next_after_variant_id.unwrap();
    let third = c.search(&owner, "kaos", 2, Some(&cursor)).await.unwrap();
    assert!(third.items.is_empty() && third.next_after_variant_id.is_none());
    let red = &created.variants[0];
    let bundle = c
        .put_product(
            &owner,
            &new_id(),
            &bundle_of(&red.id, &created.base_unit_id),
        )
        .await
        .unwrap();
    let line = &bundle.components[0];
    assert_eq!(line.product_id.as_deref(), Some(shirt_id.as_str()));
    assert_eq!(line.name.as_deref(), Some("Kaos Polos – Small / Red"));
    assert_eq!(line.sku.as_deref(), Some(red.sku.as_str()));
    assert_eq!(line.unit_name.as_deref(), Some("pcs"));
    assert!(!bundle.purchasable || bundle.product_type == "bundle");

    // Archive and unarchive.
    let archived = c
        .set_archived(&owner, &shirt_id, true, "a-1")
        .await
        .unwrap();
    assert!(archived.archived && archived.archived_at.is_some());
    let active = c.products(&owner, &query("kaos")).await.unwrap();
    assert!(
        active.items.iter().all(|p| p.id != shirt_id),
        "only the bundle is left"
    );
    let only_archived = ProductQuery {
        archived: true,
        ..ProductQuery::default()
    };
    assert_eq!(
        c.products(&owner, &only_archived)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        code_of(c.products(&kasir, &only_archived).await),
        (403, "permission_denied".into())
    );
    assert_eq!(
        code_of(c.product(&kasir, &shirt_id).await),
        (403, "permission_denied".into())
    );
    assert_eq!(
        code_of(c.set_archived(&kasir, &shirt_id, false, "a-2").await),
        (403, "permission_denied".into())
    );
    // Budi (Manager at one branch) sees archived products.
    assert_eq!(
        c.products(&budi, &only_archived).await.unwrap().items.len(),
        1
    );
    let back = c
        .set_archived(&owner, &shirt_id, false, "a-3")
        .await
        .unwrap();
    assert!(!back.archived && back.archived_at.is_none());

    // Delete (spec 2.2 §9, C3b): never-used rows only.
    let in_use = |result: Result<(), ClientError>| {
        let err = result.unwrap_err();
        let p = err.problem().unwrap();
        (p.status, p.code.clone(), p.reason().map(str::to_owned))
    };
    assert_eq!(
        in_use(c.delete_product(&owner, &shirt_id).await),
        (409, "in_use".into(), Some("bundle_component".into()))
    );
    assert_eq!(
        code_of(c.delete_product(&kasir, &bundle.id).await),
        (403, "permission_denied".into())
    );
    c.delete_product(&budi, &bundle.id).await.unwrap();
    assert_eq!(
        code_of(c.product(&owner, &bundle.id).await),
        (404, "not_found".into())
    );
    c.delete_product(&owner, &shirt_id).await.unwrap();
    assert_eq!(
        code_of(c.delete_product(&owner, &shirt_id).await),
        (404, "not_found".into())
    );
    assert_eq!(
        in_use(c.delete_category(&owner, &drinks).await),
        (409, "in_use".into(), Some("has_children".into()))
    );
    c.delete_category(&owner, &water).await.unwrap();
    c.delete_category(&owner, &drinks).await.unwrap();
    assert!(c.categories(&owner, true).await.unwrap().is_empty());
    let mut bottled = kaos("AIR1", "AIR1-BC");
    bottled.brand_id = Some(aqua.clone());
    c.put_product(&owner, &new_id(), &bottled).await.unwrap();
    assert_eq!(
        in_use(c.delete_brand(&owner, &aqua).await),
        (409, "in_use".into(), Some("has_products".into()))
    );
    let typo = new_id();
    let body = BrandBody {
        name: "Aqau".into(),
        archived: false,
    };
    c.put_brand(&owner, &typo, &body).await.unwrap();
    assert_eq!(
        code_of(c.delete_brand(&kasir, &typo).await),
        (403, "permission_denied".into())
    );
    c.delete_brand(&owner, &typo).await.unwrap();
    assert_eq!(
        code_of(c.delete_brand(&owner, &typo).await),
        (404, "not_found".into())
    );

    // Logout ends the session.
    c.logout(&owner).await.unwrap();
    assert_eq!(code_of(c.me(&owner).await), (401, "invalid_session".into()));
}

#[tokio::test]
async fn http_back_office_against_the_server() {
    let s = serve().await;
    let c = HttpService::user(&s.root);
    contract(&c, [&s.branches[0], &s.branches[1]]).await;
}

#[tokio::test]
async fn fake_back_office_follows_the_same_contract() {
    let fake = FakeService::default();
    contract(&fake, [sultan_client::fake::BRANCH_ID, OTHER_BRANCH_ID]).await;
    assert!(fake.calls_of("put_product").len() >= 6);
}

/// A user session alone (no device token) receives `catalog.changed` on the events socket.
#[tokio::test]
async fn catalog_events_reach_a_back_office_session() {
    let s = serve().await;
    let c = HttpService::user(&s.root);
    let token = c
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let seen: Arc<Mutex<Vec<ServerEvent>>> = Arc::default();
    let sink: EventSink = {
        let seen = seen.clone();
        Arc::new(move |e| seen.lock().unwrap().push(e))
    };
    let watching = tokio::spawn(c.watch_events(&token, sink));
    // Write until the socket is up and the event arrives.
    for attempt in 0..50 {
        let body = BrandBody {
            name: format!("Merek {attempt}"),
            archived: false,
        };
        c.put_brand(&token, &new_id(), &body).await.unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        if seen
            .lock()
            .unwrap()
            .iter()
            .any(|e| e.kind == "catalog.changed")
        {
            break;
        }
    }
    let event = seen
        .lock()
        .unwrap()
        .iter()
        .find(|e| e.kind == "catalog.changed")
        .cloned()
        .expect("catalog.changed arrives");
    assert_eq!(event.data["scope"], "brand");
    c.logout(&token).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(5), watching).await;

    // The fake sends the same event after a write.
    let fake = FakeService::default();
    let token = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let seen: Arc<Mutex<Vec<ServerEvent>>> = Arc::default();
    let sink: EventSink = {
        let seen = seen.clone();
        Arc::new(move |e| seen.lock().unwrap().push(e))
    };
    fake.watch_events(&token, sink).await.unwrap();
    let body = BrandBody {
        name: "Aqua".into(),
        archived: false,
    };
    fake.put_brand(&token, &new_id(), &body).await.unwrap();
    assert_eq!(seen.lock().unwrap()[0].data["scope"], "brand");
}

#[tokio::test]
async fn unreachable_server_is_a_network_error() {
    let c = HttpService::user("http://127.0.0.1:1");
    for result in [
        c.password_login("owner", "x").await.map(|_| ()),
        c.product_image("t", "p").await.map(|_| ()),
        c.delete_product_image("t", "p").await,
        c.search("t", "q", 5, None).await.map(|_| ()),
        c.delete_product("t", "p").await,
        c.branch_blocks("t", "p").await.map(|_| ()),
        c.generate("t", Generate::Barcode, "k").await.map(|_| ()),
    ] {
        assert!(matches!(result, Err(ClientError::Network(_))), "{result:?}");
    }
}

/// The fake's test helpers: seeding, "frozen after use", injected failures, stored state.
#[tokio::test]
async fn fake_helpers_and_frozen_after_use() {
    let fake = FakeService::default();
    fake.add_backoffice_user(
        sultan_client::fake::BackOfficeUser::new("u-x", "x", "x-pass-123", "X").catalog_admin(),
    );
    let token = fake.password_login("x", "x-pass-123").await.unwrap().token;
    let (cat, brand) = (new_id(), new_id());
    fake.seed_category(
        &cat,
        &CategoryBody {
            name: "Pakaian".into(),
            ..CategoryBody::default()
        },
    );
    fake.seed_brand(&brand, "Sultan");
    assert_eq!(fake.stored_categories()[0].name, "Pakaian");
    assert_eq!(fake.stored_brands()[0].name, "Sultan");
    let id = new_id();
    let mut shirt = kaos("KAOS", "K");
    shirt.category_id = Some(cat.clone());
    let stored = fake.seed_product(&id, &shirt);
    assert_eq!(fake.product_ids(), std::slice::from_ref(&id));
    assert_eq!(fake.stored_product(&id).unwrap().code, stored.code);
    assert!(
        fake.calls_of("put_product").is_empty(),
        "seeding is not a call"
    );

    // Used: the type is frozen, a rename is fine.
    fake.mark_used(&id);
    let mut changed = stored.clone();
    changed.product_type = "service".into();
    let err = fake.put_product(&token, &id, &changed).await.unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!(
        (p.code.as_str(), p.field()),
        ("frozen_after_use", Some("type"))
    );
    let mut renamed = stored.clone();
    renamed.name = "Kaos Oblong".into();
    let renamed = fake.put_product(&token, &id, &renamed).await.unwrap();
    assert!(renamed.used);
    // An unknown brand is an invalid reference.
    let mut bad = stored.clone();
    bad.brand_id = Some(new_id());
    assert_eq!(
        code_of(fake.put_product(&token, &id, &bad).await),
        (422, "invalid_reference".into())
    );
    assert_eq!(
        code_of(fake.put_product(&token, "not-a-uuid", &stored).await),
        (400, "invalid_body".into())
    );

    fake.put_branch_blocks(&token, &id, &[OTHER_BRANCH_ID.into()])
        .await
        .unwrap();
    assert_eq!(fake.stored_blocks(&id), [OTHER_BRANCH_ID]);
    fake.put_product_image(&token, &id, PNG.to_vec())
        .await
        .unwrap();
    assert_eq!(fake.stored_image(&id).unwrap().len(), PNG.len());

    fake.fail_on("product", ClientError::Network("down".into()));
    assert!(matches!(
        fake.product(&token, &id).await,
        Err(ClientError::Network(_))
    ));
    assert!(fake.product(&token, &id).await.is_ok());
    assert_eq!(
        code_of(fake.me("nobody").await),
        (401, "invalid_session".into())
    );
    assert!(
        fake.search(&token, "  ", 5, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    // A sold product is in use.
    let err = fake.delete_product(&token, &id).await.unwrap_err();
    assert_eq!(err.problem().unwrap().reason(), Some("used"));
    assert_eq!(
        code_of(fake.delete_category(&token, &new_id()).await),
        (404, "not_found".into())
    );
    assert_eq!(
        code_of(fake.delete_brand(&token, &new_id()).await),
        (404, "not_found".into())
    );
    let err = fake.delete_category(&token, &cat).await.unwrap_err();
    assert_eq!(err.problem().unwrap().reason(), Some("has_products"));
    // An unknown search cursor ends the search.
    let page = fake.search(&token, "kaos", 5, Some("gone")).await.unwrap();
    assert!(page.items.is_empty());
}
