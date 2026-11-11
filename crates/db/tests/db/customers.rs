//! Customers, groups, tiers, the balance ledger and its settings (spec 2.5, phase M2).

// Customers are named a, b, c, … after the spec's merge examples (A absorbs B).
#![allow(
    clippy::many_single_char_names,
    reason = "a, b, c follow the spec's examples"
)]

use std::borrow::Cow;
use std::path::Path;

use sultan_core::access::Permission;
use sultan_core::money::Money;
use sultan_db::audit::{self, AuditFilter};
use sultan_db::customers::{
    self as cust, BalanceEntry, BalanceEntryKind, Birthday, Customer, CustomerBalanceSettings,
    CustomerDbError, CustomerGroup, CustomerId, CustomerRecord, ERASED_NAME, MembershipTier,
    Written,
};
use sultan_db::seed::{self, CASHIER_ROLE, MANAGER_ROLE, OWNER_ROLE, STOCK_CLERK_ROLE};
use sultan_db::{Db, outbox, roles};
use uuid::Uuid;

use crate::catalog_fixtures::{actor, trail};
use crate::common::{MySqlTestDb, Org, count, db_tests, org, raw, t};

db_tests!(
    put_create_replay_and_update,
    put_validates_and_checks_references,
    phone_is_unique_among_active_customers,
    synced_sign_up_parks_a_clashing_phone,
    lookup_by_phone_and_code,
    name_search_is_paged_and_ignores_case,
    archive_and_unarchive,
    erase_blanks_personal_data,
    merge_fills_fields_and_moves_the_balance,
    merge_refusals_and_chains,
    balance_ledger_and_cache,
    balance_ledger_is_append_only,
    balance_settings_defaults_and_put,
    groups,
    tiers,
    customer_permissions_on_a_fresh_database,
);

/// A `YYYYMMDD` day.
const fn ymd(y: u32, m: u32, d: u32) -> u32 {
    y * 10_000 + m * 100 + d
}

fn customer(code: &str, name: &str, phone: Option<&str>, o: &Org) -> Customer {
    Customer {
        id: Uuid::now_v7(),
        code: code.to_owned(),
        name: name.to_owned(),
        phone: phone.map(str::to_owned),
        email: None,
        birthday: None,
        address: None,
        company_name: None,
        tax_id: None,
        tax_address: None,
        notes: None,
        group: None,
        tier: None,
        tier_valid_until: None,
        manual_tier: false,
        signed_up_branch: Some(o.jkt.id),
        signed_up_at: t(10),
    }
}

async fn put(db: &Db, o: &Org, c: &Customer) -> Result<Written, CustomerDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = cust::put_customer(&mut tx, c, &actor(o), t(20)).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

async fn get(db: &Db, id: CustomerId) -> CustomerRecord {
    cust::get_customer(db, id).await.unwrap().unwrap()
}

/// Runs one write in its own transaction, committed when it succeeds.
macro_rules! commit {
    ($db:expr, |$tx:ident| $body:expr) => {{
        let mut $tx = $db.begin().await.unwrap();
        let result = $body.await;
        if result.is_ok() {
            $tx.commit().await.unwrap();
        }
        result
    }};
}

async fn entry(
    db: &Db,
    customer: CustomerId,
    kind: BalanceEntryKind,
    amount: i64,
) -> Result<(Written, Money), CustomerDbError> {
    let e = BalanceEntry::new(customer, kind, Money::from_minor(amount), t(30));
    commit!(db, |tx| cust::append_balance_entry(&mut tx, &e, t(30)))
}

async fn put_create_replay_and_update(db: &Db) {
    let o = org(db).await;
    let before = trail(db).await;
    let mut c = customer("K7QH29XM", "  Budi Santoso ", Some("+628123456789"), &o);
    c.email = Some("  ".to_owned());
    c.birthday = Some(Birthday {
        month: 2,
        day: 29,
        year: Some(1990),
    });
    c.tier_valid_until = Some(ymd(2026, 12, 31));
    assert_eq!(put(db, &o, &c).await.unwrap(), Written::Created);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let stored = get(db, c.id).await;
    assert_eq!(stored.customer.name, "Budi Santoso");
    assert_eq!(stored.customer.email, None, "blank becomes None");
    assert_eq!(stored.customer.birthday, c.birthday);
    assert_eq!(stored.customer.tier_valid_until, Some(ymd(2026, 12, 31)));
    assert_eq!(stored.balance, Money::ZERO);
    assert!(stored.is_active());
    assert_eq!(stored.created_at, t(20));

    // Replaying the same body changes nothing: no audit or outbox row.
    assert_eq!(put(db, &o, &c).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));

    // An update keeps the code and the sign-up fields.
    let mut edit = c.clone();
    "ZZZZ2222".clone_into(&mut edit.code);
    edit.signed_up_at = t(99);
    edit.signed_up_branch = Some(o.bdg.id);
    edit.notes = Some("VIP".to_owned());
    assert_eq!(put(db, &o, &edit).await.unwrap(), Written::Updated);
    let stored = get(db, c.id).await;
    assert_eq!(stored.customer.code, "K7QH29XM");
    assert_eq!(stored.customer.signed_up_at, t(10));
    assert_eq!(stored.customer.signed_up_branch, Some(o.jkt.id));
    assert_eq!(stored.customer.notes.as_deref(), Some("VIP"));
    // The same edit again (with the ignored code) is a no-op.
    assert_eq!(put(db, &o, &edit).await.unwrap(), Written::Unchanged);

    let events = outbox::list_unsynced(db, 100).await.unwrap();
    let last = events.last().unwrap();
    assert_eq!(last.kind, "customer.put");
    assert_eq!(last.payload["code"], "K7QH29XM");
    assert_eq!(last.payload["balance"], 0);
    let audit = audit::query(
        db,
        &AuditFilter {
            target: Some(format!("customer:{}", c.id)),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].action, "customer.put");
    assert_eq!(
        audit[0].before.as_ref().unwrap()["notes"],
        serde_json::Value::Null
    );
    assert_eq!(audit[0].after.as_ref().unwrap()["notes"], "VIP");
}

async fn put_validates_and_checks_references(db: &Db) {
    let o = org(db).await;
    let mut c = customer("AAAA2222", " ", None, &o);
    assert!(matches!(
        put(db, &o, &c).await,
        Err(CustomerDbError::Invalid { field: "name", .. })
    ));
    "Ani".clone_into(&mut c.name);
    "  ".clone_into(&mut c.code);
    assert!(matches!(
        put(db, &o, &c).await,
        Err(CustomerDbError::Invalid { field: "code", .. })
    ));
    "AAAA2222".clone_into(&mut c.code);
    c.birthday = Some(Birthday {
        month: 4,
        day: 31,
        year: None,
    });
    assert!(matches!(
        put(db, &o, &c).await,
        Err(CustomerDbError::Invalid {
            field: "birthday",
            ..
        })
    ));
    c.birthday = None;
    c.group = Some(Uuid::now_v7());
    let err = put(db, &o, &c).await.unwrap_err();
    assert_eq!(err.code(), Some("invalid_reference"));
    c.group = None;
    c.tier = Some(Uuid::now_v7());
    assert!(matches!(
        put(db, &o, &c).await,
        Err(CustomerDbError::InvalidReference("membership_tier"))
    ));
    c.tier = None;
    put(db, &o, &c).await.unwrap();

    // Another customer with the same code: the caller draws a new one.
    let other = customer("AAAA2222", "Dewi", None, &o);
    let err = put(db, &o, &other).await.unwrap_err();
    assert!(matches!(err, CustomerDbError::CodeTaken { ref code } if code == "AAAA2222"));
    assert_eq!(count(db, "customer").await, 1);
}

async fn phone_is_unique_among_active_customers(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let phone = "+628123456789";
    let a = customer("AAAA2222", "Ani", Some(phone), &o);
    put(db, &o, &a).await.unwrap();
    let b = customer("BBBB2222", "Budi", Some(phone), &o);
    match put(db, &o, &b).await.unwrap_err() {
        CustomerDbError::PhoneTaken {
            customer,
            code,
            phone: p,
        } => {
            assert_eq!(
                (customer, code.as_str(), p.as_str()),
                (a.id, "AAAA2222", phone)
            );
        }
        other => panic!("{other:?}"),
    }
    // Setting the phone later on an update is checked too.
    let mut b2 = b.clone();
    b2.phone = None;
    put(db, &o, &b2).await.unwrap();
    b2.phone = Some(phone.to_owned());
    assert_eq!(
        put(db, &o, &b2).await.unwrap_err().code(),
        Some("phone_taken")
    );

    // An archived holder does not block the phone.
    commit!(db, |tx| cust::archive_customer(&mut tx, a.id, &act, t(40))).unwrap();
    assert_eq!(put(db, &o, &b2).await.unwrap(), Written::Updated);

    // The unique index is the safety net: a raw second active holder is refused.
    let raw_insert = |code: &str| {
        format!(
            "INSERT INTO customer (id, code, name, phone, manual_tier, signed_up_at, search_key, \
             created_at, updated_at) VALUES (X'{}', '{code}', 'X', '{phone}', 0, 0, 'x', 0, 0)",
            Uuid::now_v7().simple()
        )
    };
    assert!(raw(db, &raw_insert("CCCC2222")).await.is_err());
    // NULL phones never clash.
    let n1 = customer("DDDD2222", "No phone 1", None, &o);
    let n2 = customer("EEEE2222", "No phone 2", None, &o);
    put(db, &o, &n1).await.unwrap();
    put(db, &o, &n2).await.unwrap();
}

async fn synced_sign_up_parks_a_clashing_phone(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let phone = "+628111111111";
    let a = customer("AAAA2222", "Ani", Some(phone), &o);
    put(db, &o, &a).await.unwrap();
    let b = customer("BBBB2222", "Ani Branch", Some(phone), &o);
    let synced = |c: Customer| async move {
        commit!(db, |tx| cust::put_synced_customer(&mut tx, &c, &act, t(50)))
    };
    assert_eq!(synced(b.clone()).await.unwrap(), Written::Created);
    let stored = get(db, b.id).await;
    assert_eq!(stored.customer.phone, None);
    assert_eq!(stored.phone_conflict.as_deref(), Some(phone));
    // The same synced record again changes nothing.
    let before = trail(db).await;
    assert_eq!(synced(b.clone()).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, before);
    // A synced record without a clash is stored as is.
    let c = customer("CCCC2222", "Citra", Some("+628222222222"), &o);
    synced(c.clone()).await.unwrap();
    assert_eq!(get(db, c.id).await.customer.phone, c.phone);

    let dups = cust::list_duplicates(db, 0, 0).await.unwrap();
    assert_eq!(dups.len(), 1);
    assert_eq!(dups[0].customer.id, b.id);

    // Merging the duplicate into the original resolves it.
    commit!(db, |tx| cust::merge_customers(
        &mut tx,
        a.id,
        b.id,
        &act,
        t(60)
    ))
    .unwrap();
    assert!(cust::list_duplicates(db, 0, 10).await.unwrap().is_empty());

    // Merging the other way round: the survivor takes the phone and its conflict is cleared.
    let d = customer("DDDD2222", "Dodi", Some("+628333333333"), &o);
    put(db, &o, &d).await.unwrap();
    let e = customer("EEEE2222", "Dodi Branch", Some("+628333333333"), &o);
    synced(e.clone()).await.unwrap();
    let survivor = commit!(db, |tx| cust::merge_customers(
        &mut tx,
        e.id,
        d.id,
        &act,
        t(61)
    ))
    .unwrap();
    assert_eq!(survivor.customer.phone.as_deref(), Some("+628333333333"));
    assert_eq!(survivor.phone_conflict, None);
}

async fn lookup_by_phone_and_code(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let a = customer("AAAA2222", "Ani", Some("+628100000001"), &o);
    let b = customer("BBBB2222", "Budi", Some("+628100000002"), &o);
    let c = customer("CCCC2222", "Citra", Some("+628100000003"), &o);
    for x in [&a, &b, &c] {
        put(db, &o, x).await.unwrap();
    }
    let by_phone = cust::lookup_by_phone(db, "+628100000001").await.unwrap();
    assert_eq!(by_phone.unwrap().customer.id, a.id);
    let by_code = cust::lookup_by_code(db, "BBBB2222").await.unwrap();
    assert_eq!(by_code.unwrap().customer.id, b.id);
    assert!(
        cust::lookup_by_code(db, "ZZZZ9999")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cust::lookup_by_phone(db, "+620000")
            .await
            .unwrap()
            .is_none()
    );

    // Merged: B's code and phone find A.
    commit!(db, |tx| cust::merge_customers(
        &mut tx,
        a.id,
        b.id,
        &act,
        t(60)
    ))
    .unwrap();
    assert_eq!(
        cust::lookup_by_code(db, "BBBB2222")
            .await
            .unwrap()
            .unwrap()
            .customer
            .id,
        a.id
    );
    assert_eq!(
        cust::lookup_by_phone(db, "+628100000002")
            .await
            .unwrap()
            .unwrap()
            .customer
            .id,
        a.id
    );

    // Archived and erased customers are not found.
    commit!(db, |tx| cust::archive_customer(&mut tx, c.id, &act, t(61))).unwrap();
    assert!(
        cust::lookup_by_code(db, "CCCC2222")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cust::lookup_by_phone(db, "+628100000003")
            .await
            .unwrap()
            .is_none()
    );
    commit!(db, |tx| cust::erase_customer(&mut tx, a.id, &act, t(62))).unwrap();
    assert!(
        cust::lookup_by_code(db, "AAAA2222")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cust::lookup_by_code(db, "BBBB2222")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cust::lookup_by_phone(db, "+628100000001")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        cust::lookup_by_phone(db, "+628100000002")
            .await
            .unwrap()
            .is_none()
    );
}

async fn name_search_is_paged_and_ignores_case(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let mut names = Vec::new();
    for i in 0..25 {
        let code = format!("S{i:03}AAAA");
        let c = customer(&code, &format!("Budi {i:02}"), None, &o);
        put(db, &o, &c).await.unwrap();
        names.push(c);
    }
    let mut company = customer("COMPANY2", "Rina", None, &o);
    company.company_name = Some("Toko Budiman".to_owned());
    put(db, &o, &company).await.unwrap();
    let mut accent = customer("ACCENT22", "José 100% Ok_", None, &o);
    accent.company_name = None;
    put(db, &o, &accent).await.unwrap();

    let page1 = cust::search_customers(db, "BUDI", 0, 0).await.unwrap();
    assert_eq!(page1.len(), 20, "at most 20");
    assert_eq!(page1[0].customer.name, "Budi 00");
    let page2 = cust::search_customers(db, "budi", 20, 50).await.unwrap();
    assert_eq!(page2.len(), 6);
    assert_eq!(
        page2.last().unwrap().customer.name,
        "Rina",
        "company name matches"
    );
    assert_eq!(
        cust::search_customers(db, "jose", 0, 5).await.unwrap()[0]
            .customer
            .id,
        accent.id
    );
    assert_eq!(
        cust::search_customers(db, "100%", 0, 5)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        cust::search_customers(db, "1%0", 0, 5)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        cust::search_customers(db, "k_", 0, 5).await.unwrap().len(),
        1
    );
    assert!(
        cust::search_customers(db, "   ", 0, 5)
            .await
            .unwrap()
            .is_empty()
    );

    commit!(db, |tx| cust::archive_customer(
        &mut tx,
        names[0].id,
        &act,
        t(70)
    ))
    .unwrap();
    let after = cust::search_customers(db, "budi 00", 0, 5).await.unwrap();
    assert!(after.is_empty(), "archived customers are not found");
}

async fn archive_and_unarchive(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let a = customer("AAAA2222", "Ani", Some("+628100000001"), &o);
    put(db, &o, &a).await.unwrap();
    let before = trail(db).await;
    let arch =
        |id| async move { commit!(db, |tx| cust::archive_customer(&mut tx, id, &act, t(40))) };
    let unarch =
        |id| async move { commit!(db, |tx| cust::unarchive_customer(&mut tx, id, &act, t(41))) };
    assert_eq!(arch(a.id).await.unwrap(), Written::Updated);
    assert_eq!(arch(a.id).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(get(db, a.id).await.archived_at, Some(t(40)));
    // An archived customer is not edited.
    assert!(matches!(
        put(db, &o, &a).await,
        Err(CustomerDbError::Archived)
    ));

    // Unarchive is refused while another customer holds the phone.
    let b = customer("BBBB2222", "Budi", Some("+628100000001"), &o);
    put(db, &o, &b).await.unwrap();
    assert_eq!(unarch(a.id).await.unwrap_err().code(), Some("phone_taken"));
    commit!(db, |tx| cust::archive_customer(&mut tx, b.id, &act, t(42))).unwrap();
    assert_eq!(unarch(a.id).await.unwrap(), Written::Updated);
    assert_eq!(unarch(a.id).await.unwrap(), Written::Unchanged);
    assert!(get(db, a.id).await.is_active());

    assert!(matches!(
        arch(Uuid::now_v7()).await,
        Err(CustomerDbError::NotFound("customer"))
    ));
    // A merged customer stays archived.
    let c = customer("CCCC2222", "Citra", None, &o);
    put(db, &o, &c).await.unwrap();
    commit!(db, |tx| cust::merge_customers(
        &mut tx,
        a.id,
        c.id,
        &act,
        t(43)
    ))
    .unwrap();
    assert!(matches!(unarch(c.id).await, Err(CustomerDbError::Merged)));
    assert!(matches!(
        put(db, &o, &c).await,
        Err(CustomerDbError::Merged)
    ));
    // An erased customer is gone for archive and unarchive.
    commit!(db, |tx| cust::erase_customer(&mut tx, a.id, &act, t(44))).unwrap();
    assert_eq!(arch(a.id).await.unwrap_err().code(), Some("not_found"));
    assert_eq!(unarch(a.id).await.unwrap_err().code(), Some("not_found"));
}

async fn erase_blanks_personal_data(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let mut a = customer("AAAA2222", "Budi Santoso", Some("+628100000001"), &o);
    a.email = Some("budi@example.com".to_owned());
    a.birthday = Some(Birthday {
        month: 1,
        day: 2,
        year: None,
    });
    a.address = Some("Jl. Mawar 1".to_owned());
    a.company_name = Some("PT Budi".to_owned());
    a.tax_id = Some("0123456789012345".to_owned());
    a.tax_address = Some("Jl. Pajak".to_owned());
    a.notes = Some("likes coffee".to_owned());
    put(db, &o, &a).await.unwrap();
    let erase = || async { commit!(db, |tx| cust::erase_customer(&mut tx, a.id, &act, t(50))) };

    // Refused while the balance is not zero.
    entry(db, a.id, BalanceEntryKind::ChangeSaved, 700)
        .await
        .unwrap();
    match erase().await.unwrap_err() {
        CustomerDbError::BalanceNotZero { balance } => assert_eq!(balance.minor(), 700),
        other => panic!("{other:?}"),
    }
    entry(db, a.id, BalanceEntryKind::Payout, -700)
        .await
        .unwrap();

    let before = trail(db).await;
    let erased = erase().await.unwrap();
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let c = &erased.customer;
    assert_eq!(c.name, ERASED_NAME);
    assert_eq!(c.code, "AAAA2222");
    for field in [
        &c.phone,
        &c.email,
        &c.address,
        &c.company_name,
        &c.tax_id,
        &c.tax_address,
        &c.notes,
    ] {
        assert_eq!(*field, None);
    }
    assert_eq!(c.birthday, None);
    assert_eq!(erased.archived_at, Some(t(50)));
    assert_eq!(erased.erased_at, Some(t(50)));
    // The audit entry says that it happened, not what was erased; older entries stay.
    let entries = audit::query(
        db,
        &AuditFilter {
            target: Some(format!("customer:{}", a.id)),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries[0].action, "customer.erase");
    assert_eq!(entries[0].before, None);
    assert_eq!(
        entries[0].after,
        Some(serde_json::json!({ "erased": true }))
    );
    assert_eq!(entries[1].after.as_ref().unwrap()["name"], "Budi Santoso");
    let events = outbox::list_unsynced(db, 100).await.unwrap();
    assert_eq!(events.last().unwrap().payload["name"], ERASED_NAME);
    assert_eq!(
        events.last().unwrap().payload["phone"],
        serde_json::Value::Null
    );

    // A second erase is not found; the phone is free again.
    assert!(matches!(erase().await, Err(CustomerDbError::NotFound(_))));
    put(
        db,
        &o,
        &customer("BBBB2222", "Budi", Some("+628100000001"), &o),
    )
    .await
    .unwrap();
    // An archived customer can be erased too (archived_at is kept).
    let c = customer("CCCC2222", "Citra", None, &o);
    put(db, &o, &c).await.unwrap();
    commit!(db, |tx| cust::archive_customer(&mut tx, c.id, &act, t(51))).unwrap();
    let erased = commit!(db, |tx| cust::erase_customer(&mut tx, c.id, &act, t(52))).unwrap();
    assert_eq!(erased.archived_at, Some(t(51)));
    assert_eq!(erased.erased_at, Some(t(52)));
}

#[allow(clippy::too_many_lines)] // one merge story: fields, balance, entries, outbox
async fn merge_fills_fields_and_moves_the_balance(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let gold = MembershipTier {
        id: Uuid::now_v7(),
        name: "Gold".to_owned(),
        rank: 2,
        archived_at: None,
    };
    let wholesale = CustomerGroup {
        id: Uuid::now_v7(),
        name: "Wholesale".to_owned(),
        archived_at: None,
    };
    commit!(db, |tx| cust::put_tier(&mut tx, &gold, &act, t(1))).unwrap();
    commit!(db, |tx| cust::put_group(&mut tx, &wholesale, &act, t(1))).unwrap();

    let mut a = customer("AAAA2222", "Budi", None, &o);
    a.email = Some("a@example.com".to_owned());
    let mut b = customer("BBBB2222", "Budi S", Some("+628100000002"), &o);
    b.email = Some("b@example.com".to_owned());
    b.address = Some("Jl. B".to_owned());
    b.tier = Some(gold.id);
    b.tier_valid_until = Some(ymd(2027, 1, 31));
    b.manual_tier = true;
    b.group = Some(wholesale.id);
    put(db, &o, &a).await.unwrap();
    put(db, &o, &b).await.unwrap();
    entry(db, a.id, BalanceEntryKind::ChangeSaved, 1_000)
        .await
        .unwrap();
    entry(db, b.id, BalanceEntryKind::ChangeSaved, 2_200)
        .await
        .unwrap();

    let before = trail(db).await;
    let merged = commit!(db, |tx| cust::merge_customers(
        &mut tx,
        a.id,
        b.id,
        &act,
        t(60)
    ))
    .unwrap();
    // One audit row and one outbox row for each customer.
    assert_eq!(trail(db).await, (before.0 + 2, before.1 + 2));
    let m = &merged.customer;
    assert_eq!(m.name, "Budi");
    assert_eq!(m.email.as_deref(), Some("a@example.com"), "A's fields win");
    assert_eq!(
        m.address.as_deref(),
        Some("Jl. B"),
        "empty fields are filled"
    );
    assert_eq!(m.phone.as_deref(), Some("+628100000002"));
    assert_eq!(m.tier, Some(gold.id));
    assert_eq!(m.tier_valid_until, Some(ymd(2027, 1, 31)));
    assert!(m.manual_tier);
    assert_eq!(m.group, Some(wholesale.id));
    assert_eq!(merged.balance.minor(), 3_200);

    let b_after = get(db, b.id).await;
    assert_eq!(b_after.merged_into, Some(a.id));
    assert_eq!(b_after.archived_at, Some(t(60)));
    assert_eq!(b_after.balance, Money::ZERO);
    assert_eq!(cust::ledger_balance(db, b.id).await.unwrap(), Money::ZERO);
    assert_eq!(cust::ledger_balance(db, a.id).await.unwrap().minor(), 3_200);
    let a_entries = cust::get_balance(db, a.id, None, 0).await.unwrap().unwrap();
    assert_eq!(a_entries.entries[0].kind, BalanceEntryKind::MergeIn);
    assert_eq!(a_entries.entries[0].amount.minor(), 2_200);
    assert_eq!(a_entries.entries[0].user, Some(o.manager.id));
    let b_entries = cust::get_balance(db, b.id, None, 0).await.unwrap().unwrap();
    assert_eq!(b_entries.entries[0].kind, BalanceEntryKind::MergeOut);
    assert_eq!(b_entries.entries[0].amount.minor(), -2_200);

    let events = outbox::list_unsynced(db, 100).await.unwrap();
    let kinds: Vec<_> = events[events.len() - 2..]
        .iter()
        .map(|e| {
            (
                e.kind.as_str(),
                e.payload["id"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("customer.put", a.id.to_string()),
            ("customer.put", b.id.to_string())
        ]
    );

    // A negative balance moves too, and A keeps its own tier.
    let mut c = customer("CCCC2222", "Citra", None, &o);
    c.tier = Some(gold.id);
    let mut d = customer("DDDD2222", "Citra D", None, &o);
    let silver = MembershipTier {
        id: Uuid::now_v7(),
        name: "Silver".to_owned(),
        rank: 1,
        archived_at: None,
    };
    commit!(db, |tx| cust::put_tier(&mut tx, &silver, &act, t(1))).unwrap();
    d.tier = Some(silver.id);
    put(db, &o, &c).await.unwrap();
    put(db, &o, &d).await.unwrap();
    entry(db, d.id, BalanceEntryKind::Payment, -500)
        .await
        .unwrap();
    let merged = commit!(db, |tx| cust::merge_customers(
        &mut tx,
        c.id,
        d.id,
        &act,
        t(61)
    ))
    .unwrap();
    assert_eq!(merged.customer.tier, Some(gold.id));
    assert_eq!(merged.balance.minor(), -500);
    assert_eq!(get(db, d.id).await.balance, Money::ZERO);

    // Without a balance, no ledger entries are written.
    let e = customer("EEEE2222", "Eko", None, &o);
    let f = customer("FFFF2222", "Eko F", None, &o);
    put(db, &o, &e).await.unwrap();
    put(db, &o, &f).await.unwrap();
    let ledger = count(db, "customer_balance_entry").await;
    commit!(db, |tx| cust::merge_customers(
        &mut tx,
        e.id,
        f.id,
        &act,
        t(62)
    ))
    .unwrap();
    assert_eq!(count(db, "customer_balance_entry").await, ledger);
}

async fn merge_refusals_and_chains(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let a = customer("AAAA2222", "Ani", None, &o);
    let b = customer("BBBB2222", "Budi", None, &o);
    let c = customer("CCCC2222", "Citra", None, &o);
    let d = customer("DDDD2222", "Dewi", None, &o);
    for x in [&a, &b, &c, &d] {
        put(db, &o, x).await.unwrap();
    }
    let merge = |into, from| async move {
        commit!(db, |tx| cust::merge_customers(
            &mut tx,
            into,
            from,
            &act,
            t(60)
        ))
    };
    assert!(matches!(
        merge(a.id, a.id).await,
        Err(CustomerDbError::SameCustomer)
    ));
    assert!(matches!(
        merge(a.id, Uuid::now_v7()).await,
        Err(CustomerDbError::NotFound(_))
    ));
    merge(a.id, b.id).await.unwrap();
    // Into or from a merged customer → merged.
    assert_eq!(merge(b.id, c.id).await.unwrap_err().code(), Some("merged"));
    assert_eq!(merge(c.id, b.id).await.unwrap_err().code(), Some("merged"));
    // Archived customers are not merged.
    commit!(db, |tx| cust::archive_customer(&mut tx, d.id, &act, t(61))).unwrap();
    assert!(matches!(
        merge(c.id, d.id).await,
        Err(CustomerDbError::Archived)
    ));
    assert!(matches!(
        merge(d.id, c.id).await,
        Err(CustomerDbError::Archived)
    ));

    // Merging A (which absorbed B) into C re-points B at C: no chain.
    merge(c.id, a.id).await.unwrap();
    assert_eq!(get(db, b.id).await.merged_into, Some(c.id));
    assert_eq!(get(db, a.id).await.merged_into, Some(c.id));
    assert_eq!(
        cust::lookup_by_code(db, "BBBB2222")
            .await
            .unwrap()
            .unwrap()
            .customer
            .id,
        c.id
    );
}

#[allow(clippy::too_many_lines)] // one ledger story: append, replay, signs, paging, lists
async fn balance_ledger_and_cache(db: &Db) {
    let o = org(db).await;
    let a = customer("AAAA2222", "Ani", None, &o);
    put(db, &o, &a).await.unwrap();
    let outbox_before = count(db, "outbox").await;
    let audit_before = count(db, "audit_log").await;
    // Spec 2.5 §15 case 7: +700, +2 000, −500 → 2 200.
    let mut last = Money::ZERO;
    for (kind, amount) in [
        (BalanceEntryKind::ChangeSaved, 700),
        (BalanceEntryKind::ChangeSaved, 2_000),
        (BalanceEntryKind::Payment, -500),
    ] {
        let (written, balance) = entry(db, a.id, kind, amount).await.unwrap();
        assert_eq!(written, Written::Created);
        last = balance;
    }
    assert_eq!(last.minor(), 2_200);
    assert_eq!(get(db, a.id).await.balance.minor(), 2_200);
    assert_eq!(cust::ledger_balance(db, a.id).await.unwrap().minor(), 2_200);
    // Each entry adds an outbox row, but no audit row.
    assert_eq!(count(db, "outbox").await, outbox_before + 3);
    assert_eq!(count(db, "audit_log").await, audit_before);
    let event = outbox::list_unsynced(db, 100).await.unwrap().pop().unwrap();
    assert_eq!(event.kind, "customer.balance_entry");
    assert_eq!(event.payload["amount"], -500);
    assert_eq!(event.payload["kind"], "payment");

    // A replay with the same ID is a no-op; other values under that ID are refused.
    let mut e = BalanceEntry::new(
        a.id,
        BalanceEntryKind::PaymentReversed,
        Money::from_minor(500),
        t(31),
    );
    e.branch = Some(o.jkt.id);
    e.register = Some(o.register.id);
    e.user = Some(o.cashier.id);
    e.document = Some(Uuid::now_v7());
    let append = |e: BalanceEntry| async move {
        commit!(db, |tx| cust::append_balance_entry(&mut tx, &e, t(31)))
    };
    assert_eq!(
        append(e.clone()).await.unwrap(),
        (Written::Created, Money::from_minor(2_700))
    );
    assert_eq!(
        append(e.clone()).await.unwrap(),
        (Written::Unchanged, Money::from_minor(2_700))
    );
    let mut other = e.clone();
    other.amount = Money::from_minor(600);
    assert!(matches!(append(other).await, Err(CustomerDbError::EntryConflict(id)) if id == e.id));

    // Signs per kind; zero is never allowed.
    for (kind, amount) in [
        (BalanceEntryKind::ChangeSaved, -1),
        (BalanceEntryKind::Payment, 1),
        (BalanceEntryKind::ChangeSavedReversed, 1),
        (BalanceEntryKind::PaymentReversed, -1),
        (BalanceEntryKind::Payout, 0),
    ] {
        let err = entry(db, a.id, kind, amount).await.unwrap_err();
        assert!(
            matches!(
                err,
                CustomerDbError::Invalid {
                    field: "amount",
                    ..
                }
            ),
            "{kind:?}"
        );
    }
    // A positive payout writes off a negative balance.
    assert!(entry(db, a.id, BalanceEntryKind::Payout, 5).await.is_ok());
    assert!(matches!(
        entry(db, Uuid::now_v7(), BalanceEntryKind::ChangeSaved, 1).await,
        Err(CustomerDbError::NotFound("customer"))
    ));

    // Paging, newest first.
    let page = cust::get_balance(db, a.id, None, 2).await.unwrap().unwrap();
    assert_eq!(page.balance.minor(), 2_705);
    assert_eq!(page.entries.len(), 2);
    assert_eq!(page.entries[1], e);
    let rest = cust::get_balance(db, a.id, Some(page.entries[1].id), 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(rest.entries.len(), 3);
    assert!(
        cust::get_balance(db, Uuid::now_v7(), None, 0)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        cust::ledger_balance(db, Uuid::now_v7()).await.unwrap(),
        Money::ZERO
    );

    // Negative balances.
    let b = customer("BBBB2222", "Budi", None, &o);
    put(db, &o, &b).await.unwrap();
    entry(db, b.id, BalanceEntryKind::Payment, -300)
        .await
        .unwrap();
    let negative = cust::list_negative_balances(db, 0, 0).await.unwrap();
    assert_eq!(negative.len(), 1);
    assert_eq!(negative[0].customer.id, b.id);
    assert!(
        cust::list_negative_balances(db, 1, 10)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn balance_ledger_is_append_only(db: &Db) {
    let o = org(db).await;
    let a = customer("AAAA2222", "Ani", None, &o);
    put(db, &o, &a).await.unwrap();
    entry(db, a.id, BalanceEntryKind::ChangeSaved, 700)
        .await
        .unwrap();
    assert!(
        raw(db, "UPDATE customer_balance_entry SET amount = 1")
            .await
            .is_err()
    );
    assert!(raw(db, "DELETE FROM customer_balance_entry").await.is_err());
    assert_eq!(count(db, "customer_balance_entry").await, 1);
    // The CHECK constraints are a second safety net for the signs.
    let bad = format!(
        "INSERT INTO customer_balance_entry (id, customer_id, kind, amount, occurred_at) \
         VALUES (X'{}', X'{}', 'payment', 5, 0)",
        Uuid::now_v7().simple(),
        a.id.simple()
    );
    assert!(raw(db, &bad).await.is_err());
}

async fn balance_settings_defaults_and_put(db: &Db) {
    let o = org(db).await;
    let act = actor(&o);
    let defaults = cust::get_balance_settings(db).await.unwrap();
    assert_eq!(defaults.max_saved_per_sale.minor(), 5_000);
    assert_eq!(defaults.max_balance.minor(), 100_000);
    let save = |s: CustomerBalanceSettings| async move {
        commit!(db, |tx| cust::put_balance_settings(&mut tx, &s, &act, t(5)))
    };
    let before = trail(db).await;
    assert_eq!(save(defaults).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, before);
    let off = CustomerBalanceSettings {
        max_saved_per_sale: Money::ZERO,
        max_balance: Money::from_minor(50_000),
    };
    assert_eq!(save(off).await.unwrap(), Written::Updated);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(cust::get_balance_settings(db).await.unwrap(), off);
    let negative = CustomerBalanceSettings {
        max_saved_per_sale: Money::from_minor(-1),
        ..off
    };
    assert!(matches!(
        save(negative).await,
        Err(CustomerDbError::Invalid {
            field: "max_saved_per_sale",
            ..
        })
    ));
    let negative = CustomerBalanceSettings {
        max_balance: Money::from_minor(-1),
        ..off
    };
    assert_eq!(
        save(negative).await.unwrap_err().code(),
        Some("invalid_field")
    );
}

async fn groups(db: &Db) {
    let o = org(db).await;
    let a = actor(&o);
    let mut wholesale = CustomerGroup {
        id: Uuid::now_v7(),
        name: " Wholesale ".to_owned(),
        archived_at: None,
    };
    let before = trail(db).await;
    assert_eq!(
        commit!(db, |tx| cust::put_group(&mut tx, &wholesale, &a, t(1))).unwrap(),
        Written::Created
    );
    assert_eq!(
        commit!(db, |tx| cust::put_group(&mut tx, &wholesale, &a, t(2))).unwrap(),
        Written::Unchanged
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let stored = cust::get_group(db, wholesale.id).await.unwrap().unwrap();
    assert_eq!(stored.name, "Wholesale");
    let dup = CustomerGroup {
        id: Uuid::now_v7(),
        name: "Wholesale".to_owned(),
        archived_at: None,
    };
    assert!(matches!(
        commit!(db, |tx| cust::put_group(&mut tx, &dup, &a, t(3))),
        Err(CustomerDbError::Duplicate { field: "name", .. })
    ));
    // Case-sensitive like all text.
    let lower = CustomerGroup {
        name: "wholesale".to_owned(),
        ..dup
    };
    commit!(db, |tx| cust::put_group(&mut tx, &lower, &a, t(3))).unwrap();
    "Grosir".clone_into(&mut wholesale.name);
    assert_eq!(
        commit!(db, |tx| cust::put_group(&mut tx, &wholesale, &a, t(4))).unwrap(),
        Written::Updated
    );
    assert_eq!(
        commit!(db, |tx| cust::archive_group(
            &mut tx,
            wholesale.id,
            &a,
            t(5)
        ))
        .unwrap(),
        Written::Updated
    );
    // Archiving again (or a PUT with another time) keeps the first archive time.
    assert_eq!(
        commit!(db, |tx| cust::archive_group(
            &mut tx,
            wholesale.id,
            &a,
            t(6)
        ))
        .unwrap(),
        Written::Unchanged
    );
    assert_eq!(
        cust::get_group(db, wholesale.id)
            .await
            .unwrap()
            .unwrap()
            .archived_at,
        Some(t(5))
    );
    assert_eq!(cust::list_groups(db, false).await.unwrap().len(), 1);
    let all = cust::list_groups(db, true).await.unwrap();
    assert_eq!(
        all.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(),
        ["Grosir", "wholesale"]
    );
    assert!(matches!(
        commit!(db, |tx| cust::archive_group(
            &mut tx,
            Uuid::now_v7(),
            &a,
            t(7)
        )),
        Err(CustomerDbError::NotFound("customer_group"))
    ));
    assert!(matches!(
        commit!(db, |tx| cust::put_group(
            &mut tx,
            &CustomerGroup {
                name: String::new(),
                ..lower.clone()
            },
            &a,
            t(7)
        )),
        Err(CustomerDbError::Invalid { field: "name", .. })
    ));
}

async fn tiers(db: &Db) {
    let o = org(db).await;
    let a = actor(&o);
    // Tiers: unique name and rank, best first.
    let tier = |name: &str, rank| MembershipTier {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        rank,
        archived_at: None,
    };
    let silver = tier("Silver", 1);
    let gold = tier("Gold", 2);
    for x in [&silver, &gold] {
        assert_eq!(
            commit!(db, |tx| cust::put_tier(&mut tx, x, &a, t(1))).unwrap(),
            Written::Created
        );
    }
    assert_eq!(
        commit!(db, |tx| cust::put_tier(&mut tx, &gold, &a, t(1))).unwrap(),
        Written::Unchanged
    );
    assert!(matches!(
        commit!(db, |tx| cust::put_tier(&mut tx, &tier("Gold", 3), &a, t(2))),
        Err(CustomerDbError::Duplicate { field: "name", .. })
    ));
    let err = commit!(db, |tx| cust::put_tier(
        &mut tx,
        &tier("Platinum", 2),
        &a,
        t(2)
    ))
    .unwrap_err();
    assert!(matches!(err, CustomerDbError::Duplicate { field: "rank", ref value } if value == "2"));
    let names = |tiers: Vec<MembershipTier>| tiers.into_iter().map(|t| t.name).collect::<Vec<_>>();
    assert_eq!(
        names(cust::list_tiers(db, false).await.unwrap()),
        ["Gold", "Silver"]
    );
    commit!(db, |tx| cust::archive_tier(&mut tx, silver.id, &a, t(3))).unwrap();
    assert_eq!(names(cust::list_tiers(db, false).await.unwrap()), ["Gold"]);
    assert_eq!(
        names(cust::list_tiers(db, true).await.unwrap()),
        ["Gold", "Silver"]
    );
    // Unarchive by a PUT without archived_at.
    assert_eq!(
        commit!(db, |tx| cust::put_tier(&mut tx, &silver, &a, t(4))).unwrap(),
        Written::Updated
    );
    assert_eq!(
        cust::get_tier(db, silver.id)
            .await
            .unwrap()
            .unwrap()
            .archived_at,
        None
    );
    assert!(matches!(
        commit!(db, |tx| cust::archive_tier(
            &mut tx,
            Uuid::now_v7(),
            &a,
            t(5)
        )),
        Err(CustomerDbError::NotFound("membership_tier"))
    ));
    assert!(cust::get_tier(db, Uuid::now_v7()).await.unwrap().is_none());
    let outbox_kinds: Vec<_> = outbox::list_unsynced(db, 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("customer_group") || k.starts_with("membership_tier"))
        .collect();
    assert!(outbox_kinds.contains(&"membership_tier.put".to_owned()));
}

const CUSTOMER_PERMS: [Permission; 6] = [
    Permission::CustomerCreate,
    Permission::CustomerSearch,
    Permission::CustomerEdit,
    Permission::CustomerManage,
    Permission::CustomerErase,
    Permission::CustomerBalancePayout,
];

async fn customer_grants(db: &Db, role: sultan_core::ids::RoleId) -> Vec<Permission> {
    let role = roles::get_role(db, role).await.unwrap().unwrap();
    role.grants
        .iter()
        .map(sultan_core::access::Grant::permission)
        .filter(|p| CUSTOMER_PERMS.contains(p))
        .collect()
}

async fn assert_customer_grants(db: &Db) {
    use Permission as P;
    let mut owner = customer_grants(db, OWNER_ROLE).await;
    owner.sort();
    assert_eq!(owner, CUSTOMER_PERMS);
    let mut manager = customer_grants(db, MANAGER_ROLE).await;
    manager.sort();
    assert_eq!(
        manager,
        [
            P::CustomerCreate,
            P::CustomerSearch,
            P::CustomerEdit,
            P::CustomerManage,
            P::CustomerBalancePayout
        ]
    );
    assert_eq!(customer_grants(db, CASHIER_ROLE).await, [P::CustomerCreate]);
    assert!(customer_grants(db, STOCK_CLERK_ROLE).await.is_empty());
}

async fn customer_permissions_on_a_fresh_database(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    assert_customer_grants(db).await;
    seed::seed_defaults(db, t(2)).await.unwrap();
    assert_customer_grants(db).await;
}

const CUSTOMERS_MIGRATION: i64 = 20_261_111_000_002;

/// The migrations before the customers, as an older release shipped them.
async fn old_migrator(backend: &str) -> sqlx::migrate::Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut migrator = sqlx::migrate::Migrator::new(dir).await.unwrap();
    let old: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version < CUSTOMERS_MIGRATION)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(old);
    migrator
}

/// An install migrated from before 2.5: the migration adds the customer grants to the existing
/// seeded roles (Owner, Manager, Cashier) and nothing else.
async fn upgrade_adds_customer_grants(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    raw(
        db,
        "DELETE FROM role_permission WHERE perm LIKE 'customer.%'",
    )
    .await
    .unwrap();
    assert!(customer_grants(db, MANAGER_ROLE).await.is_empty());
    let manager_before = roles::get_role(db, MANAGER_ROLE).await.unwrap().unwrap();
    db.migrate().await.unwrap();
    let manager = roles::get_role(db, MANAGER_ROLE).await.unwrap().unwrap();
    assert_eq!(manager.grants.len(), manager_before.grants.len() + 5);
    assert_customer_grants(db).await;
    seed::seed_defaults(db, t(3)).await.unwrap();
    assert_customer_grants(db).await;
    let settings = cust::get_balance_settings(db).await.unwrap();
    assert_eq!(settings.max_balance.minor(), 100_000);
}

#[tokio::test]
async fn sqlite_upgrade_adds_customer_grants() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let Db::Sqlite(pool) = &db else {
        unreachable!()
    };
    old_migrator("sqlite").await.run(pool).await.unwrap();
    upgrade_adds_customer_grants(&db).await;
}

#[tokio::test]
async fn mysql_upgrade_adds_customer_grants() {
    let Some(test_db) = MySqlTestDb::create_empty().await else {
        return;
    };
    let Db::MySql(pool) = &test_db.db else {
        unreachable!()
    };
    old_migrator("mysql").await.run(pool).await.unwrap();
    upgrade_adds_customer_grants(&test_db.db).await;
    test_db.drop().await;
}
