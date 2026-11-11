//! Customers (spec 2.5, phase M3): one contract run against [`HttpService`] on the real router
//! (in-memory `SQLite`, `127.0.0.1:0`) and against [`FakeService`], so the fake answers like the
//! server: sign-up and replay, `phone_taken`, lookup by phone and code, name search, edit with a
//! tier, the balance and its payout (expected cash of the close report), merge, archive, erase,
//! review lists, groups, tiers and the balance settings.

use std::sync::{Arc, Mutex};

use sultan_client::customers::{CUSTOMER_CREATE, CUSTOMER_ERASE, CUSTOMER_MANAGE, CUSTOMER_SEARCH};
use sultan_client::fake::{ERASED_NAME, FakeService, SEED_BUDI, SEED_GOLD, SEED_SITI};
use sultan_client::{
    BackOffice, BalanceSettings, ClientError, CloseRequest, CustomerBody, CustomerQuery, EventSink,
    GroupBody, HttpService, ServerEvent, Service, TierBody,
};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, RegisterId, UserId};
use sultan_core::money::Money;
use sultan_db::customers::{self as cust, BalanceEntry, BalanceEntryKind};
use sultan_db::org::{Branch, Company, Register};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, NodeRole, SystemClock, credentials, router, token};

struct Server {
    root: String,
    device: String,
    ani: String,
    budi: String,
    db: Db,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn user(username: &str, name: &str, pin: Option<&str>, password: Option<&str>) -> User {
    User {
        id: UserId::generate(),
        username: username.to_owned(),
        display_name: name.to_owned(),
        pin_hash: pin.map(|p| credentials::hash_pin(p).unwrap()),
        password_hash: password.map(|p| credentials::hash_password(p).unwrap()),
        active: true,
    }
}

/// Branch JKT01 with a paired register; `owner` (Owner, password), `budi` (Manager, PIN 9999 and
/// password), `ani` (Cashier, PIN 1234) and `kasir` (Cashier, password), as in the fake.
async fn serve() -> Server {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    let now = SystemClock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let branch = Branch {
        id: BranchId::generate(),
        company: company.id,
        code: "JKT01".to_owned(),
        name: "Toko Jakarta".to_owned(),
        address: String::new(),
        npwp: None,
        receipt_header: String::new(),
        receipt_footer: String::new(),
        timezone: "Asia/Jakarta".to_owned(),
        archived: false,
    };
    let register = Register {
        id: RegisterId::generate(),
        branch: branch.id,
        code: "R1".to_owned(),
        name: "Kasir 1".to_owned(),
        default_location: None,
        archived: false,
    };
    let owner = user("owner", "Owner", None, Some("owner-pass-1"));
    let budi = user("budi", "Budi", Some("9999"), Some("budi-pass-1"));
    let ani = user("ani", "Ani", Some("1234"), None);
    let kasir = user("kasir", "Kasir", None, Some("kasir-pass-1"));
    let device = token::generate();
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    org::save_branch(&mut tx, &branch, now).await.unwrap();
    org::save_register(&mut tx, &register, now).await.unwrap();
    let here = BranchScope::Branch(branch.id);
    for (u, role, scope) in [
        (&owner, seed::OWNER_ROLE, BranchScope::All),
        (&budi, seed::MANAGER_ROLE, here),
        (&ani, seed::CASHIER_ROLE, here),
        (&kasir, seed::CASHIER_ROLE, here),
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
    let state = AppState::new(db.clone(), NodeRole::Standalone, Arc::new(SystemClock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    Server {
        root: format!("http://{addr}"),
        device,
        ani: ani.id.to_string(),
        budi: budi.id.to_string(),
        db,
        task,
    }
}

fn new_id() -> String {
    sultan_core::ids::ProductId::generate().to_string()
}

#[track_caller]
fn code_of<T: std::fmt::Debug>(result: Result<T, ClientError>) -> (u16, String) {
    let err = result.unwrap_err();
    let p = err.problem().unwrap_or_else(|| panic!("{err:?}"));
    (p.status, p.code.clone())
}

/// What part one of the contract leaves for part two.
struct Setup {
    owner: String,
    ani: String,
    budi: String,
    rudi: String,
}

/// Sign-up, lookup, search, edit with a tier, groups and tiers.
#[allow(clippy::too_many_lines)] // one flow over the whole customer book
async fn contract_one(c: &dyn BackOffice, ani_id: &str, budi_id: &str) -> Setup {
    let owner = c
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let kasir = c
        .password_login("kasir", "kasir-pass-1")
        .await
        .unwrap()
        .token;
    let budi_bo = c.password_login("budi", "budi-pass-1").await.unwrap().token;
    let me = c.me(&owner).await.unwrap();
    assert!(me.holds_everywhere(CUSTOMER_ERASE));
    let me = c.me(&budi_bo).await.unwrap();
    assert!(me.holds_here(CUSTOMER_MANAGE) && !me.holds_here(CUSTOMER_ERASE));
    let me = c.me(&kasir).await.unwrap();
    assert!(me.holds_here(CUSTOMER_CREATE) && !me.holds_here(CUSTOMER_SEARCH));

    // Groups and tiers.
    let group = new_id();
    let reseller = GroupBody {
        name: " Reseller ".into(),
        archived: false,
    };
    let g = c
        .put_customer_group(&owner, &group, &reseller)
        .await
        .unwrap();
    assert_eq!(
        (g.id.as_str(), g.name.as_str()),
        (group.as_str(), "Reseller")
    );
    let clash = c.put_customer_group(&owner, &new_id(), &reseller).await;
    assert_eq!(code_of(clash), (409, "conflict".into()));
    assert_eq!(
        code_of(c.put_customer_group(&kasir, &new_id(), &reseller).await).0,
        403
    );
    assert!(
        c.customer_groups(&owner, false)
            .await
            .unwrap()
            .iter()
            .any(|g| g.id == group)
    );
    let platinum = new_id();
    let tier = TierBody {
        name: "Platinum".into(),
        rank: 30,
        archived: false,
    };
    c.put_membership_tier(&owner, &platinum, &tier)
        .await
        .unwrap();
    let rank_clash = TierBody {
        name: "Diamond".into(),
        ..tier.clone()
    };
    let err = c
        .put_membership_tier(&owner, &new_id(), &rank_clash)
        .await
        .unwrap_err();
    assert_eq!(err.problem().and_then(|p| p.field()), Some("rank"));
    let tiers = c.membership_tiers(&owner, true).await.unwrap();
    assert_eq!(tiers[0].id, platinum, "best first");

    // Sign-up at the till, replay, phone taken.
    let ani = c.pin_login(ani_id, "1234").await.unwrap().token;
    let budi = c.pin_login(budi_id, "9999").await.unwrap().token;
    let rudi = new_id();
    let body = CustomerBody::sign_up(" Rudi Hartono ", Some("0813-1111-2222"));
    let created = c.sign_up_customer(&ani, &rudi, &body).await.unwrap();
    assert_eq!(created.name, "Rudi Hartono");
    assert_eq!(created.phone.as_deref(), Some("+6281311112222"));
    assert_eq!(created.masked_phone.as_deref(), Some("0813*****222"));
    assert_eq!(created.code.len(), 8);
    assert_eq!(created.display_code.len(), 9);
    assert_eq!(
        c.sign_up_customer(&ani, &rudi, &body).await.unwrap(),
        created
    );
    let err = c
        .sign_up_customer(
            &ani,
            &new_id(),
            &CustomerBody::sign_up("Other", Some("+62 813 1111 2222")),
        )
        .await
        .unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!((p.status, p.code.as_str()), (409, "phone_taken"));
    assert_eq!(p.customer_code(), Some(created.display_code.as_str()));
    assert_eq!(p.masked_phone(), Some("0813*****222"));
    let bad = CustomerBody::sign_up("X", Some("12"));
    assert_eq!(
        code_of(c.sign_up_customer(&ani, &new_id(), &bad).await),
        (422, "invalid_phone".into())
    );
    let mut with_tier = body.clone();
    with_tier.tier_id = Some(platinum.clone());
    let err = c
        .sign_up_customer(&ani, &rudi, &with_tier)
        .await
        .unwrap_err();
    assert_eq!(
        err.problem().and_then(|p| p.permission()),
        Some(CUSTOMER_EDIT_KEY)
    );

    // Lookup.
    assert_eq!(
        c.customer_by_phone(&ani, "0813 1111 2222")
            .await
            .unwrap()
            .id,
        rudi
    );
    let typed = created.display_code.to_lowercase();
    assert_eq!(c.customer_by_code(&ani, &typed).await.unwrap().id, rudi);
    assert_eq!(
        code_of(c.customer_by_phone(&ani, "0899-0000-0001").await),
        (404, "not_found".into())
    );
    assert_eq!(
        code_of(c.customer_by_code(&ani, "K7QH-29X0").await),
        (422, "invalid_code".into())
    );

    // What the register shows: the PIN session's permission keys.
    let ani_keys = c.permissions(&ani).await.unwrap();
    assert!(ani_keys.iter().any(|k| k == CUSTOMER_CREATE));
    assert!(!ani_keys.iter().any(|k| k == CUSTOMER_SEARCH));
    let budi_keys = c.permissions(&budi).await.unwrap();
    for key in [CUSTOMER_SEARCH, "customer.balance_payout"] {
        assert!(budi_keys.iter().any(|k| k == key), "{key}");
    }
    assert!(c.permissions("nope").await.is_err());

    // Name search needs `customer.search`.
    let q = CustomerQuery::search("hartono");
    assert_eq!(
        code_of(c.search_customers(&ani, &q).await),
        (403, "permission_denied".into())
    );
    let page = c.search_customers(&budi, &q).await.unwrap();
    assert_eq!(
        page.items.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
        [rudi.as_str()]
    );
    assert_eq!(page.next_page, None);
    let listed = c
        .search_customers(&budi, &CustomerQuery::default())
        .await
        .unwrap();
    assert!(listed.items.iter().any(|c| c.id == rudi));

    // Edit with a tier in the back office.
    let mut edit = created.body();
    edit.tier_id = Some(platinum.clone());
    edit.tier_valid_until = Some("2099-12-31".into());
    edit.group_id = Some(group.clone());
    edit.email = Some("Rudi@Example.com".into());
    let edited = c.put_customer(&owner, &rudi, &edit).await.unwrap();
    assert_eq!(edited.tier_name.as_deref(), Some("Platinum"));
    assert_eq!(edited.tier_applies, Some(true));
    assert_eq!(edited.group_name.as_deref(), Some("Reseller"));
    assert_eq!(edited.email.as_deref(), Some("rudi@example.com"));
    assert_eq!(edited.code, created.code, "the code never changes");
    assert_eq!(c.customer(&owner, &rudi).await.unwrap(), edited);
    assert_eq!(code_of(c.customer(&owner, &new_id()).await).0, 404);
    Setup {
        owner,
        ani,
        budi,
        rudi,
    }
}

const CUSTOMER_EDIT_KEY: &str = "customer.edit";

/// The balance and its payout, merge, archive, erase, review lists and settings. Rudi has a
/// balance of 2 200 by now.
#[allow(clippy::too_many_lines)] // one flow over the balance and the HQ actions
async fn contract_two(c: &dyn BackOffice, s: &Setup) {
    let Setup {
        owner,
        ani,
        budi,
        rudi,
    } = s;
    let kasir = c
        .password_login("kasir", "kasir-pass-1")
        .await
        .unwrap()
        .token;
    let balance = c.customer_balance(ani, rudi, None).await.unwrap();
    assert_eq!((balance.balance, balance.entries.len()), (2_200, 1));
    assert_eq!(balance.next_before_id, None);

    // Payout: permission, open session, amount, idempotency, expected cash.
    let r = c.balance_payout(ani, rudi, 2_200, "p-ani").await;
    assert_eq!(code_of(r), (403, "permission_denied".into()));
    let r = c.balance_payout(owner, rudi, 2_200, "p-bo").await;
    assert_eq!(code_of(r), (403, "register_session_required".into()));
    let r = c.balance_payout(budi, rudi, 2_200, "p-0").await;
    assert_eq!(code_of(r), (409, "no_open_session".into()));
    let session = c.open_session(budi, 100_000, &new_id()).await.unwrap();
    let err = c
        .balance_payout(budi, rudi, 2_500, "p-1")
        .await
        .unwrap_err();
    assert_eq!(
        err.problem().and_then(sultan_client::Problem::balance),
        Some(2_200)
    );
    let write_off = c.balance_payout(budi, rudi, -1, "p-2").await.unwrap_err();
    assert_eq!(
        write_off.problem().and_then(|p| p.reason()),
        Some("owner_only")
    );
    let paid = c.balance_payout(budi, rudi, 2_200, "p-3").await.unwrap();
    assert_eq!(
        (paid.balance, paid.entry.amount, paid.entry.kind.as_str()),
        (0, -2_200, "payout")
    );
    assert_eq!(
        paid.entry.cashier_session_id.as_deref(),
        Some(session.id.as_str())
    );
    assert_eq!(
        c.balance_payout(budi, rudi, 2_200, "p-3").await.unwrap(),
        paid
    );
    let close = CloseRequest {
        counted_cash: 97_800,
        counts: None,
    };
    let report = c
        .close_session(budi, &session.id, &close, &new_id())
        .await
        .unwrap();
    assert_eq!(
        (report.cash_out, report.expected_cash, report.over_short),
        (2_200, 97_800, 0)
    );

    // Merge another sign-up into Rudi: its code finds Rudi afterwards.
    let dup = new_id();
    let other = c
        .sign_up_customer(ani, &dup, &CustomerBody::sign_up("Rudi H.", None))
        .await
        .unwrap();
    let merged = c.merge_customer(owner, rudi, &dup, "m-1").await.unwrap();
    assert_eq!(merged.id, *rudi);
    assert_eq!(
        c.merge_customer(owner, rudi, &dup, "m-1").await.unwrap(),
        merged,
        "replayed"
    );
    assert_eq!(
        code_of(c.merge_customer(owner, rudi, &dup, "m-2").await),
        (409, "merged".into())
    );
    let err = c
        .merge_customer(owner, rudi, rudi, "m-3")
        .await
        .unwrap_err();
    assert_eq!(err.problem().and_then(|p| p.field()), Some("from"));
    assert_eq!(
        c.customer_by_code(ani, &other.code).await.unwrap().id,
        *rudi
    );

    // Archive hides, unarchive brings back; archived reads need `customer.manage`.
    let archived = c
        .set_customer_archived(owner, rudi, true, "a-1")
        .await
        .unwrap();
    assert!(archived.archived);
    assert_eq!(
        code_of(c.customer(&kasir, rudi).await),
        (403, "permission_denied".into())
    );
    assert_eq!(
        code_of(c.customer_by_phone(ani, "081311112222").await).0,
        404
    );
    assert!(
        !c.set_customer_archived(owner, rudi, false, "a-2")
            .await
            .unwrap()
            .archived
    );

    // Erase: Owner only, final.
    assert_eq!(code_of(c.erase_customer(&kasir, rudi, "e-0").await).0, 403);
    let erased = c.erase_customer(owner, rudi, "e-1").await.unwrap();
    assert_eq!(
        (erased.name.as_str(), erased.phone.as_deref()),
        (ERASED_NAME, None)
    );
    assert!(erased.erased_at.is_some() && erased.archived);
    assert_eq!(
        code_of(c.erase_customer(owner, rudi, "e-2").await),
        (404, "not_found".into())
    );

    // Review lists.
    assert!(
        c.customer_duplicates(owner, 1)
            .await
            .unwrap()
            .next_page
            .is_none()
    );
    assert!(
        c.negative_balances(owner, 1)
            .await
            .unwrap()
            .items
            .iter()
            .all(|c| c.balance < 0)
    );
    assert_eq!(code_of(c.negative_balances(&kasir, 1).await).0, 403);
    assert_eq!(code_of(c.customer_duplicates(owner, 0).await).0, 422);

    // Balance settings.
    assert_eq!(
        c.customer_balance_settings(ani).await.unwrap(),
        BalanceSettings::default()
    );
    let new = BalanceSettings {
        max_saved_per_sale: 2_000,
        max_balance: 50_000,
    };
    assert_eq!(
        code_of(c.put_customer_balance_settings(&kasir, &new).await).0,
        403
    );
    let negative = BalanceSettings {
        max_balance: -1,
        ..new
    };
    let err = c
        .put_customer_balance_settings(owner, &negative)
        .await
        .unwrap_err();
    assert_eq!(err.problem().and_then(|p| p.field()), Some("max_balance"));
    assert_eq!(
        c.put_customer_balance_settings(owner, &new).await.unwrap(),
        new
    );
    assert_eq!(c.customer_balance_settings(owner).await.unwrap(), new);
}

#[tokio::test]
async fn http_customers_against_the_server() {
    let server = serve().await;
    let c = HttpService::new(&server.root, &server.device);
    let setup = contract_one(&c, &server.ani, &server.budi).await;
    let id = setup.rudi.parse().unwrap();
    let entry = BalanceEntry::new(
        id,
        BalanceEntryKind::ChangeSaved,
        Money::from_minor(2_200),
        SystemClock.now(),
    );
    let mut tx = server.db.begin().await.unwrap();
    cust::append_balance_entry(&mut tx, &entry, SystemClock.now())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    contract_two(&c, &setup).await;
}

#[tokio::test]
async fn fake_customers_follow_the_same_contract() {
    let fake = FakeService::default();
    let setup = contract_one(&fake, "u-ani", "u-budi").await;
    fake.add_customer_balance(&setup.rudi, "change_saved", 2_200);
    contract_two(&fake, &setup).await;
    assert_eq!(fake.balance_settings().max_balance, 50_000);
}

/// The fake's seed, its events and the paging of its balance entries.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one walk through the seed and the helpers
async fn fake_seed_events_and_paging() {
    let fake = FakeService::default();
    let budi = fake.pin_login("u-budi", "9999").await.unwrap().token;
    let seeded = fake.customer_by_code(&budi, "K7QH-29XM").await.unwrap();
    assert_eq!(
        (seeded.id.as_str(), seeded.name.as_str()),
        (SEED_BUDI, "Budi Santoso")
    );
    assert_eq!(seeded.tier_id.as_deref(), Some(SEED_GOLD));
    assert_eq!(
        (seeded.tier_name.as_deref(), seeded.balance),
        (Some("Gold"), 2_200)
    );
    let siti = fake
        .customer_by_phone(&budi, "0812-9876-5432")
        .await
        .unwrap();
    assert_eq!(siti.id, SEED_SITI);
    assert_eq!(siti.group_name.as_deref(), Some("Grosir"));
    assert_eq!(siti.masked_phone.as_deref(), Some("0812*****432"));
    let page = fake
        .search_customers(&budi, &CustomerQuery::search("toko"))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1, "the company name matches");
    let owner = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let tiers = fake.membership_tiers(&owner, false).await.unwrap();
    assert_eq!(
        tiers.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["Gold", "Silver"]
    );

    // Events: `customer.changed` without personal data.
    let events = Arc::new(Mutex::new(Vec::new()));
    let seen = events.clone();
    let sink: EventSink = Arc::new(move |e: ServerEvent| seen.lock().unwrap().push(e));
    fake.watch_events(&budi, sink).await.unwrap();
    fake.set_customer_archived(&owner, SEED_SITI, true, "k")
        .await
        .unwrap();
    let got = events.lock().unwrap().clone();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].kind, "customer.changed");
    assert_eq!(got[0].data, serde_json::json!({ "customer_id": SEED_SITI }));
    // An unarchive that changes nothing still answers the customer.
    let again = fake
        .set_customer_archived(&owner, SEED_SITI, true, "k2")
        .await
        .unwrap();
    assert!(again.archived);
    let all = CustomerQuery {
        include_archived: true,
        ..CustomerQuery::default()
    };
    assert_eq!(
        fake.search_customers(&owner, &all)
            .await
            .unwrap()
            .items
            .len(),
        2
    );

    // Balance entries page by 50, newest first.
    for i in 0..55 {
        fake.add_customer_balance(SEED_BUDI, "change_saved", i + 1);
    }
    let first = fake.customer_balance(&budi, SEED_BUDI, None).await.unwrap();
    assert_eq!((first.entries.len(), first.entries[0].amount), (50, 55));
    let cursor = first.next_before_id.unwrap();
    let second = fake
        .customer_balance(&budi, SEED_BUDI, Some(&cursor))
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 6);
    assert_eq!(
        code_of(fake.customer_balance(&budi, "nope", None).await).0,
        404
    );

    // Negative balances and possible duplicates.
    fake.add_customer_balance(SEED_SITI, "payment", -300);
    let negative = fake.negative_balances(&owner, 1).await.unwrap();
    assert_eq!(
        negative.items[0].id, SEED_SITI,
        "archived customers included"
    );
    let mut dup = fake.customer(&owner, SEED_BUDI).await.unwrap();
    dup.id = "customer-dup".into();
    dup.code = "ZZZZ2222".into();
    dup.phone = None;
    dup.phone_conflict = Some("+628123456789".into());
    fake.seed_customer(dup);
    let dups = fake.customer_duplicates(&owner, 1).await.unwrap();
    assert_eq!(dups.items[0].id, "customer-dup");

    // Injected failures and recorded calls.
    fake.fail_next(ClientError::Network("down".into()));
    assert!(
        fake.customer_by_phone(&budi, "0812-3456-789")
            .await
            .is_err()
    );
    assert_eq!(fake.calls_of("customer_by_phone").len(), 2);
    assert_eq!(
        code_of(fake.customer_by_phone("bad", "0812-3456-789").await).0,
        401
    );
}

/// Rules of the fake that the contract does not reach.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one table of rules, then one flow
async fn fake_rules() {
    let fake = FakeService::default();
    let owner = fake
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let put = |body: CustomerBody| fake.put_customer(&owner, &new_id(), &body);
    let cases = [
        (
            CustomerBody {
                tax_id: Some("1".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_tax_id",
        ),
        (
            CustomerBody {
                code: Some("K7QH29XM".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "conflict",
        ),
        (
            CustomerBody {
                email: Some("a@b@c".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_field",
        ),
        (
            CustomerBody {
                birth_month: Some(1),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_field",
        ),
        (
            CustomerBody {
                birth_month: Some(2),
                birth_day: Some(30),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_field",
        ),
        (
            CustomerBody {
                birth_month: Some(1),
                birth_day: Some(1),
                birth_year: Some(0),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_field",
        ),
        (
            CustomerBody {
                tier_valid_until: Some("2027-01-01".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_field",
        ),
        (
            CustomerBody {
                tier_id: Some(SEED_GOLD.into()),
                tier_valid_until: Some("x".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_field",
        ),
        (
            CustomerBody {
                group_id: Some("nope".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_reference",
        ),
        (
            CustomerBody {
                tier_id: Some("nope".into()),
                ..CustomerBody::sign_up("A", None)
            },
            "invalid_reference",
        ),
        (CustomerBody::sign_up(" ", None), "invalid_field"),
    ];
    for (body, code) in cases {
        assert_eq!(code_of(put(body.clone()).await).1, code, "{body:?}");
    }
    let full = CustomerBody {
        code: Some("m2m2-m2m2".into()),
        birth_month: Some(2),
        birth_day: Some(29),
        birth_year: Some(1992),
        tax_id: Some("01.234.567.8-901.000".into()),
        tier_id: Some(SEED_GOLD.into()),
        tier_valid_until: Some("2020-1-1".into()),
        ..CustomerBody::sign_up("Lia", Some("+65 9123 4567"))
    };
    let lia = put(full).await.unwrap();
    assert_eq!(
        (lia.code.as_str(), lia.display_code.as_str()),
        ("M2M2M2M2", "M2M2-M2M2")
    );
    assert_eq!(lia.tier_valid_until.as_deref(), Some("2020-01-01"));
    assert_eq!(lia.tier_applies, Some(false), "expired");
    assert_eq!(lia.tax_id.as_deref(), Some("012345678901000"));

    // Archived and merged customers are not edited; unarchive clashes on the phone.
    let siti = fake.customer(&owner, SEED_SITI).await.unwrap();
    fake.set_customer_archived(&owner, SEED_SITI, true, "a")
        .await
        .unwrap();
    let mut edit = siti.body();
    assert_eq!(
        code_of(fake.put_customer(&owner, SEED_SITI, &edit).await).1,
        "archived"
    );
    edit.name = "Siti R.".into();
    assert_eq!(
        code_of(fake.put_customer(&owner, SEED_SITI, &edit).await).1,
        "archived"
    );
    let taker = CustomerBody::sign_up("Taker", Some("0812-9876-5432"));
    let taker = fake.put_customer(&owner, &new_id(), &taker).await.unwrap();
    let err = fake
        .set_customer_archived(&owner, SEED_SITI, false, "u")
        .await
        .unwrap_err();
    let p = err.problem().unwrap();
    assert_eq!(p.code, "phone_taken");
    assert_eq!(p.customer_code(), Some(taker.display_code.as_str()));
    // Merge a customer with a balance into one without a tier: the tier and balance move.
    let a = put(CustomerBody::sign_up("A", None)).await.unwrap();
    let merged = fake
        .merge_customer(&owner, &a.id, SEED_BUDI, "m")
        .await
        .unwrap();
    assert_eq!(
        (merged.balance, merged.tier_id.as_deref()),
        (2_200, Some(SEED_GOLD))
    );
    assert_eq!(merged.phone.as_deref(), Some("+628123456789"));
    let unarchive = fake
        .set_customer_archived(&owner, SEED_BUDI, false, "mu")
        .await;
    assert_eq!(code_of(unarchive).1, "merged");
    assert_eq!(
        code_of(
            fake.put_customer(&owner, SEED_BUDI, &CustomerBody::sign_up("B", None))
                .await
        )
        .1,
        "merged"
    );
    assert_eq!(
        code_of(fake.merge_customer(&owner, SEED_SITI, &a.id, "m2").await).1,
        "archived"
    );
    // A had no phone, so it took Budi's.
    let budi = fake.pin_login("u-budi", "9999").await.unwrap().token;
    assert_eq!(
        fake.customer_by_phone(&budi, "0812-3456-789")
            .await
            .unwrap()
            .id,
        a.id
    );
    // A cashier at the register cannot edit or search, a manager can; groups and tiers.
    let ani = fake.pin_login("u-ani", "1234").await.unwrap().token;
    let mut renamed = merged.body();
    renamed.name = "A2".into();
    assert_eq!(
        code_of(fake.sign_up_customer(&ani, &a.id, &renamed).await).0,
        403
    );
    assert_eq!(
        fake.sign_up_customer(&budi, &a.id, &renamed)
            .await
            .unwrap()
            .name,
        "A2"
    );
    let tier = TierBody {
        name: "Gold".into(),
        rank: 9,
        archived: false,
    };
    assert_eq!(
        code_of(fake.put_membership_tier(&owner, "t", &tier).await).1,
        "conflict"
    );
    let gold = TierBody {
        name: "Gold".into(),
        rank: 2,
        archived: true,
    };
    assert!(
        fake.put_membership_tier(&owner, SEED_GOLD, &gold)
            .await
            .unwrap()
            .archived
    );
    assert_eq!(fake.membership_tiers(&owner, false).await.unwrap().len(), 1);
    assert_eq!(
        fake.customer(&owner, &a.id).await.unwrap().tier_applies,
        Some(false)
    );
    let group = GroupBody {
        name: "Grosir".into(),
        archived: true,
    };
    fake.put_customer_group(&owner, sultan_client::fake::SEED_GROUP, &group)
        .await
        .unwrap();
    assert!(
        fake.customer_groups(&owner, false)
            .await
            .unwrap()
            .is_empty()
    );
    let limit = CustomerQuery {
        limit: Some(21),
        ..CustomerQuery::search("a")
    };
    assert_eq!(
        code_of(fake.search_customers(&budi, &limit).await).1,
        "invalid_field"
    );
    let page0 = CustomerQuery {
        page: Some(0),
        ..CustomerQuery::search("a")
    };
    assert_eq!(
        code_of(fake.search_customers(&budi, &page0).await).1,
        "invalid_field"
    );
    let kasir = fake
        .password_login("kasir", "kasir-pass-1")
        .await
        .unwrap()
        .token;
    let all = CustomerQuery {
        include_archived: true,
        ..CustomerQuery::default()
    };
    assert!(fake.search_customers(&budi, &all).await.is_ok());
    assert_eq!(code_of(fake.search_customers(&kasir, &all).await).0, 403);
    assert_eq!(code_of(fake.customer_balance_settings("bad").await).0, 401);
    let erase = fake.erase_customer(&owner, &a.id, "e").await;
    assert_eq!(code_of(erase).1, "balance_not_zero");
}
