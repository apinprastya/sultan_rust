//! Pricing (phase R3, spec 2.3 §10): one contract run against [`HttpService`] on the real router
//! (in-memory `SQLite`) and against [`FakeService`], so the pricing UI tests see the server's
//! answers: price lists and entries with their errors (`price_list_conflict`, `price_overlap`,
//! `invalid_rule`, `permission_denied`), product prices, tax rates, settings, the lookup price
//! and the quote with a Gold customer.

use std::sync::Arc;

use serde_json::json;
use sultan_client::fake::{FakeService, PriceCustomer};
use sultan_client::pricing::DEFAULT_PRICE_LIST;
use sultan_client::{
    BackOffice, ClientError, HttpService, PriceEntryBody, PriceListBody, PriceRule, PriceTier,
    PricingSettings, Product, QuoteLine, QuoteRequest, Service, TaxRate,
};
use sultan_core::access::BranchScope;
use sultan_core::catalog::gtin_check_digit;
use sultan_core::ids::{BranchId, CompanyId, ProductId, RegisterId, UserId};
use sultan_db::catalog::Actor;
use sultan_db::customers::{self, Customer, MembershipTier};
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router, token};

const STANDARD: &str = "00000000-0000-7000-8000-000000000101";

struct Server {
    root: String,
    device: String,
    gold_tier: String,
    gold_customer: String,
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

/// Owner `owner` (all branches), cashier `kasir` (password) and Ani (PIN 1234) at JKT01 with
/// register R1; a Gold tier and a customer holding it.
#[allow(clippy::too_many_lines)] // the whole fixture, in order
async fn serve() -> Server {
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
    let actor = Actor {
        user: None,
        branch: None,
        register: None,
    };
    let tier = MembershipTier {
        id: ProductId::generate().into(),
        name: "Gold".to_owned(),
        rank: 1,
        archived_at: None,
    };
    let customer = Customer {
        id: ProductId::generate().into(),
        code: "C1".to_owned(),
        name: "Budi".to_owned(),
        phone: None,
        email: None,
        birthday: None,
        address: None,
        company_name: None,
        tax_id: None,
        tax_address: None,
        notes: None,
        group: None,
        tier: Some(tier.id),
        tier_valid_until: None,
        manual_tier: false,
        signed_up_branch: None,
        signed_up_at: now,
    };
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
            user("kasir", Some("kasir-pass-1"), None),
            seed::CASHIER_ROLE,
            BranchScope::Branch(jkt.id),
        ),
        (
            user("ani", None, Some("1234")),
            seed::CASHIER_ROLE,
            BranchScope::Branch(jkt.id),
        ),
    ] {
        users::save_user(&mut tx, &u, now).await.unwrap();
        roles::replace_assignments(&mut tx, u.id, &[(role, scope)], now)
            .await
            .unwrap();
    }
    devices::insert_device_token(&mut tx, &token::hash(&device), register.id, now)
        .await
        .unwrap();
    customers::put_tier(&mut tx, &tier, &actor, now)
        .await
        .unwrap();
    customers::put_customer(&mut tx, &customer, &actor, now)
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
        gold_tier: tier.id.to_string(),
        gold_customer: customer.id.to_string(),
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

fn entry(variant: &str, unit: &str, tiers: Vec<PriceTier>) -> PriceEntryBody {
    PriceEntryBody {
        variant_id: variant.to_owned(),
        unit_id: unit.to_owned(),
        valid_from: None,
        valid_to: None,
        tiers,
    }
}

fn list(kind: &str, target: Option<&str>, rule: Option<PriceRule>) -> PriceListBody {
    PriceListBody {
        kind: kind.to_owned(),
        target_id: target.map(str::to_owned),
        name: format!("{kind} list"),
        active: true,
        rule,
        archived: false,
    }
}

#[allow(clippy::too_many_lines)] // one contract, step by step
async fn contract(office: &dyn BackOffice, register: &dyn Service, gold_tier: &str, gold: &str) {
    let owner = office
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let kasir = office
        .password_login("kasir", "kasir-pass-1")
        .await
        .unwrap()
        .token;
    let (pcs, box_, variant) = (new_id(), new_id(), new_id());
    let barcode = format!("899100000003{}", gtin_check_digit("899100000003").unwrap());
    let product: Product = serde_json::from_value(json!({
        "code": "TEH", "name": "Teh Botol", "type": "stock", "tax_class_id": STANDARD,
        "units": [
            { "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 },
            { "id": box_, "name": "box", "kind": "integer", "factor_thousandths": 24000 },
        ],
        "base_unit_id": pcs,
        "variants": [{ "id": variant }],
        "barcodes": [
            { "id": new_id(), "variant_id": variant, "unit_id": pcs, "code": barcode },
            { "id": new_id(), "variant_id": variant, "unit_id": box_, "code": "TEH-BOX" },
        ],
    }))
    .unwrap();
    let product_id = new_id();
    office
        .put_product(&owner, &product_id, &product)
        .await
        .unwrap();

    // Lists and entries.
    let lists = office.price_lists(&owner, false).await.unwrap();
    assert_eq!(lists.len(), 1);
    assert_eq!(lists[0].id, DEFAULT_PRICE_LIST);
    let entry_id = new_id();
    let body = entry(
        &variant,
        &pcs,
        vec![
            PriceTier::fixed(12_000, 9_000),
            PriceTier::fixed(1_000, 10_000),
        ],
    );
    let stored = office
        .put_price_entry(&owner, DEFAULT_PRICE_LIST, &entry_id, &body)
        .await
        .unwrap();
    assert_eq!(stored.tiers[0], PriceTier::fixed(1_000, 10_000));
    assert_eq!(
        office
            .put_price_entry(&owner, DEFAULT_PRICE_LIST, &entry_id, &body)
            .await
            .unwrap(),
        stored
    );
    let overlap = office
        .put_price_entry(&owner, DEFAULT_PRICE_LIST, &new_id(), &body)
        .await;
    let err = overlap.unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!((p.status, p.code.as_str()), (409, "price_overlap"));
    assert_eq!(p.extra["entry_id"], entry_id);
    let ruled = entry(
        &variant,
        &box_,
        vec![PriceTier::rule(1_000, PriceRule::pct(-500))],
    );
    assert_eq!(
        code_of(
            office
                .put_price_entry(&owner, DEFAULT_PRICE_LIST, &new_id(), &ruled)
                .await
        ),
        (422, "invalid_rule".into())
    );
    let mut default = list("default", None, Some(PriceRule::pct(-500)));
    default.name = "Default".into();
    assert_eq!(
        code_of(
            office
                .put_price_list(&owner, DEFAULT_PRICE_LIST, &default)
                .await
        ),
        (422, "invalid_rule".into())
    );
    let gold_list = new_id();
    let body = list("tier", Some(gold_tier), Some(PriceRule::pct(-500)));
    let stored = office
        .put_price_list(&owner, &gold_list, &body)
        .await
        .unwrap();
    assert_eq!(
        (stored.kind.as_str(), stored.rule),
        ("tier", Some(PriceRule::pct(-500)))
    );
    assert_eq!(
        code_of(office.put_price_list(&owner, &new_id(), &body).await),
        (409, "price_list_conflict".into())
    );
    assert_eq!(
        code_of(office.put_price_list(&kasir, &new_id(), &body).await),
        (403, "permission_denied".into())
    );
    let page = office
        .price_entries(&owner, DEFAULT_PRICE_LIST, Some(&variant), None)
        .await
        .unwrap();
    assert_eq!((page.entries.len(), page.next_after_id), (1, None));
    let empty = office
        .price_entries(&owner, DEFAULT_PRICE_LIST, None, Some(&entry_id))
        .await
        .unwrap();
    assert!(empty.entries.is_empty());
    let prices = office.product_prices(&owner, &product_id).await.unwrap();
    assert_eq!((prices.lists.len(), prices.entries.len()), (2, 1));
    assert_eq!(
        code_of(office.product_prices(&owner, &new_id()).await),
        (404, "not_found".into())
    );

    // Tax rates and settings.
    let rates = office.tax_rates(&owner, STANDARD).await.unwrap();
    assert_eq!(
        rates,
        vec![TaxRate {
            rate_bp: 1100,
            valid_from: "2022-04-01".into()
        }]
    );
    let future = TaxRate {
        rate_bp: 1200,
        valid_from: "2099-01-01".into(),
    };
    assert_eq!(
        office
            .put_tax_rate(&owner, STANDARD, &future)
            .await
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        code_of(office.tax_rates(&owner, &new_id()).await),
        (404, "not_found".into())
    );
    let settings = office.pricing_settings(&owner).await.unwrap();
    assert_eq!(settings, PricingSettings::default());
    let rounding = PricingSettings {
        cash_rounding: "nearest_100".into(),
        ..settings
    };
    assert_eq!(
        office
            .put_pricing_settings(&owner, &rounding)
            .await
            .unwrap(),
        rounding
    );
    let bad = PricingSettings {
        cash_rounding: "up".into(),
        ..PricingSettings::default()
    };
    assert_eq!(
        code_of(office.put_pricing_settings(&owner, &bad).await),
        (422, "invalid_field".into())
    );

    // The register: lookup price and quote (cashier session and device token alone).
    let users = register.lock_users().await.unwrap();
    let ani = users
        .iter()
        .find(|u| u.display_name.contains("ni"))
        .unwrap();
    let cashier = register.pin_login(&ani.id, "1234").await.unwrap().token;
    let found = register.lookup(&cashier, &barcode).await.unwrap();
    assert_eq!(found.unit_price, Some(10_000));
    // One box counts 24 pcs: the tier from 12 → 24 × 9 000.
    let found = register.lookup(&cashier, "TEH-BOX").await.unwrap();
    assert_eq!(found.unit_price, Some(216_000));
    let walk_in = QuoteRequest {
        customer_id: None,
        lines: vec![QuoteLine::new(&variant, &pcs, 5_000)],
    };
    let q = register.quote(&cashier, &walk_in).await.unwrap();
    assert_eq!(
        (q.lines[0].unit_price, q.totals.amount),
        (Some(10_000), 50_000)
    );
    assert_eq!(q.lines[0].tax.as_ref().unwrap().tax, 4_955);
    assert_eq!(q.cash_rounding, "nearest_100");
    let q = register.quote("", &walk_in).await.unwrap();
    assert_eq!(q.totals.amount, 50_000);
    let gold_cart = QuoteRequest {
        customer_id: Some(gold.to_owned()),
        lines: vec![
            QuoteLine::new(&variant, &pcs, 5_000),
            QuoteLine::new(&variant, &box_, 1_000),
        ],
    };
    // 5 pcs + 1 box count 29 → the tier 9 000, Gold −5% → 8 550; the box falls back to 24 ×.
    let q = register.quote(&cashier, &gold_cart).await.unwrap();
    let source = q.lines[1].source.as_ref().unwrap();
    assert_eq!(
        (q.lines[0].unit_price, q.lines[1].unit_price),
        (Some(8_550), Some(205_200))
    );
    assert_eq!((source.kind.as_str(), source.unit_fallback), ("tier", true));
    assert_eq!(source.list_id, gold_list);
    let unknown = QuoteRequest {
        customer_id: Some(new_id()),
        lines: Vec::new(),
    };
    assert_eq!(
        code_of(register.quote(&cashier, &unknown).await),
        (422, "invalid_reference".into())
    );

    // Archive the Gold list; delete the entry: no price.
    let archived = office
        .archive_price_list(&owner, &gold_list, "k-1")
        .await
        .unwrap();
    assert!(archived.archived);
    let q = register.quote(&cashier, &gold_cart).await.unwrap();
    assert_eq!(q.lines[0].unit_price, Some(9_000));
    office
        .delete_price_entry(&owner, DEFAULT_PRICE_LIST, &entry_id)
        .await
        .unwrap();
    assert_eq!(
        code_of(
            office
                .delete_price_entry(&owner, DEFAULT_PRICE_LIST, &entry_id)
                .await
        ),
        (404, "not_found".into())
    );
    let q = register.quote(&cashier, &walk_in).await.unwrap();
    assert_eq!(q.lines[0].error.as_ref().unwrap().code, "no_price");
    assert_eq!(q.totals.amount, 0);
    assert_eq!(
        register
            .lookup(&cashier, &barcode)
            .await
            .unwrap()
            .unit_price,
        None
    );
    assert_eq!(office.price_lists(&owner, true).await.unwrap().len(), 2);
}

#[tokio::test]
async fn http_pricing_against_the_server() {
    let s = serve().await;
    let office = HttpService::user(&s.root);
    let register = HttpService::new(&s.root, &s.device);
    contract(&office, &register, &s.gold_tier, &s.gold_customer).await;
}

#[tokio::test]
async fn fake_pricing_follows_the_same_contract() {
    let fake = FakeService::default();
    fake.seed_price_customer(
        "customer-1",
        PriceCustomer {
            tier: Some("tier-gold".into()),
            ..PriceCustomer::default()
        },
    );
    contract(&fake, &fake, "tier-gold", "customer-1").await;
    assert!(!fake.calls_of("quote").is_empty());
}

#[tokio::test]
async fn fake_pricing_demo_and_failures() {
    let fake = FakeService::default();
    let demo = fake.seed_pricing_demo();
    let cashier = fake.pin_login("u-ani", "1234").await.unwrap().token;
    let found = fake.lookup(&cashier, "AQUA-BOX").await.unwrap();
    assert_eq!(found.unit_price, Some(84_000));
    let teh = &demo.teh;
    let request = QuoteRequest {
        customer_id: Some(demo.gold_customer.clone()),
        lines: vec![QuoteLine::new(
            &teh.variants[0].id,
            &teh.base_unit_id,
            12_000,
        )],
    };
    let q = fake.quote(&cashier, &request).await.unwrap();
    // 12 pcs → the tier 4 500, Gold −5% → 4 275.
    assert_eq!(q.lines[0].unit_price, Some(4_275));
    assert_eq!(q.lines[0].source.as_ref().unwrap().list_id, demo.gold_list);
    let owner = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let me = fake.me(&owner).await.unwrap();
    assert!(me.permissions.contains(&"price.manage".to_owned()));
    fake.fail_next(ClientError::Network("down".into()));
    assert!(matches!(
        fake.quote(&cashier, &request).await,
        Err(ClientError::Network(_))
    ));
    assert_eq!(
        code_of(fake.quote("nope", &request).await),
        (401, "invalid_session".into())
    );
    let c = HttpService::new("http://127.0.0.1:1", "device");
    assert!(matches!(
        c.quote("", &request).await,
        Err(ClientError::Network(_))
    ));
}
