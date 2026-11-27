//! Purchasing, transfers, opname and reorder (spec 2.8, phase N2; §17 cases 14–19).

// Each test is one flow through a document's life, with short names for its steps.
#![allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::similar_names
)]

use sultan_core::ids::{
    BranchId, CategoryId, GoodsReceiptId, LocationId, OpnameId, PurchaseOrderId, SupplierReturnId,
    TransferId, UnitId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::stock::{MovementKind, TaxMode, Uncounted};
use sultan_db::Db;
use sultan_db::org::StockLocation;
use sultan_db::stock::{
    self, CountInput, NewGoodsReceipt, NewGoodsReceiptLine, NewOpname, NewPurchaseOrder,
    NewPurchaseOrderLine, NewSupplierReturn, NewSupplierReturnLine, NewTransfer, NewTransferLine,
    OpnameScope, PaymentKind, PurchaseOrderStatus, ReceiptStatus, ReorderRule, StockDbError,
    SupplierItem, TransferStatus, Written,
};

use crate::common::{db_tests, t};
use crate::sales_fixtures::{Item, NOV16, cash_sale, post, rp};
use crate::stock::{HOUR, Shop, assert_balanced, cost, day, opening, q, shop};

db_tests!(
    receipt_posts_stock_cost_lots_and_payable,
    purchase_order_lifecycle,
    receipt_void_before_and_after_use,
    supplier_return_against_a_receipt_line,
    transfer_request_ship_receive,
    opname_counts_and_posts,
    reorder_rules_and_suggestions,
);

fn po_line(v: VariantId, u: UnitId, qty: i64, price: i64) -> NewPurchaseOrderLine {
    NewPurchaseOrderLine {
        variant: v,
        unit: u,
        qty: q(qty),
        price: rp(price),
    }
}

async fn ordered_po(db: &Db, s: &Shop, lines: Vec<NewPurchaseOrderLine>) -> PurchaseOrderId {
    let po = NewPurchaseOrder {
        id: PurchaseOrderId::generate(),
        branch: s.till.o.jkt.id,
        supplier: s.supplier.id,
        location: None,
        order_date: Some(day(20_261_116)),
        expected_date: None,
        note: None,
        user: s.till.o.manager.id,
        lines,
    };
    let mut tx = db.begin().await.unwrap();
    let (w, _) = stock::put_purchase_order(&mut tx, &po, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Created);
    let (w, ordered) = stock::order_purchase_order(&mut tx, po.id, s.till.o.manager.id, t(NOV16))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (w, ordered.status),
        (Written::Updated, PurchaseOrderStatus::Ordered)
    );
    assert!(ordered.number.unwrap().starts_with("JKT01-PO-261116-"));
    po.id
}

fn gr_line(v: VariantId, u: UnitId, qty: i64, price: i64) -> NewGoodsReceiptLine {
    NewGoodsReceiptLine {
        variant: v,
        unit: u,
        qty: q(qty),
        price: rp(price),
        discount: Money::ZERO,
        order_line: None,
        batch: None,
        expiry: None,
    }
}

fn receipt(s: &Shop, invoice: &str, lines: Vec<NewGoodsReceiptLine>) -> NewGoodsReceipt {
    NewGoodsReceipt {
        id: GoodsReceiptId::generate(),
        branch: s.till.o.jkt.id,
        supplier: s.supplier.id,
        location: None,
        invoice_no: invoice.to_owned(),
        invoice_date: day(20_261_116),
        payment: PaymentKind::Credit,
        due_date: None,
        tax_mode: TaxMode::None,
        tax_rate_bp: None,
        header_discount: Money::ZERO,
        other_costs: Money::ZERO,
        note: None,
        user: s.till.o.manager.id,
        lines,
    }
}

/// Writes and posts a receipt; returns the post result.
async fn post_receipt(
    db: &Db,
    r: &NewGoodsReceipt,
    at: i64,
) -> Result<(Written, stock::ReceiptPosted), StockDbError> {
    let mut tx = db.begin().await.unwrap();
    stock::put_goods_receipt(&mut tx, r, t(at)).await?;
    let out = stock::post_goods_receipt(&mut tx, r.id, r.user, t(at)).await;
    if out.is_ok() {
        tx.commit().await.unwrap();
    }
    out
}

/// Spec §17 case 14: posting moves the stock in at the landed cost (header discount, added PPN
/// and other costs spread), fills the lots, the supplier items' last price, the PO lines and
/// creates the payable; replaying is harmless; a second posted receipt with the same invoice
/// number of the supplier is a conflict.
async fn receipt_posts_stock_cost_lots_and_payable(db: &Db) {
    let s = shop(db).await;
    let (tea, kaos, milk) = (Shop::v(&s.tea), Shop::v(&s.kaos), Shop::v(&s.milk));
    let pcs = Shop::base(&s.tea);
    let po = ordered_po(
        db,
        &s,
        vec![
            po_line(tea, pcs, 10, 1_000),
            po_line(kaos, s.box_unit(), 2, 24_000),
        ],
    )
    .await;
    let mut r = receipt(
        &s,
        " INV-1 ",
        vec![
            NewGoodsReceiptLine {
                order_line: Some((po, 1)),
                ..gr_line(tea, pcs, 6, 1_000)
            },
            NewGoodsReceiptLine {
                order_line: Some((po, 2)),
                ..gr_line(kaos, s.box_unit(), 1, 24_000)
            },
            NewGoodsReceiptLine {
                batch: Some(" M1 ".into()),
                expiry: Some(day(20_270_101)),
                ..gr_line(milk, Shop::base(&s.milk), 4, 1_500)
            },
        ],
    );
    r.tax_mode = TaxMode::Added;
    r.header_discount = rp(1_000);
    r.other_costs = rp(500);
    let (w, posted) = post_receipt(db, &r, NOV16 + HOUR).await.unwrap();
    assert_eq!(w, Written::Updated);
    let gr = posted.receipt;
    assert_eq!(gr.status, ReceiptStatus::Posted);
    assert_eq!(gr.number.as_deref(), Some("JKT01-GR-261116-01"));
    assert_eq!(gr.invoice_no, "INV-1");
    assert_eq!(
        (gr.net, gr.tax, gr.total, gr.landed),
        (
            Some(rp(36_000)),
            Some(rp(3_850)),
            Some(rp(38_850)),
            Some(rp(39_350))
        )
    );
    assert_eq!(gr.tax_rate_bp, 1_100, "the company's standard rate");
    assert_eq!(gr.due_date, Some(day(20_261_216)), "invoice date + 30 days");
    let landed: Vec<_> = gr.lines.iter().map(|l| l.landed.unwrap().minor()).collect();
    assert_eq!(landed, [6_558, 26_232, 6_560]);
    // Stock, cost and lots.
    assert_eq!(cost(db, &s, tea).await, (q(6), rp(6_558), Some(rp(1_093))));
    assert_eq!(cost(db, &s, kaos).await.0, q(24));
    let lots = stock::list_lots(db, None, Some(s.loc()), Some(milk), None)
        .await
        .unwrap();
    assert_eq!((lots[0].batch.as_str(), lots[0].qty), ("M1", q(4)));
    assert_eq!(gr.lines[2].lot, Some(lots[0].id));
    // Supplier items: last price per base unit (24 000 per box of 24 = 1 000), created.
    let item = stock::get_supplier_item(db, s.supplier.id, kaos)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (item.last_price, item.last_unit),
        (Some(rp(1_000)), Some(s.box_unit()))
    );
    // The PO lines.
    let order = stock::get_purchase_order(db, po).await.unwrap().unwrap();
    let received: Vec<_> = order.lines.iter().map(|l| l.received_qty).collect();
    assert_eq!(received, [q(6), q(24)]);
    assert_eq!(order.status, PurchaseOrderStatus::Ordered);
    assert_eq!(
        stock::on_order(db, s.till.o.jkt.id, tea).await.unwrap(),
        q(4)
    );
    assert!(posted.over_received.is_empty());
    // The payable.
    let payables = stock::list_payables(db, Some(s.till.o.jkt.id), None, true)
        .await
        .unwrap();
    assert_eq!(payables.len(), 1);
    assert_eq!(
        (payables[0].amount, payables[0].paid, payables[0].due_date),
        (rp(38_850), false, day(20_261_216))
    );
    // Replaying the post changes nothing.
    let mut tx = db.begin().await.unwrap();
    let (w, again) = stock::post_goods_receipt(&mut tx, r.id, r.user, t(NOV16 + 2 * HOUR))
        .await
        .unwrap();
    assert_eq!(
        (w, again.receipt.number),
        (Written::Unchanged, gr.number.clone())
    );
    // A posted receipt cannot be written.
    assert_eq!(
        stock::put_goods_receipt(&mut tx, &r, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    tx.commit().await.unwrap();
    // The same invoice number of the supplier again → conflict.
    let dup = receipt(&s, "INV-1", vec![gr_line(tea, pcs, 1, 1_000)]);
    let err = post_receipt(db, &dup, NOV16 + 3 * HOUR).await.unwrap_err();
    assert!(matches!(err, StockDbError::DuplicateInvoice(_)));
    assert_eq!(err.code(), Some("conflict"));
    // A tracked line needs an expiry when posted.
    let no_exp = receipt(&s, "INV-2", vec![gr_line(milk, Shop::base(&s.milk), 1, 1)]);
    let err = post_receipt(db, &no_exp, NOV16 + 3 * HOUR)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("expiry"));
    // PKP leaves the PPN out of the cost.
    let mut tx = db.begin().await.unwrap();
    let pkp = sultan_core::stock::InventorySettings {
        pkp: true,
        ..stock::get_inventory_settings(&mut tx).await.unwrap()
    };
    stock::put_inventory_settings(&mut tx, &pkp, t(1))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut r3 = receipt(
        &s,
        "INV-3",
        vec![gr_line(kaos, Shop::base(&s.kaos), 10, 1_000)],
    );
    r3.tax_mode = TaxMode::Added;
    r3.payment = PaymentKind::Cash;
    let (_, p3) = post_receipt(db, &r3, NOV16 + 4 * HOUR).await.unwrap();
    assert_eq!(
        (p3.receipt.total, p3.receipt.landed),
        (Some(rp(11_100)), Some(rp(10_000)))
    );
    assert_eq!(
        p3.receipt.due_date,
        Some(day(20_261_116)),
        "cash: due on the invoice date"
    );
    assert_eq!(
        stock::list_goods_receipts(db, Some(s.till.o.jkt.id), None, Some(ReceiptStatus::Posted))
            .await
            .unwrap()
            .len(),
        2
    );
    assert_balanced(db).await;
}

/// Spec §17 case 16: partial receipt, over-receipt warning, auto close, close the rest, cancel
/// only while nothing was received.
async fn purchase_order_lifecycle(db: &Db) {
    let s = shop(db).await;
    let (tea, kaos) = (Shop::v(&s.tea), Shop::v(&s.kaos));
    let pcs = Shop::base(&s.tea);
    let po = ordered_po(
        db,
        &s,
        vec![
            po_line(tea, pcs, 10, 1_000),
            po_line(kaos, s.box_unit(), 1, 24_000),
        ],
    )
    .await;
    let line = |o, v, u, n| NewGoodsReceiptLine {
        order_line: Some((po, o)),
        ..gr_line(v, u, n, 1_000)
    };
    // Partial.
    let r1 = receipt(&s, "P-1", vec![line(1, tea, pcs, 4)]);
    let (_, p) = post_receipt(db, &r1, NOV16 + HOUR).await.unwrap();
    assert!(p.over_received.is_empty());
    // The rest, with an over-receipt on line 1: the PO closes by itself.
    let r2 = receipt(
        &s,
        "P-2",
        vec![line(1, tea, pcs, 7), line(2, kaos, Shop::base(&s.kaos), 24)],
    );
    let (_, p) = post_receipt(db, &r2, NOV16 + 2 * HOUR).await.unwrap();
    assert_eq!(p.over_received, [(po, 1)]);
    let order = stock::get_purchase_order(db, po).await.unwrap().unwrap();
    assert_eq!(order.status, PurchaseOrderStatus::Closed);
    assert_eq!(
        stock::on_order(db, s.till.o.jkt.id, tea).await.unwrap(),
        q(0)
    );
    // A closed PO takes no more receipts and cannot be cancelled.
    let r3 = receipt(&s, "P-3", vec![line(1, tea, pcs, 1)]);
    assert_eq!(
        post_receipt(db, &r3, NOV16 + 3 * HOUR)
            .await
            .unwrap_err()
            .code(),
        Some("invalid_field")
    );
    let mut tx = db.begin().await.unwrap();
    let user = s.till.o.manager.id;
    assert_eq!(
        stock::cancel_purchase_order(&mut tx, po, user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    let (w, _) = stock::close_purchase_order(&mut tx, po, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    tx.commit().await.unwrap();
    // Close the rest by hand; cancel only with nothing received.
    let po2 = ordered_po(db, &s, vec![po_line(tea, pcs, 5, 1_000)]).await;
    let r4 = receipt(
        &s,
        "P-4",
        vec![NewGoodsReceiptLine {
            order_line: Some((po2, 1)),
            ..gr_line(tea, pcs, 2, 1_000)
        }],
    );
    post_receipt(db, &r4, NOV16 + 4 * HOUR).await.unwrap();
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        stock::cancel_purchase_order(&mut tx, po2, user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    let (w, closed) = stock::close_purchase_order(&mut tx, po2, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(
        (w, closed.status),
        (Written::Updated, PurchaseOrderStatus::Closed)
    );
    tx.commit().await.unwrap();
    let po3 = ordered_po(db, &s, vec![po_line(tea, pcs, 5, 1_000)]).await;
    let mut tx = db.begin().await.unwrap();
    let (w, c) = stock::cancel_purchase_order(&mut tx, po3, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(
        (w, c.status),
        (Written::Updated, PurchaseOrderStatus::Cancelled)
    );
    let (w, _) = stock::cancel_purchase_order(&mut tx, po3, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::order_purchase_order(&mut tx, po3, user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    assert_eq!(
        stock::close_purchase_order(&mut tx, po3, user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    // A draft is rewritten whole; a service cannot be on a line; an ordered PO is not a draft.
    let draft = NewPurchaseOrder {
        id: PurchaseOrderId::generate(),
        branch: s.till.o.jkt.id,
        supplier: s.supplier.id,
        location: Some(s.loc()),
        order_date: None,
        expected_date: Some(day(20_261_120)),
        note: Some(" cepat ".into()),
        user,
        lines: vec![po_line(tea, pcs, 1, 900)],
    };
    stock::put_purchase_order(&mut tx, &draft, t(NOV16))
        .await
        .unwrap();
    let (w, d) = stock::put_purchase_order(
        &mut tx,
        &NewPurchaseOrder {
            lines: vec![po_line(tea, pcs, 2, 900), po_line(kaos, s.box_unit(), 1, 1)],
            ..draft.clone()
        },
        t(NOV16),
    )
    .await
    .unwrap();
    assert_eq!(
        (w, d.lines.len(), d.note.as_deref()),
        (Written::Updated, 2, Some("cepat"))
    );
    let (w, _) = stock::order_purchase_order(&mut tx, draft.id, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Updated);
    let (w, _) = stock::order_purchase_order(&mut tx, draft.id, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::put_purchase_order(&mut tx, &draft, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    let bad = NewPurchaseOrder {
        id: PurchaseOrderId::generate(),
        lines: vec![po_line(VariantId::generate(), pcs, 1, 1)],
        ..draft.clone()
    };
    assert_eq!(
        stock::put_purchase_order(&mut tx, &bad, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("not_found")
    );
    let wrong_unit = NewPurchaseOrder {
        id: PurchaseOrderId::generate(),
        lines: vec![po_line(tea, s.box_unit(), 1, 1)],
        ..draft.clone()
    };
    assert_eq!(
        stock::put_purchase_order(&mut tx, &wrong_unit, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("invalid_field")
    );
    tx.commit().await.unwrap();
    let all = stock::list_purchase_orders(db, Some(s.till.o.jkt.id), Some(s.supplier.id), None)
        .await
        .unwrap();
    assert_eq!(all.len(), 4);
    let cancelled =
        stock::list_purchase_orders(db, None, None, Some(PurchaseOrderStatus::Cancelled))
            .await
            .unwrap();
    assert_eq!(cancelled.len(), 1);
    assert_balanced(db).await;
}

/// Spec §17 case 15: a receipt can be voided before its cost is used; after a sale of one of
/// its variants it is `receipt_in_use`; a cash receipt is paid at once.
async fn receipt_void_before_and_after_use(db: &Db) {
    let s = shop(db).await;
    let (tea, milk) = (Shop::v(&s.tea), Shop::v(&s.milk));
    let pcs = Shop::base(&s.tea);
    let po = ordered_po(db, &s, vec![po_line(tea, pcs, 10, 1_000)]).await;
    let r1 = receipt(&s, "V-1", vec![gr_line(tea, pcs, 10, 1_000)]);
    post_receipt(db, &r1, NOV16).await.unwrap();
    let r2 = receipt(
        &s,
        "V-2",
        vec![
            NewGoodsReceiptLine {
                order_line: Some((po, 1)),
                ..gr_line(tea, pcs, 10, 1_300)
            },
            NewGoodsReceiptLine {
                batch: Some("B".into()),
                expiry: Some(day(20_270_101)),
                ..gr_line(milk, Shop::base(&s.milk), 2, 500)
            },
        ],
    );
    post_receipt(db, &r2, NOV16 + HOUR).await.unwrap();
    assert_eq!(
        stock::get_purchase_order(db, po)
            .await
            .unwrap()
            .unwrap()
            .status,
        PurchaseOrderStatus::Closed
    );
    let before = cost(db, &s, tea).await;
    assert_eq!(before, (q(20), rp(23_000), Some(rp(1_150))));
    assert!(!stock::receipt_in_use(db, r2.id).await.unwrap());
    let mut tx = db.begin().await.unwrap();
    let (w, voided) = stock::void_goods_receipt(
        &mut tx,
        r2.id,
        s.till.o.cashier.id,
        Some(s.till.o.manager.id),
        t(NOV16 + 2 * HOUR),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(
        (w, voided.status),
        (Written::Updated, ReceiptStatus::Voided)
    );
    assert_eq!(voided.approved_by, Some(s.till.o.manager.id));
    assert_eq!(
        cost(db, &s, tea).await,
        (q(10), rp(10_000), Some(rp(1_000)))
    );
    assert!(
        stock::list_lots(db, None, None, Some(milk), None)
            .await
            .unwrap()
            .is_empty()
    );
    let po_after = stock::get_purchase_order(db, po).await.unwrap().unwrap();
    assert_eq!(
        (po_after.status, po_after.lines[0].received_qty),
        (PurchaseOrderStatus::Ordered, q(0)),
        "an auto-closed PO reopens"
    );
    let payables = stock::list_payables(db, None, Some(s.supplier.id), false)
        .await
        .unwrap();
    assert!(payables.iter().any(|p| p.receipt == r2.id && p.voided));
    // Voiding again is harmless; the invoice number is free again.
    let mut tx = db.begin().await.unwrap();
    let (w, _) = stock::void_goods_receipt(&mut tx, r2.id, s.till.o.cashier.id, None, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    tx.commit().await.unwrap();
    let again = receipt(&s, "V-2", vec![gr_line(tea, pcs, 1, 1_000)]);
    post_receipt(db, &again, NOV16 + 3 * HOUR).await.unwrap();
    // After a sale of the variant, the earlier receipt is in use.
    let sale = cash_sale(
        &s.till,
        &[Item {
            variant: tea,
            unit: pcs,
            price: rp(2_000),
            qty: q(1),
        }],
        NOV16 + 4 * HOUR,
    );
    post(db, &sale).await.unwrap();
    assert!(stock::receipt_in_use(db, again.id).await.unwrap());
    let mut tx = db.begin().await.unwrap();
    let err = stock::void_goods_receipt(&mut tx, again.id, s.till.o.cashier.id, None, t(NOV16))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("receipt_in_use"));
    drop(tx);
    // A cash receipt is paid, so in use at once; a draft cannot be voided.
    let mut cash = receipt(&s, "V-3", vec![gr_line(milk, Shop::base(&s.milk), 1, 1)]);
    cash.payment = PaymentKind::Cash;
    cash.lines[0].expiry = Some(day(20_270_101));
    post_receipt(db, &cash, NOV16 + 5 * HOUR).await.unwrap();
    assert!(stock::receipt_in_use(db, cash.id).await.unwrap());
    let draft = receipt(&s, "V-4", vec![gr_line(tea, pcs, 1, 1)]);
    let mut tx = db.begin().await.unwrap();
    stock::put_goods_receipt(&mut tx, &draft, t(NOV16))
        .await
        .unwrap();
    assert_eq!(
        stock::void_goods_receipt(&mut tx, draft.id, s.till.o.cashier.id, None, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    tx.commit().await.unwrap();
    assert_balanced(db).await;
}

/// Spec §17 case 17: a supplier return against a receipt line takes at most what the line
/// received minus earlier returns; it moves stock out at the average and stores the credit.
async fn supplier_return_against_a_receipt_line(db: &Db) {
    let s = shop(db).await;
    let tea = Shop::v(&s.tea);
    let pcs = Shop::base(&s.tea);
    let mut r = receipt(&s, "R-1", vec![gr_line(tea, pcs, 6, 1_000)]);
    r.tax_mode = TaxMode::Added;
    post_receipt(db, &r, NOV16).await.unwrap();
    let ret = |n| NewSupplierReturn {
        id: SupplierReturnId::generate(),
        branch: s.till.o.jkt.id,
        supplier: s.supplier.id,
        location: None,
        tax_mode: TaxMode::None,
        tax_rate_bp: None,
        note: None,
        user: s.till.o.manager.id,
        lines: vec![NewSupplierReturnLine {
            variant: tea,
            unit: pcs,
            qty: q(n),
            price: rp(1_000),
            receipt_line: Some((r.id, 1)),
        }],
    };
    let first = ret(4);
    let mut tx = db.begin().await.unwrap();
    stock::put_supplier_return(&mut tx, &first, t(NOV16 + HOUR))
        .await
        .unwrap();
    let (w, posted) = stock::post_supplier_return(&mut tx, first.id, first.user, t(NOV16 + HOUR))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(w, Written::Updated);
    assert!(
        posted
            .number
            .as_deref()
            .unwrap()
            .starts_with("JKT01-RS-261116-")
    );
    assert_eq!(posted.tax_mode, TaxMode::Added, "the receipt's tax mode");
    assert_eq!(posted.credit, Some(rp(4_440)));
    assert_eq!(
        posted.lines[0].value,
        Some(rp(-4_440)),
        "at the average (PPN in the cost)"
    );
    assert_eq!(cost(db, &s, tea).await.0, q(2));
    // 4 + 3 > 6 → invalid qty; 2 more is fine.
    let second = ret(3);
    let mut tx = db.begin().await.unwrap();
    stock::put_supplier_return(&mut tx, &second, t(NOV16))
        .await
        .unwrap();
    let err = stock::post_supplier_return(&mut tx, second.id, second.user, t(NOV16))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("qty"), "{err}");
    drop(tx);
    let third = ret(2);
    let mut tx = db.begin().await.unwrap();
    stock::put_supplier_return(&mut tx, &third, t(NOV16))
        .await
        .unwrap();
    stock::post_supplier_return(&mut tx, third.id, third.user, t(NOV16))
        .await
        .unwrap();
    let (w, _) = stock::post_supplier_return(&mut tx, third.id, third.user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::put_supplier_return(&mut tx, &third, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    // A free return (no receipt) at the stated tax mode.
    let free = NewSupplierReturn {
        lines: vec![NewSupplierReturnLine {
            receipt_line: None,
            ..ret(1).lines[0]
        }],
        ..ret(1)
    };
    stock::put_supplier_return(&mut tx, &free, t(NOV16))
        .await
        .unwrap();
    let (_, f) = stock::post_supplier_return(&mut tx, free.id, free.user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(f.credit, Some(rp(1_000)));
    tx.commit().await.unwrap();
    assert_eq!(
        stock::list_supplier_returns(db, Some(s.till.o.jkt.id), Some(s.supplier.id))
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        cost(db, &s, tea).await.0,
        q(-1),
        "negative stock is allowed"
    );
    assert_balanced(db).await;
}

async fn bdg_location(db: &Db, s: &Shop) -> LocationId {
    let l = StockLocation {
        id: LocationId::generate(),
        branch: s.till.o.bdg.id,
        name: "Toko".into(),
        receiving_default: true,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_location(&mut tx, &l, t(10))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    l.id
}

/// Spec §17 cases 11 and 18: a request at the receiver, shipped at the sender's average with
/// the lots, received with a shortage (a transit loss at the sender); cancel after shipping is
/// a conflict; a branch this node does not hold is `other_node`.
async fn transfer_request_ship_receive(db: &Db) {
    let s = shop(db).await;
    let (tea, milk) = (Shop::v(&s.tea), Shop::v(&s.milk));
    let pcs = Shop::base(&s.tea);
    let (jkt, bdg) = (s.till.o.jkt.id, s.till.o.bdg.id);
    let bdg_loc = bdg_location(db, &s).await;
    opening(
        db,
        &s,
        &[
            (tea, pcs, 30, 1_150, None),
            (milk, Shop::base(&s.milk), 2, 1_000, Some(("A", 20_270_301))),
            (milk, Shop::base(&s.milk), 2, 1_000, Some(("B", 20_270_201))),
        ],
        NOV16 - HOUR,
    )
    .await;
    let new = |ship_from| NewTransfer {
        id: TransferId::generate(),
        from_branch: jkt,
        to_branch: bdg,
        to_location: None,
        ship_from,
        note: None,
        user: s.till.o.manager.id,
        lines: vec![
            NewTransferLine {
                variant: tea,
                unit: pcs,
                qty: q(12),
            },
            NewTransferLine {
                variant: milk,
                unit: Shop::base(&s.milk),
                qty: q(3),
            },
        ],
    };
    let req = new(None);
    let user = s.till.o.manager.id;
    let mut tx = db.begin().await.unwrap();
    let (w, t1) = stock::create_transfer(&mut tx, &req, t(NOV16))
        .await
        .unwrap();
    assert_eq!(
        (w, t1.status),
        (Written::Created, TransferStatus::Requested)
    );
    assert!(
        t1.number.starts_with("BDG01-TR-261116-"),
        "numbered by the requester"
    );
    let (w, _) = stock::create_transfer(&mut tx, &req, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    // Ship 10 of the 12 asked for, and the milk in full.
    let (_, shipped) =
        stock::ship_transfer(&mut tx, req.id, None, &[(1, q(10))], user, t(NOV16 + HOUR))
            .await
            .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(shipped.status, TransferStatus::Shipped);
    let line = &shipped.lines[0];
    assert_eq!(
        (line.shipped_qty, line.shipped_value),
        (Some(q(10)), Some(rp(11_500)))
    );
    let lots: Vec<_> = shipped.lines[1]
        .lots
        .iter()
        .map(|l| (l.0.as_str(), l.2))
        .collect();
    assert_eq!(lots, [("B", q(2)), ("A", q(1))], "FEFO");
    assert_eq!(stock::in_transit(db, bdg, tea).await.unwrap(), q(10));
    assert_eq!(stock::in_transit(db, jkt, tea).await.unwrap(), q(10));
    assert_eq!(stock::branch_on_hand(db, jkt, tea).await.unwrap(), q(20));
    // Cancel after ship → conflict; ship again → unchanged.
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        stock::cancel_transfer(&mut tx, req.id, user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    let (w, _) = stock::ship_transfer(&mut tx, req.id, None, &[], user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    // More than shipped → invalid qty.
    let err = stock::receive_transfer(&mut tx, req.id, None, &[(1, q(11))], user, t(NOV16))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    drop(tx);
    // Receive 8 tea (2 lost) and 2 milk (1 lost).
    let mut tx = db.begin().await.unwrap();
    let (_, got) = stock::receive_transfer(
        &mut tx,
        req.id,
        None,
        &[(1, q(8)), (2, q(2))],
        user,
        t(NOV16 + 2 * HOUR),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(got.status, TransferStatus::Received);
    assert_eq!(got.to_location, Some(bdg_loc));
    let l = &got.lines[0];
    assert_eq!(
        (l.received_qty, l.received_value, l.lost_qty, l.lost_value),
        (Some(q(8)), Some(rp(9_200)), Some(q(2)), Some(rp(2_300)))
    );
    let c = stock::cost_state(db, bdg, tea).await.unwrap();
    assert_eq!(
        (c.qty, c.value, c.unit_cost().unwrap()),
        (q(8), rp(9_200), Some(rp(1_150)))
    );
    assert_eq!(stock::branch_on_hand(db, jkt, tea).await.unwrap(), q(20));
    assert_eq!(stock::in_transit(db, bdg, tea).await.unwrap(), q(0));
    let moves = stock::document_movements(db, *req.id.as_uuid())
        .await
        .unwrap();
    let loss: Vec<_> = moves
        .iter()
        .filter(|m| m.branch == jkt && m.kind == MovementKind::TransitLoss)
        .map(|m| (m.qty, m.value))
        .collect();
    assert_eq!(loss, [(q(-2), Some(rp(-2_300))), (q(-1), Some(rp(-1_000)))]);
    let bdg_lots: Vec<_> = stock::list_lots(db, Some(bdg), None, Some(milk), None)
        .await
        .unwrap()
        .iter()
        .map(|l| (l.batch.clone(), l.qty))
        .collect();
    assert_eq!(bdg_lots, [("B".to_owned(), q(2))]);
    // A transfer shipped at once, numbered by the sender; a request can be cancelled.
    let direct = new(Some(s.loc()));
    let mut tx = db.begin().await.unwrap();
    let (_, d) = stock::create_transfer(&mut tx, &direct, t(NOV16 + 3 * HOUR))
        .await
        .unwrap();
    assert!(d.number.starts_with("JKT01-TR-"));
    assert_eq!(d.status, TransferStatus::Shipped);
    let other = new(None);
    stock::create_transfer(&mut tx, &other, t(NOV16))
        .await
        .unwrap();
    let (w, c) = stock::cancel_transfer(&mut tx, other.id, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!((w, c.status), (Written::Updated, TransferStatus::Cancelled));
    let (w, _) = stock::cancel_transfer(&mut tx, other.id, user, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::ship_transfer(&mut tx, other.id, None, &[], user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    assert_eq!(
        stock::receive_transfer(&mut tx, other.id, None, &[], user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    // A branch this node does not hold.
    let far = NewTransfer {
        id: TransferId::generate(),
        to_branch: BranchId::generate(),
        ..new(None)
    };
    assert_eq!(
        stock::create_transfer(&mut tx, &far, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("other_node")
    );
    let same = NewTransfer {
        id: TransferId::generate(),
        to_branch: jkt,
        ..new(None)
    };
    assert_eq!(
        stock::create_transfer(&mut tx, &same, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("invalid_field")
    );
    tx.commit().await.unwrap();
    assert_eq!(
        stock::list_transfers(db, Some(bdg), None)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_eq!(
        stock::list_transfers(db, None, Some(TransferStatus::Shipped))
            .await
            .unwrap()
            .len(),
        1
    );
    assert_balanced(db).await;
}

fn count_of(v: VariantId, u: UnitId, n: i64, replace: bool) -> CountInput {
    CountInput {
        id: uuid::Uuid::now_v7(),
        variant: v,
        unit: u,
        qty: q(n),
        replace,
    }
}

/// Spec §17 cases 8, 9 and 19: two counters add up, sales during the count, one open opname per
/// location and variant, posting a cycle and a full count (uncounted set to 0).
async fn opname_counts_and_posts(db: &Db) {
    let s = shop(db).await;
    let (tea, kaos, milk) = (Shop::v(&s.tea), Shop::v(&s.kaos), Shop::v(&s.milk));
    let pcs = Shop::base(&s.tea);
    opening(db, &s, &[(tea, pcs, 10, 100, None)], NOV16 - HOUR).await;
    let user = s.till.o.manager.id;
    let cycle = NewOpname {
        id: OpnameId::generate(),
        branch: s.till.o.jkt.id,
        location: None,
        scope: OpnameScope::Cycle {
            variants: vec![tea],
            categories: vec![],
        },
        note: None,
        user,
    };
    let mut tx = db.begin().await.unwrap();
    let (w, o) = stock::start_opname(&mut tx, &cycle, t(NOV16))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(w, Written::Created);
    assert!(o.number.starts_with("JKT01-OP-261116-"));
    assert_eq!(o.lines.len(), 1);
    assert_eq!(o.lines[0].snapshot, q(10));
    // Sold 2 during the count.
    let sale = cash_sale(
        &s.till,
        &[Item {
            variant: tea,
            unit: pcs,
            price: rp(500),
            qty: q(2),
        }],
        NOV16 + 1,
    );
    post(db, &sale).await.unwrap();
    // Two counters: 3 + 4 = 7 → variance −1.
    let a = count_of(tea, pcs, 3, false);
    let mut tx = db.begin().await.unwrap();
    stock::add_counts(&mut tx, cycle.id, &[a], user, t(NOV16 + 2))
        .await
        .unwrap();
    let view = stock::add_counts(
        &mut tx,
        cycle.id,
        &[a, count_of(tea, pcs, 4, false)],
        s.till.o.cashier.id,
        t(NOV16 + 3),
    )
    .await
    .unwrap();
    let line = view.lines[0];
    assert_eq!(
        (line.since, line.counted, line.variance),
        (q(-2), Some(q(7)), Some(q(-1)))
    );
    // Outside the scope, below 0.
    assert_eq!(
        stock::add_counts(
            &mut tx,
            cycle.id,
            &[count_of(milk, Shop::base(&s.milk), 1, false)],
            user,
            t(NOV16)
        )
        .await
        .unwrap_err()
        .code(),
        Some("invalid_field")
    );
    assert_eq!(
        stock::add_counts(
            &mut tx,
            cycle.id,
            &[count_of(tea, pcs, -9, false)],
            user,
            t(NOV16)
        )
        .await
        .unwrap_err()
        .code(),
        Some("invalid_field")
    );
    // A second open opname on the same variant and location.
    let full = NewOpname {
        id: OpnameId::generate(),
        scope: OpnameScope::Full,
        ..cycle.clone()
    };
    let err = stock::start_opname(&mut tx, &full, t(NOV16))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("opname_open"));
    assert!(err.to_string().contains(&o.number));
    // Post (`stock.count` + `stock.adjust` are the server's checks).
    let (w, posted) = stock::post_opname(
        &mut tx,
        cycle.id,
        Uncounted::Leave,
        user,
        None,
        t(NOV16 + 4),
    )
    .await
    .unwrap();
    assert_eq!((w, posted.status.as_str()), (Written::Updated, "posted"));
    assert_eq!(
        (
            posted.lines[0].counted,
            posted.lines[0].variance,
            posted.lines[0].value
        ),
        (Some(q(7)), Some(q(-1)), Some(rp(-100)))
    );
    let (w, _) = stock::post_opname(&mut tx, cycle.id, Uncounted::Leave, user, None, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::add_counts(&mut tx, cycle.id, &[], user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    assert_eq!(
        stock::cancel_opname(&mut tx, cycle.id, user, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    tx.commit().await.unwrap();
    assert_eq!(
        stock::branch_on_hand(db, s.till.o.jkt.id, tea)
            .await
            .unwrap(),
        q(7)
    );
    // A full count: the locks are free again. Count milk (an excess without a cost) only.
    let mut tx = db.begin().await.unwrap();
    let (_, f) = stock::start_opname(&mut tx, &full, t(NOV16 + 5))
        .await
        .unwrap();
    assert!(f.full && f.lines.iter().any(|l| l.variant == kaos));
    stock::add_counts(
        &mut tx,
        full.id,
        &[
            count_of(milk, Shop::base(&s.milk), 9, false),
            count_of(milk, Shop::base(&s.milk), 2, true),
        ],
        user,
        t(NOV16 + 6),
    )
    .await
    .unwrap();
    let (_, posted) = stock::post_opname(
        &mut tx,
        full.id,
        Uncounted::SetZero,
        user,
        Some(user),
        t(NOV16 + 7),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(posted.uncounted, Some(Uncounted::SetZero));
    let by = |v| posted.lines.iter().find(|l| l.variant == v).unwrap();
    assert_eq!(
        (by(milk).counted, by(milk).variance, by(milk).value),
        (Some(q(2)), Some(q(2)), None)
    );
    assert_eq!((by(tea).counted, by(tea).variance), (None, Some(q(-7))));
    assert_eq!(by(kaos).variance, Some(q(0)));
    assert_eq!(
        stock::branch_on_hand(db, s.till.o.jkt.id, tea)
            .await
            .unwrap(),
        q(0)
    );
    assert_eq!(
        stock::branch_on_hand(db, s.till.o.jkt.id, milk)
            .await
            .unwrap(),
        q(2)
    );
    // Cancel, list, a cycle count without scope.
    let c3 = NewOpname {
        id: OpnameId::generate(),
        scope: OpnameScope::Cycle {
            variants: vec![],
            categories: vec![CategoryId::generate()],
        },
        ..cycle.clone()
    };
    let mut tx = db.begin().await.unwrap();
    let (_, started) = stock::start_opname(&mut tx, &c3, t(NOV16 + 8))
        .await
        .unwrap();
    assert!(started.lines.is_empty());
    let (w, _) = stock::cancel_opname(&mut tx, c3.id, user, t(NOV16 + 9))
        .await
        .unwrap();
    assert_eq!(w, Written::Updated);
    let (w, _) = stock::cancel_opname(&mut tx, c3.id, user, t(NOV16 + 9))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::post_opname(&mut tx, c3.id, Uncounted::Leave, user, None, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("conflict")
    );
    let empty = NewOpname {
        id: OpnameId::generate(),
        scope: OpnameScope::Cycle {
            variants: vec![],
            categories: vec![],
        },
        ..cycle.clone()
    };
    assert_eq!(
        stock::start_opname(&mut tx, &empty, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("invalid_field")
    );
    tx.commit().await.unwrap();
    assert_eq!(
        stock::list_opnames(db, s.till.o.jkt.id)
            .await
            .unwrap()
            .len(),
        3
    );
    assert_balanced(db).await;
}

/// Spec §10, §17 case 10 at the db: min / max rules, the low-stock list, and the suggested PO
/// from sales velocity rounded up to the purchase unit.
async fn reorder_rules_and_suggestions(db: &Db) {
    let s = shop(db).await;
    let (tea, kaos) = (Shop::v(&s.tea), Shop::v(&s.kaos));
    let pcs = Shop::base(&s.tea);
    let jkt = s.till.o.jkt.id;
    // 74 in; 60 sold over the last days (a sale of 6 voided again does not count); 5 on order.
    opening(db, &s, &[(tea, pcs, 74, 100, None)], NOV16 - 5 * 24 * HOUR).await;
    let sale = cash_sale(
        &s.till,
        &[Item {
            variant: tea,
            unit: pcs,
            price: rp(500),
            qty: q(60),
        }],
        NOV16 - 24 * HOUR,
    );
    post(db, &sale).await.unwrap();
    let back = cash_sale(
        &s.till,
        &[Item {
            variant: tea,
            unit: pcs,
            price: rp(500),
            qty: q(6),
        }],
        NOV16 - 23 * HOUR,
    );
    let (_, sold) = post(db, &back).await.unwrap();
    let v = crate::sales_fixtures::void_of(
        &s.till,
        &sold,
        s.till.session,
        sultan_core::sale::RefundMethod::Cash,
        None,
        NOV16 - 22 * HOUR,
    );
    crate::sales_fixtures::void(db, &v).await.unwrap();
    ordered_po(db, &s, vec![po_line(tea, pcs, 5, 100)]).await;
    assert_eq!(stock::branch_on_hand(db, jkt, tea).await.unwrap(), q(14));
    let mut tx = db.begin().await.unwrap();
    // The supplier's preferred item: tea, bought in pcs.
    stock::put_supplier_item(
        &mut tx,
        &SupplierItem {
            supplier: s.supplier.id,
            variant: tea,
            supplier_code: None,
            last_price: None,
            last_unit: Some(pcs),
            preferred: true,
        },
        t(NOV16),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    // Velocity 60 / 30 × (7 + 14) = 42 − (14 + 5) = 23 pcs.
    let lines = stock::suggest_purchase_order(db, s.supplier.id, jkt, t(NOV16))
        .await
        .unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(
        (lines[0].units, lines[0].base_qty, lines[0].unit),
        (23, q(23), Some(pcs))
    );
    // With a rule min 20 / max 50: 50 − 19 = 31.
    let rule = ReorderRule {
        branch: jkt,
        variant: tea,
        min: q(20),
        max: q(50),
    };
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        stock::put_reorder_rule(&mut tx, &rule, t(1)).await.unwrap(),
        Written::Created
    );
    assert_eq!(
        stock::put_reorder_rule(&mut tx, &rule, t(2)).await.unwrap(),
        Written::Unchanged
    );
    let bad = ReorderRule {
        min: q(9),
        max: q(8),
        ..rule
    };
    assert_eq!(
        stock::put_reorder_rule(&mut tx, &bad, t(3))
            .await
            .unwrap_err()
            .code(),
        Some("invalid_field")
    );
    tx.commit().await.unwrap();
    let lines = stock::suggest_purchase_order(db, s.supplier.id, jkt, t(NOV16))
        .await
        .unwrap();
    assert_eq!(lines[0].units, 31);
    // The low-stock list: tea (19 ≤ 20) and a negative kaos.
    let neg = cash_sale(
        &s.till,
        &[Item {
            variant: kaos,
            unit: Shop::base(&s.kaos),
            price: rp(500),
            qty: q(1),
        }],
        NOV16,
    );
    post(db, &neg).await.unwrap();
    let low: Vec<_> = stock::low_stock(db, jkt)
        .await
        .unwrap()
        .iter()
        .map(|l| l.variant)
        .collect();
    assert!(low.contains(&tea) && low.contains(&kaos));
    let mut tx = db.begin().await.unwrap();
    let raised = ReorderRule { min: q(10), ..rule };
    assert_eq!(
        stock::put_reorder_rule(&mut tx, &raised, t(4))
            .await
            .unwrap(),
        Written::Updated
    );
    tx.commit().await.unwrap();
    let low: Vec<_> = stock::low_stock(db, jkt)
        .await
        .unwrap()
        .iter()
        .map(|l| l.variant)
        .collect();
    assert!(!low.contains(&tea));
    assert_eq!(stock::list_reorder_rules(db, jkt).await.unwrap().len(), 1);
    let mut tx = db.begin().await.unwrap();
    assert!(stock::delete_reorder_rule(&mut tx, jkt, tea).await.unwrap());
    assert!(!stock::delete_reorder_rule(&mut tx, jkt, tea).await.unwrap());
    tx.commit().await.unwrap();
    assert!(
        stock::get_reorder_rule(db, jkt, tea)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        stock::suggest_purchase_order(db, sultan_core::ids::SupplierId::generate(), jkt, t(NOV16))
            .await
            .unwrap_err()
            .code(),
        Some("not_found")
    );
}
