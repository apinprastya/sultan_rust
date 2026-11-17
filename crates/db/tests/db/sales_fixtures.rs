//! Fixtures for the sales tests (spec 2.6, phase S2): a till with an open cashier session and a
//! customer, sales, post-voids and returns built with `core::sale` like the server will.

use sultan_core::cashier_session::CashierSession;
use sultan_core::catalog::{ProductType, Qty};
use sultan_core::customer::BalanceSettings;
use sultan_core::ids::{CashierSessionId, RegisterId, ReturnId, SaleId, UnitId, UserId, VariantId};
use sultan_core::money::Money;
use sultan_core::pricing::{CashRounding, line_amount};
use sultan_core::sale::{
    BalanceContext, Discount, LineTotals, PaidSale, Payment, PriceMode, RefundInput, RefundMethod,
    ReturnDisposition, ReturnedSoFar, SaleStatus, SoldLine, TenderKind, TenderType, TotalsLine,
    VoidInput, VoidReason, compute_totals, line_refund, plan_post_void, plan_refund,
    return_movements, sale_movements,
};
use sultan_core::tax::TaxSettings;
use sultan_db::customers::{self as cust, Customer, CustomerId};
use sultan_db::sales::{
    self, CASH_TENDER_TYPE, CUSTOMER_BALANCE_TENDER_TYPE, NewReturn, NewSale, NewSaleVoid,
    RefundTender, Return, Sale, SaleDbError, SaleLine, SaleTotals, Written,
};
use sultan_db::{Db, TokenHash, cashier_sessions};
use uuid::Uuid;

use crate::catalog_fixtures::actor;
use crate::common::{Org, org, t};

/// 2026-11-16 10:00 in Jakarta (03:00 UTC).
pub const NOV16: i64 = 1_794_798_000_000;
/// One day in milliseconds.
pub const DAY: i64 = 86_400_000;

pub const fn rp(n: i64) -> Money {
    Money::from_minor(n)
}

pub fn q(whole: i64) -> Qty {
    Qty::from_thousandths(whole * 1_000)
}

/// The seeded tender types.
pub fn cash() -> TenderType {
    TenderType {
        id: CASH_TENDER_TYPE,
        name: "Cash".into(),
        kind: TenderKind::Cash,
        needs_reference: false,
        opens_drawer: true,
        active: true,
    }
}

pub fn balance() -> TenderType {
    TenderType {
        id: CUSTOMER_BALANCE_TENDER_TYPE,
        name: "Customer balance".into(),
        kind: TenderKind::CustomerBalance,
        needs_reference: false,
        opens_drawer: false,
        active: true,
    }
}

pub fn card() -> TenderType {
    TenderType {
        id: "00000000-0000-7000-8000-000000000302".parse().unwrap(),
        name: "Debit card".into(),
        kind: TenderKind::Card,
        needs_reference: true,
        opens_drawer: false,
        active: true,
    }
}

/// A till: the org, the cashier's open session on R1, and a customer.
pub struct Till {
    pub o: Org,
    pub session: CashierSessionId,
    pub customer: CustomerId,
}

pub async fn open_session(
    db: &Db,
    user: UserId,
    register: RegisterId,
    at: i64,
) -> CashierSessionId {
    let session = CashierSession::open(
        CashierSessionId::generate(),
        user,
        register,
        Money::ZERO,
        t(at),
        &[],
    )
    .unwrap();
    let mut tx = db.begin().await.unwrap();
    cashier_sessions::insert(&mut tx, &session).await.unwrap();
    tx.commit().await.unwrap();
    session.id()
}

pub async fn customer(db: &Db, o: &Org, code: &str) -> CustomerId {
    let c = Customer {
        id: Uuid::now_v7(),
        code: code.to_owned(),
        name: format!("Pelanggan {code}"),
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
        signed_up_at: t(10),
    };
    let mut tx = db.begin().await.unwrap();
    cust::put_customer(&mut tx, &c, &actor(o), t(20))
        .await
        .unwrap();
    tx.commit().await.unwrap();
    c.id
}

pub async fn till(db: &Db) -> Till {
    let o = org(db).await;
    let session = open_session(db, o.cashier.id, o.register.id, NOV16 - 3_600_000).await;
    let customer = customer(db, &o, "K7QH29XM").await;
    Till {
        o,
        session,
        customer,
    }
}

/// One item of a cart: a stock variant sold in its base unit.
#[derive(Debug, Clone, Copy)]
pub struct Item {
    pub variant: VariantId,
    pub unit: UnitId,
    pub price: Money,
    pub qty: Qty,
}

pub fn item(price: i64, qty: i64) -> Item {
    Item {
        variant: VariantId::generate(),
        unit: UnitId::generate(),
        price: rp(price),
        qty: q(qty),
    }
}

/// How a sale is paid.
#[derive(Debug, Clone, Copy)]
pub enum Pay {
    /// Cash received.
    Cash(i64),
    /// A card for this amount (reference `1234`).
    Card(i64),
    /// The customer balance.
    Balance(i64),
    /// The exchange tender of an exchange (D12).
    Exchange(i64),
}

pub fn hash_of(id: Uuid, salt: u8) -> TokenHash {
    let mut h = [salt; 32];
    h[..16].copy_from_slice(id.as_bytes());
    h
}

/// The sale lines and totals of `items` (tax-inclusive 11%), with an optional cart discount.
pub fn lines(items: &[Item], cart_discount: Option<Discount>) -> (Vec<SaleLine>, SaleTotals) {
    let totals_lines: Vec<TotalsLine> = items
        .iter()
        .map(|i| TotalsLine {
            gross: line_amount(i.price, i.qty).unwrap(),
            line_discount: None,
            cart_discountable: true,
            tax_rate_bp: 1_100,
        })
        .collect();
    let totals = compute_totals(&totals_lines, cart_discount, TaxSettings::default()).unwrap();
    let lines = items
        .iter()
        .zip(&totals.lines)
        .map(|(i, lt): (&Item, &LineTotals)| SaleLine {
            variant: i.variant,
            unit: i.unit,
            name: format!("Item {}", i.price),
            product_type: ProductType::Stock,
            factor: q(1),
            components: vec![],
            qty: i.qty,
            unit_price: i.price,
            price_mode: PriceMode::Listed,
            price_list: None,
            override_by: None,
            scale_label: None,
            totals: *lt,
        })
        .collect();
    (lines, SaleTotals::from(&totals))
}

/// Pays `total` with `pays` (the customer's `balance` for balance tenders) and saves `save` of
/// the change.
pub fn pay(total: Money, pays: &[Pay], balance: Option<Money>, save: Money) -> PaidSale {
    let mut payment = Payment::new(total, CashRounding::None).unwrap();
    let ctx = balance.map(|b| BalanceContext { balance: b });
    for p in pays {
        match *p {
            Pay::Cash(n) => payment.add(&cash(), rp(n), None, ctx).map(|_| ()),
            Pay::Card(n) => payment.add(&card(), rp(n), Some("1234"), ctx).map(|_| ()),
            Pay::Balance(n) => payment.add(&self::balance(), rp(n), None, ctx).map(|_| ()),
            Pay::Exchange(n) => payment.add_exchange(rp(n)).map(|_| ()),
        }
        .unwrap();
    }
    if save > Money::ZERO {
        payment
            .save_change(BalanceSettings::default(), ctx, save)
            .unwrap();
    }
    payment.finish().unwrap()
}

/// A sale on the till at `at`.
pub fn sale_at(
    till: &Till,
    items: &[Item],
    cart_discount: Option<Discount>,
    customer: Option<(CustomerId, Money)>,
    pays: &[Pay],
    save: Money,
    at: i64,
) -> NewSale {
    let (lines, totals) = lines(items, cart_discount);
    let payment = pay(totals.total, pays, customer.map(|c| c.1), save);
    let stock: Vec<_> = lines.iter().map(SaleLine::stock_line).collect();
    let id = SaleId::generate();
    NewSale {
        id,
        request_hash: hash_of(*id.as_uuid(), 1),
        branch: till.o.jkt.id,
        register: till.o.register.id,
        cashier_session: till.session,
        user: till.o.cashier.id,
        customer: customer.map(|c| c.0),
        tax_invoice: false,
        tax_inclusive: true,
        totals,
        movements: sale_movements(&stock).unwrap(),
        lines,
        payment,
        posted_at: t(at),
    }
}

/// A walk-in cash sale of `items` paid with exactly the total.
pub fn cash_sale(till: &Till, items: &[Item], at: i64) -> NewSale {
    let (_, totals) = lines(items, None);
    sale_at(
        till,
        items,
        None,
        None,
        &[Pay::Cash(totals.total.minor())],
        Money::ZERO,
        at,
    )
}

pub async fn post(db: &Db, sale: &NewSale) -> Result<(Written, Sale), SaleDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = sales::post_sale(&mut tx, sale, t(sale.posted_at.unix_millis())).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

/// Posts with retries, as the server does (spec 2.1 §5).
pub async fn post_retrying(db: &Db, sale: &NewSale) -> Result<(Written, Sale), SaleDbError> {
    let mut last = None;
    for attempt in 0..10_u64 {
        let mut tx = db.begin().await.map_err(SaleDbError::from)?;
        match sales::post_sale(&mut tx, sale, sale.posted_at).await {
            Ok(r) => {
                tx.commit().await?;
                return Ok(r);
            }
            Err(e) if e.is_retryable() => {
                drop(tx);
                last = Some(e);
                tokio::time::sleep(std::time::Duration::from_millis(5 * (attempt + 1))).await;
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap())
}

/// The refund tenders of a plan: cash and balance, each when not 0.
pub fn refund_tenders(cash_amount: Money, to_balance: Money) -> Vec<RefundTender> {
    let mut out = Vec::new();
    if cash_amount > Money::ZERO {
        out.push(RefundTender {
            tender_type: Some(CASH_TENDER_TYPE),
            kind: TenderKind::Cash,
            amount: cash_amount,
        });
    }
    if to_balance > Money::ZERO {
        out.push(RefundTender {
            tender_type: Some(CUSTOMER_BALANCE_TENDER_TYPE),
            kind: TenderKind::CustomerBalance,
            amount: to_balance,
        });
    }
    out
}

/// A post-void of `sale` by the manager in `session`, planned as if the sale were still posted.
pub fn void_of(
    till: &Till,
    sale: &Sale,
    session: CashierSessionId,
    method: RefundMethod,
    balance: Option<Money>,
    at: i64,
) -> NewSaleVoid {
    let plan = plan_post_void(VoidInput {
        status: SaleStatus::Posted,
        has_returns: false,
        tenders: &sale.tenders,
        method,
        balance,
        settings: BalanceSettings::default(),
        rounding: CashRounding::None,
    })
    .unwrap();
    let tenders = refund_tenders(
        plan.refund.cash,
        plan.refund
            .payment_reversed
            .saturating_add(plan.refund.to_balance),
    );
    NewSaleVoid {
        sale: sale.header.id,
        branch: till.o.jkt.id,
        register: till.o.register.id,
        cashier_session: session,
        user: till.o.manager.id,
        reason: VoidReason::WrongItem,
        method,
        plan,
        tenders,
        voided_at: t(at),
    }
}

pub async fn void(db: &Db, v: &NewSaleVoid) -> Result<Sale, SaleDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = sales::void_sale(&mut tx, v, v.voided_at).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}

/// What a return takes: `(sale line number, qty, disposition)`.
pub type Pick = (u32, i64, ReturnDisposition);

/// A return by receipt of `picks` from `sale`, refunded with `method` (`balance_paid` = the part
/// of the sale paid with the balance not reversed yet), the exchange sale optional.
#[allow(clippy::too_many_arguments)] // mirrors what the server gathers for one return
pub fn return_of(
    till: &Till,
    sale: &Sale,
    picks: &[Pick],
    method: RefundMethod,
    balance: Option<Money>,
    balance_paid: Money,
    exchange: Option<(NewSale, Money)>,
    at: i64,
) -> NewReturn {
    let mut so_far: Vec<ReturnedSoFar> = sale.lines.iter().map(|l| l.returned).collect();
    let mut lines = Vec::new();
    let mut movements = Vec::new();
    for (no, qty, disposition) in picks {
        let sold = &sale.lines[*no as usize - 1];
        let i = *no as usize - 1;
        let refund = line_refund(
            SoldLine {
                qty: sold.line.qty,
                amount: sold.line.totals.total,
            },
            so_far[i],
            q(*qty),
        )
        .unwrap();
        so_far[i].qty = so_far[i].qty.checked_add(q(*qty)).unwrap();
        so_far[i].refunded = so_far[i].refunded.saturating_add(refund);
        let line = sales::ReturnLine {
            sale_line: Some(*no),
            variant: sold.line.variant,
            unit: sold.line.unit,
            name: sold.line.name.clone(),
            product_type: sold.line.product_type,
            factor: sold.line.factor,
            components: sold.line.components.clone(),
            qty: q(*qty),
            refund,
            disposition: disposition.clone(),
        };
        movements.extend(return_movements(&line.stock_line(), disposition).unwrap());
        lines.push(line);
    }
    let total = lines
        .iter()
        .fold(Money::ZERO, |a, l| a.saturating_add(l.refund));
    let exchange_tender = exchange.as_ref().map_or(Money::ZERO, |e| e.1);
    let plan = plan_refund(RefundInput {
        amount: total.saturating_sub(exchange_tender),
        balance_paid,
        method,
        balance,
        settings: BalanceSettings::default(),
        rounding: CashRounding::None,
    })
    .unwrap();
    let mut tenders = refund_tenders(
        plan.cash,
        plan.payment_reversed.saturating_add(plan.to_balance),
    );
    if exchange_tender > Money::ZERO {
        tenders.push(RefundTender {
            tender_type: None,
            kind: TenderKind::Exchange,
            amount: exchange_tender,
        });
    }
    let id = ReturnId::generate();
    NewReturn {
        id,
        request_hash: hash_of(*id.as_uuid(), 2),
        sale: Some(sale.header.id),
        branch: till.o.jkt.id,
        register: till.o.register.id,
        cashier_session: till.session,
        user: till.o.cashier.id,
        customer: sale.header.customer,
        reason: Some("  rusak  ".into()),
        lines,
        cash_rounding: plan.cash_rounding,
        tenders,
        balance_entries: plan.balance_entries,
        movements,
        exchange: exchange.map(|e| e.0),
        posted_at: t(at),
    }
}

pub async fn post_return(db: &Db, r: &NewReturn) -> Result<(Written, Return), SaleDbError> {
    let mut tx = db.begin().await.unwrap();
    let result = sales::post_return(&mut tx, r, r.posted_at).await;
    if result.is_ok() {
        tx.commit().await.unwrap();
    }
    result
}
