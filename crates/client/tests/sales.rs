//! Sales (phase S3, spec 2.6): every sales call of [`HttpService`] against the real router
//! (in-memory `SQLite`), and the same flow against [`FakeService`], so the register and back-office
//! screens of S4–S6 see the server's answers: tender types, a sale with a stale cart and a
//! replay, receipts and reprints, a post-void, till voids, returns and an exchange, held carts,
//! the purchase history and the back office's tender types and returns.

use std::sync::Arc;

use serde_json::json;
use sultan_client::fake::{FakeService, TENDER_CASH, TENDER_QRIS};
use sultan_client::{
    BackOffice, CartVoidRequest, ClientError, DocQuery, ExchangeRequest, HoldRequest,
    OverrideRequest, ReturnLineRequest, ReturnRequest, SaleLineRequest, SaleRequest, Service,
    TenderRequest, TenderTypeBody, VoidRequest, VoidedLine,
};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, ProductId, RegisterId, UserId};
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router, token};

const STANDARD: &str = "00000000-0000-7000-8000-000000000101";
const DEFAULT_LIST: &str = "00000000-0000-7000-8000-000000000201";
const CASH: &str = "00000000-0000-7000-8000-000000000301";

struct Server {
    root: String,
    device: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn user(username: &str, password: Option<&str>, pin: Option<&str>) -> User {
    User {
        id: UserId::generate(),
        username: username.to_owned(),
        display_name: username.to_owned(),
        pin_hash: pin.map(|p| credentials::hash_pin(p).unwrap()),
        password_hash: password.map(|p| credentials::hash_password(p).unwrap()),
        active: true,
    }
}

/// JKT01 with register R1: `owner` (password), Ani (cashier, PIN 1234), Budi (manager, PIN 9999).
async fn serve() -> (Server, String) {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let jkt = Branch {
        id: BranchId::generate(),
        company: company.id,
        code: "JKT01".to_owned(),
        name: "JKT01".to_owned(),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    };
    let register = Register {
        id: RegisterId::generate(),
        branch: jkt.id,
        code: "R1".to_owned(),
        name: "Kasir 1".to_owned(),
        default_location: None,
        archived: false,
    };
    let device = token::generate();
    let budi = user("budi", None, Some("9999"));
    let budi_id = budi.id.to_string();
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    org::save_branch(&mut tx, &jkt, now).await.unwrap();
    org::save_register(&mut tx, &register, now).await.unwrap();
    for (u, role, scope) in [
        (
            user("owner", Some("owner-pass-1"), None),
            seed::OWNER_ROLE,
            BranchScope::All,
        ),
        (
            user("ani", None, Some("1234")),
            seed::CASHIER_ROLE,
            BranchScope::Branch(jkt.id),
        ),
        (budi, seed::MANAGER_ROLE, BranchScope::Branch(jkt.id)),
    ] {
        users::save_user(&mut tx, &u, now).await.unwrap();
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
    (
        Server {
            root: format!("http://{addr}"),
            device,
            task,
        },
        budi_id,
    )
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

/// A product priced 10 000 per pcs, created over raw HTTP as the owner; `(variant, pcs)`.
async fn product(root: &str, owner: &str) -> (String, String) {
    let (id, variant, pcs) = (new_id(), new_id(), new_id());
    let http = reqwest::Client::new();
    let r = http
        .put(format!("{root}/api/v1/products/{id}"))
        .bearer_auth(owner)
        .json(&json!({
            "code": "TEH", "name": "Teh", "type": "stock", "tax_class_id": STANDARD,
            "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
            "base_unit_id": pcs, "variants": [{ "id": variant }],
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let r = http
        .put(format!(
            "{root}/api/v1/price-lists/{DEFAULT_LIST}/entries/{}",
            new_id()
        ))
        .bearer_auth(owner)
        .json(&json!({ "variant_id": variant, "unit_id": pcs,
            "tiers": [{ "min_qty_thousandths": 1000, "price": 10_000 }] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    (variant, pcs)
}

fn sale(variant: &str, unit: &str, qty: i64, cash_type: &str, cash: i64) -> SaleRequest {
    SaleRequest {
        lines: vec![SaleLineRequest {
            variant_id: variant.to_owned(),
            unit_id: unit.to_owned(),
            qty_thousandths: qty * 1000,
            ..SaleLineRequest::default()
        }],
        tenders: vec![TenderRequest {
            tender_type_id: cash_type.to_owned(),
            amount: cash,
            reference: None,
        }],
        ..SaleRequest::default()
    }
}

/// Posts `body` after taking the fresh totals from `409 cart_stale`.
async fn post(register: &dyn Service, token: &str, mut body: SaleRequest) -> sultan_client::Sale {
    let stale = register.post_sale(token, &new_id(), &body, None).await;
    let err = stale.unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!(p.code, "cart_stale");
    body.totals = p.quote_totals().unwrap();
    register
        .post_sale(token, &new_id(), &body, None)
        .await
        .unwrap()
}

/// The register flow shared by the server and the fake (see the module docs).
#[allow(clippy::too_many_lines)] // one flow, step by step
async fn flow(
    register: &dyn Service,
    cashier: &str,
    approver: &str,
    approver_pin: &str,
    (variant, unit, price): (&str, &str, i64),
    (cash_type, qris_type): (&str, &str),
) {
    let types = register.tender_types(cashier).await.unwrap();
    assert_eq!(types[0].name, "Tunai");
    assert!(types.iter().any(|t| t.id == qris_type && t.needs_reference));
    // No session yet.
    let body = sale(variant, unit, 1, cash_type, 10_000);
    assert_eq!(
        code_of(register.post_sale(cashier, &new_id(), &body, None).await),
        (409, "no_open_session".to_owned())
    );
    let session = register.open_session(cashier, 0, &new_id()).await.unwrap();
    let older = post(register, cashier, sale(variant, unit, 1, cash_type, 10_000)).await;
    assert_eq!(older.summary.total, price);
    let mut body = sale(variant, unit, 2, cash_type, 50_000);
    let stale = register.post_sale(cashier, &new_id(), &body, None).await;
    body.totals = stale
        .unwrap_err()
        .problem()
        .unwrap()
        .quote_totals()
        .unwrap();
    let id = new_id();
    let last = register.post_sale(cashier, &id, &body, None).await.unwrap();
    assert_eq!(last.summary.change_given, 50_000 - 2 * price);
    assert_eq!(
        register.post_sale(cashier, &id, &body, None).await.unwrap(),
        last
    );
    let mut other = body.clone();
    other.tenders[0].amount = 20_000;
    assert_eq!(
        code_of(register.post_sale(cashier, &id, &other, None).await),
        (409, "conflict".to_owned())
    );
    assert_eq!(register.sale(cashier, &id).await.unwrap(), last);
    let by_number = register
        .sale_by_number(cashier, &last.summary.number)
        .await
        .unwrap();
    assert_eq!(by_number.summary.id, id);
    let page = register
        .sales(
            cashier,
            &DocQuery {
                cashier_session_id: Some(session.id.clone()),
                ..DocQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    // Receipts: the last of the session reprints freely; the older one needs a manager.
    let receipt = register
        .receipt(cashier, &id, true, "en", None)
        .await
        .unwrap();
    assert!(
        receipt.copy && receipt.text.contains("COPY"),
        "{}",
        receipt.text
    );
    let older_id = older.summary.id.clone();
    assert_eq!(
        code_of(register.receipt(cashier, &older_id, true, "en", None).await),
        (403, "needs_override".to_owned())
    );
    let grant = |action: &str, context: &str, value: Option<i64>| OverrideRequest {
        action: action.to_owned(),
        value,
        context_ref: context.to_owned(),
        approver_id: approver.to_owned(),
        approver_pin: approver_pin.to_owned(),
    };
    let t = register
        .request_override(cashier, &grant("sale.reprint", &older_id, None))
        .await
        .unwrap()
        .token;
    let receipt = register
        .receipt(cashier, &older_id, true, "id", Some(&t))
        .await
        .unwrap();
    assert!(receipt.copy);
    // Built by `core::sale::build_receipt`: Indonesian labels, the product's name, 32 columns.
    for want in ["SALINAN", "TOTAL", "Teh"] {
        assert!(receipt.text.contains(want), "{want}: {}", receipt.text);
    }
    assert!(receipt.lines.iter().all(|l| l.chars().count() <= 32));
    // A till void of a line.
    let void_line = CartVoidRequest {
        kind: "line".to_owned(),
        cart_id: new_id(),
        reason: "wrong_item".to_owned(),
        reason_text: None,
        lines: vec![VoidedLine {
            variant_id: variant.to_owned(),
            name: "Teh".to_owned(),
            qty_thousandths: 1000,
            amount: price,
        }],
    };
    let v = register
        .cart_void(cashier, &void_line, &new_id(), None)
        .await
        .unwrap();
    assert_eq!(
        (v.amount, v.cashier_session_id.as_str()),
        (price, session.id.as_str())
    );
    // F5: a cashier opens the drawer with a manager's approval for the session.
    let drawer = sultan_client::DrawerRequest {
        reason: Some(" change ".to_owned()),
    };
    assert_eq!(
        code_of(
            register
                .open_drawer(cashier, &drawer, &new_id(), None)
                .await
        ),
        (403, "needs_override".to_owned())
    );
    let t = register
        .request_override(cashier, &grant("drawer.open", &session.id, None))
        .await
        .unwrap()
        .token;
    let opened = register
        .open_drawer(cashier, &drawer, &new_id(), Some(&t))
        .await
        .unwrap();
    assert_eq!(
        (opened.cashier_session_id.as_str(), opened.reason.as_deref()),
        (session.id.as_str(), Some("change"))
    );
    assert_eq!(opened.approved_by.as_deref(), Some(approver));
    // Return 1 of the 2 by receipt, then exchange the other for 1 new.
    let ret = |qty: i64| ReturnRequest {
        sale_id: Some(id.clone()),
        refund: "cash".to_owned(),
        lines: vec![ReturnLineRequest {
            sale_line_no: Some(1),
            variant_id: variant.to_owned(),
            unit_id: unit.to_owned(),
            qty_thousandths: qty * 1000,
            write_off_reason: None,
        }],
        ..ReturnRequest::default()
    };
    let r = register
        .post_return(cashier, &new_id(), &ret(1), None)
        .await
        .unwrap();
    assert_eq!(r.summary.refund, price);
    // The return receipt (phase S6); the last return of the session is reprinted for free.
    let slip = register
        .return_receipt(cashier, &r.summary.id, false, "id", None)
        .await
        .unwrap();
    assert!(slip.text.contains("RETUR"), "{}", slip.text);
    assert_eq!(slip.return_id, r.summary.id);
    assert_eq!(slip.barcode, r.summary.number);
    let copy = register
        .return_receipt(cashier, &r.summary.id, true, "en", None)
        .await
        .unwrap();
    assert!(copy.copy && copy.text.contains("COPY"), "{}", copy.text);
    let first_return = r.summary.id.clone();
    let mut exchange = ret(1);
    let mut new_sale = sale(variant, unit, 1, cash_type, 0);
    new_sale.tenders.clear();
    new_sale.totals = older.summary.totals();
    exchange.exchange = Some(ExchangeRequest {
        sale_id: new_id(),
        sale: new_sale,
    });
    let r = register
        .post_return(cashier, &new_id(), &exchange, None)
        .await
        .unwrap();
    assert_eq!(r.tenders[0].kind, "exchange");
    assert_eq!(r.exchange_sale.unwrap().summary.total, price);
    // An older return's copy needs `sale.reprint`.
    assert_eq!(
        code_of(
            register
                .return_receipt(cashier, &first_return, true, "en", None)
                .await
        ),
        (403, "needs_override".to_owned())
    );
    // A sale with returns cannot be voided; the older one can, with a manager.
    let void = VoidRequest {
        reason: "wrong_item".to_owned(),
        reason_text: None,
        refund: "cash".to_owned(),
    };
    let t = register
        .request_override(cashier, &grant("sale.post_void", &id, None))
        .await
        .unwrap()
        .token;
    assert_eq!(
        code_of(
            register
                .void_sale(cashier, &id, &void, &new_id(), Some(&t))
                .await
        ),
        (409, "has_returns".to_owned())
    );
    assert_eq!(
        code_of(
            register
                .void_sale(cashier, &older_id, &void, &new_id(), None)
                .await
        ),
        (403, "needs_override".to_owned())
    );
    let t = register
        .request_override(cashier, &grant("sale.post_void", &older_id, None))
        .await
        .unwrap()
        .token;
    let voided = register
        .void_sale(cashier, &older_id, &void, &new_id(), Some(&t))
        .await
        .unwrap();
    assert_eq!(voided.summary.status, "voided");
    assert_eq!(voided.void.unwrap().cash, price);
    // Held carts.
    let cart = new_id();
    let hold = HoldRequest {
        note: Some("meja 2".to_owned()),
        payload: json!({ "lines": [] }),
        item_count: 1,
        total: 10_000,
        ..HoldRequest::default()
    };
    let parked = register.hold_cart(cashier, &cart, &hold).await.unwrap();
    assert_eq!(parked.note.as_deref(), Some("meja 2"));
    assert_eq!(register.held_carts(cashier).await.unwrap().len(), 1);
    let taken = register
        .take_held_cart(cashier, &cart, &new_id())
        .await
        .unwrap();
    assert!(taken.taken_by.is_some());
    assert_eq!(
        code_of(register.take_held_cart(cashier, &cart, &new_id()).await),
        (409, "cart_taken".to_owned())
    );
    let other_cart = new_id();
    register
        .hold_cart(cashier, &other_cart, &hold)
        .await
        .unwrap();
    assert_eq!(
        code_of(register.delete_held_cart(cashier, &other_cart, None).await),
        (403, "needs_override".to_owned())
    );
    let t = register
        .request_override(cashier, &grant("sale.void", &other_cart, None))
        .await
        .unwrap()
        .token;
    register
        .delete_held_cart(cashier, &other_cart, Some(&t))
        .await
        .unwrap();
    assert!(register.held_carts(cashier).await.unwrap().is_empty());
}

#[tokio::test]
async fn sales_over_http() {
    let (server, budi_id) = serve().await;
    let office = sultan_client::HttpService::user(&server.root);
    let register = sultan_client::HttpService::new(&server.root, &server.device);
    let owner = office
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let item = product(&server.root, &owner).await;
    let users = register.lock_users().await.unwrap();
    let ani = users.iter().find(|u| u.display_name == "ani").unwrap();
    let cashier = register.pin_login(&ani.id, "1234").await.unwrap().token;
    flow(
        &register,
        &cashier,
        &budi_id,
        "9999",
        (&item.0, &item.1, 10_000),
        (CASH, "00000000-0000-7000-8000-000000000303"),
    )
    .await;
    // History: a customer's sale.
    let customer = new_id();
    let r = reqwest::Client::new()
        .put(format!("{}/api/v1/customers/{customer}", server.root))
        .bearer_auth(&owner)
        .json(&json!({ "name": "Bu Sari" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 201);
    let mut body = sale(&item.0, &item.1, 1, CASH, 10_000);
    body.customer_id = Some(customer.clone());
    post(&register, &cashier, body).await;
    let history = register
        .customer_history(&cashier, &customer, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 1);
    assert_eq!(history.items[0].customer_name, "Bu Sari");
    assert!(
        register
            .customer_history(&cashier, &customer, Some("nope"))
            .await
            .is_err()
    );
    assert_eq!(
        register
            .recent_receipts(&cashier, &customer)
            .await
            .unwrap()
            .len(),
        1
    );
    // Back office: tender types and returns.
    let id = new_id();
    let body = TenderTypeBody {
        name: "Debit BCA".to_owned(),
        kind: "card".to_owned(),
        needs_reference: true,
        active: true,
        ..TenderTypeBody::default()
    };
    let created = office.put_tender_type(&owner, &id, &body).await.unwrap();
    assert_eq!(created.kind, "card");
    let archived = office.delete_tender_type(&owner, &id).await.unwrap();
    assert!(archived.archived);
    assert_eq!(office.all_tender_types(&owner).await.unwrap().len(), 5);
    let returns = office.returns(&owner, &DocQuery::default()).await.unwrap();
    assert_eq!(returns.items.len(), 2);
    let one = office
        .sale_return(&owner, &returns.items[0].id)
        .await
        .unwrap();
    assert_eq!(one.summary.id, returns.items[0].id);
    assert_eq!(
        code_of(office.sale_return(&owner, &new_id()).await),
        (404, "not_found".to_owned())
    );
    settings_and_lists(&office, &owner, &customer).await;
}

/// Phase S6: the sales settings, the status filter and the names in the lists.
async fn settings_and_lists(office: &dyn BackOffice, owner: &str, customer: &str) {
    let mut settings = office.sales_settings(owner).await.unwrap();
    assert_eq!(settings.return_days, 30);
    assert!(!settings.receipt_widths.is_empty());
    settings.return_days = 0;
    settings.receipt_widths.truncate(1);
    settings.receipt_widths[0].receipt_width = 42;
    let stored = office.put_sales_settings(owner, &settings).await.unwrap();
    assert_eq!(stored.return_days, 0);
    assert_eq!(stored.receipt_widths[0].receipt_width, 42);
    settings.receipt_widths[0].receipt_width = 40;
    assert_eq!(
        code_of(office.put_sales_settings(owner, &settings).await).1,
        "invalid_field"
    );
    let voided = office
        .sales(
            owner,
            &DocQuery {
                status: Some("voided".to_owned()),
                ..DocQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(voided.items.len(), 1);
    let named = office
        .sales(
            owner,
            &DocQuery {
                customer_id: Some(customer.to_owned()),
                ..DocQuery::default()
            },
        )
        .await
        .unwrap();
    assert!(named.items[0].customer_name.is_some());
    assert!(named.items[0].customer_code.is_some());
    let page = office
        .sales(
            owner,
            &DocQuery {
                limit: Some(1),
                ..DocQuery::default()
            },
        )
        .await
        .unwrap();
    let next = page.next_before_id.clone().unwrap();
    let more = office
        .sales(
            owner,
            &DocQuery {
                limit: Some(1),
                before_id: Some(next),
                ..DocQuery::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(more.items[0].id, page.items[0].id);
}

#[tokio::test]
async fn sales_in_the_fake() {
    let fake = FakeService::default();
    let demo = fake.seed_pricing_demo();
    let variant = demo.teh.variants[0].id.clone();
    let unit = demo.teh.base_unit_id.clone();
    let cashier = fake.pin_login("u-ani", "1234").await.unwrap().token;
    // The pricing customer gets a record, so lists can name it.
    fake.seed_customer(sultan_client::Customer {
        id: demo.gold_customer.clone(),
        code: "G0LD0001".to_owned(),
        name: "Ibu Emas".to_owned(),
        ..sultan_client::Customer::default()
    });
    // The fake's Teh costs 5 000: a sale of 2 is 10 000.
    flow_fake(&fake, &cashier, (&variant, &unit), &demo.gold_customer).await;
    let owner = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let body = TenderTypeBody {
        name: "Debit BCA".to_owned(),
        kind: "card".to_owned(),
        active: true,
        ..TenderTypeBody::default()
    };
    let t = fake.put_tender_type(&owner, "t-bca", &body).await.unwrap();
    assert_eq!(t.name, "Debit BCA");
    let mut bad = body.clone();
    bad.kind = "qris".to_owned();
    assert_eq!(
        code_of(fake.put_tender_type(&owner, "t-bca", &bad).await).1,
        "invalid_field"
    );
    bad.kind = "exchange".to_owned();
    assert_eq!(
        code_of(fake.put_tender_type(&owner, "t-x", &bad).await).1,
        "invalid_field"
    );
    assert!(
        fake.delete_tender_type(&owner, "t-bca")
            .await
            .unwrap()
            .archived
    );
    assert_eq!(
        code_of(fake.put_tender_type(&owner, "t-bca", &body).await).1,
        "archived"
    );
    assert_eq!(
        code_of(fake.delete_tender_type(&owner, "t-none").await).1,
        "not_found"
    );
    assert_eq!(fake.all_tender_types(&owner).await.unwrap().len(), 5);
    assert_eq!(fake.tender_types(&owner).await.unwrap().len(), 4);
    let budi = fake
        .password_login("budi", "budi-pass-1")
        .await
        .unwrap()
        .token;
    assert_eq!(
        code_of(fake.put_tender_type(&budi, "t-y", &body).await).1,
        "permission_denied"
    );
    let returns = fake.returns(&owner, &DocQuery::default()).await.unwrap();
    assert!(!returns.items.is_empty());
    settings_and_lists(&fake, &owner, &demo.gold_customer).await;
    assert_eq!(
        code_of(
            fake.put_sales_settings(&budi, &fake.sales_settings(&owner).await.unwrap())
                .await
        )
        .1,
        "permission_denied"
    );
    let one = fake
        .sale_return(&owner, &returns.items[0].id)
        .await
        .unwrap();
    assert_eq!(one.summary.id, returns.items[0].id);
    assert_eq!(
        code_of(fake.sale_return(&owner, "nope").await).1,
        "not_found"
    );
    assert_eq!(fake.cart_voids().len(), 1);
}

/// The fake flow (its own tender IDs and prices).
async fn flow_fake(
    fake: &FakeService,
    cashier: &str,
    (variant, unit): (&str, &str),
    customer: &str,
) {
    let register: &dyn Service = fake;
    flow(
        register,
        cashier,
        "u-budi",
        "9999",
        (variant, unit, 5_000),
        (TENDER_CASH, TENDER_QRIS),
    )
    .await;
    // History of a seeded customer.
    let mut body = sale(variant, unit, 1, TENDER_CASH, 5_000);
    body.customer_id = Some(customer.to_owned());
    let with_customer = post(register, cashier, body).await;
    let receipt = register
        .receipt(cashier, &with_customer.summary.id, false, "en", None)
        .await
        .unwrap();
    assert!(receipt.text.contains("Cashier"), "{}", receipt.text);
    assert_eq!(receipt.barcode, with_customer.summary.number);
    let history = register
        .customer_history(cashier, customer, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 1);
    let recent = register.recent_receipts(cashier, customer).await.unwrap();
    assert_eq!(recent.len(), 1);
}
