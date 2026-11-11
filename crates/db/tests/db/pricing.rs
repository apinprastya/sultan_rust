//! Price lists, entries, tax rates, pricing settings and the resolution loader (spec 2.3, R2).

use std::borrow::Cow;
use std::path::Path;

use chrono::NaiveDate;
use sultan_core::access::{Limit, Permission};
use sultan_core::catalog::{Qty, UnitKind};
use sultan_core::ids::{RoleId, TaxClassId, UnitId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{
    CartLine, CashRounding, LineMode, PriceEntry, PriceEntryId, PriceListId, PriceListKind,
    PriceTier, PricingError, Rule, SaleItem, TierMembership, TierPrice, resolve_cart,
};
use sultan_core::tax::{TaxRate, TaxSettings, rate_on};
use sultan_db::audit::{self, AuditFilter};
use sultan_db::catalog::{EXEMPT_TAX_CLASS, ProductDoc, STANDARD_TAX_CLASS, get_product};
use sultan_db::customers::{self as cust, Customer, CustomerGroup, MembershipTier};
use sultan_db::pricing::{
    self as pr, DEFAULT_PRICE_LIST, EntryFilter, ListFilter, PriceListRecord, PriceTargets,
    PricingDbError, PricingSettings, Written,
};
use sultan_db::seed::{self, CASHIER_ROLE, MANAGER_ROLE, OWNER_ROLE};
use sultan_db::{Db, roles};
use uuid::Uuid;

use crate::catalog_fixtures::{actor, put, simple, trail, unit};
use crate::common::{MySqlTestDb, Org, db_tests, org, raw, t};

db_tests!(
    price_list_put_replay_and_conflict,
    price_list_validation,
    default_list_rules,
    entry_put_replay_and_audit,
    entry_overlap_and_validation,
    entry_list_and_delete,
    resolution_follows_the_chain,
    resolution_dates_tiers_and_lists,
    tax_rates_seed_and_history,
    pricing_settings_defaults_and_put,
    price_permissions_on_a_fresh_database,
);

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

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}

fn rp(v: i64) -> Money {
    Money::from_minor(v)
}

fn qty(whole: i64) -> Qty {
    Qty::from_thousandths(whole * 1_000)
}

fn list(kind: PriceListKind, target: Option<Uuid>, name: &str) -> PriceListRecord {
    PriceListRecord {
        id: PriceListId::generate(),
        kind,
        target,
        name: name.to_owned(),
        active: true,
        rule: None,
        archived_at: None,
    }
}

fn fixed(min_qty: i64, price: i64) -> PriceTier {
    PriceTier {
        min_qty: qty(min_qty),
        price: TierPrice::Fixed(rp(price)),
    }
}

fn entry(variant: VariantId, unit: UnitId, tiers: Vec<PriceTier>) -> PriceEntry {
    PriceEntry {
        id: PriceEntryId::generate(),
        variant,
        unit,
        valid_from: None,
        valid_to: None,
        tiers,
    }
}

async fn put_list(db: &Db, o: &Org, l: &PriceListRecord) -> Result<Written, PricingDbError> {
    commit!(db, |tx| pr::put_price_list(&mut tx, l, &actor(o), t(20)))
}

async fn put_entry(
    db: &Db,
    o: &Org,
    list: PriceListId,
    e: &PriceEntry,
) -> Result<Written, PricingDbError> {
    commit!(db, |tx| pr::put_price_entry(
        &mut tx,
        list,
        e,
        &actor(o),
        t(30)
    ))
}

/// A stock product with units pcs and box(24); one variant.
async fn product(db: &Db, o: &Org, code: &str) -> (ProductDoc, VariantId, UnitId, UnitId) {
    let mut doc = simple(code, &format!("{code}-1"));
    let box_ = unit("box", UnitKind::Integer, 24_000);
    let box_id = box_.id;
    doc.draft.units.units.push(box_);
    let (_, stored) = put(db, o, &doc).await;
    let variant = stored.draft.variants[0].id;
    let pcs = stored.draft.units.base;
    (stored, variant, pcs, box_id)
}

async fn group(db: &Db, o: &Org, name: &str) -> Uuid {
    let g = CustomerGroup {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        archived_at: None,
    };
    commit!(db, |tx| cust::put_group(&mut tx, &g, &actor(o), t(5))).unwrap();
    g.id
}

async fn tier(db: &Db, o: &Org, name: &str, rank: i64) -> Uuid {
    let tr = MembershipTier {
        id: Uuid::now_v7(),
        name: name.to_owned(),
        rank,
        archived_at: None,
    };
    commit!(db, |tx| cust::put_tier(&mut tx, &tr, &actor(o), t(5))).unwrap();
    tr.id
}

async fn customer(db: &Db, o: &Org, code: &str) -> Uuid {
    let c = Customer {
        id: Uuid::now_v7(),
        code: code.to_owned(),
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
        tier: None,
        tier_valid_until: None,
        manual_tier: false,
        signed_up_branch: Some(o.jkt.id),
        signed_up_at: t(5),
    };
    commit!(db, |tx| cust::put_customer(&mut tx, &c, &actor(o), t(5))).unwrap();
    c.id
}

#[allow(clippy::too_many_lines)] // one story: create, replay, conflict, archive, list
async fn price_list_put_replay_and_conflict(db: &Db) {
    let o = org(db).await;
    let mut jkt = list(PriceListKind::Branch, Some(o.jkt.id.into()), " Jakarta ");
    let before = trail(db).await;
    assert_eq!(put_list(db, &o, &jkt).await.unwrap(), Written::Created);
    assert_eq!(put_list(db, &o, &jkt).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let stored = pr::get_price_list(db, jkt.id).await.unwrap().unwrap();
    assert_eq!(stored.name, "Jakarta");
    assert!(stored.is_live());

    jkt.rule = Some(Rule::Pct(-500));
    assert_eq!(put_list(db, &o, &jkt).await.unwrap(), Written::Updated);
    let audit = audit::query(
        db,
        &AuditFilter {
            target: Some(format!("price_list:{}", jkt.id)),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].action, "price_list.put");
    assert_eq!(
        audit[0].before.as_ref().unwrap()["rule"],
        serde_json::Value::Null
    );
    assert_eq!(audit[0].after.as_ref().unwrap()["rule"]["pct_bp"], -500);

    // A second active list for the same branch.
    let mut second = list(PriceListKind::Branch, Some(o.jkt.id.into()), "Jakarta 2");
    let err = put_list(db, &o, &second).await.unwrap_err();
    assert!(
        matches!(err, PricingDbError::ListConflict { existing } if existing == jkt.id),
        "{err}"
    );
    assert_eq!(err.code(), Some("price_list_conflict"));
    // Switched off it may exist; switching it on conflicts again.
    second.active = false;
    assert_eq!(put_list(db, &o, &second).await.unwrap(), Written::Created);
    second.active = true;
    assert!(matches!(
        put_list(db, &o, &second).await,
        Err(PricingDbError::ListConflict { .. })
    ));
    // Archive the first: the second may go live. Another branch never conflicts.
    let w = commit!(db, |tx| pr::archive_price_list(
        &mut tx,
        jkt.id,
        &actor(&o),
        t(40)
    ));
    assert_eq!(w.unwrap(), Written::Updated);
    assert_eq!(put_list(db, &o, &second).await.unwrap(), Written::Updated);
    let bdg = list(PriceListKind::Branch, Some(o.bdg.id.into()), "Bandung");
    assert_eq!(put_list(db, &o, &bdg).await.unwrap(), Written::Created);
    // Archiving again keeps the first archive time.
    let w = commit!(db, |tx| pr::archive_price_list(
        &mut tx,
        jkt.id,
        &actor(&o),
        t(50)
    ));
    assert_eq!(w.unwrap(), Written::Unchanged);
    let archived = pr::get_price_list(db, jkt.id).await.unwrap().unwrap();
    assert_eq!(archived.archived_at, Some(t(40)));
    // Unarchiving it while the second is live conflicts.
    let mut back = archived.clone();
    back.archived_at = None;
    assert!(matches!(
        put_list(db, &o, &back).await,
        Err(PricingDbError::ListConflict { .. })
    ));

    // The database index is the safety net.
    let raw_insert = format!(
        "INSERT INTO price_list (id, kind, target_id, name, active, archived_at, created_at, \
         updated_at) VALUES (X'{}', 'branch', X'{}', 'raw', 1, NULL, 0, 0)",
        Uuid::now_v7().simple(),
        o.jkt.id.as_uuid().simple()
    );
    assert!(raw(db, &raw_insert).await.is_err());

    // Listing.
    let all = pr::list_price_lists(db, ListFilter::default())
        .await
        .unwrap();
    let names: Vec<_> = all.iter().map(|l| l.name.as_str()).collect();
    assert_eq!(names, ["Bandung", "Jakarta 2", "Default"]);
    let with_archived = pr::list_price_lists(
        db,
        ListFilter {
            kind: Some(PriceListKind::Branch),
            target: Some(o.jkt.id.into()),
            include_archived: true,
        },
    )
    .await
    .unwrap();
    assert_eq!(with_archived.len(), 2);
    let missing = commit!(db, |tx| pr::archive_price_list(
        &mut tx,
        PriceListId::generate(),
        &actor(&o),
        t(40)
    ));
    assert!(matches!(
        missing,
        Err(PricingDbError::NotFound("price_list"))
    ));
}

async fn price_list_validation(db: &Db) {
    let o = org(db).await;
    let gold = tier(db, &o, "Gold", 1).await;
    let wholesale = group(db, &o, "Wholesale").await;
    let budi = customer(db, &o, "K7QH29XM").await;
    for (kind, target) in [
        (PriceListKind::Tier, gold),
        (PriceListKind::Group, wholesale),
        (PriceListKind::Customer, budi),
    ] {
        let l = list(kind, Some(target), kind.as_str());
        assert_eq!(put_list(db, &o, &l).await.unwrap(), Written::Created);
        // Each kind's target lives in its own table.
        let wrong = list(kind, Some(o.jkt.id.into()), "wrong");
        assert!(matches!(
            put_list(db, &o, &wrong).await,
            Err(PricingDbError::InvalidReference("target"))
        ));
    }
    let cases: [(PriceListRecord, &str); 4] = [
        (list(PriceListKind::Branch, None, "x"), "target_id"),
        (
            list(PriceListKind::Branch, Some(o.jkt.id.into()), " "),
            "name",
        ),
        (list(PriceListKind::Default, None, "Second default"), "kind"),
        (
            list(
                PriceListKind::Branch,
                Some(o.jkt.id.into()),
                &"x".repeat(101),
            ),
            "name",
        ),
    ];
    for (l, field) in cases {
        let err = put_list(db, &o, &l).await.unwrap_err();
        assert!(
            matches!(err, PricingDbError::Invalid { field: f, .. } if f == field),
            "{field}: {err}"
        );
        assert_eq!(err.code(), Some("invalid_field"));
    }
    // Kind and target are fixed once created.
    let mut jkt = list(PriceListKind::Branch, Some(o.jkt.id.into()), "Jakarta");
    put_list(db, &o, &jkt).await.unwrap();
    jkt.target = Some(o.bdg.id.into());
    assert!(matches!(
        put_list(db, &o, &jkt).await,
        Err(PricingDbError::Invalid {
            field: "target_id",
            ..
        })
    ));
    jkt.kind = PriceListKind::Group;
    assert!(matches!(
        put_list(db, &o, &jkt).await,
        Err(PricingDbError::Invalid { field: "kind", .. })
    ));
    // A % rule of −100% is never a price.
    let mut bdg = list(PriceListKind::Branch, Some(o.bdg.id.into()), "Bandung");
    bdg.rule = Some(Rule::Pct(-10_000));
    let err = put_list(db, &o, &bdg).await.unwrap_err();
    assert_eq!(err.code(), Some("invalid_rule"));
}

async fn default_list_rules(db: &Db) {
    let o = org(db).await;
    let default = pr::get_price_list(db, DEFAULT_PRICE_LIST)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        DEFAULT_PRICE_LIST.to_string(),
        "00000000-0000-7000-8000-000000000201"
    );
    assert_eq!(default.kind, PriceListKind::Default);
    assert_eq!(default.target, None);
    assert_eq!(default.name, "Default");
    assert!(default.is_live());
    assert_eq!(default.rule, None);

    let mut with_rule = default.clone();
    with_rule.rule = Some(Rule::Pct(-500));
    let err = put_list(db, &o, &with_rule).await.unwrap_err();
    assert_eq!(err.code(), Some("invalid_rule"));
    let mut off = default.clone();
    off.active = false;
    let err = put_list(db, &o, &off).await.unwrap_err();
    assert!(matches!(
        err,
        PricingDbError::Rule(PricingError::InvalidField {
            field: "active",
            ..
        })
    ));
    let err = commit!(db, |tx| pr::archive_price_list(
        &mut tx,
        DEFAULT_PRICE_LIST,
        &actor(&o),
        t(40)
    ))
    .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    let mut renamed = default.clone();
    "Harga umum".clone_into(&mut renamed.name);
    assert_eq!(put_list(db, &o, &renamed).await.unwrap(), Written::Updated);

    // A rule inside a default-list entry is refused too (spec 2.3 §15.4).
    let (_, v, pcs, _) = product(db, &o, "P1").await;
    let e = entry(
        v,
        pcs,
        vec![PriceTier {
            min_qty: qty(1),
            price: TierPrice::Rule(Rule::Pct(-500)),
        }],
    );
    let err = put_entry(db, &o, DEFAULT_PRICE_LIST, &e).await.unwrap_err();
    assert_eq!(err.code(), Some("invalid_rule"));

    // A second default list is refused by the database as well.
    let raw_insert = format!(
        "INSERT INTO price_list (id, kind, target_id, name, active, archived_at, created_at, \
         updated_at) VALUES (X'{}', 'default', NULL, 'raw', 1, NULL, 0, 0)",
        Uuid::now_v7().simple()
    );
    assert!(raw(db, &raw_insert).await.is_err());
}

async fn entry_put_replay_and_audit(db: &Db) {
    let o = org(db).await;
    let (doc, v, pcs, _) = product(db, &o, "P1").await;
    assert!(!get_product(db, doc.draft.id).await.unwrap().unwrap().used);
    let mut e = entry(v, pcs, vec![fixed(12, 9_000), fixed(1, 10_000)]);
    let before = trail(db).await;
    assert_eq!(
        put_entry(db, &o, DEFAULT_PRICE_LIST, &e).await.unwrap(),
        Written::Created
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    // The variant is used now (spec 2.2 §2).
    assert!(get_product(db, doc.draft.id).await.unwrap().unwrap().used);
    // Tiers are stored by min_qty, so a replay in any order changes nothing.
    let stored = pr::get_price_entry(db, e.id).await.unwrap().unwrap();
    assert_eq!(stored.list, DEFAULT_PRICE_LIST);
    assert_eq!(stored.entry.tiers, [fixed(1, 10_000), fixed(12, 9_000)]);
    assert_eq!(
        put_entry(db, &o, DEFAULT_PRICE_LIST, &e).await.unwrap(),
        Written::Unchanged
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));

    e.tiers = vec![fixed(1, 10_500)];
    e.valid_from = Some(d(2026, 12, 1));
    assert_eq!(
        put_entry(db, &o, DEFAULT_PRICE_LIST, &e).await.unwrap(),
        Written::Updated
    );
    let stored = pr::get_price_entry(db, e.id).await.unwrap().unwrap();
    assert_eq!(stored.entry, e);
    let audit = audit::query(
        db,
        &AuditFilter {
            target: Some(format!("price_entry:{}", e.id)),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[0].action, "price_entry.put");
    let old = audit[0].before.as_ref().unwrap();
    let new = audit[0].after.as_ref().unwrap();
    assert_eq!(old["tiers"][0]["price"], 10_000);
    assert_eq!(old["tiers"][1]["price"], 9_000);
    assert_eq!(new["tiers"][0]["price"], 10_500);
    assert_eq!(new["valid_from"], 20_261_201);
    assert_eq!(new["variant_id"], v.to_string());

    // A rule tier in a branch list.
    let jkt = list(PriceListKind::Branch, Some(o.jkt.id.into()), "Jakarta");
    put_list(db, &o, &jkt).await.unwrap();
    let r = entry(
        v,
        pcs,
        vec![PriceTier {
            min_qty: qty(1),
            price: TierPrice::Rule(Rule::Amount(rp(-500))),
        }],
    );
    put_entry(db, &o, jkt.id, &r).await.unwrap();
    let stored = pr::get_price_entry(db, r.id).await.unwrap().unwrap();
    assert_eq!(stored.entry, r);
}

async fn entry_overlap_and_validation(db: &Db) {
    let o = org(db).await;
    let (_, v, pcs, box_) = product(db, &o, "P1").await;
    let (_, other_v, other_pcs, _) = product(db, &o, "P2").await;
    let mut old = entry(v, pcs, vec![fixed(1, 10_000)]);
    old.valid_to = Some(d(2026, 11, 30));
    put_entry(db, &o, DEFAULT_PRICE_LIST, &old).await.unwrap();
    // A scheduled change: the new entry starts the day after.
    let mut new = entry(v, pcs, vec![fixed(1, 10_500)]);
    new.valid_from = Some(d(2026, 12, 1));
    assert_eq!(
        put_entry(db, &o, DEFAULT_PRICE_LIST, &new).await.unwrap(),
        Written::Created
    );
    // Overlapping dates.
    let mut clash = entry(v, pcs, vec![fixed(1, 9_000)]);
    clash.valid_from = Some(d(2026, 11, 30));
    clash.valid_to = Some(d(2026, 11, 30));
    let err = put_entry(db, &o, DEFAULT_PRICE_LIST, &clash)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PricingDbError::Rule(PricingError::PriceOverlap { other, .. }) if other == old.id),
        "{err}"
    );
    assert_eq!(err.code(), Some("price_overlap"));
    // Another unit, or another list, does not overlap.
    clash.unit = box_;
    put_entry(db, &o, DEFAULT_PRICE_LIST, &clash).await.unwrap();
    let jkt = list(PriceListKind::Branch, Some(o.jkt.id.into()), "Jakarta");
    put_list(db, &o, &jkt).await.unwrap();
    let mut branch = entry(v, pcs, vec![fixed(1, 11_000)]);
    branch.valid_from = Some(d(2026, 11, 30));
    put_entry(db, &o, jkt.id, &branch).await.unwrap();
    // Moving an entry to another list is refused.
    let err = put_entry(db, &o, jkt.id, &old).await.unwrap_err();
    assert!(matches!(
        err,
        PricingDbError::Invalid {
            field: "list_id",
            ..
        }
    ));

    // core validation: no tiers, repeated min_qty, negative price, dates reversed.
    let mut bad = entry(v, box_, vec![]);
    bad.valid_from = Some(d(2030, 1, 1));
    let err = put_entry(db, &o, DEFAULT_PRICE_LIST, &bad)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    bad.tiers = vec![fixed(1, 1), fixed(1, 2)];
    assert!(put_entry(db, &o, DEFAULT_PRICE_LIST, &bad).await.is_err());
    bad.tiers = vec![fixed(1, -1)];
    assert!(put_entry(db, &o, DEFAULT_PRICE_LIST, &bad).await.is_err());
    bad.tiers = vec![fixed(1, 0)];
    bad.valid_to = Some(d(2029, 1, 1));
    let err = put_entry(db, &o, DEFAULT_PRICE_LIST, &bad)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        PricingDbError::Rule(PricingError::InvalidField {
            field: "valid_to",
            ..
        })
    ));

    // References.
    let unknown = entry(VariantId::generate(), pcs, vec![fixed(1, 1)]);
    let err = put_entry(db, &o, DEFAULT_PRICE_LIST, &unknown)
        .await
        .unwrap_err();
    assert!(matches!(err, PricingDbError::InvalidReference("variant")));
    assert_eq!(err.code(), Some("invalid_reference"));
    let foreign_unit = entry(v, other_pcs, vec![fixed(1, 1)]);
    assert!(matches!(
        put_entry(db, &o, DEFAULT_PRICE_LIST, &foreign_unit).await,
        Err(PricingDbError::InvalidReference("unit"))
    ));
    let free = entry(other_v, other_pcs, vec![fixed(1, 0)]);
    assert!(matches!(
        put_entry(db, &o, PriceListId::generate(), &free).await,
        Err(PricingDbError::NotFound("price_list"))
    ));
    // A free item (price 0) is allowed.
    put_entry(db, &o, DEFAULT_PRICE_LIST, &free).await.unwrap();
}

#[allow(clippy::too_many_lines)] // one story: write, list by every filter, page, delete
async fn entry_list_and_delete(db: &Db) {
    let o = org(db).await;
    let (doc1, v1, pcs1, box1) = product(db, &o, "P1").await;
    let (_, v2, pcs2, _) = product(db, &o, "P2").await;
    let jkt = list(PriceListKind::Branch, Some(o.jkt.id.into()), "Jakarta");
    put_list(db, &o, &jkt).await.unwrap();
    let pcs_entry = entry(v1, pcs1, vec![fixed(1, 10_000)]);
    let box_entry = entry(v1, box1, vec![fixed(1, 230_000)]);
    let other_entry = entry(v2, pcs2, vec![fixed(1, 5_000)]);
    let jkt_entry = entry(v1, pcs1, vec![fixed(1, 11_000)]);
    for e in [&pcs_entry, &box_entry, &other_entry] {
        put_entry(db, &o, DEFAULT_PRICE_LIST, e).await.unwrap();
    }
    put_entry(db, &o, jkt.id, &jkt_entry).await.unwrap();

    let ids = |records: Vec<pr::PriceEntryRecord>| -> Vec<PriceEntryId> {
        records.into_iter().map(|r| r.entry.id).collect()
    };
    let in_default = pr::list_price_entries(
        db,
        EntryFilter {
            list: Some(DEFAULT_PRICE_LIST),
            ..EntryFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ids(in_default),
        [pcs_entry.id, box_entry.id, other_entry.id]
    );
    let of_v1 = pr::list_price_entries(
        db,
        EntryFilter {
            variant: Some(v1),
            ..EntryFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(of_v1), [pcs_entry.id, box_entry.id, jkt_entry.id]);
    let of_product = pr::list_price_entries(
        db,
        EntryFilter {
            product: Some(doc1.draft.id),
            list: Some(jkt.id),
            ..EntryFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(of_product.len(), 1);
    assert_eq!(of_product[0].entry.tiers, [fixed(1, 11_000)]);
    // Paging by ID.
    let page = pr::list_price_entries(
        db,
        EntryFilter {
            limit: 2,
            ..EntryFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(page), [pcs_entry.id, box_entry.id]);
    let rest = pr::list_price_entries(
        db,
        EntryFilter {
            after: Some(box_entry.id),
            limit: 5_000,
            ..EntryFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(rest), [other_entry.id, jkt_entry.id]);

    // Delete: only within its own list.
    let before = trail(db).await;
    let wrong = commit!(db, |tx| pr::delete_price_entry(
        &mut tx,
        jkt.id,
        pcs_entry.id,
        &actor(&o),
        t(50)
    ));
    assert!(matches!(
        wrong,
        Err(PricingDbError::NotFound("price_entry"))
    ));
    commit!(db, |tx| pr::delete_price_entry(
        &mut tx,
        DEFAULT_PRICE_LIST,
        pcs_entry.id,
        &actor(&o),
        t(50)
    ))
    .unwrap();
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert!(
        pr::get_price_entry(db, pcs_entry.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(crate::common::count(db, "price_tier").await, 3);
    let audit = audit::query(
        db,
        &AuditFilter {
            target: Some(format!("price_entry:{}", pcs_entry.id)),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(audit[0].action, "price_entry.delete");
    assert_eq!(
        audit[0].before.as_ref().unwrap()["tiers"][0]["price"],
        10_000
    );
    assert!(audit[0].after.is_none());
    // The ID may be written again (a new entry).
    assert_eq!(
        put_entry(db, &o, DEFAULT_PRICE_LIST, &pcs_entry)
            .await
            .unwrap(),
        Written::Created
    );
}

fn line(variant: VariantId, unit: UnitId, base: UnitId, factor: i64, q: i64) -> CartLine {
    CartLine {
        item: SaleItem {
            variant,
            unit,
            base_unit: base,
            factor: qty(factor),
        },
        qty: qty(q),
        mode: LineMode::Listed,
    }
}

/// Unit prices of `lines`, `None` for `no_price`.
fn prices(chain: &sultan_core::pricing::PriceChain<'_>, lines: &[CartLine]) -> Vec<Option<i64>> {
    resolve_cart(chain, lines)
        .into_iter()
        .map(|r| match r {
            Ok(p) => Some(p.unit_price.minor()),
            Err(PricingError::NoPrice { .. }) => None,
            Err(e) => panic!("{e}"),
        })
        .collect()
}

/// Spec 2.3 §12 cases 3, 4, 5 and 9, priced by `core` from the loaded lists.
#[allow(clippy::too_many_lines)] // the spec's cases on one set of lists
async fn resolution_follows_the_chain(db: &Db) {
    let o = org(db).await;
    let (_, v, pcs, box_) = product(db, &o, "P1").await;
    let (_, unpriced, unpriced_pcs, _) = product(db, &o, "P2").await;
    let wholesale = group(db, &o, "Wholesale").await;
    let gold = tier(db, &o, "Gold", 1).await;
    let budi = customer(db, &o, "K7QH29XM").await;

    put_entry(
        db,
        &o,
        DEFAULT_PRICE_LIST,
        &entry(v, pcs, vec![fixed(1, 10_000)]),
    )
    .await
    .unwrap();
    // Bali (here BDG) sells at 11 000 and has a box price.
    let bdg = list(PriceListKind::Branch, Some(o.bdg.id.into()), "Bandung");
    put_list(db, &o, &bdg).await.unwrap();
    put_entry(db, &o, bdg.id, &entry(v, pcs, vec![fixed(1, 11_000)]))
        .await
        .unwrap();
    put_entry(db, &o, bdg.id, &entry(v, box_, vec![fixed(1, 250_000)]))
        .await
        .unwrap();
    // Wholesale: only "8 500 from 12".
    let ws = list(PriceListKind::Group, Some(wholesale), "Wholesale");
    put_list(db, &o, &ws).await.unwrap();
    put_entry(db, &o, ws.id, &entry(v, pcs, vec![fixed(12, 8_500)]))
        .await
        .unwrap();
    // Gold: −5% on everything.
    let mut gl = list(PriceListKind::Tier, Some(gold), "Gold");
    gl.rule = Some(Rule::Pct(-500));
    put_list(db, &o, &gl).await.unwrap();
    // Budi's contract.
    let contract = list(PriceListKind::Customer, Some(budi), "Budi");
    put_list(db, &o, &contract).await.unwrap();
    put_entry(db, &o, contract.id, &entry(v, pcs, vec![fixed(1, 7_000)]))
        .await
        .unwrap();

    let date = d(2026, 11, 11);
    let gold_member = TierMembership {
        tier_archived: false,
        valid_until: None,
    };
    let load = |targets: PriceTargets| async move {
        pr::load_for_resolution(db, &targets, &[v, unpriced], date)
            .await
            .unwrap()
    };
    let pcs_line = |q| line(v, pcs, pcs, 1, q);
    let box_line = line(v, box_, pcs, 24, 1);

    // Case 4: fall through.
    let lists = load(PriceTargets {
        branch: Some(o.jkt.id),
        group: Some(wholesale),
        ..PriceTargets::default()
    })
    .await;
    assert!(lists.branch.is_none() && lists.tier.is_none() && lists.contract.is_none());
    assert_eq!(lists.group.as_ref().unwrap().id, ws.id);
    let chain = lists.chain(date);
    assert_eq!(prices(&chain, &[pcs_line(5)]), [Some(10_000)]);
    assert_eq!(prices(&chain, &[pcs_line(12)]), [Some(8_500)]);
    // No price for a variant without entries.
    assert_eq!(
        prices(&chain, &[line(unpriced, unpriced_pcs, unpriced_pcs, 1, 1)]),
        [None]
    );

    // Case 5: a rule on the next price.
    let gold_at = |branch| PriceTargets {
        branch: Some(branch),
        tier: Some((gold, gold_member)),
        ..PriceTargets::default()
    };
    let at_bdg = load(gold_at(o.bdg.id)).await;
    assert_eq!(prices(&at_bdg.chain(date), &[pcs_line(1)]), [Some(10_450)]);
    let at_jkt = load(gold_at(o.jkt.id)).await;
    assert_eq!(prices(&at_jkt.chain(date), &[pcs_line(1)]), [Some(9_500)]);

    // Case 9: unit fallback. No box price at JKT → 24 × pcs; Gold applies inside the pcs price.
    let plain_jkt = load(PriceTargets {
        branch: Some(o.jkt.id),
        ..PriceTargets::default()
    })
    .await;
    assert_eq!(prices(&plain_jkt.chain(date), &[box_line]), [Some(240_000)]);
    assert_eq!(prices(&at_jkt.chain(date), &[box_line]), [Some(228_000)]);
    // The box price in the BDG list wins; Gold −5% applies to it.
    let plain_bdg = load(PriceTargets {
        branch: Some(o.bdg.id),
        ..PriceTargets::default()
    })
    .await;
    assert_eq!(prices(&plain_bdg.chain(date), &[box_line]), [Some(250_000)]);
    assert_eq!(prices(&at_bdg.chain(date), &[box_line]), [Some(237_500)]);

    // Case 3: contract beats tier beats group beats branch beats default.
    let everything = PriceTargets {
        branch: Some(o.bdg.id),
        customer: Some(budi),
        group: Some(wholesale),
        tier: Some((gold, gold_member)),
    };
    let all = load(everything).await;
    assert_eq!(all.contract.as_ref().unwrap().id, contract.id);
    assert_eq!(prices(&all.chain(date), &[pcs_line(12)]), [Some(7_000)]);
    // The store chain ignores the customer lists.
    assert_eq!(
        prices(&all.store_chain(date), &[pcs_line(12)]),
        [Some(11_000)]
    );
    // Without the contract: Gold −5% on the group's 8 500 → 8 075.
    let no_contract = load(PriceTargets {
        customer: None,
        ..everything
    })
    .await;
    assert_eq!(
        prices(&no_contract.chain(date), &[pcs_line(12)]),
        [Some(8_075)]
    );
    // An expired tier falls through to the group (spec 2.3 §12 case 12).
    let expired = load(PriceTargets {
        customer: None,
        tier: Some((
            gold,
            TierMembership {
                tier_archived: false,
                valid_until: Some(d(2026, 11, 10)),
            },
        )),
        ..everything
    })
    .await;
    assert_eq!(prices(&expired.chain(date), &[pcs_line(12)]), [Some(8_500)]);
}

/// Entry dates, cart-wide tiers, inactive lists and an empty variant set.
async fn resolution_dates_tiers_and_lists(db: &Db) {
    let o = org(db).await;
    let (_, v, pcs, box_) = product(db, &o, "P1").await;
    let mut old = entry(v, pcs, vec![fixed(1, 10_000), fixed(12, 9_000)]);
    old.valid_to = Some(d(2026, 11, 30));
    let mut new = entry(v, pcs, vec![fixed(1, 10_500)]);
    new.valid_from = Some(d(2026, 12, 1));
    put_entry(db, &o, DEFAULT_PRICE_LIST, &old).await.unwrap();
    put_entry(db, &o, DEFAULT_PRICE_LIST, &new).await.unwrap();
    let targets = PriceTargets {
        branch: Some(o.jkt.id),
        ..PriceTargets::default()
    };

    // Case 11: the sale date picks the entry.
    let nov = d(2026, 11, 30);
    let lists = pr::load_for_resolution(db, &targets, &[v], nov)
        .await
        .unwrap();
    assert_eq!(lists.default.entries.len(), 1);
    assert_eq!(lists.default.entries[0].id, old.id);
    // Case 2: 10 pcs + 1 pack… here 1 box(24): Q = 34 ≥ 12 → the pcs line is at the tier.
    let cart = [line(v, pcs, pcs, 1, 10), line(v, box_, pcs, 24, 1)];
    assert_eq!(
        prices(&lists.chain(nov), &cart),
        [Some(9_000), Some(216_000)]
    );
    let dec = d(2026, 12, 1);
    let lists = pr::load_for_resolution(db, &targets, &[v], dec)
        .await
        .unwrap();
    assert_eq!(lists.default.entries[0].id, new.id);
    assert_eq!(
        prices(&lists.chain(dec), &[line(v, pcs, pcs, 1, 1)]),
        [Some(10_500)]
    );

    // An inactive branch list is not loaded.
    let mut jkt = list(PriceListKind::Branch, Some(o.jkt.id.into()), "Jakarta");
    jkt.active = false;
    put_list(db, &o, &jkt).await.unwrap();
    put_entry(db, &o, jkt.id, &entry(v, pcs, vec![fixed(1, 1)]))
        .await
        .unwrap();
    let lists = pr::load_for_resolution(db, &targets, &[v], dec)
        .await
        .unwrap();
    assert!(lists.branch.is_none());
    jkt.active = true;
    put_list(db, &o, &jkt).await.unwrap();
    let lists = pr::load_for_resolution(db, &targets, &[v], dec)
        .await
        .unwrap();
    assert_eq!(lists.branch.as_ref().unwrap().entries.len(), 1);
    assert_eq!(
        prices(&lists.chain(dec), &[line(v, pcs, pcs, 1, 1)]),
        [Some(1)]
    );

    // No variants: the lists come without entries.
    let lists = pr::load_for_resolution(db, &targets, &[], dec)
        .await
        .unwrap();
    assert!(lists.default.entries.is_empty());
    assert!(lists.branch.as_ref().unwrap().entries.is_empty());
}

async fn tax_rates_seed_and_history(db: &Db) {
    let o = org(db).await;
    let rate = |bp, date| TaxRate {
        rate_bp: bp,
        valid_from: date,
    };
    assert_eq!(
        pr::list_tax_rates(db, STANDARD_TAX_CLASS).await.unwrap(),
        [rate(1_100, d(2022, 4, 1))]
    );
    assert_eq!(
        pr::list_tax_rates(db, EXEMPT_TAX_CLASS).await.unwrap(),
        [rate(0, d(2000, 1, 1))]
    );
    let a = actor(&o);
    let put_rate = |class: TaxClassId, r: TaxRate| async move {
        commit!(db, |tx| pr::put_tax_rate(&mut tx, class, r, &a, t(60)))
    };
    // Case 16: 12% from 2025-01-01, entered ahead of time.
    let before = trail(db).await;
    let twelve = rate(1_200, d(2025, 1, 1));
    assert_eq!(
        put_rate(STANDARD_TAX_CLASS, twelve).await.unwrap(),
        Written::Created
    );
    assert_eq!(
        put_rate(STANDARD_TAX_CLASS, twelve).await.unwrap(),
        Written::Unchanged
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let history = pr::list_tax_rates(db, STANDARD_TAX_CLASS).await.unwrap();
    assert_eq!(history, [rate(1_100, d(2022, 4, 1)), twelve]);
    assert_eq!(rate_on(&history, d(2024, 12, 31)), Some(1_100));
    assert_eq!(rate_on(&history, d(2025, 1, 1)), Some(1_200));
    // Correcting the rate of the same day.
    let corrected = rate(1_150, d(2025, 1, 1));
    assert_eq!(
        put_rate(STANDARD_TAX_CLASS, corrected).await.unwrap(),
        Written::Updated
    );
    let audit = audit::query(
        db,
        &AuditFilter {
            target: Some(format!("tax_class:{STANDARD_TAX_CLASS}")),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(audit[0].action, "tax_rate.put");
    assert_eq!(audit[0].before.as_ref().unwrap()["rate_bp"], 1_200);
    assert_eq!(audit[0].after.as_ref().unwrap()["rate_bp"], 1_150);
    let by_class = pr::tax_rates_by_class(db).await.unwrap();
    assert_eq!(by_class.len(), 2);
    assert_eq!(by_class[&STANDARD_TAX_CLASS].len(), 2);
    assert_eq!(by_class[&EXEMPT_TAX_CLASS], [rate(0, d(2000, 1, 1))]);
    let err = put_rate(TaxClassId::generate(), twelve).await.unwrap_err();
    assert!(matches!(err, PricingDbError::NotFound("tax_class")));
}

async fn pricing_settings_defaults_and_put(db: &Db) {
    let o = org(db).await;
    let settings = pr::get_pricing_settings(db).await.unwrap();
    assert_eq!(settings, PricingSettings::default());
    assert!(settings.tax.inclusive && settings.tax.registered);
    assert_eq!(settings.cash_rounding, CashRounding::None);
    let before = trail(db).await;
    let changed = PricingSettings {
        tax: TaxSettings {
            inclusive: false,
            registered: true,
        },
        cash_rounding: CashRounding::Down500,
    };
    let a = actor(&o);
    let put_settings =
        |s| async move { commit!(db, |tx| pr::put_pricing_settings(&mut tx, s, &a, t(70))) };
    assert_eq!(put_settings(changed).await.unwrap(), Written::Updated);
    assert_eq!(put_settings(changed).await.unwrap(), Written::Unchanged);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(pr::get_pricing_settings(db).await.unwrap(), changed);
    for rounding in CashRounding::ALL {
        let s = PricingSettings {
            cash_rounding: rounding,
            ..changed
        };
        put_settings(s).await.unwrap();
        assert_eq!(pr::get_pricing_settings(db).await.unwrap(), s);
    }
}

/// `(price.override limit, has price.manage)` of a role; `None` when it lacks `price.override`.
async fn price_grants(db: &Db, role: RoleId) -> (Option<Option<Limit>>, bool) {
    let role = roles::get_role(db, role).await.unwrap().unwrap();
    let find = |p| role.grants.iter().find(|g| g.permission() == p);
    (
        find(Permission::PriceOverride).map(sultan_core::access::Grant::limit),
        find(Permission::PriceManage).is_some(),
    )
}

async fn price_permissions_on_a_fresh_database(db: &Db) {
    for now in [t(1), t(2)] {
        seed::seed_defaults(db, now).await.unwrap();
        assert_eq!(price_grants(db, OWNER_ROLE).await, (Some(None), true));
        assert_eq!(
            price_grants(db, MANAGER_ROLE).await,
            (Some(Some(Limit::MaxPct(2_000))), true)
        );
        assert_eq!(price_grants(db, CASHIER_ROLE).await, (None, false));
    }
}

const PRICING_MIGRATION: i64 = 20_261_111_000_003;

/// The migrations before pricing, as an older release shipped them.
async fn old_migrator(backend: &str) -> sqlx::migrate::Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut migrator = sqlx::migrate::Migrator::new(dir).await.unwrap();
    let old: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version < PRICING_MIGRATION)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(old);
    migrator
}

/// An install migrated from before 2.3: roles as the older seed wrote them (Manager's
/// `price.override` unlimited, no `price.manage`). The migration adds `price.manage` to Owner and
/// Manager and keeps the unlimited `price.override`.
async fn upgrade_adds_price_manage(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    raw(
        db,
        "DELETE FROM role_permission WHERE perm = 'price.manage'",
    )
    .await
    .unwrap();
    raw(
        db,
        "UPDATE role_permission SET limit_value = NULL WHERE perm = 'price.override'",
    )
    .await
    .unwrap();
    assert_eq!(price_grants(db, MANAGER_ROLE).await, (Some(None), false));
    db.migrate().await.unwrap();
    assert_eq!(price_grants(db, OWNER_ROLE).await, (Some(None), true));
    assert_eq!(price_grants(db, MANAGER_ROLE).await, (Some(None), true));
    assert_eq!(price_grants(db, CASHIER_ROLE).await, (None, false));
    // The seed leaves the edited Manager alone.
    seed::seed_defaults(db, t(3)).await.unwrap();
    assert_eq!(price_grants(db, MANAGER_ROLE).await, (Some(None), true));
    assert!(
        pr::get_price_list(db, DEFAULT_PRICE_LIST)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn sqlite_upgrade_adds_price_manage() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let Db::Sqlite(pool) = &db else {
        unreachable!()
    };
    old_migrator("sqlite").await.run(pool).await.unwrap();
    upgrade_adds_price_manage(&db).await;
}

#[tokio::test]
async fn mysql_upgrade_adds_price_manage() {
    let Some(test_db) = MySqlTestDb::create_empty().await else {
        return;
    };
    let Db::MySql(pool) = &test_db.db else {
        unreachable!()
    };
    old_migrator("mysql").await.run(pool).await.unwrap();
    upgrade_adds_price_manage(&test_db.db).await;
    test_db.drop().await;
}
