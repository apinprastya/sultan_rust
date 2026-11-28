//! The fake's inventory (spec 2.8, phase N3): enough of the server's rules for the back-office
//! screens of N4–N6 and the till's on-hand line, with the maths of `sultan_core::stock` (the
//! moving average [`CostState`], receipt totals and landed cost, FEFO lots, opname variance,
//! transfer splits).
//!
//! - **Places:** the fake's branch [`BRANCH_ID`] (`JKT01`) has the receiving default
//!   [`LOC_MAIN`] and a second location [`LOC_BACK`]; [`OTHER_BRANCH_ID`] (`BDG01`) has
//!   [`LOC_OTHER`]. A branch other than these two is `409 other_node`.
//! - **Variants** come from the fake catalog ([`FakeService::seed_product`]): units and factors,
//!   `track_expiry`, `expiry_warn_days`, category and brand. Services and bundles are refused.
//! - **Who:** `owner` and `budi` hold every inventory key and `report.profit`; the Stock clerk
//!   [`STOCK_CLERK`] (`gudang` / `gudang-pass-1`, added by [`FakeService::add_stock_clerk`])
//!   receives, transfers and counts, without `report.profit`, so cost fields are left out;
//!   `kasir` reads nothing (`403`). A receipt void by someone without `stock.adjust` needs an
//!   override token for `stock.adjust` on the receipt ID ([`crate::Service::request_override`]).
//! - **Ledger:** every posting appends movements and updates the cost per (branch, variant) and
//!   the lots; on hand is the ledger's sum. Postings answer `negative_after` and queue
//!   `stock.changed` (and `transfer.changed`).
//! - **Documents:** POs (draft / order / close / cancel, received by receipt lines, closing by
//!   themselves, reopened by a void), goods receipts (draft, post with landed cost, lots, last
//!   price, payable, duplicate invoice `409 conflict`, void with `409 receipt_in_use`),
//!   supplier returns (at most received − returned), transfers (request / ship / receive with a
//!   transit loss at the sender / cancel), internal transfers, adjustments, opening stock and
//!   opnames (one open per location and variant: `409 opname_open`), reorder rules.
//!
//! Known gaps: velocity suggestions (the fake has no sales in this ledger: only rules suggest),
//! keyset paging of the stock card by movement ID only, no branch scoping of permissions.

use std::collections::{BTreeMap, HashMap};

use serde_json::json;
use sultan_core::catalog::Qty;
use sultan_core::doc_number::{BranchTimeZone, LocalDay};
use sultan_core::money::Money;
use sultan_core::stock::{
    CostState, LotStock, OpnameLine as CoreOpnameLine, ReasonDirection, ReceiptInput,
    ReceiptLineInput, SEED_REASONS, StockError, StockReason as CoreReason, TaxMode, Uncounted,
    check_adjust_line, check_rule, counted, fefo, line_value, opname_moves, per_base_price,
    receipt_totals, split_value, supplier_return_credit, to_base,
};
use sultan_core::time::Timestamp;

use super::backoffice::{BackOfficeUser, OTHER_BRANCH_ID, finish};
use super::{BRANCH_ID, FakeService, State};
use crate::catalog::Product;
use crate::stock::{
    Adjustment, AdjustmentBody, AdjustmentLine, CardPage, CardQuery, CardRow, CountEntry,
    GoodsReceipt, InternalLine, InternalTransfer, InternalTransferBody, InventorySettings, Lot,
    LotQuery, MoveBody, Movement, NegativeStock, OnHandPage, OnHandQuery, OnHandRow, OpeningBody,
    OpeningLine, OpeningStock, Opname, OpnameBody, OpnameLine, OrderAction, OrderLineRef, Payable,
    PayableQuery, PurchaseOrder, PurchaseOrderBody, PurchaseOrderLine, ReceiptBody, ReceiptLine,
    ReorderRule, ReorderRuleBody, SEED_STOCK_REASONS, StockDocQuery, StockReason, StockReasonBody,
    SuggestedLine, Supplier, SupplierBody, SupplierItem, SupplierItemBody, SupplierReturn,
    SupplierReturnBody, SupplierReturnLine, TillStock, Transfer, TransferAction, TransferBody,
    TransferLine, TravelLot,
};
use crate::{ClientError, Problem, ServerEvent};

/// The receiving default of [`BRANCH_ID`].
pub const LOC_MAIN: &str = "loc-main";
/// A second location of [`BRANCH_ID`].
pub const LOC_BACK: &str = "loc-back";
/// The receiving default of [`OTHER_BRANCH_ID`].
pub const LOC_OTHER: &str = "loc-other";
/// The Stock clerk's user ID (`gudang` / `gudang-pass-1`).
pub const STOCK_CLERK: &str = "u-gudang";

fn problem(status: u16, code: &str) -> Problem {
    Problem::new(status, code, code)
}

fn invalid(field: &str) -> ClientError {
    problem(422, "invalid_field").with("field", field).into()
}

fn denied(permission: &str) -> ClientError {
    problem(403, "permission_denied")
        .with("permission", permission)
        .into()
}

fn not_found(what: &str) -> ClientError {
    Problem::new(404, "not_found", &format!("{what} not found")).into()
}

fn conflict() -> ClientError {
    problem(409, "conflict").into()
}

/// A `core::stock` rule error as the server answers it.
fn rule(e: &StockError) -> ClientError {
    let mut p = problem(422, e.code());
    if let Some(field) = e.field() {
        p = p.with("field", field);
    }
    p.into()
}

fn q(thousandths: i64) -> Qty {
    Qty::from_thousandths(thousandths)
}

fn neg(x: Qty) -> Qty {
    q(-x.thousandths())
}

fn day_text(d: LocalDay) -> String {
    let (y, m, dd) = d.ymd();
    format!("{y:04}-{m:02}-{dd:02}")
}

fn parse_day(field: &str, value: &str) -> Result<LocalDay, ClientError> {
    let digits: String = value.trim().chars().filter(|c| *c != '-').collect();
    if value.trim().len() != 10 {
        return Err(invalid(field));
    }
    digits
        .parse::<i64>()
        .ok()
        .and_then(LocalDay::from_key)
        .ok_or_else(|| invalid(field))
}

fn opt_day(field: &str, value: Option<&String>) -> Result<Option<LocalDay>, ClientError> {
    value
        .filter(|v| !v.trim().is_empty())
        .map(|v| parse_day(field, v))
        .transpose()
}

fn branch_code(branch: &str) -> Option<&'static str> {
    match branch {
        BRANCH_ID => Some("JKT01"),
        OTHER_BRANCH_ID => Some("BDG01"),
        _ => None,
    }
}

fn receiving(branch: &str) -> Option<&'static str> {
    match branch {
        BRANCH_ID => Some(LOC_MAIN),
        OTHER_BRANCH_ID => Some(LOC_OTHER),
        _ => None,
    }
}

fn location_branch(location: &str) -> Option<&'static str> {
    match location {
        LOC_MAIN | LOC_BACK => Some(BRANCH_ID),
        LOC_OTHER => Some(OTHER_BRANCH_ID),
        _ => None,
    }
}

/// The location of a document: the given one (of the branch), else the receiving default.
fn doc_location(branch: &str, location: Option<&String>) -> Result<String, ClientError> {
    match location {
        Some(l) if location_branch(l) == Some(branch) => Ok(l.clone()),
        Some(_) => Err(not_found("location")),
        None => receiving(branch)
            .map(str::to_owned)
            .ok_or_else(|| not_found("location")),
    }
}

/// What the caller may do (spec 2.8 §12).
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // one flag per permission key
struct Role {
    id: String,
    profit: bool,
    adjust: bool,
    order: bool,
    receive: bool,
    transfer: bool,
    count: bool,
    supplier: bool,
    read: bool,
}

/// A variant as the ledger needs it.
#[derive(Debug, Clone)]
struct Info {
    product: Product,
    sku: String,
}

impl Info {
    fn factor(&self, unit: &str) -> Result<Qty, ClientError> {
        self.product
            .units
            .iter()
            .find(|u| u.id == unit)
            .map(|u| q(u.factor_thousandths))
            .ok_or_else(|| invalid("unit"))
    }

    fn base_unit_factor(&self) -> Qty {
        self.factor(&self.product.base_unit_id).unwrap_or(Qty::ONE)
    }
}

/// How a movement is valued (`db::stock::Valuation`).
#[derive(Debug, Clone, Copy)]
enum Val {
    Average,
    Receive(Money),
    Increase { typed: Option<Money>, need: bool },
    Reverse(Money),
    Fixed(Option<Money>),
}

/// Which lot a movement uses.
#[derive(Debug, Clone)]
enum LotRule {
    None,
    Fefo,
    Into(String, LocalDay),
    Lot(String),
}

#[derive(Debug, Clone)]
struct LotRow {
    id: String,
    branch: String,
    location: String,
    variant: String,
    batch: String,
    expiry: LocalDay,
    qty: Qty,
}

#[derive(Debug, Clone)]
struct OpnameState {
    opname: Opname,
    mark: usize,
    entries: BTreeMap<String, Vec<sultan_core::stock::CountEntry>>,
    seen: Vec<String>,
}

/// The fake's inventory state.
pub(super) struct Stock {
    clerks: Vec<String>,
    ledger: Vec<Movement>,
    costs: HashMap<(String, String), CostState>,
    lots: Vec<LotRow>,
    suppliers: Vec<Supplier>,
    items: Vec<SupplierItem>,
    reasons: Vec<StockReason>,
    settings: InventorySettings,
    rules: Vec<ReorderRule>,
    orders: Vec<(PurchaseOrder, bool)>,
    receipts: Vec<GoodsReceipt>,
    returns: Vec<SupplierReturn>,
    transfers: Vec<Transfer>,
    internals: Vec<InternalTransfer>,
    adjustments: Vec<Adjustment>,
    openings: Vec<OpeningStock>,
    opnames: Vec<OpnameState>,
    payables: Vec<Payable>,
    seq: HashMap<String, u32>,
    serial: u64,
}

impl Default for Stock {
    fn default() -> Self {
        Self {
            clerks: Vec::new(),
            ledger: Vec::new(),
            costs: HashMap::new(),
            lots: Vec::new(),
            suppliers: Vec::new(),
            items: Vec::new(),
            reasons: SEED_REASONS
                .iter()
                .zip(SEED_STOCK_REASONS)
                .map(|(r, id)| StockReason {
                    id: id.to_owned(),
                    name: r.name.to_owned(),
                    direction: r.direction.as_str().to_owned(),
                    note_required: r.note_required,
                    active: true,
                    sort: r.sort,
                    archived: false,
                })
                .collect(),
            settings: InventorySettings::default(),
            rules: Vec::new(),
            orders: Vec::new(),
            receipts: Vec::new(),
            returns: Vec::new(),
            transfers: Vec::new(),
            internals: Vec::new(),
            adjustments: Vec::new(),
            openings: Vec::new(),
            opnames: Vec::new(),
            payables: Vec::new(),
            seq: HashMap::new(),
            serial: 0,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Cost views: what a caller without `report.profit` does not see.

fn hide_moves(moves: &mut [Movement]) {
    for m in moves {
        m.value = None;
    }
}

fn view_order(mut o: PurchaseOrder, profit: bool) -> PurchaseOrder {
    if !profit {
        for l in &mut o.lines {
            l.price = None;
        }
    }
    o
}

fn view_receipt(mut r: GoodsReceipt, profit: bool) -> GoodsReceipt {
    if !profit {
        r.landed = None;
        for l in &mut r.lines {
            l.landed = None;
            l.unit_cost = None;
        }
        hide_moves(&mut r.movements);
    }
    r
}

fn view_return(mut r: SupplierReturn, profit: bool) -> SupplierReturn {
    if !profit {
        for l in &mut r.lines {
            l.value = None;
        }
        hide_moves(&mut r.movements);
    }
    r
}

fn view_transfer(mut t: Transfer, profit: bool) -> Transfer {
    if !profit {
        for l in &mut t.lines {
            l.shipped_value = None;
            l.received_value = None;
            l.lost_value = None;
        }
        hide_moves(&mut t.movements);
    }
    t
}

fn view_internal(mut t: InternalTransfer, profit: bool) -> InternalTransfer {
    if !profit {
        for l in &mut t.lines {
            l.value = None;
        }
        hide_moves(&mut t.movements);
    }
    t
}

fn view_adjustment(mut a: Adjustment, profit: bool) -> Adjustment {
    if !profit {
        for l in &mut a.lines {
            l.unit_cost = None;
            l.value = None;
        }
        hide_moves(&mut a.movements);
    }
    a
}

fn view_opening(mut o: OpeningStock, profit: bool) -> OpeningStock {
    if !profit {
        for l in &mut o.lines {
            l.unit_cost = None;
            l.value = None;
        }
        hide_moves(&mut o.movements);
    }
    o
}

fn view_opname(mut o: Opname, profit: bool) -> Opname {
    if !profit {
        for l in &mut o.lines {
            l.value = None;
        }
        hide_moves(&mut o.movements);
    }
    o
}

// ---------------------------------------------------------------------------------------------
// The state: who, variants, numbers, the ledger.

impl State {
    fn stock_role(&self, token: &str) -> Result<Role, ClientError> {
        if let Ok(user) = self.caller(token) {
            // A register user: the manager (reports) holds the keys; a cashier none.
            let all = user.reports;
            return Ok(Role {
                id: user.id,
                profit: all,
                adjust: all,
                order: all,
                receive: all,
                transfer: all,
                count: all,
                supplier: all,
                read: all,
            });
        }
        let user = self.bo_user(token)?;
        if user.manage || user.manage_here {
            return Ok(Role {
                id: user.id,
                profit: true,
                adjust: true,
                order: true,
                receive: true,
                transfer: true,
                count: true,
                supplier: true,
                read: true,
            });
        }
        let clerk = self.stock.clerks.contains(&user.id);
        Ok(Role {
            id: user.id,
            receive: clerk,
            transfer: clerk,
            count: clerk,
            read: clerk,
            ..Role::default()
        })
    }

    fn reader_role(&self, token: &str) -> Result<Role, ClientError> {
        let role = self.stock_role(token)?;
        if role.read {
            Ok(role)
        } else {
            Err(denied("supplier.manage"))
        }
    }

    fn info(&self, variant: &str) -> Result<Info, ClientError> {
        let product = self
            .catalog
            .product_of_variant(variant)
            .cloned()
            .ok_or_else(|| not_found("variant"))?;
        if matches!(product.product_type.as_str(), "service" | "bundle") {
            return Err(invalid("variant"));
        }
        let sku = product
            .variants
            .iter()
            .find(|v| v.id == variant)
            .map(|v| v.sku.clone())
            .unwrap_or_default();
        Ok(Info { product, sku })
    }

    fn st_today(&self) -> LocalDay {
        LocalDay::of(
            Timestamp::from_unix_millis(self.now),
            BranchTimeZone::default(),
        )
    }

    fn stock_id(&mut self, prefix: &str) -> String {
        self.stock.serial += 1;
        format!("{prefix}-{:06}", self.stock.serial)
    }

    fn st_number(&mut self, branch: &str, prefix: &str) -> Result<String, ClientError> {
        let code = branch_code(branch).ok_or_else(|| problem(409, "other_node"))?;
        let day = self.st_today();
        let key = format!("{code}-{prefix}-{}", day.yymmdd());
        let seq = self.stock.seq.entry(key.clone()).or_insert(0);
        *seq += 1;
        Ok(format!("{key}-{seq:02}"))
    }

    fn on_hand_at(&self, branch: &str, location: Option<&str>, variant: &str) -> Qty {
        q(self
            .stock
            .ledger
            .iter()
            .filter(|m| {
                m.branch_id == branch
                    && m.variant_id == variant
                    && location.is_none_or(|l| m.location_id.as_deref() == Some(l))
            })
            .map(|m| m.qty_thousandths)
            .sum())
    }

    /// Posts one movement (split per lot taken FEFO), like `db::stock::post_moves`.
    #[allow(clippy::too_many_arguments)] // the movement's fields, as the server's `Move`
    fn post(
        &mut self,
        (branch, location, variant): (&str, &str, &str),
        kind: &str,
        qty: Qty,
        val: Val,
        lot: &LotRule,
        (doc_kind, doc_id, line): (&str, &str, Option<u32>),
    ) -> Result<Vec<Movement>, ClientError> {
        let tracked = self.info(variant).is_ok_and(|i| i.product.track_expiry);
        let mut parts: Vec<(Qty, Option<String>)> = Vec::new();
        match lot {
            LotRule::Fefo if tracked && qty.thousandths() < 0 => {
                let lots: Vec<LotStock<usize>> = self
                    .stock
                    .lots
                    .iter()
                    .enumerate()
                    .filter(|(_, l)| {
                        l.branch == branch && l.location == location && l.variant == variant
                    })
                    .map(|(i, l)| LotStock {
                        lot: i,
                        expiry: l.expiry,
                        qty: l.qty,
                    })
                    .collect();
                for take in fefo(&lots, neg(qty)).map_err(|e| rule(&e))? {
                    let id = take.lot.map(|i| self.stock.lots[i].id.clone());
                    parts.push((neg(take.qty), id));
                }
            }
            LotRule::Into(batch, expiry) => {
                let found = self.stock.lots.iter().position(|l| {
                    l.branch == branch
                        && l.location == location
                        && l.variant == variant
                        && l.batch == *batch
                        && l.expiry == *expiry
                });
                let id = if let Some(i) = found {
                    self.stock.lots[i].id.clone()
                } else {
                    {
                        let id = self.stock_id("lot");
                        self.stock.lots.push(LotRow {
                            id: id.clone(),
                            branch: branch.to_owned(),
                            location: location.to_owned(),
                            variant: variant.to_owned(),
                            batch: batch.clone(),
                            expiry: *expiry,
                            qty: Qty::ZERO,
                        });
                        id
                    }
                };
                parts.push((qty, Some(id)));
            }
            LotRule::Lot(id) => parts.push((qty, Some(id.clone()))),
            LotRule::None | LotRule::Fefo => parts.push((qty, None)),
        }
        let mut out = Vec::new();
        for (part, lot_id) in parts {
            let cost = self
                .stock
                .costs
                .entry((branch.to_owned(), variant.to_owned()))
                .or_default();
            let positive = part.is_positive();
            let value = match val {
                Val::Average if positive => cost.restore(part),
                Val::Average => cost.issue(neg(part)),
                Val::Receive(v) => cost.receive(part, v).map(Some),
                Val::Increase { typed, need } => cost.increase(part, typed, need),
                Val::Reverse(v) => cost.reverse(neg(part), v).map(Some),
                Val::Fixed(v) => cost.add_fixed(part, v).map(|()| v),
            }
            .map_err(|e| rule(&e))?;
            if let Some(id) = &lot_id
                && let Some(l) = self.stock.lots.iter_mut().find(|l| l.id == *id)
            {
                l.qty = q(l.qty.thousandths() + part.thousandths());
            }
            let id = self.stock_id("mv");
            let m = Movement {
                id,
                branch_id: branch.to_owned(),
                location_id: Some(location.to_owned()),
                variant_id: variant.to_owned(),
                kind: kind.to_owned(),
                qty_thousandths: part.thousandths(),
                value: Some(value.map(Money::minor)),
                lot_id,
                doc_kind: doc_kind.to_owned(),
                doc_id: doc_id.to_owned(),
                doc_line: line,
                occurred_at: self.now,
            };
            self.stock.ledger.push(m.clone());
            out.push(m);
        }
        Ok(out)
    }

    fn doc_moves(&self, doc: &str) -> Vec<Movement> {
        self.stock
            .ledger
            .iter()
            .filter(|m| m.doc_id == doc)
            .cloned()
            .collect()
    }

    fn negative_after(&self, doc: &str) -> Vec<NegativeStock> {
        let mut out: Vec<NegativeStock> = Vec::new();
        for m in self.doc_moves(doc).iter().filter(|m| m.qty_thousandths < 0) {
            let loc = m.location_id.clone();
            if out
                .iter()
                .any(|n| n.location_id == loc && n.variant_id == m.variant_id)
            {
                continue;
            }
            let now = self.on_hand_at(&m.branch_id, loc.as_deref(), &m.variant_id);
            if now.thousandths() < 0 {
                out.push(NegativeStock {
                    location_id: loc,
                    variant_id: m.variant_id.clone(),
                    qty_thousandths: now.thousandths(),
                });
            }
        }
        out
    }

    /// Queues `stock.changed` for the variants `doc` moved, per branch.
    fn stock_changed(&mut self, doc: &str) {
        let mut by_branch: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for m in self.doc_moves(doc) {
            let v = by_branch.entry(m.branch_id).or_default();
            if !v.contains(&m.variant_id) {
                v.push(m.variant_id);
            }
        }
        for (branch, variants) in by_branch {
            self.catalog.push_event(ServerEvent {
                kind: "stock.changed".into(),
                data: json!({ "branch_id": branch, "variant_ids": variants }),
            });
        }
    }

    fn sum_value(moves: &[Movement]) -> Option<i64> {
        moves
            .iter()
            .try_fold(0_i64, |a, m| m.value.flatten().map(|v| a + v))
    }
}

// ---------------------------------------------------------------------------------------------
// HQ data: suppliers, items, reasons, settings.

impl State {
    fn st_suppliers(&self, token: &str, archived: bool) -> Result<Vec<Supplier>, ClientError> {
        self.reader_role(token)?;
        let mut list: Vec<Supplier> = self
            .stock
            .suppliers
            .iter()
            .filter(|s| archived || !s.archived)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.code.cmp(&b.code));
        Ok(list)
    }

    fn st_put_supplier(
        &mut self,
        token: &str,
        id: &str,
        body: &SupplierBody,
    ) -> Result<Supplier, ClientError> {
        if !self.stock_role(token)?.supplier {
            return Err(denied("supplier.manage"));
        }
        let (code, name) = sultan_core::stock::check_supplier(&sultan_core::stock::SupplierInput {
            code: &body.code,
            name: &body.name,
            terms_days: body.terms_days,
            lead_days: body.lead_days,
        })
        .map_err(|e| rule(&e))?;
        if self
            .stock
            .suppliers
            .iter()
            .any(|s| s.code == code && s.id != id)
        {
            return Err(conflict());
        }
        let supplier = Supplier {
            id: id.to_owned(),
            code,
            name,
            address: body.address.clone(),
            phone: body.phone.clone(),
            email: body.email.clone(),
            tax_id: body.tax_id.clone(),
            pkp: body.pkp,
            terms_days: body.terms_days,
            lead_days: body.lead_days,
            notes: body.notes.clone(),
            active: body.active,
            archived: false,
        };
        match self.stock.suppliers.iter_mut().find(|s| s.id == id) {
            Some(s) if s.archived => return Err(problem(409, "archived").into()),
            Some(s) => *s = supplier.clone(),
            None => self.stock.suppliers.push(supplier.clone()),
        }
        Ok(supplier)
    }

    fn st_delete_supplier(&mut self, token: &str, id: &str) -> Result<Supplier, ClientError> {
        if !self.stock_role(token)?.supplier {
            return Err(denied("supplier.manage"));
        }
        let s = self
            .stock
            .suppliers
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| not_found("supplier"))?;
        s.archived = true;
        s.active = false;
        Ok(s.clone())
    }

    fn st_items(&self, token: &str, supplier: &str) -> Result<Vec<SupplierItem>, ClientError> {
        let role = self.reader_role(token)?;
        Ok(self
            .stock
            .items
            .iter()
            .filter(|i| i.supplier_id == supplier)
            .map(|i| SupplierItem {
                last_price: if role.profit { i.last_price } else { None },
                ..i.clone()
            })
            .collect())
    }

    fn st_put_item(
        &mut self,
        token: &str,
        supplier: &str,
        variant: &str,
        body: &SupplierItemBody,
    ) -> Result<SupplierItem, ClientError> {
        let role = self.stock_role(token)?;
        if !role.supplier {
            return Err(denied("supplier.manage"));
        }
        if self.stock.suppliers.iter().all(|s| s.id != supplier) {
            return Err(not_found("supplier"));
        }
        let code = body
            .supplier_code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .map(str::to_owned);
        if code.as_ref().is_some_and(|c| c.chars().count() > 40) {
            return Err(invalid("supplier_code"));
        }
        if body.preferred {
            for i in self
                .stock
                .items
                .iter_mut()
                .filter(|i| i.variant_id == variant)
            {
                i.preferred = false;
            }
        }
        let item = self.item_mut(supplier, variant);
        item.supplier_code = code;
        item.preferred = body.preferred;
        Ok(item.clone())
    }

    fn item_mut(&mut self, supplier: &str, variant: &str) -> &mut SupplierItem {
        let pos = self
            .stock
            .items
            .iter()
            .position(|i| i.supplier_id == supplier && i.variant_id == variant);
        let i = pos.unwrap_or_else(|| {
            self.stock.items.push(SupplierItem {
                supplier_id: supplier.to_owned(),
                variant_id: variant.to_owned(),
                last_price: Some(None),
                ..SupplierItem::default()
            });
            self.stock.items.len() - 1
        });
        &mut self.stock.items[i]
    }

    fn st_reasons(&self, token: &str, archived: bool) -> Result<Vec<StockReason>, ClientError> {
        self.reader_role(token)?;
        let mut list: Vec<StockReason> = self
            .stock
            .reasons
            .iter()
            .filter(|r| archived || (r.active && !r.archived))
            .cloned()
            .collect();
        list.sort_by_key(|r| r.sort);
        Ok(list)
    }

    fn st_put_reason(
        &mut self,
        token: &str,
        id: &str,
        body: &StockReasonBody,
    ) -> Result<StockReason, ClientError> {
        if !self.stock_role(token)?.adjust {
            return Err(denied("stock.adjust"));
        }
        let name = sultan_core::stock::check_reason_name(&body.name).map_err(|e| rule(&e))?;
        if ReasonDirection::from_key(&body.direction).is_none() {
            return Err(invalid("direction"));
        }
        let used = self
            .stock
            .adjustments
            .iter()
            .flat_map(|a| &a.lines)
            .any(|l| l.reason_id == id);
        let reason = StockReason {
            id: id.to_owned(),
            name,
            direction: body.direction.clone(),
            note_required: body.note_required,
            active: body.active,
            sort: body.sort,
            archived: false,
        };
        match self.stock.reasons.iter_mut().find(|r| r.id == id) {
            Some(r) if r.archived => Err(problem(409, "archived").into()),
            Some(r) if r.direction != body.direction && used => Err(invalid("direction")),
            Some(r) => {
                *r = reason.clone();
                Ok(reason)
            }
            None => {
                self.stock.reasons.push(reason.clone());
                Ok(reason)
            }
        }
    }

    fn st_delete_reason(&mut self, token: &str, id: &str) -> Result<StockReason, ClientError> {
        if !self.stock_role(token)?.adjust {
            return Err(denied("stock.adjust"));
        }
        let r = self
            .stock
            .reasons
            .iter_mut()
            .find(|r| r.id == id)
            .ok_or_else(|| not_found("stock reason"))?;
        r.archived = true;
        Ok(r.clone())
    }

    fn st_put_settings(
        &mut self,
        token: &str,
        body: &InventorySettings,
    ) -> Result<InventorySettings, ClientError> {
        if !self.stock_role(token)?.adjust {
            return Err(denied("stock.adjust"));
        }
        sultan_core::stock::InventorySettings {
            pkp: body.pkp,
            standard_ppn_rate_bp: body.standard_ppn_rate_bp,
            expiry_warn_days: body.expiry_warn_days,
            velocity_days: body.velocity_days,
            cover_days: body.cover_days,
        }
        .check()
        .map_err(|e| rule(&e))?;
        self.stock.settings = *body;
        Ok(*body)
    }
}

// ---------------------------------------------------------------------------------------------
// Reads: on hand, card, lots, payables, the till.

fn cursor_key(r: &OnHandRow) -> String {
    format!(
        "{}.{}.{}",
        r.branch_id,
        r.location_id.clone().unwrap_or_default(),
        r.variant_id
    )
}

impl State {
    fn warn_days(&self, product: &Product) -> u32 {
        product
            .expiry_warn_days
            .unwrap_or(self.stock.settings.expiry_warn_days)
    }

    fn expiring_lot(&self, l: &LotRow, today: LocalDay) -> bool {
        let days = self
            .info(&l.variant)
            .map_or(self.stock.settings.expiry_warn_days, |i| {
                self.warn_days(&i.product)
            });
        l.qty.is_positive()
            && !matches!(
                sultan_core::stock::expiry_status(l.expiry, today, days),
                sultan_core::stock::ExpiryStatus::Fine
            )
    }

    fn on_order(&self, branch: &str, variant: &str) -> i64 {
        self.stock
            .orders
            .iter()
            .filter(|(o, _)| o.branch_id == branch && o.status == "ordered")
            .flat_map(|(o, _)| &o.lines)
            .filter(|l| l.variant_id == variant)
            .map(|l| (l.base_qty_thousandths - l.received_qty_thousandths).max(0))
            .sum()
    }

    fn in_transit(&self, branch: &str, variant: &str) -> i64 {
        self.stock
            .transfers
            .iter()
            .filter(|t| {
                t.status == "shipped" && (t.from_branch_id == branch || t.to_branch_id == branch)
            })
            .flat_map(|t| &t.lines)
            .filter(|l| l.variant_id == variant)
            .filter_map(|l| l.shipped_qty_thousandths)
            .sum()
    }

    fn unit_cost(&self, branch: &str, variant: &str) -> Option<Money> {
        self.stock
            .costs
            .get(&(branch.to_owned(), variant.to_owned()))
            .and_then(|c| c.unit_cost().ok().flatten())
    }

    #[allow(clippy::too_many_lines)] // one list with its filters
    fn st_on_hand(&self, token: &str, query: &OnHandQuery) -> Result<OnHandPage, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let mut keys: Vec<(String, String)> = Vec::new();
        for m in self.stock.ledger.iter().filter(|m| m.branch_id == branch) {
            let key = (
                m.location_id.clone().unwrap_or_default(),
                m.variant_id.clone(),
            );
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        keys.sort();
        let today = self.st_today();
        let mut rows = Vec::new();
        for (location, variant) in keys {
            let info = self.info(&variant).ok();
            let product = info.as_ref().map(|i| &i.product);
            let qty = self.on_hand_at(&branch, Some(&location), &variant);
            let reorder = self
                .stock
                .rules
                .iter()
                .find(|r| r.branch_id == branch && r.variant_id == variant);
            let on_order = self.on_order(&branch, &variant);
            let branch_qty = self.on_hand_at(&branch, None, &variant).thousandths();
            let keep = query.location_id.as_ref().is_none_or(|l| *l == location)
                && query.variant_id.as_ref().is_none_or(|v| *v == variant)
                && query
                    .category_id
                    .as_ref()
                    .is_none_or(|c| product.and_then(|p| p.category_id.as_ref()) == Some(c))
                && query
                    .brand_id
                    .as_ref()
                    .is_none_or(|b| product.and_then(|p| p.brand_id.as_ref()) == Some(b))
                && query.q.as_deref().map(str::trim).is_none_or(|t| {
                    t.is_empty()
                        || product.is_some_and(|p| {
                            p.name.to_lowercase().contains(&t.to_lowercase())
                                || p.code.eq_ignore_ascii_case(t)
                                || info.as_ref().is_some_and(|i| i.sku.eq_ignore_ascii_case(t))
                                || p.barcodes
                                    .iter()
                                    .any(|b| b.variant_id == variant && b.code == t)
                        })
                });
            let filtered = match query.filter.as_deref() {
                None | Some("") => true,
                Some("negative") => qty.thousandths() < 0,
                Some("low") => {
                    qty.thousandths() < 0
                        || reorder.is_some_and(|r| branch_qty + on_order <= r.min_thousandths)
                }
                Some("expiring") => self.stock.lots.iter().any(|l| {
                    l.branch == branch
                        && l.location == location
                        && l.variant == variant
                        && self.expiring_lot(l, today)
                }),
                Some(_) => return Err(invalid("filter")),
            };
            if !(keep && filtered) {
                continue;
            }
            let cost = self.unit_cost(&branch, &variant);
            let row = OnHandRow {
                branch_id: branch.clone(),
                location_id: Some(location.clone()),
                variant_id: variant.clone(),
                product_id: product.map(|p| p.id.clone()),
                product_code: product.map(|p| p.code.clone()).unwrap_or_default(),
                product_name: product.map(|p| p.name.clone()).unwrap_or_default(),
                sku: info.as_ref().map(|i| i.sku.clone()).unwrap_or_default(),
                qty_thousandths: qty.thousandths(),
                on_order_thousandths: on_order,
                in_transit_thousandths: self.in_transit(&branch, &variant),
                min_thousandths: reorder.map(|r| r.min_thousandths),
                max_thousandths: reorder.map(|r| r.max_thousandths),
                unit_cost: role.profit.then(|| cost.map(Money::minor)),
                value: role.profit.then(|| {
                    cost.map(|c| {
                        let v = i128::from(c.minor()) * i128::from(qty.thousandths());
                        i64::try_from((v + v.signum() * 500) / 1000).unwrap_or(i64::MAX)
                    })
                }),
            };
            rows.push(row);
        }
        if let Some(cursor) = &query.cursor {
            let at = rows
                .iter()
                .position(|r| cursor_key(r) == *cursor)
                .ok_or_else(|| invalid("cursor"))?;
            rows.drain(..=at);
        }
        let limit = query.limit.unwrap_or(50).clamp(1, 500) as usize;
        let full = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = rows.last().filter(|_| full).map(cursor_key);
        Ok(OnHandPage { rows, next_cursor })
    }

    fn st_card(
        &self,
        token: &str,
        variant: &str,
        query: &CardQuery,
    ) -> Result<CardPage, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let moves: Vec<&Movement> = self
            .stock
            .ledger
            .iter()
            .filter(|m| {
                m.branch_id == branch
                    && m.variant_id == variant
                    && query
                        .location_id
                        .as_ref()
                        .is_none_or(|l| m.location_id.as_ref() == Some(l))
            })
            .collect();
        let mut balance = 0;
        let mut rows = Vec::new();
        for m in moves {
            balance += m.qty_thousandths;
            if query.from.is_some_and(|f| m.occurred_at < f)
                || query.until.is_some_and(|u| m.occurred_at >= u)
                || query.cursor.as_ref().is_some_and(|c| m.id <= *c)
            {
                continue;
            }
            let mut movement = m.clone();
            if !role.profit {
                movement.value = None;
            }
            rows.push(CardRow {
                movement,
                balance_thousandths: balance,
            });
        }
        let limit = query.limit.unwrap_or(100).clamp(1, 1_000) as usize;
        let full = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = rows.last().filter(|_| full).map(|r| r.movement.id.clone());
        Ok(CardPage { rows, next_cursor })
    }

    fn st_lots(&self, token: &str, query: &LotQuery) -> Result<Vec<Lot>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let today = self.st_today();
        let mut lots: Vec<&LotRow> = self
            .stock
            .lots
            .iter()
            .filter(|l| {
                l.branch == branch
                    && l.qty.is_positive()
                    && query.location_id.as_ref().is_none_or(|x| *x == l.location)
                    && query.variant_id.as_ref().is_none_or(|x| *x == l.variant)
                    && (!query.expiring || self.expiring_lot(l, today))
            })
            .collect();
        lots.sort_by_key(|l| l.expiry);
        Ok(lots
            .into_iter()
            .map(|l| {
                let reference = self
                    .stock
                    .costs
                    .get(&(branch.clone(), l.variant.clone()))
                    .and_then(|c| c.reference);
                Lot {
                    id: l.id.clone(),
                    branch_id: l.branch.clone(),
                    location_id: l.location.clone(),
                    variant_id: l.variant.clone(),
                    batch: l.batch.clone(),
                    expiry: day_text(l.expiry),
                    expired: l.expiry < today,
                    qty_thousandths: l.qty.thousandths(),
                    value: role.profit.then(|| {
                        reference
                            .and_then(|r| r.value_of(l.qty).ok())
                            .map(Money::minor)
                    }),
                }
            })
            .collect())
    }

    fn st_payables(&self, token: &str, query: &PayableQuery) -> Result<Vec<Payable>, ClientError> {
        self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let open = match query.status.as_deref().unwrap_or("open") {
            "open" => true,
            "all" => false,
            _ => return Err(invalid("status")),
        };
        let mut list: Vec<Payable> = self
            .stock
            .payables
            .iter()
            .filter(|p| {
                p.branch_id == branch
                    && query
                        .supplier_id
                        .as_ref()
                        .is_none_or(|s| *s == p.supplier_id)
                    && (!open || (!p.paid && !p.voided))
            })
            .cloned()
            .collect();
        list.sort_by(|a, b| a.due_date.cmp(&b.due_date));
        Ok(list)
    }

    fn st_till(&self, token: &str, variant: &str) -> Result<TillStock, ClientError> {
        self.caller(token)?;
        Ok(TillStock {
            variant_id: variant.to_owned(),
            branch_id: BRANCH_ID.to_owned(),
            qty_thousandths: self.on_hand_at(BRANCH_ID, None, variant).thousandths(),
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Purchase orders, receipts, supplier returns.

fn status_ok(want: Option<&String>, status: &str) -> bool {
    want.is_none_or(|s| s.is_empty() || s == status)
}

impl State {
    fn live_supplier(&self, id: &str) -> Result<Supplier, ClientError> {
        let s = self
            .stock
            .suppliers
            .iter()
            .find(|s| s.id == id)
            .cloned()
            .ok_or_else(|| not_found("supplier"))?;
        if s.archived {
            return Err(problem(409, "archived").into());
        }
        Ok(s)
    }

    fn st_orders(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> Result<Vec<PurchaseOrder>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        Ok(self
            .stock
            .orders
            .iter()
            .rev()
            .map(|(o, _)| o)
            .filter(|o| {
                o.branch_id == branch
                    && query
                        .supplier_id
                        .as_ref()
                        .is_none_or(|s| *s == o.supplier_id)
                    && status_ok(query.status.as_ref(), &o.status)
            })
            .map(|o| view_order(o.clone(), role.profit))
            .collect())
    }

    fn st_order(&self, token: &str, id: &str) -> Result<PurchaseOrder, ClientError> {
        let role = self.reader_role(token)?;
        self.stock
            .orders
            .iter()
            .find(|(o, _)| o.id == id)
            .map(|(o, _)| view_order(o.clone(), role.profit))
            .ok_or_else(|| not_found("purchase order"))
    }

    fn st_put_order(
        &mut self,
        token: &str,
        id: &str,
        body: &PurchaseOrderBody,
    ) -> Result<PurchaseOrder, ClientError> {
        let role = self.stock_role(token)?;
        if !role.order {
            return Err(denied("purchase.order"));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        if let Some((o, _)) = self.stock.orders.iter().find(|(o, _)| o.id == id)
            && o.status != "draft"
        {
            return Err(conflict());
        }
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        self.live_supplier(&body.supplier_id)?;
        let location = doc_location(&branch, body.location_id.as_ref())?;
        let mut lines = Vec::new();
        for (i, l) in body.lines.iter().enumerate() {
            if l.qty_thousandths <= 0 {
                return Err(invalid("qty"));
            }
            if l.price < 0 {
                return Err(invalid("price"));
            }
            let factor = self.info(&l.variant_id)?.factor(&l.unit_id)?;
            let base = to_base(q(l.qty_thousandths), factor).map_err(|e| rule(&e))?;
            lines.push(PurchaseOrderLine {
                line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                price: Some(Some(l.price)),
                base_qty_thousandths: base.thousandths(),
                received_qty_thousandths: 0,
            });
        }
        let order = PurchaseOrder {
            id: id.to_owned(),
            number: None,
            branch_id: branch,
            supplier_id: body.supplier_id.clone(),
            location_id: location,
            status: "draft".into(),
            order_date: body.order_date.clone(),
            expected_date: body.expected_date.clone(),
            note: body.note.clone(),
            user_id: role.id,
            lines,
            ..PurchaseOrder::default()
        };
        match self.stock.orders.iter_mut().find(|(o, _)| o.id == id) {
            Some(slot) => slot.0 = order.clone(),
            None => self.stock.orders.push((order.clone(), false)),
        }
        Ok(order)
    }

    fn st_order_action(
        &mut self,
        token: &str,
        id: &str,
        action: OrderAction,
    ) -> Result<PurchaseOrder, ClientError> {
        if !self.stock_role(token)?.order {
            return Err(denied("purchase.order"));
        }
        let i = self
            .stock
            .orders
            .iter()
            .position(|(o, _)| o.id == id)
            .ok_or_else(|| not_found("purchase order"))?;
        let now = self.now;
        let status = self.stock.orders[i].0.status.clone();
        match (action, status.as_str()) {
            (OrderAction::Order, "ordered" | "closed")
            | (OrderAction::Close, "closed")
            | (OrderAction::Cancel, "cancelled") => {}
            (OrderAction::Order, "draft") => {
                let branch = self.stock.orders[i].0.branch_id.clone();
                let number = self.st_number(&branch, "PO")?;
                let o = &mut self.stock.orders[i].0;
                o.number = Some(number);
                o.status = "ordered".into();
                o.ordered_at = Some(now);
            }
            (OrderAction::Close, "ordered") => {
                let (o, by_hand) = &mut self.stock.orders[i];
                o.status = "closed".into();
                o.closed_at = Some(now);
                *by_hand = true;
            }
            (OrderAction::Cancel, "draft" | "ordered")
                if self.stock.orders[i]
                    .0
                    .lines
                    .iter()
                    .all(|l| l.received_qty_thousandths == 0) =>
            {
                let o = &mut self.stock.orders[i].0;
                o.status = "cancelled".into();
                o.cancelled_at = Some(now);
            }
            _ => return Err(conflict()),
        }
        Ok(self.stock.orders[i].0.clone())
    }

    fn st_receipts(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> Result<Vec<GoodsReceipt>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        Ok(self
            .stock
            .receipts
            .iter()
            .rev()
            .filter(|r| {
                r.branch_id == branch
                    && query
                        .supplier_id
                        .as_ref()
                        .is_none_or(|s| *s == r.supplier_id)
                    && status_ok(query.status.as_ref(), &r.status)
            })
            .map(|r| view_receipt(r.clone(), role.profit))
            .collect())
    }

    fn st_receipt(&self, token: &str, id: &str) -> Result<GoodsReceipt, ClientError> {
        let role = self.reader_role(token)?;
        let mut r = self
            .stock
            .receipts
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| not_found("goods receipt"))?;
        r.movements = self.doc_moves(id);
        Ok(view_receipt(r, role.profit))
    }

    fn st_put_receipt(
        &mut self,
        token: &str,
        id: &str,
        body: &ReceiptBody,
    ) -> Result<GoodsReceipt, ClientError> {
        let role = self.stock_role(token)?;
        if !role.receive {
            return Err(denied("purchase.receive"));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        if let Some(r) = self.stock.receipts.iter().find(|r| r.id == id)
            && r.status != "draft"
        {
            return Err(conflict());
        }
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        self.live_supplier(&body.supplier_id)?;
        let location = doc_location(&branch, body.location_id.as_ref())?;
        let invoice = body.invoice_no.trim();
        if invoice.is_empty() || invoice.chars().count() > 60 {
            return Err(invalid("invoice_no"));
        }
        parse_day("invoice_date", &body.invoice_date)?;
        let payment = body.payment.clone().unwrap_or_else(|| "credit".into());
        if !matches!(payment.as_str(), "credit" | "cash") {
            return Err(invalid("payment"));
        }
        let mode = body.tax_mode.clone().unwrap_or_else(|| "none".into());
        if TaxMode::from_key(&mode).is_none() {
            return Err(invalid("tax_mode"));
        }
        if body.header_discount < 0 || body.other_costs < 0 {
            return Err(invalid("header_discount"));
        }
        let mut lines = Vec::new();
        for (i, l) in body.lines.iter().enumerate() {
            if l.qty_thousandths <= 0 {
                return Err(invalid("qty"));
            }
            if l.price < 0 || l.discount < 0 {
                return Err(invalid("price"));
            }
            opt_day("expiry", l.expiry.as_ref())?;
            let factor = self.info(&l.variant_id)?.factor(&l.unit_id)?;
            let base = to_base(q(l.qty_thousandths), factor).map_err(|e| rule(&e))?;
            lines.push(ReceiptLine {
                line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                price: l.price,
                discount: l.discount,
                order_id: l.order_id.clone(),
                order_line_no: l.order_line_no,
                batch: l.batch.clone().filter(|b| !b.trim().is_empty()),
                expiry: l.expiry.clone(),
                base_qty_thousandths: base.thousandths(),
                ..ReceiptLine::default()
            });
        }
        let receipt = GoodsReceipt {
            id: id.to_owned(),
            branch_id: branch,
            supplier_id: body.supplier_id.clone(),
            location_id: location,
            status: "draft".into(),
            invoice_no: invoice.to_owned(),
            invoice_date: body.invoice_date.clone(),
            payment,
            due_date: body.due_date.clone(),
            tax_mode: mode,
            tax_rate_bp: body
                .tax_rate_bp
                .unwrap_or(self.stock.settings.standard_ppn_rate_bp),
            header_discount: body.header_discount,
            other_costs: body.other_costs,
            note: body.note.clone(),
            user_id: role.id,
            lines,
            ..GoodsReceipt::default()
        };
        match self.stock.receipts.iter_mut().find(|r| r.id == id) {
            Some(r) => *r = receipt.clone(),
            None => self.stock.receipts.push(receipt.clone()),
        }
        Ok(view_receipt(receipt, role.profit))
    }

    #[allow(clippy::too_many_lines)] // one posting, its steps in the order of spec §6
    fn st_post_receipt(&mut self, token: &str, id: &str) -> Result<GoodsReceipt, ClientError> {
        let role = self.stock_role(token)?;
        if !role.receive {
            return Err(denied("purchase.receive"));
        }
        let r = self
            .stock
            .receipts
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| not_found("goods receipt"))?;
        match r.status.as_str() {
            "posted" => {
                let mut r = r;
                r.movements = self.doc_moves(id);
                return Ok(view_receipt(r, role.profit));
            }
            "voided" => return Err(conflict()),
            _ => {}
        }
        if self.stock.receipts.iter().any(|o| {
            o.status == "posted" && o.supplier_id == r.supplier_id && o.invoice_no == r.invoice_no
        }) {
            return Err(conflict());
        }
        let mut lot_rules = Vec::new();
        for l in &r.lines {
            let tracked = self.info(&l.variant_id)?.product.track_expiry;
            let expiry = opt_day("expiry", l.expiry.as_ref())?;
            lot_rules.push(match (tracked, expiry) {
                (true, None) => return Err(invalid("expiry")),
                (true, Some(e)) => LotRule::Into(l.batch.clone().unwrap_or_default(), e),
                (false, _) => LotRule::None,
            });
        }
        let inputs: Vec<ReceiptLineInput> = r
            .lines
            .iter()
            .map(|l| ReceiptLineInput {
                qty: q(l.qty_thousandths),
                price: Money::from_minor(l.price),
                discount: Money::from_minor(l.discount),
                factor: q(l.factor_thousandths),
            })
            .collect();
        let totals = receipt_totals(&ReceiptInput {
            tax_mode: TaxMode::from_key(&r.tax_mode).unwrap_or(TaxMode::None),
            rate_bp: r.tax_rate_bp,
            header_discount: Money::from_minor(r.header_discount),
            other_costs: Money::from_minor(r.other_costs),
            pkp: self.stock.settings.pkp,
            lines: &inputs,
        })
        .map_err(|e| rule(&e))?;
        let number = self.st_number(&r.branch_id, "GR")?;
        let mut over = Vec::new();
        let mut lines = r.lines.clone();
        for ((l, t), lot) in lines.iter_mut().zip(&totals.lines).zip(&lot_rules) {
            let moves = self.post(
                (&r.branch_id, &r.location_id, &l.variant_id),
                "receipt",
                t.base_qty,
                Val::Receive(t.landed),
                lot,
                ("goods_receipt", id, Some(l.line_no)),
            )?;
            l.header_discount = Some(t.header_discount.minor());
            l.tax = Some(t.tax.minor());
            l.other_costs = Some(t.other_costs.minor());
            l.landed = Some(Some(t.landed.minor()));
            l.unit_cost = Some(
                i64::try_from(
                    (2 * i128::from(t.landed.minor()) * i128::from(l.factor_thousandths)
                        + i128::from(l.base_qty_thousandths))
                        / (2 * i128::from(l.base_qty_thousandths.max(1))),
                )
                .ok(),
            );
            l.lot_id = moves.first().and_then(|m| m.lot_id.clone());
            let per_base = per_base_price(Money::from_minor(l.price), q(l.factor_thousandths))
                .map_err(|e| rule(&e))?;
            let item = self.item_mut(&r.supplier_id, &l.variant_id);
            item.last_price = Some(Some(per_base.minor()));
            item.last_unit_id = Some(l.unit_id.clone());
            if let (Some(order), Some(no)) = (&l.order_id, l.order_line_no) {
                let (o, _) = self
                    .stock
                    .orders
                    .iter_mut()
                    .find(|(o, _)| o.id == *order && o.status == "ordered")
                    .ok_or_else(|| invalid("order_line"))?;
                let line = o
                    .lines
                    .iter_mut()
                    .find(|x| x.line_no == no && x.variant_id == l.variant_id)
                    .ok_or_else(|| invalid("order_line"))?;
                line.received_qty_thousandths += l.base_qty_thousandths;
                if line.received_qty_thousandths > line.base_qty_thousandths {
                    over.push(OrderLineRef {
                        order_id: order.clone(),
                        line_no: no,
                    });
                }
                if o.lines
                    .iter()
                    .all(|x| x.received_qty_thousandths >= x.base_qty_thousandths)
                {
                    o.status = "closed".into();
                    o.closed_at = Some(self.now);
                }
            }
        }
        let invoice_day = parse_day("invoice_date", &r.invoice_date)?;
        let terms = self
            .stock
            .suppliers
            .iter()
            .find(|s| s.id == r.supplier_id)
            .map_or(0, |s| s.terms_days);
        let due = match (
            r.payment.as_str(),
            opt_day("due_date", r.due_date.as_ref())?,
        ) {
            ("cash", _) => invoice_day,
            (_, Some(d)) => d,
            _ => invoice_day
                .add_days(i64::from(terms))
                .unwrap_or(invoice_day),
        };
        let payable = Payable {
            id: self.stock_id("payable"),
            branch_id: r.branch_id.clone(),
            supplier_id: r.supplier_id.clone(),
            receipt_id: id.to_owned(),
            receipt_number: Some(number.clone()),
            amount: totals.total.minor(),
            due_date: day_text(due),
            paid: r.payment == "cash",
            voided: false,
        };
        self.stock.payables.push(payable);
        let now = self.now;
        let pkp = self.stock.settings.pkp;
        let stored = self
            .stock
            .receipts
            .iter_mut()
            .find(|x| x.id == id)
            .ok_or_else(|| not_found("goods receipt"))?;
        stored.status = "posted".into();
        stored.number = Some(number);
        stored.due_date = Some(day_text(due));
        stored.pkp = Some(pkp);
        stored.net = Some(totals.net.minor());
        stored.tax = Some(totals.tax.minor());
        stored.total = Some(totals.total.minor());
        stored.landed = Some(Some(totals.landed.minor()));
        stored.posted_at = Some(now);
        stored.lines = lines;
        let mut answer = stored.clone();
        answer.movements = self.doc_moves(id);
        answer.over_received = over;
        answer.negative_after = self.negative_after(id);
        self.stock_changed(id);
        Ok(view_receipt(answer, role.profit))
    }

    #[allow(clippy::too_many_lines)] // the approval, the use check, then the reversal
    fn st_void_receipt(
        &mut self,
        token: &str,
        id: &str,
        override_token: Option<&str>,
    ) -> Result<GoodsReceipt, ClientError> {
        let role = self.stock_role(token)?;
        if !role.receive {
            return Err(denied("purchase.receive"));
        }
        let r = self
            .stock
            .receipts
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| not_found("goods receipt"))?;
        match r.status.as_str() {
            "voided" => {
                let mut r = r;
                r.movements = self.doc_moves(id);
                return Ok(view_receipt(r, role.profit));
            }
            "draft" => return Err(conflict()),
            _ => {}
        }
        let approved_by = if role.adjust {
            None
        } else {
            let Some(t) = override_token else {
                return Err(problem(403, "needs_override")
                    .with("permission", "stock.adjust")
                    .into());
            };
            match self.overrides.get(t).cloned() {
                Some((a, None, c, approver)) if a == "stock.adjust" && c == id => {
                    self.overrides.remove(t);
                    Some(approver)
                }
                _ => return Err(problem(403, "invalid_override").into()),
            }
        };
        let paid = self
            .stock
            .payables
            .iter()
            .any(|p| p.receipt_id == id && p.paid);
        let first = self
            .stock
            .ledger
            .iter()
            .position(|m| m.doc_id == id)
            .unwrap_or(usize::MAX);
        let used = self.stock.ledger.iter().enumerate().any(|(i, m)| {
            i > first
                && m.doc_id != id
                && m.branch_id == r.branch_id
                && r.lines.iter().any(|l| l.variant_id == m.variant_id)
        });
        if paid || used {
            return Err(problem(409, "receipt_in_use").into());
        }
        for l in r.lines.iter().rev() {
            let landed = l.landed.flatten().unwrap_or(0);
            self.post(
                (&r.branch_id, &r.location_id, &l.variant_id),
                "receipt_void",
                q(-l.base_qty_thousandths),
                Val::Reverse(Money::from_minor(landed)),
                &l.lot_id.clone().map_or(LotRule::None, LotRule::Lot),
                ("goods_receipt", id, Some(l.line_no)),
            )?;
            if let (Some(order), Some(no)) = (&l.order_id, l.order_line_no)
                && let Some((o, by_hand)) =
                    self.stock.orders.iter_mut().find(|(o, _)| o.id == *order)
            {
                if let Some(x) = o.lines.iter_mut().find(|x| x.line_no == no) {
                    x.received_qty_thousandths -= l.base_qty_thousandths;
                }
                if o.status == "closed" && !*by_hand {
                    o.status = "ordered".into();
                    o.closed_at = None;
                }
            }
        }
        for p in self
            .stock
            .payables
            .iter_mut()
            .filter(|p| p.receipt_id == id)
        {
            p.voided = true;
        }
        let now = self.now;
        let stored = self
            .stock
            .receipts
            .iter_mut()
            .find(|x| x.id == id)
            .ok_or_else(|| not_found("goods receipt"))?;
        stored.status = "voided".into();
        stored.voided_at = Some(now);
        stored.approved_by = approved_by;
        let mut answer = stored.clone();
        answer.movements = self.doc_moves(id);
        answer.negative_after = self.negative_after(id);
        self.stock_changed(id);
        Ok(view_receipt(answer, role.profit))
    }

    fn st_returns(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> Result<Vec<SupplierReturn>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        Ok(self
            .stock
            .returns
            .iter()
            .rev()
            .filter(|r| {
                r.branch_id == branch
                    && query
                        .supplier_id
                        .as_ref()
                        .is_none_or(|s| *s == r.supplier_id)
            })
            .map(|r| view_return(r.clone(), role.profit))
            .collect())
    }

    fn st_return(&self, token: &str, id: &str) -> Result<SupplierReturn, ClientError> {
        let role = self.reader_role(token)?;
        let mut r = self
            .stock
            .returns
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| not_found("supplier return"))?;
        r.movements = self.doc_moves(id);
        Ok(view_return(r, role.profit))
    }

    fn st_put_return(
        &mut self,
        token: &str,
        id: &str,
        body: &SupplierReturnBody,
    ) -> Result<SupplierReturn, ClientError> {
        let role = self.stock_role(token)?;
        if !role.receive {
            return Err(denied("purchase.receive"));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        if self
            .stock
            .returns
            .iter()
            .any(|r| r.id == id && r.status != "draft")
        {
            return Err(conflict());
        }
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        self.live_supplier(&body.supplier_id)?;
        let location = doc_location(&branch, body.location_id.as_ref())?;
        let mode = body.tax_mode.clone().unwrap_or_else(|| "none".into());
        if TaxMode::from_key(&mode).is_none() {
            return Err(invalid("tax_mode"));
        }
        let mut lines = Vec::new();
        for (i, l) in body.lines.iter().enumerate() {
            if l.qty_thousandths <= 0 {
                return Err(invalid("qty"));
            }
            if l.receipt_id.is_some() != l.receipt_line_no.is_some() {
                return Err(invalid("receipt_line_no"));
            }
            let factor = self.info(&l.variant_id)?.factor(&l.unit_id)?;
            let base = to_base(q(l.qty_thousandths), factor).map_err(|e| rule(&e))?;
            lines.push(SupplierReturnLine {
                line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                price: l.price,
                receipt_id: l.receipt_id.clone(),
                receipt_line_no: l.receipt_line_no,
                base_qty_thousandths: base.thousandths(),
                value: None,
            });
        }
        let r = SupplierReturn {
            id: id.to_owned(),
            branch_id: branch,
            supplier_id: body.supplier_id.clone(),
            location_id: location,
            status: "draft".into(),
            tax_mode: mode,
            tax_rate_bp: body
                .tax_rate_bp
                .unwrap_or(self.stock.settings.standard_ppn_rate_bp),
            note: body.note.clone(),
            user_id: role.id,
            lines,
            ..SupplierReturn::default()
        };
        match self.stock.returns.iter_mut().find(|x| x.id == id) {
            Some(x) => *x = r.clone(),
            None => self.stock.returns.push(r.clone()),
        }
        Ok(view_return(r, role.profit))
    }

    fn st_post_return(&mut self, token: &str, id: &str) -> Result<SupplierReturn, ClientError> {
        let role = self.stock_role(token)?;
        if !role.receive {
            return Err(denied("purchase.receive"));
        }
        let r = self
            .stock
            .returns
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| not_found("supplier return"))?;
        if r.status == "posted" {
            let mut r = r;
            r.movements = self.doc_moves(id);
            return Ok(view_return(r, role.profit));
        }
        let mut mode = (r.tax_mode.clone(), r.tax_rate_bp);
        let mut lots = Vec::new();
        for l in &r.lines {
            let Some((receipt, no)) = l.receipt_id.as_ref().zip(l.receipt_line_no) else {
                lots.push(LotRule::Fefo);
                continue;
            };
            let gr = self
                .stock
                .receipts
                .iter()
                .find(|x| {
                    x.id == *receipt && x.status == "posted" && x.supplier_id == r.supplier_id
                })
                .ok_or_else(|| invalid("receipt_line"))?;
            let line = gr
                .lines
                .iter()
                .find(|x| x.line_no == no && x.variant_id == l.variant_id)
                .ok_or_else(|| invalid("receipt_line"))?;
            if lots.iter().all(|x| matches!(x, LotRule::Fefo)) {
                mode = (gr.tax_mode.clone(), gr.tax_rate_bp);
            }
            let returned: i64 = self
                .stock
                .returns
                .iter()
                .filter(|x| x.status == "posted")
                .flat_map(|x| &x.lines)
                .chain(r.lines.iter().take_while(|x| x.line_no < l.line_no))
                .filter(|x| x.receipt_id.as_ref() == Some(receipt) && x.receipt_line_no == Some(no))
                .map(|x| x.base_qty_thousandths)
                .sum();
            if returned + l.base_qty_thousandths > line.base_qty_thousandths {
                return Err(invalid("qty"));
            }
            lots.push(line.lot_id.clone().map_or(LotRule::Fefo, LotRule::Lot));
        }
        let credit = supplier_return_credit(
            &r.lines
                .iter()
                .map(|l| (q(l.qty_thousandths), Money::from_minor(l.price)))
                .collect::<Vec<_>>(),
            TaxMode::from_key(&mode.0).unwrap_or(TaxMode::None),
            mode.1,
        )
        .map_err(|e| rule(&e))?;
        let number = self.st_number(&r.branch_id, "RS")?;
        let mut lines = r.lines.clone();
        for (l, lot) in lines.iter_mut().zip(&lots) {
            let moves = self.post(
                (&r.branch_id, &r.location_id, &l.variant_id),
                "supplier_return",
                q(-l.base_qty_thousandths),
                Val::Average,
                lot,
                ("supplier_return", id, Some(l.line_no)),
            )?;
            l.value = Some(State::sum_value(&moves));
        }
        let now = self.now;
        let stored = self
            .stock
            .returns
            .iter_mut()
            .find(|x| x.id == id)
            .ok_or_else(|| not_found("supplier return"))?;
        stored.status = "posted".into();
        stored.number = Some(number);
        stored.credit = Some(credit.minor());
        stored.tax_mode = mode.0;
        stored.tax_rate_bp = mode.1;
        stored.posted_at = Some(now);
        stored.lines = lines;
        let mut answer = stored.clone();
        answer.movements = self.doc_moves(id);
        answer.negative_after = self.negative_after(id);
        self.stock_changed(id);
        Ok(view_return(answer, role.profit))
    }
}

// ---------------------------------------------------------------------------------------------
// Transfers, internal transfers, adjustments, opening stock.

impl State {
    fn st_transfers(
        &self,
        token: &str,
        query: &StockDocQuery,
    ) -> Result<Vec<Transfer>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = query.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        Ok(self
            .stock
            .transfers
            .iter()
            .rev()
            .filter(|t| {
                (t.from_branch_id == branch || t.to_branch_id == branch)
                    && status_ok(query.status.as_ref(), &t.status)
            })
            .map(|t| view_transfer(t.clone(), role.profit))
            .collect())
    }

    fn st_transfer(&self, token: &str, id: &str) -> Result<Transfer, ClientError> {
        let role = self.reader_role(token)?;
        let mut t = self
            .stock
            .transfers
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| not_found("transfer"))?;
        t.movements = self.doc_moves(id);
        Ok(view_transfer(t, role.profit))
    }

    fn transfer_changed(&mut self, t: &Transfer) {
        for branch in [&t.from_branch_id, &t.to_branch_id] {
            self.catalog.push_event(ServerEvent {
                kind: "transfer.changed".into(),
                data: json!({
                    "transfer_id": t.id, "number": t.number, "status": t.status,
                    "from_branch_id": t.from_branch_id, "to_branch_id": t.to_branch_id,
                    "branch": branch,
                }),
            });
        }
    }

    fn st_put_transfer(
        &mut self,
        token: &str,
        id: &str,
        body: &TransferBody,
    ) -> Result<Transfer, ClientError> {
        let role = self.stock_role(token)?;
        if !role.transfer {
            return Err(denied("stock.transfer"));
        }
        if let Some(t) = self.stock.transfers.iter().find(|t| t.id == id) {
            return Ok(view_transfer(t.clone(), role.profit));
        }
        if body.from_branch_id == body.to_branch_id {
            return Err(invalid("to_branch"));
        }
        if branch_code(&body.from_branch_id).is_none() || branch_code(&body.to_branch_id).is_none()
        {
            return Err(problem(409, "other_node").into());
        }
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        let to_location = doc_location(&body.to_branch_id, body.to_location_id.as_ref())?;
        let mut lines = Vec::new();
        for (i, l) in body.lines.iter().enumerate() {
            if l.qty_thousandths < 0 {
                return Err(invalid("qty"));
            }
            let factor = self.info(&l.variant_id)?.factor(&l.unit_id)?;
            lines.push(TransferLine {
                line_no: u32::try_from(i + 1).unwrap_or(u32::MAX),
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                ..TransferLine::default()
            });
        }
        let creator = if body.ship_from_location_id.is_some() {
            body.from_branch_id.clone()
        } else {
            body.to_branch_id.clone()
        };
        let number = self.st_number(&creator, "TR")?;
        let t = Transfer {
            id: id.to_owned(),
            number,
            from_branch_id: body.from_branch_id.clone(),
            to_branch_id: body.to_branch_id.clone(),
            to_location_id: Some(to_location),
            status: "requested".into(),
            note: body.note.clone(),
            user_id: role.id,
            lines,
            ..Transfer::default()
        };
        self.stock.transfers.push(t.clone());
        self.transfer_changed(&t);
        if let Some(from) = &body.ship_from_location_id {
            let ship = MoveBody {
                location_id: Some(from.clone()),
                lines: Vec::new(),
            };
            return self.st_transfer_action(token, id, TransferAction::Ship, &ship);
        }
        Ok(view_transfer(t, role.profit))
    }

    #[allow(clippy::too_many_lines)] // ship, receive and cancel of one transfer
    fn st_transfer_action(
        &mut self,
        token: &str,
        id: &str,
        action: TransferAction,
        body: &MoveBody,
    ) -> Result<Transfer, ClientError> {
        let role = self.stock_role(token)?;
        if !role.transfer {
            return Err(denied("stock.transfer"));
        }
        let mut t = self
            .stock
            .transfers
            .iter()
            .find(|t| t.id == id)
            .cloned()
            .ok_or_else(|| not_found("transfer"))?;
        let qty_of = |line: &TransferLine, full: i64| -> i64 {
            body.lines
                .iter()
                .find(|x| x.line_no == line.line_no)
                .map_or(full, |x| {
                    x.qty_thousandths * line.factor_thousandths / 1_000
                })
        };
        match (action, t.status.as_str()) {
            (TransferAction::Ship, "requested") => {
                let from = doc_location(&t.from_branch_id, body.location_id.as_ref())?;
                for l in &mut t.lines {
                    let full = l.qty_thousandths * l.factor_thousandths / 1_000;
                    let shipped = qty_of(l, full);
                    if shipped < 0 {
                        return Err(invalid("qty"));
                    }
                    let mut value = Some(0);
                    if shipped > 0 {
                        let moves = self.post(
                            (&t.from_branch_id, &from, &l.variant_id),
                            "transfer_out",
                            q(-shipped),
                            Val::Average,
                            &LotRule::Fefo,
                            ("transfer", id, Some(l.line_no)),
                        )?;
                        value = State::sum_value(&moves).map(|v| -v);
                        for m in moves.iter().filter(|m| m.lot_id.is_some()) {
                            if let Some(lot) = self
                                .stock
                                .lots
                                .iter()
                                .find(|x| Some(&x.id) == m.lot_id.as_ref())
                            {
                                l.lots.push(TravelLot {
                                    batch: lot.batch.clone(),
                                    expiry: day_text(lot.expiry),
                                    qty_thousandths: -m.qty_thousandths,
                                });
                            }
                        }
                    }
                    l.shipped_qty_thousandths = Some(shipped);
                    l.shipped_value = Some(value);
                }
                t.status = "shipped".into();
                t.from_location_id = Some(from);
                t.shipped_at = Some(self.now);
            }
            (TransferAction::Receive, "shipped") => {
                let to = doc_location(
                    &t.to_branch_id,
                    body.location_id.as_ref().or(t.to_location_id.as_ref()),
                )?;
                let from = t.from_location_id.clone().unwrap_or_default();
                for l in &mut t.lines {
                    let shipped = l.shipped_qty_thousandths.unwrap_or(0);
                    let got = qty_of(l, shipped);
                    let (got_value, lost_value) = split_value(
                        q(shipped),
                        l.shipped_value.flatten().map(Money::from_minor),
                        q(got),
                    )
                    .map_err(|e| rule(&e))?;
                    if got > 0 {
                        let val = got_value.map_or(Val::Average, Val::Receive);
                        // The travelling lots go into the same batch and expiry (§9).
                        let first = l.lots.first().and_then(|x| {
                            parse_day("expiry", &x.expiry)
                                .ok()
                                .map(|d| (x.batch.clone(), d))
                        });
                        let lot = first.map_or(LotRule::None, |(b, d)| LotRule::Into(b, d));
                        self.post(
                            (&t.to_branch_id, &to, &l.variant_id),
                            "transfer_in",
                            q(got),
                            val,
                            &lot,
                            ("transfer", id, Some(l.line_no)),
                        )?;
                    }
                    let lost = shipped - got;
                    if lost > 0 {
                        for (kind, qty) in [("transfer_in", lost), ("transit_loss", -lost)] {
                            let value = lost_value.map(|v| {
                                if qty < 0 {
                                    Money::from_minor(-v.minor())
                                } else {
                                    v
                                }
                            });
                            self.post(
                                (&t.from_branch_id, &from, &l.variant_id),
                                kind,
                                q(qty),
                                Val::Fixed(value),
                                &LotRule::None,
                                ("transfer", id, Some(l.line_no)),
                            )?;
                        }
                    }
                    l.received_qty_thousandths = Some(got);
                    l.received_value = Some(got_value.map(Money::minor));
                    l.lost_qty_thousandths = Some(lost);
                    l.lost_value = Some(lost_value.map(Money::minor));
                }
                t.status = "received".into();
                t.to_location_id = Some(to);
                t.received_at = Some(self.now);
            }
            (TransferAction::Cancel, "requested") => t.status = "cancelled".into(),
            (TransferAction::Cancel, "cancelled") => {}
            _ => return Err(conflict()),
        }
        if let Some(stored) = self.stock.transfers.iter_mut().find(|x| x.id == id) {
            *stored = t.clone();
        }
        self.transfer_changed(&t);
        t.movements = self.doc_moves(id);
        t.negative_after = self.negative_after(id);
        if action != TransferAction::Cancel {
            self.stock_changed(id);
        }
        Ok(view_transfer(t, role.profit))
    }

    fn st_internals(
        &self,
        token: &str,
        branch: Option<&str>,
    ) -> Result<Vec<InternalTransfer>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = branch.unwrap_or(BRANCH_ID);
        Ok(self
            .stock
            .internals
            .iter()
            .rev()
            .filter(|t| t.branch_id == branch)
            .map(|t| view_internal(t.clone(), role.profit))
            .collect())
    }

    fn st_put_internal(
        &mut self,
        token: &str,
        id: &str,
        body: &InternalTransferBody,
    ) -> Result<InternalTransfer, ClientError> {
        let role = self.stock_role(token)?;
        if !role.transfer {
            return Err(denied("stock.transfer"));
        }
        if let Some(t) = self.stock.internals.iter().find(|t| t.id == id) {
            return Ok(view_internal(t.clone(), role.profit));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let from = doc_location(&branch, Some(&body.from_location_id))?;
        let to = doc_location(&branch, Some(&body.to_location_id))?;
        if from == to {
            return Err(invalid("to_location"));
        }
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        let number = self.st_number(&branch, "IT")?;
        let mut lines = Vec::new();
        for (i, l) in body.lines.iter().enumerate() {
            if l.qty_thousandths <= 0 {
                return Err(invalid("qty"));
            }
            let factor = self.info(&l.variant_id)?.factor(&l.unit_id)?;
            let base = to_base(q(l.qty_thousandths), factor).map_err(|e| rule(&e))?;
            let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let out = self.post(
                (&branch, &from, &l.variant_id),
                "internal_out",
                neg(base),
                Val::Average,
                &LotRule::Fefo,
                ("internal_transfer", id, Some(no)),
            )?;
            for m in &out {
                let lot = m
                    .lot_id
                    .as_ref()
                    .and_then(|x| self.stock.lots.iter().find(|y| y.id == *x))
                    .map_or(LotRule::None, |y| LotRule::Into(y.batch.clone(), y.expiry));
                self.post(
                    (&branch, &to, &l.variant_id),
                    "internal_in",
                    q(-m.qty_thousandths),
                    Val::Fixed(m.value.flatten().map(|v| Money::from_minor(-v))),
                    &lot,
                    ("internal_transfer", id, Some(no)),
                )?;
            }
            lines.push(InternalLine {
                line_no: no,
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                base_qty_thousandths: base.thousandths(),
                value: Some(State::sum_value(&out).map(|v| -v)),
            });
        }
        let t = InternalTransfer {
            id: id.to_owned(),
            number,
            branch_id: branch,
            from_location_id: from,
            to_location_id: to,
            note: body.note.clone(),
            user_id: role.id,
            posted_at: self.now,
            lines,
            ..InternalTransfer::default()
        };
        self.stock.internals.push(t.clone());
        let mut answer = t;
        answer.movements = self.doc_moves(id);
        answer.negative_after = self.negative_after(id);
        self.stock_changed(id);
        Ok(view_internal(answer, role.profit))
    }

    fn st_adjustments(
        &self,
        token: &str,
        branch: Option<&str>,
    ) -> Result<Vec<Adjustment>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = branch.unwrap_or(BRANCH_ID);
        Ok(self
            .stock
            .adjustments
            .iter()
            .rev()
            .filter(|a| a.branch_id == branch)
            .map(|a| view_adjustment(a.clone(), role.profit))
            .collect())
    }

    fn st_adjustment(&self, token: &str, id: &str) -> Result<Adjustment, ClientError> {
        let role = self.reader_role(token)?;
        let mut a = self
            .stock
            .adjustments
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| not_found("adjustment"))?;
        a.movements = self.doc_moves(id);
        Ok(view_adjustment(a, role.profit))
    }

    fn st_put_adjustment(
        &mut self,
        token: &str,
        id: &str,
        body: &AdjustmentBody,
    ) -> Result<Adjustment, ClientError> {
        let role = self.stock_role(token)?;
        if !role.adjust {
            return Err(denied("stock.adjust"));
        }
        if let Some(a) = self.stock.adjustments.iter().find(|a| a.id == id) {
            return Ok(view_adjustment(a.clone(), role.profit));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let location = doc_location(&branch, body.location_id.as_ref())?;
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        // Check every line first, so a refused line posts nothing.
        let mut checked = Vec::new();
        for l in &body.lines {
            let reason = self
                .stock
                .reasons
                .iter()
                .find(|r| r.id == l.reason_id)
                .ok_or_else(|| not_found("stock reason"))?;
            let core = CoreReason {
                direction: ReasonDirection::from_key(&reason.direction)
                    .unwrap_or(ReasonDirection::Both),
                active: reason.active && !reason.archived,
                note_required: reason.note_required,
            };
            let note = check_adjust_line(q(l.qty_thousandths), core, l.note.as_deref())
                .map_err(|e| rule(&e))?;
            if l.unit_cost.is_some_and(|c| c < 0) {
                return Err(invalid("unit_cost"));
            }
            let factor = self.info(&l.variant_id)?.factor(&l.unit_id)?;
            let base = to_base(q(l.qty_thousandths), factor).map_err(|e| rule(&e))?;
            if base.is_positive()
                && l.unit_cost.is_none()
                && self.unit_cost(&branch, &l.variant_id).is_none()
            {
                return Err(invalid("unit_cost"));
            }
            checked.push((l, note, factor, base));
        }
        let number = self.st_number(&branch, "ADJ")?;
        let mut lines = Vec::new();
        for (i, (l, note, factor, base)) in checked.into_iter().enumerate() {
            let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let typed = l
                .unit_cost
                .map(|c| line_value(q(l.qty_thousandths.abs()), Money::from_minor(c)))
                .transpose()
                .map_err(|e| rule(&e))?;
            let increase = base.is_positive();
            let moves = self.post(
                (&branch, &location, &l.variant_id),
                "adjust",
                base,
                if increase {
                    Val::Increase { typed, need: true }
                } else {
                    Val::Average
                },
                if increase {
                    &LotRule::None
                } else {
                    &LotRule::Fefo
                },
                ("adjustment", id, Some(no)),
            )?;
            lines.push(AdjustmentLine {
                line_no: no,
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                reason_id: l.reason_id.clone(),
                note,
                unit_cost: Some(l.unit_cost),
                base_qty_thousandths: base.thousandths(),
                value: Some(State::sum_value(&moves)),
            });
        }
        let a = Adjustment {
            id: id.to_owned(),
            number,
            branch_id: branch,
            location_id: location,
            note: body.note.clone(),
            user_id: role.id,
            posted_at: self.now,
            lines,
            ..Adjustment::default()
        };
        self.stock.adjustments.push(a.clone());
        let mut answer = a;
        answer.movements = self.doc_moves(id);
        answer.negative_after = self.negative_after(id);
        self.stock_changed(id);
        Ok(view_adjustment(answer, role.profit))
    }

    fn st_openings(
        &self,
        token: &str,
        branch: Option<&str>,
    ) -> Result<Vec<OpeningStock>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = branch.unwrap_or(BRANCH_ID);
        Ok(self
            .stock
            .openings
            .iter()
            .rev()
            .filter(|o| o.branch_id == branch)
            .map(|o| view_opening(o.clone(), role.profit))
            .collect())
    }

    fn st_put_opening(
        &mut self,
        token: &str,
        id: &str,
        body: &OpeningBody,
    ) -> Result<OpeningStock, ClientError> {
        let role = self.stock_role(token)?;
        if !role.adjust {
            return Err(denied("stock.adjust"));
        }
        if let Some(o) = self.stock.openings.iter().find(|o| o.id == id) {
            return Ok(view_opening(o.clone(), role.profit));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let location = doc_location(&branch, body.location_id.as_ref())?;
        if body.lines.is_empty() {
            return Err(invalid("lines"));
        }
        let mut checked = Vec::new();
        for l in &body.lines {
            if l.qty_thousandths <= 0 {
                return Err(invalid("qty"));
            }
            if l.unit_cost < 0 {
                return Err(invalid("unit_cost"));
            }
            let info = self.info(&l.variant_id)?;
            let factor = info.factor(&l.unit_id)?;
            let expiry = opt_day("expiry", l.expiry.as_ref())?;
            let lot = sultan_core::stock::check_lot(
                info.product.track_expiry,
                l.batch.as_deref(),
                expiry,
            )
            .map_err(|e| rule(&e))?;
            let base = to_base(q(l.qty_thousandths), factor).map_err(|e| rule(&e))?;
            let value = line_value(q(l.qty_thousandths), Money::from_minor(l.unit_cost))
                .map_err(|e| rule(&e))?;
            checked.push((l, factor, base, lot, value));
        }
        let number = self.st_number(&branch, "OS")?;
        let mut lines = Vec::new();
        for (i, (l, factor, base, lot, value)) in checked.into_iter().enumerate() {
            let no = u32::try_from(i + 1).unwrap_or(u32::MAX);
            let rule_ = lot
                .clone()
                .map_or(LotRule::None, |(b, e)| LotRule::Into(b, e));
            let moves = self.post(
                (&branch, &location, &l.variant_id),
                "opening",
                base,
                Val::Receive(value),
                &rule_,
                ("opening_stock", id, Some(no)),
            )?;
            lines.push(OpeningLine {
                line_no: no,
                variant_id: l.variant_id.clone(),
                unit_id: l.unit_id.clone(),
                factor_thousandths: factor.thousandths(),
                qty_thousandths: l.qty_thousandths,
                unit_cost: Some(Some(l.unit_cost)),
                batch: lot.as_ref().map(|(b, _)| b.clone()),
                expiry: lot.as_ref().map(|(_, e)| day_text(*e)),
                base_qty_thousandths: base.thousandths(),
                value: Some(Some(value.minor())),
                lot_id: moves.first().and_then(|m| m.lot_id.clone()),
            });
        }
        let o = OpeningStock {
            id: id.to_owned(),
            number,
            branch_id: branch,
            location_id: location,
            note: body.note.clone(),
            user_id: role.id,
            posted_at: self.now,
            lines,
            movements: Vec::new(),
        };
        self.stock.openings.push(o.clone());
        let mut answer = o;
        answer.movements = self.doc_moves(id);
        self.stock_changed(id);
        Ok(view_opening(answer, role.profit))
    }
}

// ---------------------------------------------------------------------------------------------
// Opnames and reorder.

impl State {
    fn since(&self, o: &Opname, mark: usize, variant: &str) -> i64 {
        self.stock.ledger[mark.min(self.stock.ledger.len())..]
            .iter()
            .filter(|m| {
                m.branch_id == o.branch_id
                    && m.location_id.as_deref() == Some(o.location_id.as_str())
                    && m.variant_id == variant
                    && m.doc_id != o.id
            })
            .map(|m| m.qty_thousandths)
            .sum()
    }

    /// The opname with live `since` and variance.
    fn live(&self, st: &OpnameState) -> Opname {
        let mut o = st.opname.clone();
        if o.status == "counting" {
            for l in &mut o.lines {
                l.since_thousandths = self.since(&st.opname, st.mark, &l.variant_id);
                l.variance_thousandths = l
                    .counted_thousandths
                    .map(|c| c - l.snapshot_thousandths - l.since_thousandths);
            }
        }
        o
    }

    fn st_opnames(&self, token: &str, branch: Option<&str>) -> Result<Vec<Opname>, ClientError> {
        let role = self.reader_role(token)?;
        let branch = branch.unwrap_or(BRANCH_ID);
        Ok(self
            .stock
            .opnames
            .iter()
            .rev()
            .filter(|o| o.opname.branch_id == branch)
            .map(|o| {
                let mut x = view_opname(o.opname.clone(), role.profit);
                x.lines.clear();
                x
            })
            .collect())
    }

    fn st_opname(&self, token: &str, id: &str) -> Result<Opname, ClientError> {
        let role = self.reader_role(token)?;
        let st = self
            .stock
            .opnames
            .iter()
            .find(|o| o.opname.id == id)
            .ok_or_else(|| not_found("opname"))?;
        let mut o = self.live(st);
        o.movements = self.doc_moves(id);
        Ok(view_opname(o, role.profit))
    }

    fn st_start_opname(
        &mut self,
        token: &str,
        id: &str,
        body: &OpnameBody,
    ) -> Result<Opname, ClientError> {
        let role = self.stock_role(token)?;
        if !role.count {
            return Err(denied("stock.count"));
        }
        if let Some(st) = self.stock.opnames.iter().find(|o| o.opname.id == id) {
            return Ok(view_opname(self.live(st), role.profit));
        }
        let branch = body.branch_id.clone().unwrap_or_else(|| BRANCH_ID.into());
        let location = doc_location(&branch, body.location_id.as_ref())?;
        let full = match body.scope.as_str() {
            "full" => true,
            "cycle" => false,
            _ => return Err(invalid("scope")),
        };
        let mut variants: Vec<String> = Vec::new();
        let stock_types = |p: &Product| matches!(p.product_type.as_str(), "stock" | "weighed");
        for p in self.catalog.all_products().filter(|p| stock_types(p)) {
            let in_scope = full
                || p.category_id
                    .as_ref()
                    .is_some_and(|c| body.category_ids.contains(c));
            if in_scope {
                variants.extend(p.variants.iter().map(|v| v.id.clone()));
            }
        }
        if full {
            for m in self.stock.ledger.iter().filter(|m| {
                m.branch_id == branch && m.location_id.as_deref() == Some(location.as_str())
            }) {
                variants.push(m.variant_id.clone());
            }
        } else {
            for v in &body.variant_ids {
                self.info(v)?;
                variants.push(v.clone());
            }
        }
        variants.sort();
        variants.dedup();
        if let Some(other) = self.stock.opnames.iter().find(|o| {
            o.opname.status == "counting"
                && o.opname.location_id == location
                && o.opname
                    .lines
                    .iter()
                    .any(|l| variants.contains(&l.variant_id))
        }) {
            return Err(problem(409, "opname_open")
                .with("opname", other.opname.number.clone())
                .into());
        }
        let number = self.st_number(&branch, "OP")?;
        let lines = variants
            .iter()
            .map(|v| OpnameLine {
                variant_id: v.clone(),
                snapshot_thousandths: self.on_hand_at(&branch, Some(&location), v).thousandths(),
                ..OpnameLine::default()
            })
            .collect();
        let st = OpnameState {
            opname: Opname {
                id: id.to_owned(),
                number,
                branch_id: branch,
                location_id: location,
                full,
                status: "counting".into(),
                note: body.note.clone(),
                user_id: role.id,
                started_at: self.now,
                lines,
                ..Opname::default()
            },
            mark: self.stock.ledger.len(),
            entries: BTreeMap::new(),
            seen: Vec::new(),
        };
        let answer = self.live(&st);
        self.stock.opnames.push(st);
        Ok(view_opname(answer, role.profit))
    }

    fn st_add_counts(
        &mut self,
        token: &str,
        id: &str,
        entries: &[CountEntry],
    ) -> Result<Opname, ClientError> {
        let role = self.stock_role(token)?;
        if !role.count {
            return Err(denied("stock.count"));
        }
        let i = self
            .stock
            .opnames
            .iter()
            .position(|o| o.opname.id == id)
            .ok_or_else(|| not_found("opname"))?;
        if self.stock.opnames[i].opname.status != "counting" {
            return Err(conflict());
        }
        for e in entries {
            if self.stock.opnames[i].seen.contains(&e.id) {
                continue;
            }
            let factor = self.info(&e.variant_id)?.factor(&e.unit_id)?;
            let base = to_base(q(e.qty_thousandths), factor).map_err(|e| rule(&e))?;
            let (branch, location, full, mark) = {
                let st = &self.stock.opnames[i];
                (
                    st.opname.branch_id.clone(),
                    st.opname.location_id.clone(),
                    st.opname.full,
                    st.mark,
                )
            };
            let has_line = self.stock.opnames[i]
                .opname
                .lines
                .iter()
                .any(|l| l.variant_id == e.variant_id);
            if !has_line {
                if !full {
                    return Err(invalid("variant"));
                }
                let now = self.on_hand_at(&branch, Some(&location), &e.variant_id);
                let since = self.since(&self.stock.opnames[i].opname, mark, &e.variant_id);
                self.stock.opnames[i].opname.lines.push(OpnameLine {
                    variant_id: e.variant_id.clone(),
                    snapshot_thousandths: now.thousandths() - since,
                    ..OpnameLine::default()
                });
            }
            let st = &mut self.stock.opnames[i];
            let list = st.entries.entry(e.variant_id.clone()).or_default();
            list.push(sultan_core::stock::CountEntry {
                base_qty: base,
                replace: e.replace,
            });
            let total = counted(list).map_err(|e| rule(&e))?;
            if total.is_some_and(|t| t.thousandths() < 0) {
                list.pop();
                return Err(invalid("qty"));
            }
            st.seen.push(e.id.clone());
            if let Some(l) = st
                .opname
                .lines
                .iter_mut()
                .find(|l| l.variant_id == e.variant_id)
            {
                l.counted_thousandths = total.map(Qty::thousandths);
            }
        }
        let answer = self.live(&self.stock.opnames[i]);
        Ok(view_opname(answer, role.profit))
    }

    fn st_post_opname(
        &mut self,
        token: &str,
        id: &str,
        uncounted: &str,
    ) -> Result<Opname, ClientError> {
        let role = self.stock_role(token)?;
        if !role.count {
            return Err(denied("stock.count"));
        }
        if !role.adjust {
            return Err(denied("stock.adjust"));
        }
        let choice = Uncounted::from_key(uncounted).ok_or_else(|| invalid("uncounted"))?;
        let i = self
            .stock
            .opnames
            .iter()
            .position(|o| o.opname.id == id)
            .ok_or_else(|| not_found("opname"))?;
        match self.stock.opnames[i].opname.status.as_str() {
            "posted" => return self.st_opname(token, id),
            "cancelled" => return Err(conflict()),
            _ => {}
        }
        let live = self.live(&self.stock.opnames[i]);
        let lines: Vec<CoreOpnameLine<usize>> = live
            .lines
            .iter()
            .enumerate()
            .map(|(i, l)| CoreOpnameLine {
                variant: i,
                snapshot: q(l.snapshot_thousandths),
                since: q(l.since_thousandths),
                counted: l.counted_thousandths.map(q),
            })
            .collect();
        let moves = opname_moves(&lines, live.full, choice).map_err(|e| rule(&e))?;
        let mut values: HashMap<String, Option<i64>> = HashMap::new();
        for (index, delta) in moves {
            let variant = live.lines[index].variant_id.clone();
            let up = delta.is_positive();
            let posted = self.post(
                (&live.branch_id, &live.location_id, &variant),
                "count",
                delta,
                if up {
                    Val::Increase {
                        typed: None,
                        need: false,
                    }
                } else {
                    Val::Average
                },
                if up { &LotRule::None } else { &LotRule::Fefo },
                ("opname", id, None),
            )?;
            values.insert(variant, State::sum_value(&posted));
        }
        let now = self.now;
        let st = &mut self.stock.opnames[i];
        st.opname.lines = live.lines;
        for l in &mut st.opname.lines {
            l.since_thousandths = 0;
            l.value = Some(values.get(&l.variant_id).copied().flatten());
        }
        st.opname.status = "posted".into();
        st.opname.uncounted = Some(choice.as_str().to_owned());
        st.opname.posted_at = Some(now);
        let mut answer = st.opname.clone();
        answer.movements = self.doc_moves(id);
        answer.negative_after = self.negative_after(id);
        self.stock_changed(id);
        Ok(view_opname(answer, role.profit))
    }

    fn st_cancel_opname(&mut self, token: &str, id: &str) -> Result<Opname, ClientError> {
        let role = self.stock_role(token)?;
        if !role.count {
            return Err(denied("stock.count"));
        }
        let st = self
            .stock
            .opnames
            .iter_mut()
            .find(|o| o.opname.id == id)
            .ok_or_else(|| not_found("opname"))?;
        match st.opname.status.as_str() {
            "counting" => st.opname.status = "cancelled".into(),
            "cancelled" => {}
            _ => return Err(conflict()),
        }
        Ok(view_opname(st.opname.clone(), role.profit))
    }

    fn st_rules(&self, token: &str, branch: Option<&str>) -> Result<Vec<ReorderRule>, ClientError> {
        self.reader_role(token)?;
        let branch = branch.unwrap_or(BRANCH_ID);
        Ok(self
            .stock
            .rules
            .iter()
            .filter(|r| r.branch_id == branch)
            .cloned()
            .collect())
    }

    fn st_put_rule(
        &mut self,
        token: &str,
        branch: &str,
        variant: &str,
        body: &ReorderRuleBody,
    ) -> Result<ReorderRule, ClientError> {
        if !self.stock_role(token)?.order {
            return Err(denied("purchase.order"));
        }
        check_rule(q(body.min_thousandths), q(body.max_thousandths)).map_err(|e| rule(&e))?;
        let r = ReorderRule {
            branch_id: branch.to_owned(),
            variant_id: variant.to_owned(),
            min_thousandths: body.min_thousandths,
            max_thousandths: body.max_thousandths,
        };
        match self
            .stock
            .rules
            .iter_mut()
            .find(|x| x.branch_id == branch && x.variant_id == variant)
        {
            Some(x) => *x = r.clone(),
            None => self.stock.rules.push(r.clone()),
        }
        Ok(r)
    }

    fn low_line(&self, branch: &str, variant: &str) -> SuggestedLine {
        let rule = self
            .stock
            .rules
            .iter()
            .find(|r| r.branch_id == branch && r.variant_id == variant);
        SuggestedLine {
            variant_id: variant.to_owned(),
            on_hand_thousandths: self.on_hand_at(branch, None, variant).thousandths(),
            on_order_thousandths: self.on_order(branch, variant),
            min_thousandths: rule.map(|r| r.min_thousandths),
            max_thousandths: rule.map(|r| r.max_thousandths),
            ..SuggestedLine::default()
        }
    }

    fn st_suggestions(
        &self,
        token: &str,
        supplier: &str,
        branch: Option<&str>,
    ) -> Result<Vec<SuggestedLine>, ClientError> {
        let role = self.stock_role(token)?;
        if !role.order {
            return Err(denied("purchase.order"));
        }
        let branch = branch.unwrap_or(BRANCH_ID);
        if self.stock.suppliers.iter().all(|s| s.id != supplier) {
            return Err(not_found("supplier"));
        }
        let items: Vec<&SupplierItem> = self
            .stock
            .items
            .iter()
            .filter(|i| i.supplier_id == supplier)
            .collect();
        let preferred: Vec<&&SupplierItem> = items.iter().filter(|i| i.preferred).collect();
        let chosen: Vec<&SupplierItem> = if preferred.is_empty() {
            items
        } else {
            preferred.into_iter().copied().collect()
        };
        let mut out = Vec::new();
        for item in chosen {
            let mut line = self.low_line(branch, &item.variant_id);
            let (Some(min), Some(max)) = (line.min_thousandths, line.max_thousandths) else {
                continue;
            };
            let have = line.on_hand_thousandths + line.on_order_thousandths;
            if have > min {
                continue;
            }
            let info = self.info(&item.variant_id)?;
            let unit = item
                .last_unit_id
                .clone()
                .unwrap_or_else(|| info.product.base_unit_id.clone());
            let factor = info
                .factor(&unit)
                .unwrap_or_else(|_| info.base_unit_factor())
                .thousandths()
                .max(1);
            let need = max - have;
            let units = (need + factor - 1) / factor;
            line.unit_id = Some(unit);
            line.units = units;
            line.base_qty_thousandths = units * factor;
            line.price = role
                .profit
                .then(|| item.last_price.flatten().map(|p| p * factor / 1_000));
            out.push(line);
        }
        Ok(out)
    }

    fn st_low_stock(
        &self,
        token: &str,
        branch: Option<&str>,
    ) -> Result<Vec<SuggestedLine>, ClientError> {
        self.reader_role(token)?;
        let branch = branch.unwrap_or(BRANCH_ID);
        let mut variants: Vec<String> = self
            .stock
            .rules
            .iter()
            .filter(|r| r.branch_id == branch)
            .map(|r| r.variant_id.clone())
            .collect();
        for m in self.stock.ledger.iter().filter(|m| m.branch_id == branch) {
            if !variants.contains(&m.variant_id) {
                variants.push(m.variant_id.clone());
            }
        }
        Ok(variants
            .iter()
            .map(|v| self.low_line(branch, v))
            .filter(|l| {
                l.on_hand_thousandths < 0
                    || l.min_thousandths
                        .is_some_and(|min| l.on_hand_thousandths + l.on_order_thousandths <= min)
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------------------------
// The calls.

fn run<T: Send + 'static>(
    fake: &FakeService,
    method: &'static str,
    detail: String,
    body: impl FnOnce(&mut State) -> Result<T, ClientError>,
) -> crate::BoxFuture<T> {
    let (mut s, failure) = fake.begin(method, detail);
    let result = match failure {
        Some(e) => Err(e),
        None => body(&mut s),
    };
    finish(s, result)
}

/// The inventory calls of [`crate::Service`] and [`crate::BackOffice`], called from the trait
/// impls, and the fake's setup helpers.
impl FakeService {
    /// Adds the Stock clerk `gudang` / `gudang-pass-1` ([`STOCK_CLERK`]): receive, transfer and
    /// count; no adjust, no `report.profit`.
    pub fn add_stock_clerk(&self) {
        let mut s = self.state();
        s.catalog.add_user(BackOfficeUser::new(
            STOCK_CLERK,
            "gudang",
            "gudang-pass-1",
            "Gudang",
        ));
        if !s.stock.clerks.iter().any(|c| c == STOCK_CLERK) {
            s.stock.clerks.push(STOCK_CLERK.to_owned());
        }
    }

    /// The till's on hand (`GET /registers/me/stock/{variant_id}`).
    pub(super) fn till_stock_call(
        &self,
        token: &str,
        variant: &str,
    ) -> crate::BoxFuture<TillStock> {
        run(self, "till_stock", variant.to_owned(), |s| {
            s.st_till(token, variant)
        })
    }
}

/// The [`crate::BackOffice`] inventory methods, delegated from `backoffice.rs`.
impl FakeService {
    pub(super) fn st_suppliers_call(&self, t: &str, a: bool) -> crate::BoxFuture<Vec<Supplier>> {
        run(self, "suppliers", a.to_string(), |s| s.st_suppliers(t, a))
    }
    pub(super) fn st_put_supplier_call(
        &self,
        t: &str,
        id: &str,
        b: &SupplierBody,
    ) -> crate::BoxFuture<Supplier> {
        run(self, "put_supplier", id.to_owned(), |s| {
            s.st_put_supplier(t, id, b)
        })
    }
    pub(super) fn st_delete_supplier_call(&self, t: &str, id: &str) -> crate::BoxFuture<Supplier> {
        run(self, "delete_supplier", id.to_owned(), |s| {
            s.st_delete_supplier(t, id)
        })
    }
    pub(super) fn st_items_call(&self, t: &str, sup: &str) -> crate::BoxFuture<Vec<SupplierItem>> {
        run(self, "supplier_items", sup.to_owned(), |s| {
            s.st_items(t, sup)
        })
    }
    pub(super) fn st_put_item_call(
        &self,
        t: &str,
        sup: &str,
        v: &str,
        b: &SupplierItemBody,
    ) -> crate::BoxFuture<SupplierItem> {
        run(self, "put_supplier_item", format!("{sup}/{v}"), |s| {
            s.st_put_item(t, sup, v, b)
        })
    }
    pub(super) fn st_reasons_call(&self, t: &str, a: bool) -> crate::BoxFuture<Vec<StockReason>> {
        run(self, "stock_reasons", a.to_string(), |s| s.st_reasons(t, a))
    }
    pub(super) fn st_put_reason_call(
        &self,
        t: &str,
        id: &str,
        b: &StockReasonBody,
    ) -> crate::BoxFuture<StockReason> {
        run(self, "put_stock_reason", id.to_owned(), |s| {
            s.st_put_reason(t, id, b)
        })
    }
    pub(super) fn st_delete_reason_call(&self, t: &str, id: &str) -> crate::BoxFuture<StockReason> {
        run(self, "delete_stock_reason", id.to_owned(), |s| {
            s.st_delete_reason(t, id)
        })
    }
    pub(super) fn st_settings_call(&self, t: &str) -> crate::BoxFuture<InventorySettings> {
        run(self, "inventory_settings", String::new(), |s| {
            s.reader_role(t)?;
            Ok(s.stock.settings)
        })
    }
    pub(super) fn st_put_settings_call(
        &self,
        t: &str,
        b: &InventorySettings,
    ) -> crate::BoxFuture<InventorySettings> {
        run(self, "put_inventory_settings", String::new(), |s| {
            s.st_put_settings(t, b)
        })
    }
    pub(super) fn st_on_hand_call(
        &self,
        t: &str,
        qy: &OnHandQuery,
    ) -> crate::BoxFuture<OnHandPage> {
        run(self, "on_hand", format!("{qy:?}"), |s| s.st_on_hand(t, qy))
    }
    pub(super) fn st_card_call(
        &self,
        t: &str,
        v: &str,
        qy: &CardQuery,
    ) -> crate::BoxFuture<CardPage> {
        run(self, "stock_card", v.to_owned(), |s| s.st_card(t, v, qy))
    }
    pub(super) fn st_lots_call(&self, t: &str, qy: &LotQuery) -> crate::BoxFuture<Vec<Lot>> {
        run(self, "lots", format!("{qy:?}"), |s| s.st_lots(t, qy))
    }
    pub(super) fn st_payables_call(
        &self,
        t: &str,
        qy: &PayableQuery,
    ) -> crate::BoxFuture<Vec<Payable>> {
        run(self, "payables", format!("{qy:?}"), |s| {
            s.st_payables(t, qy)
        })
    }
    pub(super) fn st_orders_call(
        &self,
        t: &str,
        qy: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<PurchaseOrder>> {
        run(self, "purchase_orders", format!("{qy:?}"), |s| {
            s.st_orders(t, qy)
        })
    }
    pub(super) fn st_order_call(&self, t: &str, id: &str) -> crate::BoxFuture<PurchaseOrder> {
        run(self, "purchase_order", id.to_owned(), |s| s.st_order(t, id))
    }
    pub(super) fn st_put_order_call(
        &self,
        t: &str,
        id: &str,
        b: &PurchaseOrderBody,
    ) -> crate::BoxFuture<PurchaseOrder> {
        run(self, "put_purchase_order", id.to_owned(), |s| {
            s.st_put_order(t, id, b)
        })
    }
    pub(super) fn st_order_action_call(
        &self,
        t: &str,
        id: &str,
        a: OrderAction,
    ) -> crate::BoxFuture<PurchaseOrder> {
        run(
            self,
            "purchase_order_action",
            format!("{id} {}", a.as_str()),
            |s| s.st_order_action(t, id, a),
        )
    }
    pub(super) fn st_receipts_call(
        &self,
        t: &str,
        qy: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<GoodsReceipt>> {
        run(self, "goods_receipts", format!("{qy:?}"), |s| {
            s.st_receipts(t, qy)
        })
    }
    pub(super) fn st_receipt_call(&self, t: &str, id: &str) -> crate::BoxFuture<GoodsReceipt> {
        run(self, "goods_receipt", id.to_owned(), |s| {
            s.st_receipt(t, id)
        })
    }
    pub(super) fn st_put_receipt_call(
        &self,
        t: &str,
        id: &str,
        b: &ReceiptBody,
    ) -> crate::BoxFuture<GoodsReceipt> {
        run(self, "put_goods_receipt", id.to_owned(), |s| {
            s.st_put_receipt(t, id, b)
        })
    }
    pub(super) fn st_post_receipt_call(&self, t: &str, id: &str) -> crate::BoxFuture<GoodsReceipt> {
        run(self, "post_goods_receipt", id.to_owned(), |s| {
            s.st_post_receipt(t, id)
        })
    }
    pub(super) fn st_void_receipt_call(
        &self,
        t: &str,
        id: &str,
        o: Option<&str>,
    ) -> crate::BoxFuture<GoodsReceipt> {
        run(self, "void_goods_receipt", format!("{id} {o:?}"), |s| {
            s.st_void_receipt(t, id, o)
        })
    }
    pub(super) fn st_returns_call(
        &self,
        t: &str,
        qy: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<SupplierReturn>> {
        run(self, "supplier_returns", format!("{qy:?}"), |s| {
            s.st_returns(t, qy)
        })
    }
    pub(super) fn st_return_call(&self, t: &str, id: &str) -> crate::BoxFuture<SupplierReturn> {
        run(self, "supplier_return", id.to_owned(), |s| {
            s.st_return(t, id)
        })
    }
    pub(super) fn st_put_return_call(
        &self,
        t: &str,
        id: &str,
        b: &SupplierReturnBody,
    ) -> crate::BoxFuture<SupplierReturn> {
        run(self, "put_supplier_return", id.to_owned(), |s| {
            s.st_put_return(t, id, b)
        })
    }
    pub(super) fn st_post_return_call(
        &self,
        t: &str,
        id: &str,
    ) -> crate::BoxFuture<SupplierReturn> {
        run(self, "post_supplier_return", id.to_owned(), |s| {
            s.st_post_return(t, id)
        })
    }
    pub(super) fn st_transfers_call(
        &self,
        t: &str,
        qy: &StockDocQuery,
    ) -> crate::BoxFuture<Vec<Transfer>> {
        run(self, "transfers", format!("{qy:?}"), |s| {
            s.st_transfers(t, qy)
        })
    }
    pub(super) fn st_transfer_call(&self, t: &str, id: &str) -> crate::BoxFuture<Transfer> {
        run(self, "transfer", id.to_owned(), |s| s.st_transfer(t, id))
    }
    pub(super) fn st_put_transfer_call(
        &self,
        t: &str,
        id: &str,
        b: &TransferBody,
    ) -> crate::BoxFuture<Transfer> {
        run(self, "put_transfer", id.to_owned(), |s| {
            s.st_put_transfer(t, id, b)
        })
    }
    pub(super) fn st_transfer_action_call(
        &self,
        t: &str,
        id: &str,
        a: TransferAction,
        b: &MoveBody,
    ) -> crate::BoxFuture<Transfer> {
        run(
            self,
            "transfer_action",
            format!("{id} {}", a.as_str()),
            |s| s.st_transfer_action(t, id, a, b),
        )
    }
    pub(super) fn st_internals_call(
        &self,
        t: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<InternalTransfer>> {
        run(self, "internal_transfers", format!("{b:?}"), |s| {
            s.st_internals(t, b)
        })
    }
    pub(super) fn st_put_internal_call(
        &self,
        t: &str,
        id: &str,
        b: &InternalTransferBody,
    ) -> crate::BoxFuture<InternalTransfer> {
        run(self, "put_internal_transfer", id.to_owned(), |s| {
            s.st_put_internal(t, id, b)
        })
    }
    pub(super) fn st_adjustments_call(
        &self,
        t: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<Adjustment>> {
        run(self, "adjustments", format!("{b:?}"), |s| {
            s.st_adjustments(t, b)
        })
    }
    pub(super) fn st_adjustment_call(&self, t: &str, id: &str) -> crate::BoxFuture<Adjustment> {
        run(self, "adjustment", id.to_owned(), |s| {
            s.st_adjustment(t, id)
        })
    }
    pub(super) fn st_put_adjustment_call(
        &self,
        t: &str,
        id: &str,
        b: &AdjustmentBody,
    ) -> crate::BoxFuture<Adjustment> {
        run(self, "put_adjustment", id.to_owned(), |s| {
            s.st_put_adjustment(t, id, b)
        })
    }
    pub(super) fn st_openings_call(
        &self,
        t: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<OpeningStock>> {
        run(self, "opening_stocks", format!("{b:?}"), |s| {
            s.st_openings(t, b)
        })
    }
    pub(super) fn st_put_opening_call(
        &self,
        t: &str,
        id: &str,
        b: &OpeningBody,
    ) -> crate::BoxFuture<OpeningStock> {
        run(self, "put_opening_stock", id.to_owned(), |s| {
            s.st_put_opening(t, id, b)
        })
    }
    pub(super) fn st_opnames_call(
        &self,
        t: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<Opname>> {
        run(self, "opnames", format!("{b:?}"), |s| s.st_opnames(t, b))
    }
    pub(super) fn st_opname_call(&self, t: &str, id: &str) -> crate::BoxFuture<Opname> {
        run(self, "opname", id.to_owned(), |s| s.st_opname(t, id))
    }
    pub(super) fn st_start_opname_call(
        &self,
        t: &str,
        id: &str,
        b: &OpnameBody,
    ) -> crate::BoxFuture<Opname> {
        run(self, "start_opname", id.to_owned(), |s| {
            s.st_start_opname(t, id, b)
        })
    }
    pub(super) fn st_add_counts_call(
        &self,
        t: &str,
        id: &str,
        e: &[CountEntry],
    ) -> crate::BoxFuture<Opname> {
        run(self, "add_counts", id.to_owned(), |s| {
            s.st_add_counts(t, id, e)
        })
    }
    pub(super) fn st_post_opname_call(
        &self,
        t: &str,
        id: &str,
        u: &str,
    ) -> crate::BoxFuture<Opname> {
        run(self, "post_opname", format!("{id} {u}"), |s| {
            s.st_post_opname(t, id, u)
        })
    }
    pub(super) fn st_cancel_opname_call(&self, t: &str, id: &str) -> crate::BoxFuture<Opname> {
        run(self, "cancel_opname", id.to_owned(), |s| {
            s.st_cancel_opname(t, id)
        })
    }
    pub(super) fn st_rules_call(
        &self,
        t: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<ReorderRule>> {
        run(self, "reorder_rules", format!("{b:?}"), |s| {
            s.st_rules(t, b)
        })
    }
    pub(super) fn st_put_rule_call(
        &self,
        t: &str,
        b: &str,
        v: &str,
        body: &ReorderRuleBody,
    ) -> crate::BoxFuture<ReorderRule> {
        run(self, "put_reorder_rule", format!("{b}/{v}"), |s| {
            s.st_put_rule(t, b, v, body)
        })
    }
    pub(super) fn st_suggestions_call(
        &self,
        t: &str,
        sup: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<SuggestedLine>> {
        run(self, "reorder_suggestions", sup.to_owned(), |s| {
            s.st_suggestions(t, sup, b)
        })
    }
    pub(super) fn st_low_stock_call(
        &self,
        t: &str,
        b: Option<&str>,
    ) -> crate::BoxFuture<Vec<SuggestedLine>> {
        run(self, "low_stock", format!("{b:?}"), |s| {
            s.st_low_stock(t, b)
        })
    }
}
