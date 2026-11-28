//! Inventory (phase N3, spec 2.8): every inventory call of [`HttpService`] against the real
//! router (in-memory `SQLite`), and the same flow against [`FakeService`], so the back-office
//! screens of N4–N6 and the till's on-hand line see the server's answers: suppliers and items,
//! stock reasons, settings, opening stock, goods receipts (post, replay, void with the manager
//! rule, `receipt_in_use`), purchase orders, supplier returns, transfers with a transit loss,
//! internal transfers, adjustments, opnames (`opname_open`), reorder, on hand with hidden costs,
//! stock cards, lots, payables and the till's on hand.

use std::sync::Arc;

use serde_json::json;
use sultan_client::fake::{FakeService, LOC_BACK, LOC_MAIN, LOC_OTHER};
use sultan_client::stock::SEED_STOCK_REASONS;
use sultan_client::{
    AdjustmentBody, AdjustmentLineBody, BackOffice, CardQuery, ClientError, CountEntry,
    InternalTransferBody, InventorySettings, LineQty, LotQuery, MoveBody, OnHandQuery, OpeningBody,
    OpeningLineBody, OpnameBody, OrderAction, PayableQuery, Product, PurchaseOrderBody,
    PurchaseOrderLineBody, QtyLine, ReceiptBody, ReceiptLineBody, ReorderRuleBody, Service,
    StockDocQuery, StockReasonBody, SupplierBody, SupplierItemBody, SupplierReturnBody,
    SupplierReturnLineBody, TransferAction, TransferBody,
};
use sultan_core::access::BranchScope;
use sultan_core::ids::{BranchId, CompanyId, LocationId, ProductId, RegisterId, UserId};
use sultan_db::org::{Branch, Company, Register, StockLocation};
use sultan_db::users::User;
use sultan_db::{Db, devices, org, roles, seed, users};
use sultan_server::{AppState, Clock, ManualClock, NodeRole, credentials, router, token};

const STANDARD: &str = "00000000-0000-7000-8000-000000000101";
const RUSAK: &str = SEED_STOCK_REASONS[0];

fn new_id() -> String {
    ProductId::generate().to_string()
}

#[track_caller]
fn code_of<T: std::fmt::Debug>(result: Result<T, ClientError>) -> (u16, String) {
    let err = result.unwrap_err();
    let p = err.problem().unwrap_or_else(|| panic!("{err:?}"));
    (p.status, p.code.clone())
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

/// Where the flow runs: the session's branch with two locations, and another branch.
struct Places {
    branch: String,
    other: String,
    main: String,
    back: String,
    other_loc: String,
}

struct Server {
    root: String,
    device: String,
    places: Places,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// JKT01 (register R1, locations Toko and Belakang) and SBY01 (Gudang): `owner` (password),
/// the Stock clerk `gudang` (password) and Ani (cashier, PIN 1234) at JKT01.
async fn serve() -> Server {
    let db = Db::connect("sqlite::memory:").await.unwrap();
    db.migrate().await.unwrap();
    // The test clock is 2027-01-15 (expiry dates below are relative to it).
    let clock = ManualClock::new(sultan_core::time::Timestamp::from_unix_millis(
        1_800_000_000_000,
    ));
    let now = clock.now();
    seed::seed_defaults(&db, now).await.unwrap();
    let company = Company {
        id: CompanyId::generate(),
        name: "PT Sultan".to_owned(),
    };
    let jkt = branch(company.id, "JKT01");
    let sby = branch(company.id, "SBY01");
    let register = Register {
        id: RegisterId::generate(),
        branch: jkt.id,
        code: "R1".to_owned(),
        name: "Kasir 1".to_owned(),
        default_location: None,
        archived: false,
    };
    let location = |b: BranchId, name: &str, receiving: bool| StockLocation {
        id: LocationId::generate(),
        branch: b,
        name: name.to_owned(),
        receiving_default: receiving,
        archived: false,
    };
    let (main, back, other) = (
        location(jkt.id, "Toko", true),
        location(jkt.id, "Belakang", false),
        location(sby.id, "Gudang", true),
    );
    let device = token::generate();
    let mut tx = db.begin().await.unwrap();
    org::save_company(&mut tx, &company, now).await.unwrap();
    for b in [&jkt, &sby] {
        org::save_branch(&mut tx, b, now).await.unwrap();
    }
    for l in [&main, &back, &other] {
        org::save_location(&mut tx, l, now).await.unwrap();
    }
    org::save_register(&mut tx, &register, now).await.unwrap();
    for (u, role, scope) in [
        (
            user("owner", Some("owner-pass-1"), None),
            seed::OWNER_ROLE,
            BranchScope::All,
        ),
        (
            user("gudang", Some("gudang-pass-1"), None),
            seed::STOCK_CLERK_ROLE,
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
    tx.commit().await.unwrap();
    let state = AppState::new(db, NodeRole::Standalone, Arc::new(clock));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router(state)).await.unwrap();
    });
    Server {
        root: format!("http://{addr}"),
        device,
        places: Places {
            branch: jkt.id.to_string(),
            other: sby.id.to_string(),
            main: main.id.to_string(),
            back: back.id.to_string(),
            other_loc: other.id.to_string(),
        },
        task,
    }
}

/// A stock product with one variant and unit `pcs`; `(product, variant, pcs)`.
fn product(code: &str, tracked: bool) -> (Product, String, String) {
    let (pcs, variant) = (new_id(), new_id());
    let p: Product = serde_json::from_value(json!({
        "code": code, "name": format!("Produk {code}"), "type": "stock", "tax_class_id": STANDARD,
        "units": [{ "id": pcs, "name": "pcs", "kind": "integer", "factor_thousandths": 1000 }],
        "base_unit_id": pcs,
        "variants": [{ "id": variant }],
        "track_expiry": tracked,
        "expiry_warn_days": tracked.then_some(10),
    }))
    .unwrap();
    (p, variant, pcs)
}

fn qty_line(variant: &str, unit: &str, qty: i64) -> QtyLine {
    QtyLine {
        variant_id: variant.to_owned(),
        unit_id: unit.to_owned(),
        qty_thousandths: qty * 1_000,
    }
}

fn gr_line(variant: &str, unit: &str, qty: i64, price: i64) -> ReceiptLineBody {
    ReceiptLineBody {
        variant_id: variant.to_owned(),
        unit_id: unit.to_owned(),
        qty_thousandths: qty * 1_000,
        price,
        ..ReceiptLineBody::default()
    }
}

fn receipt(supplier: &str, invoice: &str, lines: Vec<ReceiptLineBody>) -> ReceiptBody {
    ReceiptBody {
        supplier_id: supplier.to_owned(),
        invoice_no: invoice.to_owned(),
        invoice_date: "2027-01-15".to_owned(),
        lines,
        ..ReceiptBody::default()
    }
}

/// The back-office flow shared by the server and the fake (see the module docs).
#[allow(clippy::too_many_lines)] // one flow, step by step
async fn flow(office: &dyn BackOffice, p: &Places) {
    let o = office
        .password_login("owner", "owner-pass-1")
        .await
        .unwrap()
        .token;
    let clerk = office
        .password_login("gudang", "gudang-pass-1")
        .await
        .unwrap()
        .token;
    let (tea_product, tea, pcs) = product("TEH", false);
    office
        .put_product(&o, &new_id(), &tea_product)
        .await
        .unwrap();
    let (milk_product, milk, milk_pcs) = product("SUSU", true);
    let stored = office
        .put_product(&o, &new_id(), &milk_product)
        .await
        .unwrap();
    assert!(stored.track_expiry);

    // HQ data.
    let settings = office.inventory_settings(&clerk).await.unwrap();
    assert_eq!(settings, InventorySettings::default());
    assert_eq!(
        code_of(office.put_inventory_settings(&clerk, &settings).await),
        (403, "permission_denied".to_owned())
    );
    office.put_inventory_settings(&o, &settings).await.unwrap();
    let supplier = new_id();
    let mut body = SupplierBody::new("sup1", "PT Pemasok");
    body.terms_days = 30;
    let s = office.put_supplier(&o, &supplier, &body).await.unwrap();
    assert_eq!(s.code, "SUP1");
    assert_eq!(
        code_of(
            office
                .put_supplier(&o, &new_id(), &SupplierBody::new("SUP1", "Dup"))
                .await
        ),
        (409, "conflict".to_owned())
    );
    assert_eq!(
        code_of(
            office
                .put_supplier(&clerk, &new_id(), &SupplierBody::new("S9", "No"))
                .await
        ),
        (403, "permission_denied".to_owned())
    );
    assert_eq!(office.suppliers(&clerk, false).await.unwrap().len(), 1);
    let gone = new_id();
    office
        .put_supplier(&o, &gone, &SupplierBody::new("OLD", "Lama"))
        .await
        .unwrap();
    assert!(office.delete_supplier(&o, &gone).await.unwrap().archived);
    assert_eq!(office.suppliers(&o, true).await.unwrap().len(), 2);
    let reasons = office.stock_reasons(&clerk, false).await.unwrap();
    assert_eq!(reasons.len(), 6);
    let bonus = new_id();
    let reason_body = StockReasonBody {
        name: "Bonus".into(),
        direction: "in".into(),
        note_required: false,
        active: true,
        sort: 99,
    };
    office
        .put_stock_reason(&o, &bonus, &reason_body)
        .await
        .unwrap();
    assert!(
        office
            .delete_stock_reason(&o, &bonus)
            .await
            .unwrap()
            .archived
    );
    assert_eq!(office.stock_reasons(&o, true).await.unwrap().len(), 7);

    // Opening stock and a receipt: the average, lots, the last price, the payable.
    let opening = office
        .put_opening_stock(
            &o,
            &new_id(),
            &OpeningBody {
                lines: vec![OpeningLineBody {
                    variant_id: tea.clone(),
                    unit_id: pcs.clone(),
                    qty_thousandths: 5_000,
                    unit_cost: 1_000,
                    ..OpeningLineBody::default()
                }],
                ..OpeningBody::default()
            },
        )
        .await
        .unwrap();
    assert!(opening.number.contains("-OS-"), "{}", opening.number);
    assert_eq!(office.opening_stocks(&o, None).await.unwrap().len(), 1);
    let mut milk_line = gr_line(&milk, &milk_pcs, 4, 3_000);
    milk_line.batch = Some("B1".into());
    let gr_id = new_id();
    let draft = office
        .put_goods_receipt(
            &clerk,
            &gr_id,
            &receipt(
                &supplier,
                "INV-1",
                vec![gr_line(&tea, &pcs, 10, 1_100), milk_line.clone()],
            ),
        )
        .await
        .unwrap();
    assert_eq!(draft.status, "draft");
    // The tracked line needs an expiry.
    assert_eq!(
        code_of(office.post_goods_receipt(&clerk, &gr_id, &new_id()).await),
        (422, "invalid_field".to_owned())
    );
    milk_line.expiry = Some("2027-01-20".into());
    office
        .put_goods_receipt(
            &clerk,
            &gr_id,
            &receipt(
                &supplier,
                "INV-1",
                vec![gr_line(&tea, &pcs, 10, 1_100), milk_line],
            ),
        )
        .await
        .unwrap();
    let posted = office
        .post_goods_receipt(&clerk, &gr_id, &new_id())
        .await
        .unwrap();
    assert_eq!(posted.status, "posted");
    assert!(posted.number.as_deref().unwrap().contains("-GR-"));
    assert_eq!((posted.net, posted.total), (Some(23_000), Some(23_000)));
    assert_eq!(posted.due_date.as_deref(), Some("2027-02-14"));
    assert_eq!(
        posted.landed, None,
        "the clerk does not see the landed cost"
    );
    assert_eq!(posted.lines[0].price, 1_100);
    assert!(posted.lines[1].lot_id.is_some());
    assert_eq!(posted.movements.len(), 2);
    assert!(posted.movements.iter().all(|m| m.value.is_none()));
    let again = office
        .post_goods_receipt(&clerk, &gr_id, &new_id())
        .await
        .unwrap();
    assert_eq!(again.number, posted.number);
    let as_owner = office.goods_receipt(&o, &gr_id).await.unwrap();
    assert_eq!(as_owner.landed, Some(Some(23_000)));
    assert_eq!(as_owner.lines[0].unit_cost, Some(Some(1_100)));
    let dup = new_id();
    office
        .put_goods_receipt(
            &o,
            &dup,
            &receipt(&supplier, "INV-1", vec![gr_line(&tea, &pcs, 1, 1)]),
        )
        .await
        .unwrap();
    assert_eq!(
        code_of(office.post_goods_receipt(&o, &dup, &new_id()).await),
        (409, "conflict".to_owned())
    );
    let items = office.supplier_items(&o, &supplier).await.unwrap();
    assert_eq!(items.len(), 2);
    assert!(items.iter().any(|i| i.last_price == Some(Some(1_100))));
    assert!(
        office
            .supplier_items(&clerk, &supplier)
            .await
            .unwrap()
            .iter()
            .all(|i| i.last_price.is_none())
    );
    let item = office
        .put_supplier_item(
            &o,
            &supplier,
            &tea,
            &SupplierItemBody {
                supplier_code: Some(" T-1 ".into()),
                preferred: true,
            },
        )
        .await
        .unwrap();
    assert_eq!(item.supplier_code.as_deref(), Some("T-1"));
    let payables = office.payables(&o, &PayableQuery::default()).await.unwrap();
    assert_eq!(payables[0].amount, 23_000);
    // On hand: 15 tea at 16 000 → 1 067 per pcs for the owner; hidden for the clerk.
    let tea_row = |token: String| {
        let tea = tea.clone();
        let main = p.main.clone();
        async move {
            office
                .on_hand(
                    &token,
                    &OnHandQuery {
                        variant_id: Some(tea),
                        location_id: Some(main),
                        ..OnHandQuery::default()
                    },
                )
                .await
                .unwrap()
                .rows
                .remove(0)
        }
    };
    let row = tea_row(o.clone()).await;
    assert_eq!(row.qty_thousandths, 15_000);
    assert_eq!(row.unit_cost, Some(Some(1_067)));
    let row = tea_row(clerk.clone()).await;
    assert_eq!((row.unit_cost, row.value), (None, None));

    // The clerk needs a manager to void; the owner voids a receipt nothing used yet.
    assert_eq!(
        code_of(
            office
                .void_goods_receipt(&clerk, &gr_id, &new_id(), None)
                .await
        ),
        (403, "needs_override".to_owned())
    );
    let small = new_id();
    office
        .put_goods_receipt(
            &o,
            &small,
            &receipt(
                &supplier,
                "INV-2",
                vec![gr_line(&milk, &milk_pcs, 1, 1_000).tap_expiry("2027-06-01")],
            ),
        )
        .await
        .unwrap();
    office
        .post_goods_receipt(&o, &small, &new_id())
        .await
        .unwrap();
    let voided = office
        .void_goods_receipt(&o, &small, &new_id(), None)
        .await
        .unwrap();
    assert_eq!(voided.status, "voided");

    // A purchase order received in part, then closed.
    let po = new_id();
    office
        .put_purchase_order(
            &o,
            &po,
            &PurchaseOrderBody {
                supplier_id: supplier.clone(),
                lines: vec![PurchaseOrderLineBody {
                    variant_id: tea.clone(),
                    unit_id: pcs.clone(),
                    qty_thousandths: 10_000,
                    price: 1_000,
                }],
                ..PurchaseOrderBody::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        code_of(
            office
                .purchase_order_action(&o, &po, OrderAction::Close, &new_id())
                .await
        ),
        (409, "conflict".to_owned())
    );
    let ordered = office
        .purchase_order_action(&o, &po, OrderAction::Order, &new_id())
        .await
        .unwrap();
    assert!(ordered.number.as_deref().unwrap().contains("-PO-"));
    let mut from_po = gr_line(&tea, &pcs, 4, 1_000);
    from_po.order_id = Some(po.clone());
    from_po.order_line_no = Some(1);
    let gr3 = new_id();
    office
        .put_goods_receipt(&o, &gr3, &receipt(&supplier, "INV-3", vec![from_po]))
        .await
        .unwrap();
    office
        .post_goods_receipt(&o, &gr3, &new_id())
        .await
        .unwrap();
    let read = office.purchase_order(&o, &po).await.unwrap();
    assert_eq!(read.lines[0].received_qty_thousandths, 4_000);
    assert_eq!(
        code_of(
            office
                .purchase_order_action(&o, &po, OrderAction::Cancel, &new_id())
                .await
        ),
        (409, "conflict".to_owned())
    );
    let closed = office
        .purchase_order_action(&o, &po, OrderAction::Close, &new_id())
        .await
        .unwrap();
    assert_eq!(closed.status, "closed");
    assert_eq!(
        office
            .purchase_orders(
                &clerk,
                &StockDocQuery {
                    status: Some("closed".into()),
                    ..StockDocQuery::default()
                }
            )
            .await
            .unwrap()[0]
            .lines[0]
            .price,
        None
    );
    assert_eq!(
        office
            .goods_receipts(&o, &StockDocQuery::default())
            .await
            .unwrap()
            .len(),
        4
    );

    // A supplier return against the first receipt's tea line.
    let ret = |qty: i64| SupplierReturnBody {
        supplier_id: supplier.clone(),
        lines: vec![SupplierReturnLineBody {
            variant_id: tea.clone(),
            unit_id: pcs.clone(),
            qty_thousandths: qty * 1_000,
            price: 1_100,
            receipt_id: Some(gr_id.clone()),
            receipt_line_no: Some(1),
        }],
        ..SupplierReturnBody::default()
    };
    let rs = new_id();
    office
        .put_supplier_return(&clerk, &rs, &ret(2))
        .await
        .unwrap();
    let rs_posted = office
        .post_supplier_return(&clerk, &rs, &new_id())
        .await
        .unwrap();
    assert_eq!(rs_posted.credit, Some(2_200));
    let rs2 = new_id();
    office.put_supplier_return(&o, &rs2, &ret(9)).await.unwrap();
    assert_eq!(
        code_of(office.post_supplier_return(&o, &rs2, &new_id()).await),
        (422, "invalid_field".to_owned())
    );
    assert_eq!(
        office.supplier_return(&o, &rs).await.unwrap().movements[0].kind,
        "supplier_return"
    );
    assert_eq!(
        office
            .supplier_returns(&o, &StockDocQuery::default())
            .await
            .unwrap()
            .len(),
        2
    );
    // The first receipt's stock was used since.
    assert_eq!(
        code_of(office.void_goods_receipt(&o, &gr_id, &new_id(), None).await),
        (409, "receipt_in_use".to_owned())
    );

    // A transfer to the other branch: request, ship 4, receive 3 (one lost at the sender).
    let tr = new_id();
    let request = office
        .put_transfer(
            &o,
            &tr,
            &TransferBody {
                from_branch_id: p.branch.clone(),
                to_branch_id: p.other.clone(),
                lines: vec![qty_line(&tea, &pcs, 4)],
                ..TransferBody::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(request.status, "requested");
    let shipped = office
        .transfer_action(
            &o,
            &tr,
            TransferAction::Ship,
            &MoveBody::default(),
            &new_id(),
        )
        .await
        .unwrap();
    assert_eq!(shipped.lines[0].shipped_qty_thousandths, Some(4_000));
    assert_eq!(
        code_of(
            office
                .transfer_action(
                    &o,
                    &tr,
                    TransferAction::Cancel,
                    &MoveBody::default(),
                    &new_id()
                )
                .await
        ),
        (409, "conflict".to_owned())
    );
    let received = office
        .transfer_action(
            &o,
            &tr,
            TransferAction::Receive,
            &MoveBody {
                location_id: Some(p.other_loc.clone()),
                lines: vec![LineQty {
                    line_no: 1,
                    qty_thousandths: 3_000,
                }],
            },
            &new_id(),
        )
        .await
        .unwrap();
    assert_eq!(received.status, "received");
    assert_eq!(received.lines[0].lost_qty_thousandths, Some(1_000));
    assert!(received.movements.iter().any(|m| m.kind == "transit_loss"));
    assert_eq!(office.transfer(&o, &tr).await.unwrap().status, "received");
    let at_other = office
        .on_hand(
            &o,
            &OnHandQuery {
                branch_id: Some(p.other.clone()),
                ..OnHandQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(at_other.rows[0].qty_thousandths, 3_000);
    assert_eq!(
        office
            .transfers(
                &o,
                &StockDocQuery {
                    status: Some("received".into()),
                    ..StockDocQuery::default()
                }
            )
            .await
            .unwrap()
            .len(),
        1
    );

    // Inside the branch, and an adjustment.
    let it = office
        .put_internal_transfer(
            &clerk,
            &new_id(),
            &InternalTransferBody {
                from_location_id: p.main.clone(),
                to_location_id: p.back.clone(),
                lines: vec![qty_line(&tea, &pcs, 1)],
                ..InternalTransferBody::default()
            },
        )
        .await
        .unwrap();
    assert!(it.number.contains("-IT-"));
    assert_eq!(office.internal_transfers(&o, None).await.unwrap().len(), 1);
    let adj = AdjustmentBody {
        lines: vec![AdjustmentLineBody {
            variant_id: tea.clone(),
            unit_id: pcs.clone(),
            qty_thousandths: -1_000,
            reason_id: RUSAK.to_owned(),
            ..AdjustmentLineBody::default()
        }],
        ..AdjustmentBody::default()
    };
    assert_eq!(
        code_of(office.put_adjustment(&clerk, &new_id(), &adj).await),
        (403, "permission_denied".to_owned())
    );
    let adj_id = new_id();
    let posted_adj = office.put_adjustment(&o, &adj_id, &adj).await.unwrap();
    assert!(posted_adj.number.contains("-ADJ-"));
    assert_eq!(
        office.adjustment(&o, &adj_id).await.unwrap().lines[0].reason_id,
        RUSAK
    );
    assert_eq!(office.adjustments(&o, None).await.unwrap().len(), 1);

    // Opname: the clerk counts, a second open count is refused, the owner posts.
    let op = new_id();
    let cycle = OpnameBody {
        scope: "cycle".into(),
        variant_ids: vec![tea.clone()],
        ..OpnameBody::default()
    };
    let started = office.start_opname(&clerk, &op, &cycle).await.unwrap();
    let snapshot = started.lines[0].snapshot_thousandths;
    assert_eq!(
        code_of(office.start_opname(&o, &new_id(), &cycle).await),
        (409, "opname_open".to_owned())
    );
    let entry = |n: i64| CountEntry {
        id: new_id(),
        variant_id: tea.clone(),
        unit_id: pcs.clone(),
        qty_thousandths: n * 1_000,
        replace: false,
    };
    office
        .add_counts(&clerk, &op, &[entry(3)], &new_id())
        .await
        .unwrap();
    let counted = office
        .add_counts(&o, &op, &[entry(2)], &new_id())
        .await
        .unwrap();
    assert_eq!(counted.lines[0].counted_thousandths, Some(5_000));
    assert_eq!(
        counted.lines[0].variance_thousandths,
        Some(5_000 - snapshot)
    );
    assert_eq!(
        code_of(office.post_opname(&clerk, &op, "leave", &new_id()).await),
        (403, "permission_denied".to_owned())
    );
    let done = office
        .post_opname(&o, &op, "leave", &new_id())
        .await
        .unwrap();
    assert_eq!(done.status, "posted");
    assert_eq!(
        office.opname(&o, &op).await.unwrap().uncounted.as_deref(),
        Some("leave")
    );
    let op2 = new_id();
    office.start_opname(&clerk, &op2, &cycle).await.unwrap();
    assert_eq!(
        office
            .cancel_opname(&clerk, &op2, &new_id())
            .await
            .unwrap()
            .status,
        "cancelled"
    );
    assert_eq!(office.opnames(&o, None).await.unwrap().len(), 2);
    let after = tea_row(o.clone()).await;
    assert_eq!(after.qty_thousandths, 5_000);

    // Reorder.
    let rule = office
        .put_reorder_rule(
            &o,
            &p.branch,
            &tea,
            &ReorderRuleBody {
                min_thousandths: 20_000,
                max_thousandths: 50_000,
            },
        )
        .await
        .unwrap();
    assert_eq!(rule.max_thousandths, 50_000);
    assert_eq!(office.reorder_rules(&o, None).await.unwrap().len(), 1);
    let low = office.low_stock(&o, None).await.unwrap();
    assert!(low.iter().any(|l| l.variant_id == tea));
    let suggested = office
        .reorder_suggestions(&o, &supplier, None)
        .await
        .unwrap();
    assert_eq!(suggested[0].variant_id, tea);
    assert!(suggested[0].base_qty_thousandths > 0);

    // Card, lots, the expiring list, filters.
    let card = office
        .stock_card(&o, &tea, &CardQuery::default())
        .await
        .unwrap();
    let last = card.rows.last().unwrap();
    let total: i64 = office
        .on_hand(
            &o,
            &OnHandQuery {
                variant_id: Some(tea.clone()),
                ..OnHandQuery::default()
            },
        )
        .await
        .unwrap()
        .rows
        .iter()
        .map(|r| r.qty_thousandths)
        .sum();
    assert_eq!(last.balance_thousandths, total);
    let page = office
        .stock_card(
            &o,
            &tea,
            &CardQuery {
                limit: Some(2),
                ..CardQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(page.next_cursor.is_some());
    let expiring = office
        .lots(
            &o,
            &LotQuery {
                expiring: true,
                ..LotQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(expiring.len(), 1);
    assert_eq!(
        (expiring[0].batch.as_str(), expiring[0].expired),
        ("B1", false)
    );
    assert_eq!(expiring[0].value, Some(Some(12_000)));
    let filtered = office
        .on_hand(
            &o,
            &OnHandQuery {
                filter: Some("expiring".into()),
                ..OnHandQuery::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(filtered.rows[0].variant_id, milk);
    assert_eq!(
        code_of(
            office
                .on_hand(
                    &o,
                    &OnHandQuery {
                        filter: Some("odd".into()),
                        ..OnHandQuery::default()
                    }
                )
                .await
        ),
        (422, "invalid_field".to_owned())
    );
}

/// A test-only shorthand to set a receipt line's expiry.
trait TapExpiry {
    fn tap_expiry(self, day: &str) -> Self;
}

impl TapExpiry for ReceiptLineBody {
    fn tap_expiry(mut self, day: &str) -> Self {
        self.expiry = Some(day.to_owned());
        self
    }
}

#[tokio::test]
async fn http_inventory_against_the_router() {
    let server = serve().await;
    let office = sultan_client::HttpService::user(&server.root);
    flow(&office, &server.places).await;
    // The till's on hand with the register's device token and a PIN session.
    let register = sultan_client::HttpService::new(&server.root, &server.device);
    let tea = office
        .on_hand(
            &office
                .password_login("owner", "owner-pass-1")
                .await
                .unwrap()
                .token,
            &OnHandQuery::default(),
        )
        .await
        .unwrap()
        .rows[0]
        .variant_id
        .clone();
    let users = register.lock_users().await.unwrap();
    let ani = users.iter().find(|u| u.display_name == "ani").unwrap();
    let pin = register.pin_login(&ani.id, "1234").await.unwrap().token;
    let till = register.till_stock(&pin, &tea).await.unwrap();
    assert_eq!(till.branch_id, server.places.branch);
    let device_only = register.till_stock("", &tea).await.unwrap();
    assert_eq!(device_only.qty_thousandths, till.qty_thousandths);
}

#[tokio::test]
async fn fake_inventory_follows_the_server() {
    let fake = FakeService::default();
    fake.add_stock_clerk();
    flow(
        &fake,
        &Places {
            branch: sultan_client::fake::BRANCH_ID.to_owned(),
            other: sultan_client::fake::OTHER_BRANCH_ID.to_owned(),
            main: LOC_MAIN.to_owned(),
            back: LOC_BACK.to_owned(),
            other_loc: LOC_OTHER.to_owned(),
        },
    )
    .await;
    let ani = fake.pin_login("u-ani", "1234").await.unwrap().token;
    assert_eq!(
        fake.till_stock(&ani, "nothing")
            .await
            .unwrap()
            .qty_thousandths,
        0
    );
    assert_eq!(
        code_of(fake.till_stock("bad", "x").await),
        (401, "invalid_session".to_owned())
    );
    assert!(!fake.calls_of("post_goods_receipt").is_empty());
}
