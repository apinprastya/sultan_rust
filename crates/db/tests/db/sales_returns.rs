//! Returns and exchanges, held carts, lists, the purchase history and the session figures
//! (spec 2.6, phase S2).

use serde_json::json;
use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, HeldCartId, ReturnId, SaleId};
use sultan_core::money::Money;
use sultan_core::sale::{
    Discount, RefundMethod, ReturnDisposition, SaleError, SaleStatus, SessionEvent,
    StockMovementKind, TenderKey, TenderKind, net_exchange, session_report,
};
use sultan_db::Db;
use sultan_db::audit::{self, AuditFilter};
use sultan_db::sales::{
    self, CASH_TENDER_TYPE, DocFilter, HeldCart, HistoryCursor, HistoryDoc, SaleDbError, Written,
};
use uuid::Uuid;

use crate::catalog_fixtures::{actor, trail};
use crate::common::{count, db_tests, raw, t};
use crate::sales_fixtures::{
    DAY, NOV16, Pay, cash_sale, customer, item, open_session, post, post_return, q, return_of, rp,
    sale_at, till, void, void_of,
};

db_tests!(
    partial_returns_sum_to_the_line,
    exchange_posts_both_documents,
    return_refusals,
    no_receipt_return_and_balance_refund,
    held_carts_hold_take_and_delete,
    concurrent_take_has_one_winner,
    lists_filter_and_page,
    purchase_history_includes_merged_customers,
    session_events_feed_the_report,
);

#[allow(clippy::too_many_lines)] // one story: two partial returns, refusals, a write-off
async fn partial_returns_sum_to_the_line(db: &Db) {
    let till = till(db).await;
    // §15 case 13: 3 units of a discounted line: 3 × 10 000 − 1 000 cart discount = 29 000 (the
    // second line takes its share too).
    let shirt = item(10_000, 3);
    let pen = item(1_000, 1);
    let sale = cash_sale_discounted(&till, &[shirt, pen]);
    let (_, sold) = post(db, &sale).await.unwrap();
    let line_total = sold.lines[0].line.totals.total;
    // Return 1, then the other 2: the refunds sum exactly to the line.
    let first = return_of(
        &till,
        &sold,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 1_000,
    );
    let before = trail(db).await;
    let (written, r1) = post_return(db, &first).await.unwrap();
    assert_eq!(written, Written::Created);
    assert_eq!(trail(db).await, (before.0 + 1, before.1 + 1));
    assert_eq!(r1.header.number, "JKT01-R1-RT-261116-0001");
    assert_eq!(r1.header.reason.as_deref(), Some("rusak"));
    assert_eq!(r1.header.status, SaleStatus::Posted);
    assert_eq!(r1.header.sale, Some(sold.header.id));
    assert_eq!(r1.lines[0].line.sale_line, Some(1));
    assert_eq!(r1.tenders, first.tenders);
    // Replay → stored; another body → conflict.
    let (again, stored) = post_return(db, &first).await.unwrap();
    assert_eq!((again, &stored), (Written::Unchanged, &r1));
    let mut other = first.clone();
    other.request_hash = [7; 32];
    assert!(matches!(
        post_return(db, &other).await,
        Err(SaleDbError::Conflict("return", _))
    ));

    let sold = sales::get_sale(db, sold.header.id).await.unwrap().unwrap();
    assert_eq!(sold.header.return_count, 1);
    assert_eq!(sold.lines[0].returned.qty, q(1));
    let second = return_of(
        &till,
        &sold,
        &[(1, 2, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 2_000,
    );
    let (_, r2) = post_return(db, &second).await.unwrap();
    assert_eq!(r2.header.number, "JKT01-R1-RT-261116-0002");
    assert_eq!(
        r1.header.refund.saturating_add(r2.header.refund),
        line_total
    );
    let sold = sales::get_sale(db, sold.header.id).await.unwrap().unwrap();
    assert_eq!(sold.lines[0].returned.qty, q(3));
    assert_eq!(sold.lines[0].returned.refunded, line_total);
    // Stock: 3 out, 3 back.
    assert_eq!(
        sales::stock_on_hand(db, till.o.jkt.id, shirt.variant)
            .await
            .unwrap(),
        Qty::ZERO
    );
    // §15 case 14: nothing left on line 1.
    let mut third = second.clone();
    third.id = ReturnId::generate();
    third.lines[0].qty = q(1);
    let err = post_return(db, &third).await.unwrap_err();
    assert!(matches!(
        err,
        SaleDbError::Rule(SaleError::ReturnQtyExceeded { max }) if max == Qty::ZERO
    ));
    assert_eq!(err.code(), Some("return_qty_exceeded"));
    // §7: a sale with returns cannot be voided.
    let v = void_of(
        &till,
        &sold,
        till.session,
        RefundMethod::Cash,
        None,
        NOV16 + 3_000,
    );
    let err = void(db, &v).await.unwrap_err();
    assert!(matches!(err, SaleDbError::Rule(SaleError::HasReturns)));
    // Return lines read back, with the write-off reason.
    let mut write_off = return_of(
        &till,
        &sold,
        &[(
            2,
            1,
            ReturnDisposition::WriteOff {
                reason: "  pecah ".into(),
            },
        )],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 4_000,
    );
    write_off.reason = None;
    let (_, r3) = post_return(db, &write_off).await.unwrap();
    assert_eq!(
        r3.lines[0].line.disposition,
        ReturnDisposition::WriteOff {
            reason: "pecah".into()
        }
    );
    assert_eq!(r3.header.reason, None);
    // A write-off moves the item in and out again.
    let moves = sales::document_movements(db, *write_off.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(
        moves.iter().map(|m| m.movement.kind).collect::<Vec<_>>(),
        [StockMovementKind::Return, StockMovementKind::WriteOff]
    );
    assert_eq!(
        sales::stock_on_hand(db, till.o.jkt.id, pen.variant)
            .await
            .unwrap(),
        q(-1)
    );
    assert!(
        sales::get_return(db, ReturnId::generate())
            .await
            .unwrap()
            .is_none()
    );
}

fn cash_sale_discounted(
    till: &crate::sales_fixtures::Till,
    items: &[crate::sales_fixtures::Item],
) -> sales::NewSale {
    let (_, totals) = crate::sales_fixtures::lines(items, Some(Discount::Amount(rp(1_000))));
    sale_at(
        till,
        items,
        Some(Discount::Amount(rp(1_000))),
        None,
        &[Pay::Cash(totals.total.minor())],
        Money::ZERO,
        NOV16,
    )
}

#[allow(clippy::too_many_lines)] // one story, step by step
async fn exchange_posts_both_documents(db: &Db) {
    let till = till(db).await;
    // §15 case 16: return 50 000, new sale 65 000 → the customer pays 15 000.
    let (_, sold) = post(db, &cash_sale(&till, &[item(50_000, 1)], NOV16))
        .await
        .unwrap();
    let ex = net_exchange(rp(50_000), rp(65_000)).unwrap();
    let new_sale = sale_at(
        &till,
        &[item(65_000, 1)],
        None,
        None,
        &[
            Pay::Exchange(ex.exchange_tender.minor()),
            Pay::Cash(ex.customer_pays.minor()),
        ],
        Money::ZERO,
        NOV16 + DAY,
    );
    let r = return_of(
        &till,
        &sold,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        Some((new_sale.clone(), ex.exchange_tender)),
        NOV16 + DAY,
    );
    let (_, stored) = post_return(db, &r).await.unwrap();
    assert_eq!(stored.header.exchange_sale, Some(new_sale.id));
    assert_eq!(stored.tenders.len(), 1);
    assert_eq!(stored.tenders[0].kind, TenderKind::Exchange);
    let new = sales::get_sale(db, new_sale.id).await.unwrap().unwrap();
    assert_eq!(new.header.number, "JKT01-R1-261117-0001");
    assert_eq!(new.tenders[0].kind, TenderKind::Exchange);
    assert_eq!(new.tenders[0].tender_type, None);

    // New sale 40 000 → 10 000 refunded in cash.
    let (_, sold) = post(db, &cash_sale(&till, &[item(50_000, 1)], NOV16))
        .await
        .unwrap();
    let ex = net_exchange(rp(50_000), rp(40_000)).unwrap();
    assert_eq!(ex.refund, rp(10_000));
    let cheaper = sale_at(
        &till,
        &[item(40_000, 1)],
        None,
        None,
        &[Pay::Exchange(ex.exchange_tender.minor())],
        Money::ZERO,
        NOV16 + DAY,
    );
    let r = return_of(
        &till,
        &sold,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        Some((cheaper, ex.exchange_tender)),
        NOV16 + DAY,
    );
    let (_, stored) = post_return(db, &r).await.unwrap();
    let cash: Money = stored
        .tenders
        .iter()
        .filter(|t| t.kind == TenderKind::Cash)
        .fold(Money::ZERO, |a, t| a.saturating_add(t.amount));
    assert_eq!(cash, rp(10_000));

    // An exchange tender that does not pay the new sale is refused; so is an exchange tender in a
    // plain sale.
    let (_, sold) = post(db, &cash_sale(&till, &[item(50_000, 1)], NOV16))
        .await
        .unwrap();
    let mut bad = return_of(
        &till,
        &sold,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + DAY,
    );
    bad.tenders[0] = sales::RefundTender {
        tender_type: None,
        kind: TenderKind::Exchange,
        amount: rp(50_000),
    };
    let err = post_return(db, &bad).await.unwrap_err();
    assert!(
        matches!(
            err,
            SaleDbError::Invalid {
                field: "exchange",
                ..
            }
        ),
        "{err}"
    );
    let plain = sale_at(
        &till,
        &[item(40_000, 1)],
        None,
        None,
        &[Pay::Exchange(40_000)],
        Money::ZERO,
        NOV16,
    );
    let err = post(db, &plain).await.unwrap_err();
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
}

#[allow(clippy::too_many_lines)] // one row per refusal
async fn return_refusals(db: &Db) {
    let till = till(db).await;
    let (_, sold) = post(db, &cash_sale(&till, &[item(8_000, 2)], NOV16))
        .await
        .unwrap();
    let ok = return_of(
        &till,
        &sold,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 1,
    );
    let variant = |f: &dyn Fn(&mut sales::NewReturn)| {
        let mut r = ok.clone();
        r.id = ReturnId::generate();
        f(&mut r);
        r
    };
    let cases: Vec<(sales::NewReturn, &str)> = vec![
        (variant(&|r| r.lines.clear()), "lines"),
        (variant(&|r| r.lines[0].sale_line = Some(9)), "lines"),
        (variant(&|r| r.lines[0].sale_line = None), "lines"),
        (
            variant(&|r| r.lines[0].variant = sultan_core::ids::VariantId::generate()),
            "lines",
        ),
        (
            variant(&|r| {
                r.lines[0].refund = rp(1);
                r.tenders[0].amount = rp(1);
            }),
            "refund",
        ),
        (variant(&|r| r.lines[0].refund = rp(-1)), "refund"),
        (variant(&|r| r.tenders[0].amount = rp(1)), "tenders"),
        (
            variant(&|r| r.tenders[0].kind = TenderKind::Card),
            "tenders",
        ),
        (variant(&|r| r.reason = Some("x".repeat(201))), "reason"),
        (
            variant(&|r| r.movements[0].kind = StockMovementKind::Sale),
            "movements",
        ),
        (
            variant(&|r| {
                r.balance_entries = vec![
                    sultan_core::customer::BalanceEntry::new(
                        sultan_core::customer::BalanceEntryKind::ChangeSaved,
                        rp(1),
                    )
                    .unwrap(),
                ];
            }),
            "balance_entries",
        ),
        (
            variant(&|r| {
                r.balance_entries = vec![
                    sultan_core::customer::BalanceEntry::new(
                        sultan_core::customer::BalanceEntryKind::Refund,
                        rp(1),
                    )
                    .unwrap(),
                ];
            }),
            "customer",
        ),
        (
            // Spec 2.6 §19 item 34: a balance payment goes back to the sale's customer only.
            variant(&|r| {
                r.customer = Some(till.customer);
                r.balance_entries = vec![
                    sultan_core::customer::BalanceEntry::new(
                        sultan_core::customer::BalanceEntryKind::PaymentReversed,
                        rp(1),
                    )
                    .unwrap(),
                ];
            }),
            "customer",
        ),
        (
            variant(&|r| {
                r.sale = None;
                r.lines[0].sale_line = None;
                r.customer = Some(till.customer);
                r.balance_entries = vec![
                    sultan_core::customer::BalanceEntry::new(
                        sultan_core::customer::BalanceEntryKind::PaymentReversed,
                        rp(1),
                    )
                    .unwrap(),
                ];
            }),
            "balance_entries",
        ),
    ];
    for (r, field) in cases {
        let err = post_return(db, &r).await.unwrap_err();
        assert!(
            matches!(&err, SaleDbError::Invalid { field: f, .. } if *f == field),
            "{field}: {err:?}"
        );
    }
    let err = post_return(db, &variant(&|r| r.lines[0].qty = Qty::ZERO))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("invalid_qty"));
    let err = post_return(
        db,
        &variant(&|r| {
            r.lines[0].disposition = ReturnDisposition::WriteOff { reason: " ".into() };
        }),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        err,
        SaleDbError::Rule(SaleError::InvalidField {
            field: "reason",
            ..
        })
    ));
    let err = post_return(db, &variant(&|r| r.sale = Some(SaleId::generate())))
        .await
        .unwrap_err();
    assert!(matches!(err, SaleDbError::NotFound("sale")));
    // An imported sale is read-only; a voided sale cannot be returned.
    raw(
        db,
        &format!(
            "UPDATE sale SET legacy_number = 'OLD-9' WHERE number = '{}'",
            sold.header.number
        ),
    )
    .await
    .unwrap();
    let err = post_return(db, &variant(&|_| {})).await.unwrap_err();
    assert!(matches!(
        err,
        SaleDbError::Rule(SaleError::InvalidField { field: "sale", .. })
    ));
    let (_, other) = post(db, &cash_sale(&till, &[item(8_000, 1)], NOV16))
        .await
        .unwrap();
    void(
        db,
        &void_of(
            &till,
            &other,
            till.session,
            RefundMethod::Cash,
            None,
            NOV16 + 2,
        ),
    )
    .await
    .unwrap();
    let r = return_of(
        &till,
        &other,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 3,
    );
    let err = post_return(db, &r).await.unwrap_err();
    assert!(matches!(err, SaleDbError::Rule(SaleError::AlreadyVoided)));
    assert_eq!(count(db, "sale_return").await, 0);
}

async fn no_receipt_return_and_balance_refund(db: &Db) {
    let till = till(db).await;
    let (_, sold) = post(db, &cash_sale(&till, &[item(20_000, 1)], NOV16))
        .await
        .unwrap();
    // A no-receipt return of the same item, refunded to the attached customer's balance.
    let mut r = return_of(
        &till,
        &sold,
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Balance,
        Some(Money::ZERO),
        Money::ZERO,
        None,
        NOV16 + 1,
    );
    r.sale = None;
    r.lines[0].sale_line = None;
    r.customer = Some(till.customer);
    let (_, stored) = post_return(db, &r).await.unwrap();
    assert_eq!(stored.header.sale, None);
    assert_eq!(stored.tenders[0].kind, TenderKind::CustomerBalance);
    let balance = sultan_db::customers::get_balance(db, till.customer, None, 0)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(balance.balance, rp(20_000));
    assert_eq!(
        balance.entries[0].kind,
        sultan_db::customers::BalanceEntryKind::Refund
    );
    // The sale was not touched.
    let sold = sales::get_sale(db, sold.header.id).await.unwrap().unwrap();
    assert_eq!(sold.header.return_count, 0);
}

fn held(till: &crate::sales_fixtures::Till, at: i64, note: Option<&str>) -> HeldCart {
    HeldCart {
        id: HeldCartId::generate(),
        branch: till.o.jkt.id,
        register: till.o.register.id,
        user: till.o.cashier.id,
        customer: Some(till.customer),
        note: note.map(str::to_owned),
        payload: json!({ "lines": [{ "sku": "TEH", "qty": 2000 }], "discount": null }),
        item_count: 2,
        total: rp(10_000),
        held_at: t(at),
        taken_by: None,
        taken_register: None,
        taken_at: None,
    }
}

async fn put_cart(db: &Db, cart: &HeldCart) -> Result<Written, SaleDbError> {
    let mut tx = db.begin().await.unwrap();
    let r = sales::put_held_cart(&mut tx, cart).await;
    if r.is_ok() {
        tx.commit().await.unwrap();
    }
    r
}

async fn take(
    db: &Db,
    id: HeldCartId,
    till: &crate::sales_fixtures::Till,
) -> Result<HeldCart, SaleDbError> {
    let mut tx = db.begin().await.unwrap();
    let r = sales::take_held_cart(
        &mut tx,
        id,
        till.o.manager.id,
        till.o.register.id,
        t(NOV16 + 9),
    )
    .await;
    if r.is_ok() {
        tx.commit().await.unwrap();
    }
    r
}

async fn held_carts_hold_take_and_delete(db: &Db) {
    let till = till(db).await;
    let before = trail(db).await;
    let a = held(&till, NOV16, Some("  ambil nanti "));
    let b = held(&till, NOV16 + 1, None);
    assert_eq!(put_cart(db, &a).await.unwrap(), Written::Created);
    assert_eq!(put_cart(db, &b).await.unwrap(), Written::Created);
    // Held carts are branch-local: no audit, no outbox.
    assert_eq!(trail(db).await, before);
    assert_eq!(put_cart(db, &a).await.unwrap(), Written::Unchanged);
    let mut a2 = a.clone();
    a2.item_count = 3;
    assert_eq!(put_cart(db, &a2).await.unwrap(), Written::Updated);
    let list = sales::list_held_carts(db, till.o.jkt.id).await.unwrap();
    assert_eq!(list.iter().map(|c| c.id).collect::<Vec<_>>(), [b.id, a.id]);
    assert_eq!(list[1].note.as_deref(), Some("ambil nanti"));
    assert_eq!(list[1].payload, a.payload);
    assert!(
        sales::list_held_carts(db, BranchId::generate())
            .await
            .unwrap()
            .is_empty()
    );
    let err = put_cart(db, &held(&till, NOV16, Some(&"x".repeat(201))))
        .await
        .unwrap_err();
    assert!(matches!(err, SaleDbError::Invalid { field: "note", .. }));

    // Take: it leaves the list; a second take → cart_taken; holding it again too.
    let taken = take(db, a.id, &till).await.unwrap();
    assert_eq!(taken.taken_by, Some(till.o.manager.id));
    assert_eq!(taken.taken_at, Some(t(NOV16 + 9)));
    assert_eq!(taken.payload, a.payload);
    let err = take(db, a.id, &till).await.unwrap_err();
    assert!(matches!(err, SaleDbError::Rule(SaleError::CartTaken)));
    assert_eq!(err.code(), Some("cart_taken"));
    assert!(matches!(
        put_cart(db, &a2).await,
        Err(SaleDbError::Rule(SaleError::CartTaken))
    ));
    assert!(matches!(
        take(db, HeldCartId::generate(), &till).await,
        Err(SaleDbError::NotFound("held cart"))
    ));
    assert_eq!(
        sales::list_held_carts(db, till.o.jkt.id)
            .await
            .unwrap()
            .len(),
        1
    );

    // Delete: audited with the cart; a taken or unknown cart cannot be deleted.
    let mut tx = db.begin().await.unwrap();
    let deleted = sales::delete_held_cart(&mut tx, b.id, &actor(&till.o), t(NOV16 + 10))
        .await
        .unwrap();
    assert_eq!(deleted.id, b.id);
    assert!(matches!(
        sales::delete_held_cart(&mut tx, a.id, &actor(&till.o), t(1)).await,
        Err(SaleDbError::Rule(SaleError::CartTaken))
    ));
    assert!(matches!(
        sales::delete_held_cart(&mut tx, HeldCartId::generate(), &actor(&till.o), t(1)).await,
        Err(SaleDbError::NotFound("held cart"))
    ));
    tx.commit().await.unwrap();
    assert!(sales::get_held_cart(db, b.id).await.unwrap().is_none());
    let entries = audit::query(
        db,
        &AuditFilter {
            action: Some("held_cart.delete".into()),
            ..AuditFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].before.as_ref().unwrap()["item_count"], 2);
    assert!(
        sales::list_held_carts(db, till.o.jkt.id)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn concurrent_take_has_one_winner(db: &Db) {
    let till = till(db).await;
    let cart = held(&till, NOV16, None);
    put_cart(db, &cart).await.unwrap();
    // §15 case 11: two registers take it at once.
    let tasks: Vec<_> = (0..2)
        .map(|_| {
            let db = db.clone();
            let id = cart.id;
            let user = till.o.cashier.id;
            let register = till.o.register.id;
            tokio::spawn(async move {
                let mut tx = db.begin().await?;
                let r = sales::take_held_cart(&mut tx, id, user, register, t(NOV16)).await;
                if r.is_ok() {
                    tx.commit().await?;
                }
                r
            })
        })
        .collect();
    let mut won = 0;
    let mut taken = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => won += 1,
            Err(SaleDbError::Rule(SaleError::CartTaken)) => taken += 1,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!((won, taken), (1, 1));
}

#[allow(clippy::too_many_lines)] // one story, step by step
async fn lists_filter_and_page(db: &Db) {
    let till = till(db).await;
    let mut ids = Vec::new();
    for i in 0..3 {
        let mut s = cash_sale(&till, &[item(1_000, 1)], NOV16 + i * DAY);
        if i == 1 {
            s = sale_at(
                &till,
                &[item(1_000, 1)],
                None,
                Some((till.customer, Money::ZERO)),
                &[Pay::Cash(1_000)],
                Money::ZERO,
                NOV16 + DAY,
            );
        }
        ids.push(post(db, &s).await.unwrap().1);
    }
    let mut newest_first: Vec<SaleId> = ids.iter().map(|s| s.header.id).collect();
    newest_first.sort_by(|a, b| b.cmp(a));
    let all = sales::list_sales(db, &DocFilter::default()).await.unwrap();
    assert_eq!(all.iter().map(|h| h.id).collect::<Vec<_>>(), newest_first);
    let page = sales::list_sales(
        db,
        &DocFilter {
            limit: 2,
            ..DocFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(page.len(), 2);
    let next = sales::list_sales(
        db,
        &DocFilter {
            before: Some(*page[1].id.as_uuid()),
            ..DocFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(
        next.iter().map(|h| h.id).collect::<Vec<_>>(),
        [newest_first[2]]
    );
    let by = |f: DocFilter| async move { sales::list_sales(db, &f).await.unwrap().len() };
    assert_eq!(
        by(DocFilter {
            branch: Some(till.o.jkt.id),
            ..DocFilter::default()
        })
        .await,
        3
    );
    assert_eq!(
        by(DocFilter {
            branch: Some(till.o.bdg.id),
            ..DocFilter::default()
        })
        .await,
        0
    );
    assert_eq!(
        by(DocFilter {
            register: Some(till.o.register.id),
            ..DocFilter::default()
        })
        .await,
        3
    );
    assert_eq!(
        by(DocFilter {
            cashier_session: Some(till.session),
            ..DocFilter::default()
        })
        .await,
        3
    );
    assert_eq!(
        by(DocFilter {
            customer: Some(till.customer),
            ..DocFilter::default()
        })
        .await,
        1
    );
    assert_eq!(
        by(DocFilter {
            from: Some(t(NOV16 + DAY)),
            until: Some(t(NOV16 + 2 * DAY)),
            ..DocFilter::default()
        })
        .await,
        1
    );
    assert_eq!(
        by(DocFilter {
            number: Some(ids[0].header.number.clone()),
            ..DocFilter::default()
        })
        .await,
        1
    );
    // Returns list the same way.
    let r = return_of(
        &till,
        &ids[1],
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 2 * DAY,
    );
    let (_, stored) = post_return(db, &r).await.unwrap();
    let returns = sales::list_returns(db, &DocFilter::default())
        .await
        .unwrap();
    assert_eq!(returns, std::slice::from_ref(&stored.header));
    let returns = sales::list_returns(
        db,
        &DocFilter {
            number: Some("nope".into()),
            customer: Some(till.customer),
            ..DocFilter::default()
        },
    )
    .await
    .unwrap();
    assert!(returns.is_empty());
}

#[allow(clippy::too_many_lines)] // one story, step by step
async fn purchase_history_includes_merged_customers(db: &Db) {
    let till = till(db).await;
    let b = customer(db, &till.o, "B2222222").await;
    let walk_in = cash_sale(&till, &[item(1_000, 1)], NOV16);
    post(db, &walk_in).await.unwrap();
    let mut docs = Vec::new();
    for (i, who) in [till.customer, b, till.customer].into_iter().enumerate() {
        let at = NOV16 - (100 - 40 * i64::try_from(i).unwrap()) * DAY;
        let s = sale_at(
            &till,
            &[item(2_000, 1)],
            None,
            Some((who, Money::ZERO)),
            &[Pay::Cash(2_000)],
            Money::ZERO,
            at,
        );
        docs.push(post(db, &s).await.unwrap().1);
    }
    // A return of the newest one, a voided sale that stays out.
    let r = return_of(
        &till,
        &docs[2],
        &[(1, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16,
    );
    let (_, returned) = post_return(db, &r).await.unwrap();
    let voided = sale_at(
        &till,
        &[item(3_000, 1)],
        None,
        Some((till.customer, Money::ZERO)),
        &[Pay::Cash(3_000)],
        Money::ZERO,
        NOV16,
    );
    let (_, voided) = post(db, &voided).await.unwrap();
    void(
        db,
        &void_of(
            &till,
            &voided,
            till.session,
            RefundMethod::Cash,
            None,
            NOV16,
        ),
    )
    .await
    .unwrap();

    // B is merged into A: A's history has B's sale too.
    let mut tx = db.begin().await.unwrap();
    sultan_db::customers::merge_customers(&mut tx, till.customer, b, &actor(&till.o), t(NOV16))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let history = sales::purchase_history(db, till.customer, None, 0)
        .await
        .unwrap();
    let got: Vec<(HistoryDoc, Uuid)> = history.iter().map(|e| (e.doc, e.id)).collect();
    assert_eq!(
        got,
        [
            (HistoryDoc::Return, *returned.header.id.as_uuid()),
            (HistoryDoc::Sale, *docs[2].header.id.as_uuid()),
            (HistoryDoc::Sale, *docs[1].header.id.as_uuid()),
            (HistoryDoc::Sale, *docs[0].header.id.as_uuid()),
        ]
    );
    assert_eq!(history[2].customer, b);
    assert_eq!(history[1].amount, rp(2_000));
    assert_eq!(history[0].amount, rp(2_000));
    // Paged.
    let first = sales::purchase_history(db, till.customer, None, 2)
        .await
        .unwrap();
    let second =
        sales::purchase_history(db, till.customer, Some(HistoryCursor::from(&first[1])), 2)
            .await
            .unwrap();
    assert_eq!(
        first
            .iter()
            .chain(&second)
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        history.iter().map(|e| e.id).collect::<Vec<_>>()
    );
    // The till: the last receipts within 90 days (the 100-day-old sale is out).
    let recent = sales::recent_receipts(db, till.customer, t(NOV16 - 90 * DAY), 0)
        .await
        .unwrap();
    assert_eq!(
        recent.iter().map(|h| h.id).collect::<Vec<_>>(),
        [docs[2].header.id, docs[1].header.id]
    );
    assert!(
        sales::purchase_history(db, Uuid::now_v7(), None, 0)
            .await
            .unwrap()
            .is_empty()
    );
}

async fn session_events_feed_the_report(db: &Db) {
    let till = till(db).await;
    // §15 case 5: QRIS-like card 50 000 + cash 20 000 on 63 500 → change 6 500.
    let cart = [item(63_500, 1)];
    let card_cash = sale_at(
        &till,
        &cart,
        None,
        None,
        &[Pay::Card(50_000), Pay::Cash(20_000)],
        Money::ZERO,
        NOV16,
    );
    post(db, &card_cash).await.unwrap();
    // A discounted sale with saved change for the customer.
    let saving = sale_at(
        &till,
        &[item(10_000, 1), item(5_000, 1)],
        Some(Discount::Amount(rp(1_500))),
        Some((till.customer, Money::ZERO)),
        &[Pay::Cash(15_000)],
        rp(1_500),
        NOV16 + 1,
    );
    let (_, saving) = post(db, &saving).await.unwrap();
    // A cash return of the 5 000 item (its share of the discount comes off).
    let r = return_of(
        &till,
        &saving,
        &[(2, 1, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 2,
    );
    let (_, ret) = post_return(db, &r).await.unwrap();
    // A post-void in this session of a sale from another session.
    let other = open_session(db, till.o.manager.id, register_two(db, &till).await, NOV16).await;
    let mut elsewhere = cash_sale(&till, &[item(4_000, 1)], NOV16 + 3);
    elsewhere.cashier_session = other;
    let (_, elsewhere) = post(db, &elsewhere).await.unwrap();
    void(
        db,
        &void_of(
            &till,
            &elsewhere,
            till.session,
            RefundMethod::Cash,
            None,
            NOV16 + 4,
        ),
    )
    .await
    .unwrap();

    let events = sales::session_events(db, till.session).await.unwrap();
    let report = session_report(rp(100_000), &events).unwrap();
    // Cash in: 20 000 − 6 500 + 15 000 − 1 500 given... the second sale gave no change back: the
    // whole 1 500 change was saved.
    assert_eq!(report.cash_received, rp(35_000));
    assert_eq!(report.change_given, rp(6_500));
    assert_eq!(report.saved_change, rp(1_500));
    assert_eq!(
        report.cash_refunds,
        ret.header.refund.saturating_add(rp(4_000))
    );
    assert_eq!(
        report.expected_cash,
        rp(100_000 + 35_000 - 6_500 - 4_000).saturating_sub(ret.header.refund)
    );
    assert_eq!(report.post_voids.count, 1);
    assert_eq!(report.post_voids.amount, rp(4_000));
    assert_eq!(report.returns.count, 1);
    assert_eq!(report.cart_discounts.count, 1);
    assert_eq!(report.cart_discounts.amount, rp(1_500));
    let card_key = TenderKey {
        kind: TenderKind::Card,
        tender_type: Some(crate::sales_fixtures::card().id),
    };
    assert_eq!(report.tenders[&card_key].sales.amount, rp(50_000));
    let cash_key = TenderKey {
        kind: TenderKind::Cash,
        tender_type: Some(CASH_TENDER_TYPE),
    };
    assert_eq!(report.tenders[&cash_key].refunds.count, 2);
    // The other session only has the sale's tender.
    let other_events = sales::session_events(db, other).await.unwrap();
    assert_eq!(other_events.len(), 1);
    assert!(matches!(other_events[0], SessionEvent::Tender { .. }));

    // A price override and a line discount show up too.
    let mut overridden = cash_sale(&till, &[item(9_000, 2)], NOV16 + 5);
    overridden.lines[0].price_mode = sultan_core::sale::PriceMode::Overridden {
        resolved: Some(rp(10_000)),
    };
    overridden.lines[0].totals.line_discount = rp(500);
    post(db, &overridden).await.unwrap();
    let events = sales::session_events(db, till.session).await.unwrap();
    assert!(events.contains(&SessionEvent::PriceOverride(rp(2_000))));
    assert!(events.contains(&SessionEvent::LineDiscount(rp(500))));
}

async fn register_two(db: &Db, till: &crate::sales_fixtures::Till) -> sultan_core::ids::RegisterId {
    let r = sultan_db::org::Register {
        id: sultan_core::ids::RegisterId::generate(),
        branch: till.o.jkt.id,
        code: "R2".into(),
        name: "Back".into(),
        default_location: None,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_register(&mut tx, &r, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    r.id
}
