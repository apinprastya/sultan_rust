//! The stock ledger and its caches, sales and returns with values, master data (suppliers,
//! reasons, settings), adjustments, opening stock, internal transfers, the stock card, lots and
//! the inventory migration (spec 2.8, phase N2; §17 cases 13, 20, 21).

// Each test is one flow through a document's life, with short names for its steps.
#![allow(
    clippy::too_many_lines,
    clippy::many_single_char_names,
    clippy::similar_names
)]

use std::borrow::Cow;
use std::path::Path;

use sultan_core::access::Permission;
use sultan_core::catalog::Qty;
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{
    AdjustmentId, InternalTransferId, LocationId, OpeningStockId, StockReasonId, SupplierId,
    UnitId, VariantId,
};
use sultan_core::money::Money;
use sultan_core::sale::{RefundMethod, ReturnDisposition};
use sultan_core::stock::{InventorySettings, MovementKind, ReasonDirection};
use sultan_db::catalog::ProductDoc;
use sultan_db::org::StockLocation;
use sultan_db::stock::{
    self, NewAdjustment, NewAdjustmentLine, NewInternalTransfer, NewOpeningLine, NewOpeningStock,
    NewTransferLine, OnHandFilter, ProductExpiry, SEED_REASON_IDS, StockDbError, StockReasonRecord,
    Supplier, SupplierItem, Written,
};
use sultan_db::{Db, roles, seed};

use crate::catalog_fixtures::{kaos, put, simple};
use crate::common::{MySqlTestDb, count, db_tests, raw, t};
use crate::sales_fixtures::{
    Item, NOV16, Till, cash_sale, post, post_return, return_of, rp, till, void, void_of,
};

db_tests!(
    sales_write_values_and_reverse_exactly,
    opening_stock_lots_and_fefo_sales,
    adjustments_value_and_need_a_cost,
    internal_transfer_keeps_the_cost,
    suppliers_and_items,
    stock_reasons_seed_put_archive,
    inventory_settings_and_product_expiry,
    stock_card_lots_and_on_hand,
    seeded_grants,
    ledger_is_append_only_with_new_kinds,
);

/// An hour in milliseconds.
pub const HOUR: i64 = 3_600_000;

pub fn q(whole: i64) -> Qty {
    Qty::from_thousandths(whole * 1_000)
}

/// A shop: the till's org with products and a supplier.
pub struct Shop {
    pub till: Till,
    /// A simple stock product (pcs).
    pub tea: ProductDoc,
    /// A product with box(24) (four variants).
    pub kaos: ProductDoc,
    /// A tracked product (expiry lots).
    pub milk: ProductDoc,
    pub supplier: Supplier,
}

impl Shop {
    pub fn v(doc: &ProductDoc) -> VariantId {
        doc.draft.variants[0].id
    }

    pub fn base(doc: &ProductDoc) -> UnitId {
        doc.draft.units.base
    }

    /// The `box` unit of `kaos`.
    pub fn box_unit(&self) -> UnitId {
        self.kaos.draft.units.units[1].id
    }

    pub fn loc(&self) -> LocationId {
        self.till.o.location.id
    }
}

pub fn supplier(code: &str) -> Supplier {
    Supplier {
        id: SupplierId::generate(),
        code: code.to_owned(),
        name: format!("PT {code}"),
        address: None,
        phone: Some(" 0812 ".into()),
        email: None,
        tax_id: None,
        pkp: true,
        terms_days: 30,
        lead_days: 7,
        notes: None,
        active: true,
        archived_at: None,
    }
}

pub async fn shop(db: &Db) -> Shop {
    let till = till(db).await;
    let (_, tea) = put(db, &till.o, &simple("TEA", "8990001")).await;
    let (_, kaos) = put(db, &till.o, &kaos("KAOS")).await;
    let (_, milk) = put(db, &till.o, &simple("MILK", "8990002")).await;
    let s = supplier("sup1");
    let mut tx = db.begin().await.unwrap();
    stock::set_product_expiry(
        &mut tx,
        milk.draft.id,
        ProductExpiry {
            track_expiry: true,
            expiry_warn_days: Some(10),
        },
        t(20),
    )
    .await
    .unwrap();
    let (_, supplier) = stock::put_supplier(&mut tx, &s, t(20)).await.unwrap();
    tx.commit().await.unwrap();
    Shop {
        till,
        tea,
        kaos,
        milk,
        supplier,
    }
}

pub fn day(key: i64) -> LocalDay {
    LocalDay::from_key(key).unwrap()
}

/// An opening line: variant, unit, qty, unit cost, lot (batch, expiry key).
pub type OpeningRow<'a> = (VariantId, UnitId, i64, i64, Option<(&'a str, i64)>);

/// Posts opening stock of `lines` (variant, unit, qty, unit cost, lot) at the shop's location.
pub async fn opening(db: &Db, s: &Shop, lines: &[OpeningRow<'_>], at: i64) -> stock::OpeningStock {
    let o = NewOpeningStock {
        id: OpeningStockId::generate(),
        branch: s.till.o.jkt.id,
        location: None,
        note: Some(" awal ".into()),
        user: s.till.o.manager.id,
        lines: lines
            .iter()
            .map(|(v, u, qty, cost, lot)| NewOpeningLine {
                variant: *v,
                unit: *u,
                qty: q(*qty),
                unit_cost: rp(*cost),
                batch: lot.map(|l| l.0.to_owned()),
                expiry: lot.map(|l| day(l.1)),
            })
            .collect(),
    };
    let mut tx = db.begin().await.unwrap();
    let (written, doc) = stock::post_opening_stock(&mut tx, &o, t(at)).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(written, Written::Created);
    doc
}

/// The cost state of the shop's variant.
pub async fn cost(db: &Db, s: &Shop, v: VariantId) -> (Qty, Money, Option<Money>) {
    let c = stock::cost_state(db, s.till.o.jkt.id, v).await.unwrap();
    (c.qty, c.value, c.unit_cost().unwrap())
}

async fn scalar(db: &Db, sql: &str) -> i64 {
    match db {
        Db::Sqlite(pool) => sqlx::query_scalar(sql).fetch_one(pool).await.unwrap(),
        Db::MySql(pool) => sqlx::query_scalar(sql).fetch_one(pool).await.unwrap(),
    }
}

/// Spec §17 case 13: every cache equals the ledger (balance per location, cost quantity per
/// branch, lots per lot).
pub async fn assert_balanced(db: &Db) {
    let zero = "X'00000000000000000000000000000000'";
    let balance = format!(
        "SELECT COUNT(*) FROM stock_balance b WHERE b.qty <> (SELECT COALESCE(SUM(m.base_qty), 0) \
         FROM stock_movement m WHERE m.branch_id = b.branch_id AND m.variant_id = b.variant_id \
         AND COALESCE(m.location_id, {zero}) = b.location_id)"
    );
    assert_eq!(scalar(db, &balance).await, 0, "stock_balance = ledger");
    let missing = format!(
        "SELECT COUNT(*) FROM stock_movement m WHERE NOT EXISTS (SELECT 1 FROM stock_balance b \
         WHERE b.branch_id = m.branch_id AND b.variant_id = m.variant_id \
         AND b.location_id = COALESCE(m.location_id, {zero}))"
    );
    assert_eq!(
        scalar(db, &missing).await,
        0,
        "every ledger place has a balance"
    );
    let cost = "SELECT COUNT(*) FROM stock_cost c WHERE c.qty <> (SELECT COALESCE(SUM(m.base_qty), \
                0) FROM stock_movement m WHERE m.branch_id = c.branch_id \
                AND m.variant_id = c.variant_id)";
    assert_eq!(scalar(db, cost).await, 0, "stock_cost.qty = ledger");
    let lots = "SELECT COUNT(*) FROM stock_lot l WHERE l.qty <> (SELECT COALESCE(SUM(m.base_qty), \
                0) FROM stock_movement m WHERE m.lot_id = l.id)";
    assert_eq!(scalar(db, lots).await, 0, "stock_lot = ledger");
}

fn item(v: VariantId, u: UnitId, price: i64, qty: i64) -> Item {
    Item {
        variant: v,
        unit: u,
        price: rp(price),
        qty: q(qty),
    }
}

/// Spec §17 case 21: a sale after 2.8 writes a value at the average; a post-void puts back
/// exactly what it took; a restock return comes back at the sale line's value, a write-off at
/// the average.
async fn sales_write_values_and_reverse_exactly(db: &Db) {
    let s = shop(db).await;
    let tea = Shop::v(&s.tea);
    let pcs = Shop::base(&s.tea);
    // 3 @ total 1 000: the rounding case of §17 case 2.
    opening(db, &s, &[(tea, pcs, 3, 0, None)], NOV16 - 2 * HOUR).await;
    // 3 more at 333 each → 6 @ 999 (unit cost 167 for display).
    opening(db, &s, &[(tea, pcs, 3, 333, None)], NOV16 - HOUR).await;
    assert_eq!(cost(db, &s, tea).await, (q(6), rp(999), Some(rp(167))));

    let sale = cash_sale(&s.till, &[item(tea, pcs, 5_000, 2)], NOV16);
    let (_, sold) = post(db, &sale).await.unwrap();
    let moves = stock::document_movements(db, *sold.header.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(moves.len(), 1);
    assert_eq!(
        (
            moves[0].kind,
            moves[0].qty,
            moves[0].value,
            moves[0].doc_line
        ),
        (MovementKind::Sale, q(-2), Some(rp(-333)), Some(1))
    );
    assert_eq!(cost(db, &s, tea).await, (q(4), rp(666), Some(rp(167))));
    // The 2.6 view of the rows carries the value too.
    let old_view = sultan_db::sales::document_movements(db, *sold.header.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(old_view[0].value, Some(rp(-333)));

    // A post-void puts back exactly what the sale took.
    let v = void_of(
        &s.till,
        &sold,
        s.till.session,
        RefundMethod::Cash,
        None,
        NOV16 + HOUR,
    );
    void(db, &v).await.unwrap();
    let moves = stock::document_movements(db, *sold.header.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(
        (moves[1].kind, moves[1].qty, moves[1].value),
        (MovementKind::SaleVoid, q(2), Some(rp(333)))
    );
    assert_eq!(cost(db, &s, tea).await.0, q(6));
    assert_eq!(cost(db, &s, tea).await.1, rp(999));

    // A restock return at the sale line's value; a write-off at the average.
    let sale = cash_sale(&s.till, &[item(tea, pcs, 5_000, 3)], NOV16 + 2 * HOUR);
    let (_, sold) = post(db, &sale).await.unwrap();
    let sale_value = stock::document_movements(db, *sold.header.id.as_uuid())
        .await
        .unwrap()[0]
        .value
        .unwrap();
    assert_eq!(sale_value, rp(-500));
    let back = return_of(
        &s.till,
        &sold,
        &[(1, 2, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 3 * HOUR,
    );
    let (_, r) = post_return(db, &back).await.unwrap();
    let moves = stock::document_movements(db, *r.header.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(
        (
            moves[0].kind,
            moves[0].qty,
            moves[0].value,
            moves[0].doc_line
        ),
        (MovementKind::Return, q(2), Some(rp(333)), Some(1))
    );
    assert_eq!(cost(db, &s, tea).await, (q(5), rp(832), Some(rp(166))));
    // 5 more at 300 → 10 @ 2 332: the average (233) is no longer the sale line's value (167).
    opening(
        db,
        &s,
        &[(tea, pcs, 5, 300, None)],
        NOV16 + 3 * HOUR + HOUR / 2,
    )
    .await;
    assert_eq!(cost(db, &s, tea).await, (q(10), rp(2_332), Some(rp(233))));
    let off = return_of(
        &s.till,
        &sold,
        &[(
            1,
            1,
            ReturnDisposition::WriteOff {
                reason: "pecah".into(),
            },
        )],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 4 * HOUR,
    );
    let (_, r) = post_return(db, &off).await.unwrap();
    let moves = stock::document_movements(db, *r.header.id.as_uuid())
        .await
        .unwrap();
    let got: Vec<_> = moves.iter().map(|m| (m.kind, m.qty)).collect();
    assert_eq!(
        got,
        [
            (MovementKind::Return, q(1)),
            (MovementKind::WriteOff, q(-1))
        ]
    );
    // A written-off return comes in and goes out at the average: the pair leaves the cost as it
    // was (a sale-line value in would leave 10 @ 2 272).
    assert_eq!(moves[0].value, Some(rp(233)));
    assert_eq!(moves[1].value, Some(rp(-233)));
    assert_eq!(cost(db, &s, tea).await, (q(10), rp(2_332), Some(rp(233))));
    // A variant never received sells without a cost (§17 case 4).
    let stray = cash_sale(&s.till, &[item(VariantId::generate(), pcs, 100, 1)], NOV16);
    let (_, sold) = post(db, &stray).await.unwrap();
    let moves = stock::document_movements(db, *sold.header.id.as_uuid())
        .await
        .unwrap();
    assert_eq!(moves[0].value, None);
    assert_eq!(
        stock::branch_on_hand(db, s.till.o.jkt.id, tea)
            .await
            .unwrap(),
        q(10)
    );
    assert_balanced(db).await;
}

/// Opening stock with lots; sales take them first-expiry-first-out (§9, §17 case 7) and a
/// post-void puts the quantity back into the same lots.
async fn opening_stock_lots_and_fefo_sales(db: &Db) {
    let s = shop(db).await;
    let milk = Shop::v(&s.milk);
    let pcs = Shop::base(&s.milk);
    let doc = opening(
        db,
        &s,
        &[
            (milk, pcs, 5, 1_000, Some(("A", 20_270_301))),
            (milk, pcs, 3, 1_000, Some(("B", 20_270_201))),
        ],
        NOV16 - HOUR,
    )
    .await;
    assert!(doc.number.starts_with("JKT01-OS-261116-"));
    assert_eq!(doc.note.as_deref(), Some("awal"));
    assert!(doc.lines.iter().all(|l| l.lot.is_some()));
    assert_eq!(doc.lines[0].value, Some(rp(5_000)));
    // A tracked product needs an expiry.
    let mut tx = db.begin().await.unwrap();
    let err = stock::post_opening_stock(
        &mut tx,
        &NewOpeningStock {
            id: OpeningStockId::generate(),
            branch: s.till.o.jkt.id,
            location: None,
            note: None,
            user: s.till.o.manager.id,
            lines: vec![NewOpeningLine {
                variant: milk,
                unit: pcs,
                qty: q(1),
                unit_cost: rp(1),
                batch: None,
                expiry: None,
            }],
        },
        t(NOV16),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    drop(tx);

    let sale = cash_sale(&s.till, &[item(milk, pcs, 2_000, 4)], NOV16);
    let (_, sold) = post(db, &sale).await.unwrap();
    let moves = stock::document_movements(db, *sold.header.id.as_uuid())
        .await
        .unwrap();
    let lots = stock::list_lots(db, Some(s.till.o.jkt.id), None, Some(milk), None)
        .await
        .unwrap();
    // B (earlier expiry) is used up, A has 4 left.
    assert_eq!(lots.len(), 1);
    assert_eq!((lots[0].batch.as_str(), lots[0].qty), ("A", q(4)));
    assert_eq!(moves.len(), 2);
    assert_eq!((moves[0].qty, moves[1].qty), (q(-3), q(-1)));
    // Take 10 → A 4, no lot −6.
    let sale = cash_sale(&s.till, &[item(milk, pcs, 2_000, 10)], NOV16 + HOUR);
    let (_, big) = post(db, &sale).await.unwrap();
    let moves = stock::document_movements(db, *big.header.id.as_uuid())
        .await
        .unwrap();
    let got: Vec<_> = moves.iter().map(|m| (m.lot.is_some(), m.qty)).collect();
    assert_eq!(got, [(true, q(-4)), (false, q(-6))]);
    assert_eq!(
        stock::branch_on_hand(db, s.till.o.jkt.id, milk)
            .await
            .unwrap(),
        q(-6)
    );
    // The void puts the lots back.
    let v = void_of(
        &s.till,
        &big,
        s.till.session,
        RefundMethod::Cash,
        None,
        NOV16 + 2 * HOUR,
    );
    void(db, &v).await.unwrap();
    let lots = stock::list_lots(db, None, Some(s.loc()), Some(milk), None)
        .await
        .unwrap();
    assert_eq!(lots[0].qty, q(4));
    // Expiring by 2027-03-01 lists A; by 2027-02-28 nothing (B is empty).
    let soon = stock::list_lots(db, Some(s.till.o.jkt.id), None, None, Some(day(20_270_228)))
        .await
        .unwrap();
    assert!(soon.is_empty());
    let soon = stock::list_lots(db, Some(s.till.o.jkt.id), None, None, Some(day(20_270_301)))
        .await
        .unwrap();
    assert_eq!(soon.len(), 1);
    // A restock return goes back into the lots the sale line took.
    let back = return_of(
        &s.till,
        &sold,
        &[(1, 4, ReturnDisposition::Restock)],
        RefundMethod::Cash,
        None,
        Money::ZERO,
        None,
        NOV16 + 3 * HOUR,
    );
    post_return(db, &back).await.unwrap();
    let lots = stock::list_lots(db, None, Some(s.loc()), Some(milk), None)
        .await
        .unwrap();
    let got: Vec<_> = lots.iter().map(|l| (l.batch.as_str(), l.qty)).collect();
    assert_eq!(got, [("B", q(3)), ("A", q(5))]);
    // Replaying the opening stock changes nothing.
    let mut tx = db.begin().await.unwrap();
    let replay = NewOpeningStock {
        id: doc.id,
        branch: s.till.o.jkt.id,
        location: None,
        note: None,
        user: s.till.o.manager.id,
        lines: vec![],
    };
    let (w, _) = stock::post_opening_stock(&mut tx, &replay, t(NOV16))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    tx.commit().await.unwrap();
    let listed = stock::list_opening_stock(db, s.till.o.jkt.id)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_balanced(db).await;
}

fn adj_line(
    v: VariantId,
    u: UnitId,
    qty: i64,
    reason: usize,
    cost: Option<i64>,
) -> NewAdjustmentLine {
    NewAdjustmentLine {
        variant: v,
        unit: u,
        qty: q(qty),
        reason: SEED_REASON_IDS[reason],
        note: (reason == 5).then(|| "salah hitung".into()),
        unit_cost: cost.map(rp),
    }
}

async fn adjust(
    db: &Db,
    s: &Shop,
    lines: Vec<NewAdjustmentLine>,
    at: i64,
) -> Result<stock::Adjustment, StockDbError> {
    let a = NewAdjustment {
        id: AdjustmentId::generate(),
        branch: s.till.o.jkt.id,
        location: None,
        note: None,
        user: s.till.o.manager.id,
        lines,
    };
    let mut tx = db.begin().await.unwrap();
    let r = stock::post_adjustment(&mut tx, &a, t(at)).await;
    if r.is_ok() {
        tx.commit().await.unwrap();
    }
    r.map(|(_, a)| a)
}

/// Adjustments (§7, §5, §17 case 4): an increase without a cost needs a typed unit cost; the
/// reason's direction and note rules; decreases at the average.
async fn adjustments_value_and_need_a_cost(db: &Db) {
    let s = shop(db).await;
    let tea = Shop::v(&s.tea);
    let pcs = Shop::base(&s.tea);
    let kaos = Shop::v(&s.kaos);
    // No cost yet and no typed cost → invalid unit_cost.
    let err = adjust(db, &s, vec![adj_line(tea, pcs, 5, 5, None)], NOV16)
        .await
        .unwrap_err();
    assert_eq!(
        (err.code(), err.to_string().contains("unit_cost")),
        (Some("invalid_field"), true)
    );
    // A typed cost sets the first cost; the box unit converts (2 box = 48 pcs @ 24 000 / box).
    let a = adjust(
        db,
        &s,
        vec![
            adj_line(tea, pcs, 5, 5, Some(1_000)),
            NewAdjustmentLine {
                unit: s.box_unit(),
                ..adj_line(kaos, s.box_unit(), 2, 5, Some(24_000))
            },
        ],
        NOV16,
    )
    .await
    .unwrap();
    assert!(a.number.starts_with("JKT01-ADJ-261116-"));
    assert_eq!(a.lines[0].value, Some(rp(5_000)));
    assert_eq!(
        (a.lines[1].base_qty, a.lines[1].value),
        (q(48), Some(rp(48_000)))
    );
    assert_eq!(
        cost(db, &s, kaos).await,
        (q(48), rp(48_000), Some(rp(1_000)))
    );
    // Out with "Rusak" at the average; "Rusak" cannot increase; Koreksi needs a note.
    let a = adjust(db, &s, vec![adj_line(tea, pcs, -2, 0, None)], NOV16 + 1)
        .await
        .unwrap();
    assert_eq!(a.lines[0].value, Some(rp(-2_000)));
    let err = adjust(db, &s, vec![adj_line(tea, pcs, 2, 0, None)], NOV16 + 2)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("reason"));
    let mut no_note = adj_line(tea, pcs, 1, 5, None);
    no_note.note = None;
    let err = adjust(db, &s, vec![no_note], NOV16 + 3).await.unwrap_err();
    assert!(err.to_string().contains("note"));
    // An increase with a cost already: at the average, the typed cost is ignored.
    let a = adjust(
        db,
        &s,
        vec![adj_line(tea, pcs, 1, 5, Some(9_999))],
        NOV16 + 4,
    )
    .await
    .unwrap();
    assert_eq!(a.lines[0].value, Some(rp(1_000)));
    // Unknown reason, service-free checks, list and read.
    let mut bad = adj_line(tea, pcs, -1, 0, None);
    bad.reason = StockReasonId::generate();
    assert_eq!(
        adjust(db, &s, vec![bad], NOV16).await.unwrap_err().code(),
        Some("not_found")
    );
    assert_eq!(
        adjust(db, &s, vec![], NOV16).await.unwrap_err().code(),
        Some("invalid_field")
    );
    let list = stock::list_adjustments(db, s.till.o.jkt.id).await.unwrap();
    assert_eq!(list.len(), 3);
    let again = stock::get_adjustment(db, list[0].id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again, list[0]);
    // A reason used once cannot change direction.
    let mut koreksi = stock::get_stock_reason(db, SEED_REASON_IDS[0])
        .await
        .unwrap()
        .unwrap();
    koreksi.direction = ReasonDirection::Both;
    let mut tx = db.begin().await.unwrap();
    let err = stock::put_stock_reason(&mut tx, &koreksi, t(NOV16))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("direction"));
    drop(tx);
    assert_balanced(db).await;
}

/// An internal transfer moves stock and lots between locations; the cost does not change (§8).
async fn internal_transfer_keeps_the_cost(db: &Db) {
    let s = shop(db).await;
    let milk = Shop::v(&s.milk);
    let pcs = Shop::base(&s.milk);
    let tea = Shop::v(&s.tea);
    opening(
        db,
        &s,
        &[
            (milk, pcs, 3, 1_000, Some(("A", 20_270_301))),
            (tea, Shop::base(&s.tea), 3, 334, None),
        ],
        NOV16,
    )
    .await;
    let back = StockLocation {
        id: LocationId::generate(),
        branch: s.till.o.jkt.id,
        name: "Gudang".into(),
        receiving_default: false,
        archived: false,
    };
    let mut tx = db.begin().await.unwrap();
    sultan_db::org::save_location(&mut tx, &back, t(10))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = cost(db, &s, tea).await;
    let it = NewInternalTransfer {
        id: InternalTransferId::generate(),
        branch: s.till.o.jkt.id,
        from_location: s.loc(),
        to_location: back.id,
        note: None,
        user: s.till.o.manager.id,
        lines: vec![
            NewTransferLine {
                variant: milk,
                unit: pcs,
                qty: q(2),
            },
            NewTransferLine {
                variant: tea,
                unit: Shop::base(&s.tea),
                qty: q(3),
            },
        ],
    };
    let mut tx = db.begin().await.unwrap();
    let (w, doc) = stock::post_internal_transfer(&mut tx, &it, t(NOV16 + HOUR))
        .await
        .unwrap();
    let (again, _) = stock::post_internal_transfer(&mut tx, &it, t(NOV16 + HOUR))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!((w, again), (Written::Created, Written::Unchanged));
    assert!(doc.number.starts_with("JKT01-IT-"));
    assert_eq!(doc.lines[1].value, Some(rp(1_002)));
    assert_eq!(cost(db, &s, tea).await, before, "cost unchanged");
    assert_eq!(
        stock::location_on_hand(db, s.till.o.jkt.id, Some(back.id), tea)
            .await
            .unwrap(),
        q(3)
    );
    let lots = stock::list_lots(db, None, Some(back.id), Some(milk), None)
        .await
        .unwrap();
    assert_eq!((lots[0].batch.as_str(), lots[0].qty), ("A", q(2)));
    assert_eq!(
        stock::list_internal_transfers(db, s.till.o.jkt.id)
            .await
            .unwrap()
            .len(),
        1
    );
    // Same location twice is refused.
    let mut tx = db.begin().await.unwrap();
    let err = stock::post_internal_transfer(
        &mut tx,
        &NewInternalTransfer {
            id: InternalTransferId::generate(),
            to_location: s.loc(),
            ..it.clone()
        },
        t(NOV16),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    drop(tx);
    assert_balanced(db).await;
}

/// Suppliers and supplier items (§3): code cleaned and unique, archive, one preferred supplier
/// per variant.
async fn suppliers_and_items(db: &Db) {
    let s = shop(db).await;
    assert_eq!(s.supplier.code, "SUP1");
    assert_eq!(s.supplier.phone.as_deref(), Some("0812"));
    let mut tx = db.begin().await.unwrap();
    let (w, _) = stock::put_supplier(&mut tx, &s.supplier, t(30))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    let mut renamed = s.supplier.clone();
    renamed.name = "PT Baru".into();
    let (w, _) = stock::put_supplier(&mut tx, &renamed, t(31)).await.unwrap();
    assert_eq!(w, Written::Updated);
    let dup = Supplier {
        id: SupplierId::generate(),
        ..supplier("SUP1")
    };
    assert!(
        stock::put_supplier(&mut tx, &dup, t(32))
            .await
            .unwrap_err()
            .is_retryable()
    );
    tx.commit().await.unwrap();
    let other = supplier("sup2");
    let tea = Shop::v(&s.tea);
    let mut tx = db.begin().await.unwrap();
    stock::put_supplier(&mut tx, &other, t(33)).await.unwrap();
    let item = |sup, preferred| SupplierItem {
        supplier: sup,
        variant: tea,
        supplier_code: Some(" T-01 ".into()),
        last_price: None,
        last_unit: None,
        preferred,
    };
    let (w, i) = stock::put_supplier_item(&mut tx, &item(s.supplier.id, true), t(34))
        .await
        .unwrap();
    assert_eq!(
        (w, i.supplier_code.as_deref()),
        (Written::Created, Some("T-01"))
    );
    let (w, _) = stock::put_supplier_item(&mut tx, &item(other.id, true), t(35))
        .await
        .unwrap();
    assert_eq!(w, Written::Created);
    let (w, _) = stock::put_supplier_item(&mut tx, &item(other.id, true), t(36))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    tx.commit().await.unwrap();
    let items = stock::list_supplier_items(db, None, Some(tea))
        .await
        .unwrap();
    let preferred: Vec<_> = items
        .iter()
        .filter(|i| i.preferred)
        .map(|i| i.supplier)
        .collect();
    assert_eq!(preferred, [other.id], "one preferred supplier per variant");
    let mut tx = db.begin().await.unwrap();
    let err = stock::put_supplier_item(
        &mut tx,
        &SupplierItem {
            supplier: SupplierId::generate(),
            ..item(other.id, false)
        },
        t(37),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some("not_found"));
    let (w, archived) = stock::archive_supplier(&mut tx, other.id, t(38))
        .await
        .unwrap();
    assert_eq!((w, archived.active), (Written::Updated, false));
    let (w, _) = stock::archive_supplier(&mut tx, other.id, t(39))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    let err = stock::put_supplier(&mut tx, &other, t(40))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some("archived"));
    tx.commit().await.unwrap();
    assert_eq!(stock::list_suppliers(db, false).await.unwrap().len(), 1);
    assert_eq!(stock::list_suppliers(db, true).await.unwrap().len(), 2);
    assert!(
        stock::get_supplier_item(db, other.id, tea)
            .await
            .unwrap()
            .is_some()
    );
}

/// Stock reasons (§7): the six seeds, put, archive.
async fn stock_reasons_seed_put_archive(db: &Db) {
    let seeded = stock::list_stock_reasons(db, false).await.unwrap();
    let names: Vec<_> = seeded.iter().map(|r| r.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Rusak",
            "Kedaluwarsa",
            "Hilang",
            "Sampel",
            "Pemakaian sendiri",
            "Koreksi"
        ]
    );
    assert_eq!(seeded[1].id, stock::EXPIRED_REASON);
    assert!(seeded[5].note_required);
    let r = StockReasonRecord {
        id: StockReasonId::generate(),
        name: " Bonus supplier ".into(),
        direction: ReasonDirection::In,
        note_required: false,
        active: true,
        sort: 60,
        archived_at: None,
    };
    let mut tx = db.begin().await.unwrap();
    let (w, stored) = stock::put_stock_reason(&mut tx, &r, t(1)).await.unwrap();
    assert_eq!(
        (w, stored.name.as_str()),
        (Written::Created, "Bonus supplier")
    );
    let (w, _) = stock::put_stock_reason(&mut tx, &stored, t(2))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    let flipped = StockReasonRecord {
        direction: ReasonDirection::Out,
        ..stored.clone()
    };
    let (w, _) = stock::put_stock_reason(&mut tx, &flipped, t(3))
        .await
        .unwrap();
    assert_eq!(w, Written::Updated, "an unused reason may change direction");
    let err = stock::put_stock_reason(
        &mut tx,
        &StockReasonRecord {
            name: " ".into(),
            ..stored.clone()
        },
        t(4),
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), Some("invalid_field"));
    let (w, gone) = stock::archive_stock_reason(&mut tx, r.id, t(5))
        .await
        .unwrap();
    assert!(gone.archived_at.is_some() && !gone.for_rules().active);
    assert_eq!(w, Written::Updated);
    let (w, _) = stock::archive_stock_reason(&mut tx, r.id, t(6))
        .await
        .unwrap();
    assert_eq!(w, Written::Unchanged);
    assert_eq!(
        stock::put_stock_reason(&mut tx, &stored, t(7))
            .await
            .unwrap_err()
            .code(),
        Some("archived")
    );
    assert_eq!(
        stock::archive_stock_reason(&mut tx, StockReasonId::generate(), t(8))
            .await
            .unwrap_err()
            .code(),
        Some("not_found")
    );
    tx.commit().await.unwrap();
    assert_eq!(stock::list_stock_reasons(db, true).await.unwrap().len(), 7);
}

/// The inventory settings (§13) and the product's expiry fields (§9).
async fn inventory_settings_and_product_expiry(db: &Db) {
    let s = shop(db).await;
    let d = stock::get_inventory_settings(db).await.unwrap();
    assert_eq!(d, InventorySettings::default());
    let mut tx = db.begin().await.unwrap();
    assert_eq!(
        stock::put_inventory_settings(&mut tx, &d, t(1))
            .await
            .unwrap(),
        Written::Unchanged
    );
    let pkp = InventorySettings {
        pkp: true,
        cover_days: 7,
        ..d
    };
    assert_eq!(
        stock::put_inventory_settings(&mut tx, &pkp, t(2))
            .await
            .unwrap(),
        Written::Updated
    );
    let bad = InventorySettings {
        velocity_days: 0,
        ..d
    };
    assert_eq!(
        stock::put_inventory_settings(&mut tx, &bad, t(3))
            .await
            .unwrap_err()
            .code(),
        Some("invalid_field")
    );
    let e = stock::get_product_expiry(&mut tx, s.milk.draft.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        e,
        ProductExpiry {
            track_expiry: true,
            expiry_warn_days: Some(10)
        }
    );
    assert_eq!(
        stock::set_product_expiry(&mut tx, s.milk.draft.id, e, t(4))
            .await
            .unwrap(),
        Written::Unchanged
    );
    assert_eq!(
        stock::set_product_expiry(
            &mut tx,
            s.milk.draft.id,
            ProductExpiry {
                track_expiry: true,
                expiry_warn_days: Some(366)
            },
            t(5)
        )
        .await
        .unwrap_err()
        .code(),
        Some("invalid_field")
    );
    assert_eq!(
        stock::set_product_expiry(&mut tx, sultan_core::ids::ProductId::generate(), e, t(6))
            .await
            .unwrap_err()
            .code(),
        Some("not_found")
    );
    tx.commit().await.unwrap();
    assert!(stock::get_inventory_settings(db).await.unwrap().pkp);
}

/// The stock card's running balance, the on-hand list and its filters (§14, §16).
async fn stock_card_lots_and_on_hand(db: &Db) {
    let s = shop(db).await;
    let tea = Shop::v(&s.tea);
    let pcs = Shop::base(&s.tea);
    opening(db, &s, &[(tea, pcs, 10, 100, None)], NOV16 - HOUR).await;
    for (i, n) in [3, 9].iter().enumerate() {
        let sale = cash_sale(
            &s.till,
            &[item(tea, pcs, 500, *n)],
            NOV16 + i64::try_from(i).unwrap(),
        );
        post(db, &sale).await.unwrap();
    }
    let mut tx = db.begin().await.unwrap();
    let card = stock::stock_card(&mut tx, s.till.o.jkt.id, None, tea, None, None, None, 0)
        .await
        .unwrap();
    let got: Vec<_> = card.iter().map(|r| (r.movement.qty, r.balance)).collect();
    assert_eq!(got, [(q(10), q(10)), (q(-3), q(7)), (q(-9), q(-2))]);
    // The second page starts from the balance before it.
    let page = stock::stock_card(
        &mut tx,
        s.till.o.jkt.id,
        Some(s.loc()),
        tea,
        Some(t(NOV16 - 2 * HOUR)),
        Some(t(NOV16 + HOUR)),
        Some(card[0].movement.id),
        1,
    )
    .await
    .unwrap();
    assert_eq!((page.len(), page[0].balance), (1, q(7)));
    let from = stock::stock_card(
        &mut tx,
        s.till.o.jkt.id,
        None,
        tea,
        Some(t(NOV16)),
        None,
        None,
        0,
    )
    .await
    .unwrap();
    assert_eq!(from[0].balance, q(7));
    tx.commit().await.unwrap();
    let all = stock::list_on_hand(
        db,
        &OnHandFilter {
            branch: Some(s.till.o.jkt.id),
            ..OnHandFilter::default()
        },
    )
    .await
    .unwrap();
    let row = all.iter().find(|r| r.variant == tea).unwrap();
    assert_eq!(
        (row.qty, row.unit_cost, row.location),
        (q(-2), Some(rp(100)), Some(s.loc()))
    );
    let negative = stock::list_on_hand(
        db,
        &OnHandFilter {
            negative: true,
            location: Some(s.loc()),
            variant: Some(tea),
            limit: 10,
            ..OnHandFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(negative.len(), 1);
    let after = stock::list_on_hand(
        db,
        &OnHandFilter {
            after: Some((
                row.branch,
                s.loc().as_uuid().as_bytes().to_vec(),
                row.variant,
            )),
            ..OnHandFilter::default()
        },
    )
    .await
    .unwrap();
    assert!(after.iter().all(|r| r.variant != tea));
    assert_eq!(
        stock::location_on_hand(db, s.till.o.jkt.id, None, tea)
            .await
            .unwrap(),
        Qty::ZERO
    );
}

/// Spec §12, §17 case 20: the seeded roles. A Stock clerk can receive, transfer and count, not
/// adjust; a Manager has every inventory key.
async fn seeded_grants(db: &Db) {
    seed::seed_defaults(db, t(1)).await.unwrap();
    let perms = |role| async move {
        roles::get_role(db, role)
            .await
            .unwrap()
            .unwrap()
            .grants
            .iter()
            .map(sultan_core::access::Grant::permission)
            .collect::<Vec<_>>()
    };
    let clerk = perms(seed::STOCK_CLERK_ROLE).await;
    for p in [
        Permission::PurchaseReceive,
        Permission::StockTransfer,
        Permission::StockCount,
    ] {
        assert!(clerk.contains(&p), "{p}");
    }
    assert!(!clerk.contains(&Permission::StockAdjust));
    assert!(!clerk.contains(&Permission::ReportProfit));
    let manager = perms(seed::MANAGER_ROLE).await;
    for p in [
        Permission::SupplierManage,
        Permission::PurchaseOrder,
        Permission::PurchaseReceive,
        Permission::StockTransfer,
        Permission::StockCount,
        Permission::StockAdjust,
        Permission::ReportProfit,
    ] {
        assert!(manager.contains(&p), "{p}");
    }
}

/// The rebuilt ledger stays append-only and checks the new kinds' signs.
async fn ledger_is_append_only_with_new_kinds(db: &Db) {
    let s = shop(db).await;
    opening(
        db,
        &s,
        &[(Shop::v(&s.tea), Shop::base(&s.tea), 1, 1, None)],
        NOV16,
    )
    .await;
    assert!(
        raw(db, "UPDATE stock_movement SET value = 0")
            .await
            .is_err()
    );
    assert!(raw(db, "DELETE FROM stock_movement").await.is_err());
    let id = uuid::Uuid::now_v7().simple().to_string();
    for (kind, qty, doc) in [
        ("transit_loss", 1_000, "transfer"),
        ("opening", -1_000, "opening_stock"),
        ("count", 0, "opname"),
        ("teleport", 1_000, "opname"),
        ("adjust", 1_000, "invoice"),
    ] {
        let sql = format!(
            "INSERT INTO stock_movement (id, branch_id, location_id, variant_id, kind, base_qty, \
             doc_kind, doc_id, occurred_at) VALUES (X'{id}', X'{id}', NULL, X'{id}', '{kind}', \
             {qty}, '{doc}', X'{id}', 0)"
        );
        assert!(raw(db, &sql).await.is_err(), "{kind} {qty} {doc}");
    }
    assert_eq!(count(db, "stock_movement").await, 1);
}

const INVENTORY_MIGRATION: i64 = 20_261_126_000_001;

/// The migrations before 2.8, as an older release shipped them.
async fn old_migrator(backend: &str) -> sqlx::migrate::Migrator {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("migrations")
        .join(backend);
    let mut migrator = sqlx::migrate::Migrator::new(dir).await.unwrap();
    let old: Vec<_> = migrator
        .migrations
        .iter()
        .filter(|m| m.version < INVENTORY_MIGRATION)
        .cloned()
        .collect();
    migrator.migrations = Cow::Owned(old);
    migrator
}

/// Spec §17 case 13 and §4: an install from before 2.8 with 2.6 ledger rows (one without a
/// location) gets its balances and costs built from the ledger, the rows moved to the
/// receiving default, and the new grants (the Stock clerk loses `stock.adjust`).
async fn upgrade_builds_the_balance(db: &Db) {
    let o = crate::common::org(db).await;
    seed::seed_defaults(db, t(1)).await.unwrap();
    // As a 2.7 release seeded them: no inventory keys, the Stock clerk with stock.adjust.
    raw(
        db,
        "DELETE FROM role_permission WHERE perm IN ('supplier.manage', 'purchase.order', \
         'purchase.receive', 'stock.transfer', 'stock.count')",
    )
    .await
    .unwrap();
    raw(
        db,
        "INSERT INTO role_permission (role_id, perm, limit_value) \
         VALUES (X'00000000000070008000000000000004', 'stock.adjust', NULL)",
    )
    .await
    .unwrap();
    let hex = |u: uuid::Uuid| u.simple().to_string();
    let v = hex(uuid::Uuid::now_v7());
    let jkt = hex(*o.jkt.id.as_uuid());
    let bdg = hex(*o.bdg.id.as_uuid());
    let loc = hex(*o.location.id.as_uuid());
    let rows = [
        (jkt.clone(), format!("X'{loc}'"), "sale", -2_000),
        (jkt.clone(), "NULL".to_owned(), "return", 500),
        (bdg.clone(), "NULL".to_owned(), "sale", -1_000),
    ];
    for (branch, location, kind, qty) in rows {
        let id = hex(uuid::Uuid::now_v7());
        raw(
            db,
            &format!(
                "INSERT INTO stock_movement (id, branch_id, location_id, variant_id, kind, \
                 base_qty, doc_kind, doc_id, occurred_at) VALUES (X'{id}', X'{branch}', \
                 {location}, X'{v}', '{kind}', {qty}, '{}', X'{id}', 0)",
                if kind == "sale" { "sale" } else { "return" }
            ),
        )
        .await
        .unwrap();
    }
    db.migrate().await.unwrap();
    let variant: VariantId = v.parse().unwrap();
    assert_eq!(
        stock::location_on_hand(db, o.jkt.id, Some(o.location.id), variant)
            .await
            .unwrap(),
        Qty::from_thousandths(-1_500)
    );
    // BDG01 had no location: one was created for it.
    let bdg_rows = stock::list_on_hand(
        db,
        &OnHandFilter {
            branch: Some(o.bdg.id),
            ..OnHandFilter::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(bdg_rows.len(), 1);
    assert!(bdg_rows[0].location.is_some());
    assert_eq!(bdg_rows[0].unit_cost, None, "no cost from before 2.8");
    let c = stock::cost_state(db, o.jkt.id, variant).await.unwrap();
    assert_eq!((c.qty, c.reference), (Qty::from_thousandths(-1_500), None));
    assert_balanced(db).await;
    let clerk = roles::get_role(db, seed::STOCK_CLERK_ROLE)
        .await
        .unwrap()
        .unwrap();
    let keys: Vec<_> = clerk
        .grants
        .iter()
        .map(sultan_core::access::Grant::permission)
        .collect();
    assert!(keys.contains(&Permission::StockCount));
    assert!(!keys.contains(&Permission::StockAdjust));
    let manager = roles::get_role(db, seed::MANAGER_ROLE)
        .await
        .unwrap()
        .unwrap();
    assert!(
        manager
            .grants
            .iter()
            .any(|g| g.permission() == Permission::SupplierManage)
    );
    assert_eq!(count(db, "stock_reason").await, 6);
}

#[tokio::test]
async fn sqlite_upgrade_builds_the_balance() {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    let Db::Sqlite(pool) = &db else {
        unreachable!()
    };
    old_migrator("sqlite").await.run(pool).await.unwrap();
    upgrade_builds_the_balance(&db).await;
}

#[tokio::test]
async fn mysql_upgrade_builds_the_balance() {
    let Some(test_db) = MySqlTestDb::create_empty().await else {
        return;
    };
    let Db::MySql(pool) = &test_db.db else {
        unreachable!()
    };
    old_migrator("mysql").await.run(pool).await.unwrap();
    upgrade_builds_the_balance(&test_db.db).await;
    test_db.drop().await;
}
