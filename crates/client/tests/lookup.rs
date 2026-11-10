//! [`Service::lookup`] (phase C5, spec 2.2 §6, §10): one contract run against [`HttpService`] on
//! the real router (a paired register with a cashier's PIN session, in-memory `SQLite`) and
//! against [`FakeService`], so the register UI tests see the server's answers: the lookup order
//! (barcode, UPC-A ↔ EAN-13, scale codes for weight and price, SKU, PLU), the display fields, and
//! `404 not_found`, `422 not_sold_here` / `archived` / `not_sellable` / `scale_value_zero`.

use std::sync::Arc;

use serde_json::json;
use sultan_client::fake::{BRANCH_ID, FakeService, OTHER_BRANCH_ID};
use sultan_client::{BackOffice, ClientError, HttpService, Product, Service};
use sultan_core::access::BranchScope;
use sultan_core::catalog::gtin_check_digit;
use sultan_core::ids::{BranchId, CompanyId, ProductId, RegisterId, UserId};
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router, token};

const STANDARD: &str = "00000000-0000-7000-8000-000000000101";

struct Server {
    root: String,
    device: String,
    ani: String,
    branches: [String; 2],
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn branch(company: CompanyId, code: &str) -> Branch {
    Branch {
        id: BranchId::generate(),
        company,
        code: code.to_owned(),
        name: code.to_owned(),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    }
}

/// Owner `owner` (password, all branches), cashier Ani (PIN 1234) and register R1 at JKT01.
async fn serve() -> Server {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let (jkt, bdg) = (branch(company.id, "JKT01"), branch(company.id, "BDG01"));
    let register = Register {
        id: RegisterId::generate(),
        branch: jkt.id,
        code: "R1".to_owned(),
        name: "Kasir 1".to_owned(),
        default_location: None,
        archived: false,
    };
    let owner = User {
        id: UserId::generate(),
        username: "owner".to_owned(),
        display_name: "Owner".to_owned(),
        pin_hash: None,
        password_hash: Some(credentials::hash_password("owner-pass-1").unwrap()),
        active: true,
    };
    let ani = User {
        id: UserId::generate(),
        username: "ani".to_owned(),
        display_name: "Ani".to_owned(),
        pin_hash: Some(credentials::hash_pin("1234").unwrap()),
        password_hash: None,
        active: true,
    };
    let device = token::generate();
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    for b in [&jkt, &bdg] {
        org::save_branch(&mut tx, b, now).await.unwrap();
    }
    org::save_register(&mut tx, &register, now).await.unwrap();
    for (u, role, scope) in [
        (&owner, seed::OWNER_ROLE, BranchScope::All),
        (&ani, seed::CASHIER_ROLE, BranchScope::Branch(jkt.id)),
    ] {
        users::save_user(&mut tx, u, now).await.unwrap();
        roles::replace_assignments(&mut tx, u.id, &[(role, scope)], now)
            .await
            .unwrap();
    }
    devices::insert_device_token(&mut tx, &token::hash(&device), register.id, now)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let state = AppState::new(db, NodeRole::Standalone, Arc::new(SystemClock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    Server {
        root: format!("http://{addr}"),
        device,
        ani: ani.id.to_string(),
        branches: [jkt.id.to_string(), bdg.id.to_string()],
        task,
    }
}

fn new_id() -> String {
    ProductId::generate().to_string()
}

fn ean13(body: &str) -> String {
    format!("{body}{}", gtin_check_digit(body).unwrap())
}

#[track_caller]
fn code_of<T: std::fmt::Debug>(result: Result<T, ClientError>) -> (u16, String) {
    let err = result.unwrap_err();
    let p = err.problem().unwrap_or_else(|| panic!("{err:?}"));
    (p.status, p.code.clone())
}

fn product(value: serde_json::Value) -> Product {
    serde_json::from_value(value).unwrap()
}

/// Kaos Polos, Size (Small, Medium) × one Color (Red): pcs + box(24); barcode `KP-1` on Medium
/// pcs and `KP-BOX` on Medium box.
fn kaos() -> Product {
    let (pcs, box_, s, m, red) = (new_id(), new_id(), new_id(), new_id(), new_id());
    let (small, medium) = (new_id(), new_id());
    product(json!({
        "code": "KAOS", "name": "Kaos Polos", "type": "stock", "tax_class_id": STANDARD,
        "options": [
            { "id": new_id(), "name": "Size", "values": [
                { "id": s, "name": "Small", "code": "S" }, { "id": m, "name": "Medium", "code": "M" }] },
            { "id": new_id(), "name": "Color", "values": [{ "id": red, "name": "Red", "code": "RED" }] },
        ],
        "units": [
            { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": box_, "name": "box", "kind": "integer", "factor_thousandths": 24000 },
        ],
        "base_unit_id": pcs,
        "variants": [
            { "id": small, "option_value_ids": [s, red] },
            { "id": medium, "option_value_ids": [m, red] },
        ],
        "barcodes": [
            { "id": new_id(), "variant_id": medium, "unit_id": pcs, "code": "KP-1" },
            { "id": new_id(), "variant_id": medium, "unit_id": box_, "code": "KP-BOX" },
        ],
    }))
}

/// A product with one variant, one unit and optionally one barcode.
fn simple(code: &str, name: &str, kind: &str, plu: Option<u32>, barcode: Option<&str>) -> Product {
    let (unit, variant) = (new_id(), new_id());
    let (unit_name, unit_kind) = if kind == "weighed" {
        ("kg", "decimal")
    } else {
        ("pcs", "integer")
    };
    let barcodes: Vec<_> = barcode
        .map(|b| json!({ "id": new_id(), "variant_id": variant, "unit_id": unit, "code": b }))
        .into_iter()
        .collect();
    product(json!({
        "code": code, "name": name, "type": kind, "tax_class_id": STANDARD,
        "units": [{ "id": unit, "name": unit_name, "kind": unit_kind, "factor_thousandths": 1000 }],
        "base_unit_id": unit,
        "variants": [{ "id": variant, "plu": plu }],
        "barcodes": barcodes,
    }))
}

#[allow(clippy::too_many_lines)] // one contract, step by step
async fn contract(
    office: &dyn BackOffice,
    register: &dyn Service,
    cashier: &str,
    branches: [&str; 2],
) {
    let owner = office
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let shirt_id = new_id();
    let shirt = office
        .put_product(&owner, &shirt_id, &kaos())
        .await
        .unwrap();
    let keju = simple("KEJU", "Keju Edam", "weighed", Some(123), None);
    office.put_product(&owner, &new_id(), &keju).await.unwrap();
    let upc = "0036000291452";
    let aqua = simple("AQUA", "Aqua 600ml", "stock", None, Some(upc));
    office.put_product(&owner, &new_id(), &aqua).await.unwrap();
    let mut gift = simple("GIFT", "Hadiah", "stock", None, Some("GIFT-1"));
    gift.sellable = false;
    office.put_product(&owner, &new_id(), &gift).await.unwrap();

    let token = register.pin_login(cashier, "1234").await.unwrap().token;

    // Barcode: the variant, its unit and the display fields, at the register's branch.
    let found = register.lookup(&token, " KP-1 ").await.unwrap();
    let medium = &shirt.variants[1];
    assert_eq!(found.product_id, shirt_id);
    assert_eq!(found.variant_id, medium.id);
    assert_eq!(found.unit_id, shirt.base_unit_id);
    assert_eq!(found.name, "Kaos Polos – Medium / Red");
    assert_eq!(
        (
            found.product_name.as_str(),
            found.variant_label.as_str(),
            found.unit_name.as_str(),
            found.sku.as_str(),
        ),
        ("Kaos Polos", "Medium / Red", "pcs", "KAOS-M-RED")
    );
    assert_eq!(
        (found.source.as_str(), found.matched_code.as_str()),
        ("barcode", "KP-1")
    );
    assert_eq!(
        (found.qty_thousandths, found.amount, found.plu),
        (Some(1000), None, None)
    );
    assert_eq!(found.product_type, "stock");
    assert!(!found.open_price && !found.requires_note);
    assert_eq!(found.branch_id.as_deref(), Some(branches[0]));
    let found = register.lookup(&token, "KP-BOX").await.unwrap();
    assert_eq!(found.unit_name, "box");

    // SKU in lower case; UPC-A ↔ EAN-13.
    let found = register.lookup(&token, "kaos-s-red").await.unwrap();
    assert_eq!(
        (found.source.as_str(), found.matched_code.as_str()),
        ("sku", "KAOS-S-RED")
    );
    assert_eq!(found.variant_label, "Small / Red");
    let found = register.lookup(&token, &upc[1..]).await.unwrap();
    assert_eq!(
        (found.source.as_str(), found.matched_code.as_str()),
        ("normalized_barcode", upc)
    );
    assert_eq!(found.variant_label, "");

    // Scale codes (21 weight, 22 price) and the PLU typed alone.
    let weight = ean13("210012301250");
    let found = register.lookup(&token, &weight).await.unwrap();
    assert_eq!(
        (found.source.as_str(), found.matched_code.as_str()),
        ("scale_barcode", weight.as_str())
    );
    assert_eq!(
        (found.qty_thousandths, found.amount, found.plu),
        (Some(1250), None, Some(123))
    );
    assert_eq!(
        (found.product_type.as_str(), found.unit_name.as_str()),
        ("weighed", "kg")
    );
    let found = register
        .lookup(&token, &ean13("220012315000"))
        .await
        .unwrap();
    assert_eq!((found.qty_thousandths, found.amount), (None, Some(15_000)));
    let found = register.lookup(&token, "123").await.unwrap();
    assert_eq!(
        (found.source.as_str(), found.qty_thousandths),
        ("plu", Some(1000))
    );
    assert_eq!(
        code_of(register.lookup(&token, &ean13("210012300000")).await),
        (422, "scale_value_zero".into())
    );

    // Not found, not sellable.
    for missing in ["NOPE", "  ", "999999"] {
        assert_eq!(
            code_of(register.lookup(&token, missing).await),
            (404, "not_found".into()),
            "{missing:?}"
        );
    }
    assert_eq!(
        code_of(register.lookup(&token, "GIFT-1").await),
        (422, "not_sellable".into())
    );

    // Blocked at the register's branch; the back office without a branch still finds it.
    office
        .put_branch_blocks(&owner, &shirt_id, &[branches[0].to_owned()])
        .await
        .unwrap();
    assert_eq!(
        code_of(register.lookup(&token, "KP-1").await),
        (422, "not_sold_here".into())
    );
    let found = office.lookup(&owner, "KP-1").await.unwrap();
    assert_eq!(found.branch_id, None);
    office
        .put_branch_blocks(&owner, &shirt_id, &[branches[1].to_owned()])
        .await
        .unwrap();
    assert!(register.lookup(&token, "KP-1").await.is_ok());

    // Archived.
    office
        .set_archived(&owner, &shirt_id, true, "arch-1")
        .await
        .unwrap();
    assert_eq!(
        code_of(register.lookup(&token, "KP-1").await),
        (422, "archived".into())
    );
    assert_eq!(
        code_of(register.lookup(&token, "kaos-m-red").await),
        (422, "archived".into())
    );

    // A session that ended.
    register.logout(&token).await.unwrap();
    assert_eq!(
        code_of(register.lookup(&token, "123").await),
        (401, "invalid_session".into())
    );
}

#[tokio::test]
async fn http_lookup_at_a_register() {
    let s = serve().await;
    let office = HttpService::user(&s.root);
    let register = HttpService::new(&s.root, &s.device);
    contract(&office, &register, &s.ani, [&s.branches[0], &s.branches[1]]).await;
}

#[tokio::test]
async fn fake_lookup_follows_the_same_contract() {
    let fake = FakeService::default();
    contract(&fake, &fake, "u-ani", [BRANCH_ID, OTHER_BRANCH_ID]).await;
    assert!(fake.calls_of("lookup").contains(&"KP-1".to_owned()));
}

#[tokio::test]
async fn fake_lookup_failures_and_unreachable_server() {
    let fake = FakeService::default();
    fake.fail_next(ClientError::Network("down".into()));
    assert!(matches!(
        fake.lookup("t", "x").await,
        Err(ClientError::Network(_))
    ));
    let c = HttpService::new("http://127.0.0.1:1", "device");
    assert!(matches!(
        c.lookup("t", "x").await,
        Err(ClientError::Network(_))
    ));
}
