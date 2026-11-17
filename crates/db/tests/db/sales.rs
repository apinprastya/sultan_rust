//! Tender types, posted sales, numbers, post-voids, the stock ledger and the sales permissions
//! (spec 2.6, phase S2).

use std::borrow::Cow;
use std::path::Path;

use sultan_core::access::Permission;
use sultan_core::catalog::Qty;
use sultan_core::ids::{RegisterId, SaleId, TenderTypeId};
use sultan_core::money::Money;
use sultan_core::sale::{
    Discount, RefundMethod, SaleError, SaleStatus, StockMovementKind, TenderKind, TenderType,
    VoidReason,
};
use sultan_db::catalog::{self, put_product};
use sultan_db::customers::{self as cust, BalanceEntry, BalanceEntryKind, CustomerDbError};
use sultan_db::org::Register;
use sultan_db::sales::{
    self, CASH_TENDER_TYPE, SaleDbError, StockMovementRecord, TenderTypeRecord, Written,
};
use sultan_db::seed::{self, CASHIER_ROLE, MANAGER_ROLE, OWNER_ROLE};
use sultan_db::{Db, DbError, roles};
use uuid::Uuid;

use crate::catalog_fixtures::{actor, simple, trail};
use crate::common::{MySqlTestDb, count, db_tests, raw, t};
use crate::sales_fixtures::{
    DAY, NOV16, Pay, Till, cash_sale, customer, item, open_session, post, post_retrying, q, rp,
    sale_at, till, void, void_of,
};

db_tests!(
    tender_types_seed_put_and_archive,
    post_sale_stores_everything,
    replay_and_conflicting_body,
    numbers_follow_the_local_day,
    concurrent_posts_get_distinct_numbers,
    invalid_sales_are_refused,
    post_void_pays_back_and_restocks,
    post_void_refusals,
    stock_ledger_is_append_only,
    balance_refund_kind,
    sales_permissions_on_a_fresh_database,
);

fn tender(name: &str, kind: TenderKind) -> TenderType {
    TenderType {
        id: TenderTypeId::generate(),
        name: name.to_owned(),
        kind,
        needs_reference: true,
        opens_drawer: false,
        active: true,
    }
}

async fn put_tender(
    db: &Db,
    till: &Till,
    t_: &TenderType,
    sort: u32,
) -> Result<Written, SaleDbError> {
    let mut tx = db.begin().await.unwrap();
    let r = sales::put_tender_type(&mut tx, t_, sort, &actor(&till.o), t(NOV16)).await;
    if r.is_ok() {
        tx.commit().await.unwrap();
    }
    r
}

async fn tender_types_seed_put_and_archive(db: &Db) {
    let till = till(db).await;
    let seeded = sales::list_tender_types(db, false).await.unwrap();
    let names: Vec<_> = seeded.iter().map(|r| r.tender.name.as_str()).collect();
    assert_eq!(names, ["Cash", "Debit card", "QRIS", "Customer balance"]);
    assert_eq!(seeded[0].tender.id, CASH_TENDER_TYPE);
    assert!(seeded[0].tender.opens_drawer);
    assert!(seeded[1].tender.needs_reference);

    let before = trail(db).await;
    let mut bca = tender("  Debit BCA ", TenderKind::Card);
    assert_eq!(
        put_tender(db, &till, &bca, 5).await.unwrap(),
        Written::Created
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    let stored = sales::get_tender_type(db, bca.id).await.unwrap().unwrap();
    assert_eq!(stored.tender.name, "Debit BCA");
    assert_eq!(stored.sort, 5);
    // Replay: nothing written.
    assert_eq!(
        put_tender(db, &till, &bca, 5).await.unwrap(),
        Written::Unchanged
    );
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    // Update, then the list order follows sort.
    bca.active = false;
    assert_eq!(
        put_tender(db, &till, &bca, 5).await.unwrap(),
        Written::Updated
    );
    assert!(
        !sales::list_tender_types(db, false)
            .await
            .unwrap()
            .iter()
            .any(|r| r.tender.id == bca.id)
    );
    let all = sales::list_tender_types(db, true).await.unwrap();
    assert_eq!(all[1].tender.id, bca.id);
    // The kind never changes; names are checked by core; exchange is not a kind.
    let mut changed = bca.clone();
    changed.kind = TenderKind::Qris;
    let err = put_tender(db, &till, &changed, 5).await.unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"), "{err}");
    let err = put_tender(db, &till, &tender(" ", TenderKind::Cash), 0)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        SaleDbError::Rule(SaleError::InvalidField { field: "name", .. })
    ));
    let err = put_tender(db, &till, &tender("Tukar", TenderKind::Exchange), 0)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));

    // Archive: once, then unchanged; an archived type cannot be edited.
    let mut tx = db.begin().await.unwrap();
    let a = actor(&till.o);
    assert_eq!(
        sales::archive_tender_type(&mut tx, bca.id, &a, t(NOV16 + 1))
            .await
            .unwrap(),
        Written::Updated
    );
    assert_eq!(
        sales::archive_tender_type(&mut tx, bca.id, &a, t(NOV16 + 2))
            .await
            .unwrap(),
        Written::Unchanged
    );
    assert!(matches!(
        sales::archive_tender_type(&mut tx, TenderTypeId::generate(), &a, t(1)).await,
        Err(SaleDbError::NotFound("tender type"))
    ));
    tx.commit().await.unwrap();
    let archived: TenderTypeRecord = sales::get_tender_type(db, bca.id).await.unwrap().unwrap();
    assert_eq!(archived.archived_at, Some(t(NOV16 + 1)));
    assert!(matches!(
        put_tender(db, &till, &bca, 5).await,
        Err(SaleDbError::Archived)
    ));
    let kinds: Vec<String> = sultan_db::outbox::list_unsynced(db, 1_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("tender_type."))
        .collect();
    assert_eq!(
        kinds,
        ["tender_type.put", "tender_type.put", "tender_type.archive"]
    );
}

async fn post_sale_stores_everything(db: &Db) {
    let till = till(db).await;
    // A real product, so the variant is marked used.
    let doc = simple("TEH", "8991234567890");
    let mut tx = db.begin().await.unwrap();
    put_product(&mut tx, &doc, &actor(&till.o), t(100))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut tea = item(5_000, 3);
    tea.variant = doc.draft.variants[0].id;
    tea.unit = doc.draft.units.base;
    let soap = item(12_500, 1);
    // 27 500 − 10% cart discount = 24 750; paid 30 000 cash, 250 saved to the balance.
    let sale = sale_at(
        &till,
        &[tea, soap],
        Some(Discount::Pct(1_000)),
        Some((till.customer, Money::ZERO)),
        &[Pay::Cash(30_000)],
        rp(250),
        NOV16,
    );
    let before = trail(db).await;
    let (written, stored) = post(db, &sale).await.unwrap();
    assert_eq!(written, Written::Created);
    let h = &stored.header;
    assert_eq!(h.number, "JKT01-R1-261116-0001");
    assert_eq!(h.status, SaleStatus::Posted);
    assert_eq!(h.totals.total, rp(24_750));
    assert_eq!(h.totals.cart_discount, rp(2_750));
    assert_eq!(h.paid(), rp(24_750));
    assert_eq!(h.change_given, rp(5_000));
    assert_eq!(h.change_saved, rp(250));
    assert_eq!((h.line_count, h.return_count), (2, 0));
    assert_eq!(h.customer, Some(till.customer));
    assert_eq!(h.cashier_session, Some(till.session));
    assert_eq!(h.legacy_number, None);
    assert_eq!(stored.lines.len(), 2);
    assert_eq!(stored.lines[0].line_no, 1);
    assert_eq!(stored.lines[0].line, sale.lines[0]);
    assert_eq!(stored.lines[1].line, sale.lines[1]);
    assert_eq!(stored.tenders, sale.payment.tenders);
    assert!(stored.void.is_none());
    // Audit + outbox for the sale, one outbox row for the balance entry.
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 2));
    // Stock: 3 tea out at the register's location; negative is fine.
    let tea_qty = sales::stock_on_hand(db, till.o.jkt.id, tea.variant)
        .await
        .unwrap();
    assert_eq!(tea_qty, Qty::from_thousandths(-3_000));
    let moves: Vec<StockMovementRecord> = sales::document_movements(db, *sale.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(moves.len(), 2);
    assert!(moves.iter().all(|m| m.location == Some(till.o.location.id)));
    assert!(moves.iter().all(|m| m.doc_kind == "sale"));
    // Balance: +250 saved change.
    let balance = cust::get_balance(db, till.customer, None, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(balance.balance, rp(250));
    assert_eq!(balance.entries[0].kind, BalanceEntryKind::ChangeSaved);
    assert_eq!(balance.entries[0].document, Some(*sale.id.as_uuid()));
    assert_eq!(balance.entries[0].cashier_session, Some(till.session));
    // The variant is used now.
    let product = catalog::get_product(db, doc.draft.id)
        .await
        .unwrap()
        .unwrap();
    assert!(product.used);
    // Reads.
    assert_eq!(sales::get_sale(db, sale.id).await.unwrap().unwrap(), stored);
    assert_eq!(
        sales::get_sale_by_number(db, "JKT01-R1-261116-0001")
            .await
            .unwrap()
            .unwrap(),
        stored
    );
    assert!(
        sales::get_sale(db, SaleId::generate())
            .await
            .unwrap()
            .is_none()
    );
    assert!(sales::get_sale_by_number(db, "X").await.unwrap().is_none());
    assert_eq!(count(db, "sale_tender").await, 1);
}

async fn replay_and_conflicting_body(db: &Db) {
    let till = till(db).await;
    let sale = cash_sale(&till, &[item(10_000, 2)], NOV16);
    let (_, first) = post(db, &sale).await.unwrap();
    let before = (trail(db).await, count(db, "stock_movement").await);
    // §15 case 10: the same body again → the stored sale, nothing new.
    let mut again = sale.clone();
    again.posted_at = t(NOV16 + 5_000);
    let (written, replayed) = post(db, &again).await.unwrap();
    assert_eq!(written, Written::Unchanged);
    assert_eq!(replayed, first);
    assert_eq!((trail(db).await, count(db, "stock_movement").await), before);
    // Another body under the same id → conflict.
    let mut other = sale.clone();
    other.request_hash = [9; 32];
    let err = post(db, &other).await.unwrap_err();
    assert!(matches!(err, SaleDbError::Conflict("sale", id) if id == *sale.id.as_uuid()));
    assert_eq!(err.code(), Some("conflict"));
}

async fn numbers_follow_the_local_day(db: &Db) {
    let till = till(db).await;
    // §15 case 19: 23:59:59 and 00:00:00 in Asia/Jakarta (UTC+7).
    let late = 1_794_848_399_000; // 2026-11-16 16:59:59 UTC
    let midnight = 1_794_848_400_000; // 2026-11-16 17:00:00 UTC
    let numbers = [late - 1_000, late, midnight, midnight + 1_000];
    let mut got = Vec::new();
    for at in numbers {
        let (_, s) = post(db, &cash_sale(&till, &[item(1_000, 1)], at))
            .await
            .unwrap();
        got.push(s.header.number);
    }
    assert_eq!(
        got,
        [
            "JKT01-R1-261116-0001",
            "JKT01-R1-261116-0002",
            "JKT01-R1-261117-0001",
            "JKT01-R1-261117-0002",
        ]
    );
    // Another register has its own sequence.
    let r2 = Register {
        id: RegisterId::generate(),
        branch: till.o.jkt.id,
        code: "R2".into(),
        name: "Back".into(),
        default_location: None,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_register(&mut tx, &r2, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut on_r2 = cash_sale(&till, &[item(1_000, 1)], midnight);
    on_r2.register = r2.id;
    let (_, s) = post(db, &on_r2).await.unwrap();
    assert_eq!(s.header.number, "JKT01-R2-261117-0001");
    // R2 has no default location: the branch's receiving default is used.
    let moves = sales::document_movements(db, *on_r2.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(moves[0].location, Some(till.o.location.id));
    // A branch without any location: no location.
    let mut tx = db.begin().await.unwrap();
    let r3 = Register {
        id: RegisterId::generate(),
        branch: till.o.bdg.id,
        code: "R1".into(),
        name: "Bandung".into(),
        default_location: None,
        archived: false,
    };
    sultan_db::org::save_register(&mut tx, &r3, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut bdg = cash_sale(&till, &[item(1_000, 1)], NOV16);
    bdg.branch = till.o.bdg.id;
    bdg.register = r3.id;
    let (_, s) = post(db, &bdg).await.unwrap();
    assert_eq!(s.header.number, "BDG01-R1-261116-0001");
    let moves = sales::document_movements(db, *bdg.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(moves[0].location, None);
}

async fn concurrent_posts_get_distinct_numbers(db: &Db) {
    let till = till(db).await;
    let tasks: Vec<_> = (0..6)
        .map(|i| {
            let db = db.clone();
            let sale = cash_sale(&till, &[item(1_000 + i, 1)], NOV16 + i);
            tokio::spawn(async move { post_retrying(&db, &sale).await })
        })
        .collect();
    let mut numbers = Vec::new();
    for task in tasks {
        numbers.push(task.await.unwrap().unwrap().1.header.number);
    }
    numbers.sort();
    let expected: Vec<String> = (1..=6).map(|n| format!("JKT01-R1-261116-{n:04}")).collect();
    assert_eq!(numbers, expected);
    // A lost race is retryable.
    assert!(SaleDbError::Db(DbError::Conflict("x".into())).is_retryable());
}

async fn invalid_sales_are_refused(db: &Db) {
    let till = till(db).await;
    let base = cash_sale(&till, &[item(10_000, 1)], NOV16);
    let check = |mut f: Box<dyn FnMut(&mut sales::NewSale)>| {
        let mut s = base.clone();
        s.id = SaleId::generate();
        f(&mut s);
        s
    };
    let cases: Vec<(sales::NewSale, &str)> = vec![
        (check(Box::new(|s| s.lines.clear())), "lines"),
        (check(Box::new(|s| s.totals.total = rp(9_999))), "total"),
        (
            check(Box::new(|s| s.payment.tenders[0].amount = rp(9_000))),
            "tenders",
        ),
        (
            check(Box::new(|s| {
                s.payment.tenders[0].kind = TenderKind::Exchange;
                s.payment.tenders[0].tender_type = None;
            })),
            "tenders",
        ),
        (
            check(Box::new(|s| s.payment.tenders[0].tender_type = None)),
            "tenders",
        ),
        (
            check(Box::new(|s| {
                s.movements[0].kind = StockMovementKind::Return;
            })),
            "movements",
        ),
        (
            check(Box::new(|s| s.register = till_register_elsewhere())),
            "register",
        ),
    ];
    for (sale, field) in cases {
        let err = post(db, &sale).await.unwrap_err();
        match &err {
            SaleDbError::Invalid { field: f, .. } => assert_eq!(*f, field, "{err}"),
            SaleDbError::NotFound(what) => assert_eq!(*what, field, "{err}"),
            other => panic!("{other:?}"),
        }
    }
    // Balance entries need a customer.
    let mut no_customer = sale_at(
        &till,
        &[item(10_000, 1)],
        None,
        Some((till.customer, Money::ZERO)),
        &[Pay::Cash(20_000)],
        rp(100),
        NOV16,
    );
    no_customer.customer = None;
    let err = post(db, &no_customer).await.unwrap_err();
    assert!(
        matches!(
            err,
            SaleDbError::Invalid {
                field: "customer",
                ..
            }
        ),
        "{err}"
    );
    // An unknown customer.
    let mut ghost = no_customer.clone();
    ghost.id = SaleId::generate();
    ghost.customer = Some(Uuid::now_v7());
    let err = post(db, &ghost).await.unwrap_err();
    assert!(matches!(err, SaleDbError::NotFound("customer")), "{err}");
    // A register of another branch.
    let mut tx = db.begin().await.unwrap();
    let r = Register {
        id: RegisterId::generate(),
        branch: till.o.bdg.id,
        code: "R9".into(),
        name: "x".into(),
        default_location: None,
        archived: false,
    };
    sultan_db::org::save_register(&mut tx, &r, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut wrong = base.clone();
    wrong.register = r.id;
    let err = post(db, &wrong).await.unwrap_err();
    assert!(
        matches!(
            err,
            SaleDbError::Invalid {
                field: "register",
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(count(db, "sale").await, 0);
}

/// An unknown register (no row).
fn till_register_elsewhere() -> RegisterId {
    RegisterId::generate()
}

async fn post_void_pays_back_and_restocks(db: &Db) {
    let till = till(db).await;
    // Give the customer a balance of 10 000 first.
    let mut tx = db.begin().await.unwrap();
    cust::append_balance_entry(
        &mut tx,
        &BalanceEntry::new(
            till.customer,
            BalanceEntryKind::ChangeSaved,
            rp(10_000),
            t(5),
        ),
        t(5),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // 30 000: 10 000 from the balance, 20 000 card... and a 25 000 cash sale saving 500.
    let shirt = item(30_000, 1);
    let sale = sale_at(
        &till,
        &[shirt],
        None,
        Some((till.customer, rp(10_000))),
        &[Pay::Balance(10_000), Pay::Cash(25_000)],
        rp(500),
        NOV16,
    );
    let (_, stored) = post(db, &sale).await.unwrap();
    // Balance: 10 000 − 10 000 + 500.
    assert_eq!(balance_of(db, &till).await, rp(500));
    assert_eq!(
        sales::stock_on_hand(db, till.o.jkt.id, shirt.variant)
            .await
            .unwrap(),
        q(-1)
    );
    // §15 case 12: voided the next day in a new session, refund to the balance.
    let session = open_session(
        db,
        till.o.manager.id,
        register_two(db, &till).await,
        NOV16 + DAY,
    )
    .await;
    let v = void_of(
        &till,
        &stored,
        session,
        RefundMethod::Balance,
        Some(rp(500)),
        NOV16 + DAY,
    );
    // Saved change back (−500), balance tender back (+10 000), the rest (20 000 + 500 cash
    // handed over) to the balance as refund.
    assert_eq!(v.plan.change_saved_reversed, rp(500));
    let before = trail(db).await;
    let voided = void(db, &v).await.unwrap();
    assert_eq!(voided.header.status, SaleStatus::Voided);
    assert_eq!(voided.header.voided_at, Some(t(NOV16 + DAY)));
    let record = voided.void.clone().unwrap();
    assert_eq!(record.reason, VoidReason::WrongItem);
    assert_eq!(record.cashier_session, session);
    assert_eq!(record.refund, rp(30_500));
    assert_eq!(record.payment_reversed, rp(10_000));
    assert_eq!(record.cash, Money::ZERO);
    assert_eq!(record.tenders, v.tenders);
    assert_eq!(balance_of(db, &till).await, rp(10_000 + 20_500));
    assert_eq!(
        cust::ledger_balance(db, till.customer).await.unwrap(),
        balance_of(db, &till).await
    );
    // Stock is back.
    assert_eq!(
        sales::stock_on_hand(db, till.o.jkt.id, shirt.variant)
            .await
            .unwrap(),
        Qty::ZERO
    );
    let moves = sales::document_movements(db, *sale.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(
        moves.iter().map(|m| m.movement.kind).collect::<Vec<_>>(),
        [StockMovementKind::Sale, StockMovementKind::SaleVoid]
    );
    // One audit row, one outbox row for the void and three for the balance entries.
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 4));
    assert_eq!(sales::get_sale(db, sale.id).await.unwrap().unwrap(), voided);

    // A cash void with an "other" reason, in the same session.
    let paid = cash_sale(&till, &[item(7_000, 1)], NOV16 + 10);
    let (_, paid) = post(db, &paid).await.unwrap();
    let mut v = void_of(
        &till,
        &paid,
        till.session,
        RefundMethod::Cash,
        None,
        NOV16 + 20,
    );
    v.reason = VoidReason::Other("pelanggan batal".into());
    let voided = void(db, &v).await.unwrap();
    let record = voided.void.unwrap();
    assert_eq!(record.cash, rp(7_000));
    assert_eq!(record.reason, VoidReason::Other("pelanggan batal".into()));
}

async fn balance_of(db: &Db, till: &Till) -> Money {
    cust::get_balance(db, till.customer, None, 1)
        .await
        .unwrap()
        .unwrap()
        .balance
}

async fn register_two(db: &Db, till: &Till) -> RegisterId {
    let r = Register {
        id: RegisterId::generate(),
        branch: till.o.jkt.id,
        code: "R2".into(),
        name: "Back".into(),
        default_location: Some(till.o.location.id),
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_register(&mut tx, &r, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    r.id
}

#[allow(clippy::too_many_lines)] // one row per refusal
async fn post_void_refusals(db: &Db) {
    let till = till(db).await;
    let (_, stored) = post(db, &cash_sale(&till, &[item(5_000, 2)], NOV16))
        .await
        .unwrap();
    let v = void_of(
        &till,
        &stored,
        till.session,
        RefundMethod::Cash,
        None,
        NOV16 + 1,
    );
    // A plan that does not match the sale.
    let mut bad = v.clone();
    bad.plan.refund.cash_exact = rp(1);
    let err = void(db, &bad).await.unwrap_err();
    assert!(
        matches!(err, SaleDbError::Invalid { field: "plan", .. }),
        "{err}"
    );
    let mut bad = v.clone();
    bad.tenders[0].amount = rp(9_000);
    let err = void(db, &bad).await.unwrap_err();
    assert!(
        matches!(
            err,
            SaleDbError::Invalid {
                field: "tenders",
                ..
            }
        ),
        "{err}"
    );
    let mut bad = v.clone();
    bad.tenders[0].kind = TenderKind::Card;
    let err = void(db, &bad).await.unwrap_err();
    assert!(
        matches!(
            err,
            SaleDbError::Invalid {
                field: "tenders",
                ..
            }
        ),
        "{err}"
    );
    // Unknown sale.
    let mut ghost = v.clone();
    ghost.sale = SaleId::generate();
    assert!(matches!(
        void(db, &ghost).await,
        Err(SaleDbError::NotFound("sale"))
    ));
    // Void, then a second void → conflict (already voided).
    void(db, &v).await.unwrap();
    let err = void(db, &v).await.unwrap_err();
    assert!(matches!(err, SaleDbError::Rule(SaleError::AlreadyVoided)));
    assert_eq!(err.code(), Some("conflict"));
    // Balance entries for a sale without a customer.
    let (_, walk_in) = post(db, &cash_sale(&till, &[item(5_000, 1)], NOV16))
        .await
        .unwrap();
    let mut with_balance = void_of(
        &till,
        &walk_in,
        till.session,
        RefundMethod::Cash,
        None,
        NOV16,
    );
    with_balance.plan.balance_entries = vec![
        sultan_core::customer::BalanceEntry::new(
            sultan_core::customer::BalanceEntryKind::Refund,
            rp(1),
        )
        .unwrap(),
    ];
    let err = void(db, &with_balance).await.unwrap_err();
    assert!(
        matches!(
            err,
            SaleDbError::Invalid {
                field: "customer",
                ..
            }
        ),
        "{err}"
    );
    // An imported old-Sultan sale is read-only.
    raw(
        db,
        &format!(
            "UPDATE sale SET legacy_number = 'OLD-1' WHERE number = '{}'",
            walk_in.header.number
        ),
    )
    .await
    .unwrap();
    let legacy = void_of(
        &till,
        &walk_in,
        till.session,
        RefundMethod::Cash,
        None,
        NOV16,
    );
    let err = void(db, &legacy).await.unwrap_err();
    assert!(matches!(
        err,
        SaleDbError::Rule(SaleError::InvalidField { field: "sale", .. })
    ));
    assert_eq!(
        sales::get_sale_by_number(db, "OLD-1")
            .await
            .unwrap()
            .unwrap()
            .header
            .legacy_number
            .as_deref(),
        Some("OLD-1")
    );
}

async fn stock_ledger_is_append_only(db: &Db) {
    let till = till(db).await;
    post(db, &cash_sale(&till, &[item(1_000, 1)], NOV16))
        .await
        .unwrap();
    assert_eq!(count(db, "stock_movement").await, 1);
    assert!(
        raw(db, "UPDATE stock_movement SET base_qty = -2000")
            .await
            .is_err()
    );
    assert!(raw(db, "DELETE FROM stock_movement").await.is_err());
    // The sign checks.
    let id = Uuid::now_v7().simple().to_string();
    let sql = format!(
        "INSERT INTO stock_movement (id, branch_id, location_id, variant_id, kind, base_qty, \
         doc_kind, doc_id, occurred_at) VALUES (X'{id}', X'{id}', NULL, X'{id}', 'sale', 1000, \
         'sale', X'{id}', 0)"
    );
    assert!(raw(db, &sql).await.is_err());
    assert_eq!(count(db, "stock_movement").await, 1);
}

async fn balance_refund_kind(db: &Db) {
    let till = till(db).await;
    let mut tx = db.begin().await.unwrap();
    let refund = BalanceEntry::new(till.customer, BalanceEntryKind::Refund, rp(700), t(1));
    let (written, balance) = cust::append_balance_entry(&mut tx, &refund, t(1))
        .await
        .unwrap();
    assert_eq!((written, balance), (cust::Written::Created, rp(700)));
    let negative = BalanceEntry::new(till.customer, BalanceEntryKind::Refund, rp(-1), t(1));
    let err = cust::append_balance_entry(&mut tx, &negative, t(1))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        CustomerDbError::Invalid {
            field: "amount",
            ..
        }
    ));
    tx.commit().await.unwrap();
    let stored = cust::get_balance(db, till.customer, None, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.entries[0].kind, BalanceEntryKind::Refund);
    // The CHECK refuses a negative refund written directly, and the ledger stays append-only.
    let id = Uuid::now_v7().simple().to_string();
    let c = till.customer.simple().to_string();
    let sql = format!(
        "INSERT INTO customer_balance_entry (id, customer_id, kind, amount, occurred_at) \
         VALUES (X'{id}', X'{c}', 'refund', -5, 0)"
    );
    assert!(raw(db, &sql).await.is_err());
    assert!(raw(db, "DELETE FROM customer_balance_entry").await.is_err());
    assert!(
        raw(db, "UPDATE customer_balance_entry SET amount = 1")
            .await
            .is_err()
    );
}

/// `(has sale.reprint, has tender.manage)` of a role.
async fn sales_grants(db: &Db, role: sultan_core::ids::RoleId) -> (bool, bool) {
    let role = roles::get_role(db, role).await.unwrap().unwrap();
    let has = |p| role.grants.iter().any(|g| g.permission() == p);
    (has(Permission::SaleReprint), has(Permission::TenderManage))
}

async fn sales_permissions_on_a_fresh_database(db: &Db) {
    for now in [t(1), t(2)] {
        seed::seed_defaults(db, now).await.unwrap();
        assert_eq!(sales_grants(db, OWNER_ROLE).await, (true, true));
        assert_eq!(sales_grants(db, MANAGER_ROLE).await, (true, false));
        assert_eq!(sales_grants(db, CASHIER_ROLE).await, (false, false));
    }
}

const SALES_MIGRATION: i64 = 20_261_117_000_001;

async fn old_migrator(backend: &str) -> sqlx::migrate::Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut migrator = sqlx::migrate::Migrator::new(dir).await.unwrap();
    let old: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version < SALES_MIGRATION)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(old);
    migrator
}

/// An install from before 2.6: the migrations add `sale.reprint` (Owner, Manager) and
/// `tender.manage` (Owner), and the balance ledger keeps its entries and triggers while learning
/// the kind `refund`.
async fn upgrade_adds_sales(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    raw(
        db,
        "DELETE FROM role_permission WHERE perm IN ('sale.reprint', 'tender.manage')",
    )
    .await
    .unwrap();
    assert_eq!(sales_grants(db, MANAGER_ROLE).await, (false, false));
    let o = crate::common::org(db).await;
    let c = customer(db, &o, "OLD00001").await;
    let mut tx = db.begin().await.unwrap();
    cust::append_balance_entry(
        &mut tx,
        &BalanceEntry::new(c, BalanceEntryKind::ChangeSaved, rp(300), t(2)),
        t(2),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    db.migrate().await.unwrap();
    assert_eq!(sales_grants(db, OWNER_ROLE).await, (true, true));
    assert_eq!(sales_grants(db, MANAGER_ROLE).await, (true, false));
    assert_eq!(sales_grants(db, CASHIER_ROLE).await, (false, false));
    assert_eq!(cust::ledger_balance(db, c).await.unwrap(), rp(300));
    let mut tx = db.begin().await.unwrap();
    cust::append_balance_entry(
        &mut tx,
        &BalanceEntry::new(c, BalanceEntryKind::Refund, rp(200), t(3)),
        t(3),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(cust::ledger_balance(db, c).await.unwrap(), rp(500));
    assert!(raw(db, "DELETE FROM customer_balance_entry").await.is_err());
    assert_eq!(count(db, "tender_type").await, 4);
}

#[tokio::test]
async fn sqlite_upgrade_adds_sales() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let Db::Sqlite(pool) = &db else {
        unreachable!()
    };
    old_migrator("sqlite").await.run(pool).await.unwrap();
    upgrade_adds_sales(&db).await;
}

#[tokio::test]
async fn mysql_upgrade_adds_sales() {
    let Some(test_db) = MySqlTestDb::create_empty().await else {
        return;
    };
    let Db::MySql(pool) = &test_db.db else {
        unreachable!()
    };
    old_migrator("mysql").await.run(pool).await.unwrap();
    upgrade_adds_sales(&test_db.db).await;
    test_db.drop().await;
}
