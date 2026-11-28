//! The audit and outbox rows of the inventory writes (spec 2.8 §2, §12 "all writes are audited",
//! §15), appended by each write function in the caller's transaction, like `cash` and `sales`.
//!
//! HQ data (suppliers, supplier items, stock reasons, the settings) and reorder rules record the
//! stored record. Branch documents record the stored document with its lines and, once posted,
//! the ledger rows it wrote (`movements`), so a later sync can replay them.

use serde_json::{Value, json};
use sultan_core::doc_number::LocalDay;
use sultan_core::ids::{BranchId, UserId};
use sultan_core::stock::InventorySettings;
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::adjust::{Adjustment, OpeningStock};
use super::ledger::{MovementRecord, document_movements};
use super::opname::Opname;
use super::purchase::PurchaseOrder;
use super::reasons::StockReasonRecord;
use super::receipts::{GoodsReceipt, SupplierReturn};
use super::reorder::ReorderRule;
use super::suppliers::{Supplier, SupplierItem};
use super::transfers::{InternalTransfer, Transfer};
use crate::audit::{self, AuditEntry};
use crate::catalog::Actor;
use crate::conn::{Conn, Tx};
use crate::error::Result;
use crate::outbox;

/// One change as recorded: an audit row and an outbox row.
pub(crate) struct Change<'a> {
    /// The audit action, e.g. `goods_receipt.post`.
    pub action: &'a str,
    /// The audit target, e.g. `goods_receipt:<id>`.
    pub target: String,
    /// The state before.
    pub before: Option<Value>,
    /// The state after (also the outbox payload).
    pub after: Value,
    /// The outbox event kind (§15).
    pub event: &'a str,
}

/// Appends the audit row and the outbox row of `change` in `tx`.
pub(crate) async fn record(
    tx: &mut Tx,
    actor: &Actor,
    now: Timestamp,
    change: Change<'_>,
) -> Result<()> {
    let mut entry = AuditEntry::new(now, change.action);
    entry.user = actor.user;
    entry.branch = actor.branch;
    entry.register = actor.register;
    entry.target = Some(change.target);
    entry.before = change.before;
    entry.after = Some(change.after.clone());
    audit::append(&mut *tx, &entry).await?;
    outbox::append(&mut *tx, change.event, &change.after, now).await?;
    Ok(())
}

/// The actor of a branch document: its user at its branch.
pub(crate) const fn doc_actor(user: UserId, branch: BranchId) -> Actor {
    Actor {
        user: Some(user),
        branch: Some(branch),
        register: None,
    }
}

/// Records a branch document with the ledger rows it wrote.
pub(crate) async fn record_doc(
    tx: &mut Tx,
    actor: &Actor,
    now: Timestamp,
    (action, event): (&str, &str),
    id: Uuid,
    mut doc: Value,
) -> Result<()> {
    let movements = document_movements(Conn::Tx(&mut *tx), id).await?;
    doc["movements"] = Value::Array(movements.iter().map(movement_json).collect());
    let target = format!("{}:{id}", action.split('.').next().unwrap_or(action));
    record(
        tx,
        actor,
        now,
        Change {
            action,
            target,
            before: None,
            after: doc,
            event,
        },
    )
    .await
}

fn day(d: LocalDay) -> String {
    let (y, m, d) = d.ymd();
    format!("{y:04}-{m:02}-{d:02}")
}

fn opt_day(d: Option<LocalDay>) -> Option<String> {
    d.map(day)
}

fn ts(t: Option<Timestamp>) -> Option<i64> {
    t.map(Timestamp::unix_millis)
}

fn id<T: ToString>(v: &T) -> String {
    v.to_string()
}

fn opt_id<T: ToString>(v: Option<&T>) -> Option<String> {
    v.map(ToString::to_string)
}

/// A ledger row.
pub(crate) fn movement_json(m: &MovementRecord) -> Value {
    json!({
        "id": id(&m.id),
        "branch_id": id(&m.branch),
        "location_id": opt_id(m.location.as_ref()),
        "variant_id": id(&m.variant),
        "kind": m.kind.as_str(),
        "qty": m.qty.thousandths(),
        "value": m.value.map(sultan_core::money::Money::minor),
        "lot_id": opt_id(m.lot.as_ref()),
        "doc_kind": m.doc_kind,
        "doc_id": id(&m.doc_id),
        "doc_line": m.doc_line,
        "occurred_at": m.occurred_at.unix_millis(),
    })
}

pub(crate) fn supplier_json(s: &Supplier) -> Value {
    json!({
        "id": id(&s.id),
        "code": s.code,
        "name": s.name,
        "address": s.address,
        "phone": s.phone,
        "email": s.email,
        "tax_id": s.tax_id,
        "pkp": s.pkp,
        "terms_days": s.terms_days,
        "lead_days": s.lead_days,
        "notes": s.notes,
        "active": s.active,
        "archived_at": ts(s.archived_at),
    })
}

pub(crate) fn supplier_item_json(i: &SupplierItem) -> Value {
    json!({
        "supplier_id": id(&i.supplier),
        "variant_id": id(&i.variant),
        "supplier_code": i.supplier_code,
        "last_price": i.last_price.map(sultan_core::money::Money::minor),
        "last_unit_id": opt_id(i.last_unit.as_ref()),
        "preferred": i.preferred,
    })
}

pub(crate) fn reason_json(r: &StockReasonRecord) -> Value {
    json!({
        "id": id(&r.id),
        "name": r.name,
        "direction": r.direction.as_str(),
        "note_required": r.note_required,
        "active": r.active,
        "sort": r.sort,
        "archived_at": ts(r.archived_at),
    })
}

pub(crate) fn settings_json(s: &InventorySettings) -> Value {
    json!({
        "pkp": s.pkp,
        "standard_ppn_rate_bp": s.standard_ppn_rate_bp,
        "expiry_warn_days": s.expiry_warn_days,
        "velocity_days": s.velocity_days,
        "cover_days": s.cover_days,
    })
}

pub(crate) fn rule_json(r: &ReorderRule) -> Value {
    json!({
        "branch_id": id(&r.branch),
        "variant_id": id(&r.variant),
        "min": r.min.thousandths(),
        "max": r.max.thousandths(),
    })
}

pub(crate) fn purchase_order_json(o: &PurchaseOrder) -> Value {
    json!({
        "id": id(&o.id),
        "number": o.number,
        "branch_id": id(&o.branch),
        "supplier_id": id(&o.supplier),
        "location_id": id(&o.location),
        "status": o.status.as_str(),
        "order_date": opt_day(o.order_date),
        "expected_date": opt_day(o.expected_date),
        "note": o.note,
        "user_id": id(&o.user),
        "ordered_at": ts(o.ordered_at),
        "closed_at": ts(o.closed_at),
        "cancelled_at": ts(o.cancelled_at),
        "lines": o.lines.iter().map(|l| json!({
            "line_no": l.line_no,
            "variant_id": id(&l.variant),
            "unit_id": id(&l.unit),
            "factor": l.factor.thousandths(),
            "qty": l.qty.thousandths(),
            "price": l.price.minor(),
            "base_qty": l.base_qty.thousandths(),
            "received_qty": l.received_qty.thousandths(),
        })).collect::<Vec<_>>(),
    })
}

fn money(m: Option<sultan_core::money::Money>) -> Option<i64> {
    m.map(sultan_core::money::Money::minor)
}

pub(crate) fn receipt_json(r: &GoodsReceipt) -> Value {
    json!({
        "id": id(&r.id),
        "number": r.number,
        "branch_id": id(&r.branch),
        "supplier_id": id(&r.supplier),
        "location_id": id(&r.location),
        "status": r.status.as_str(),
        "invoice_no": r.invoice_no,
        "invoice_date": day(r.invoice_date),
        "payment": r.payment.as_str(),
        "due_date": opt_day(r.due_date),
        "tax_mode": r.tax_mode.as_str(),
        "tax_rate_bp": r.tax_rate_bp,
        "header_discount": r.header_discount.minor(),
        "other_costs": r.other_costs.minor(),
        "pkp": r.pkp,
        "net": money(r.net),
        "tax": money(r.tax),
        "total": money(r.total),
        "landed": money(r.landed),
        "note": r.note,
        "user_id": id(&r.user),
        "posted_at": ts(r.posted_at),
        "voided_at": ts(r.voided_at),
        "approved_by": opt_id(r.approved_by.as_ref()),
        "lines": r.lines.iter().map(receipt_line_json).collect::<Vec<_>>(),
    })
}

fn receipt_line_json(l: &super::receipts::GoodsReceiptLine) -> Value {
    json!({
            "line_no": l.line_no,
        "variant_id": id(&l.variant),
        "unit_id": id(&l.unit),
        "factor": l.factor.thousandths(),
        "qty": l.qty.thousandths(),
        "price": l.price.minor(),
        "discount": l.discount.minor(),
        "order_id": l.order_line.map(|(o, _)| o.to_string()),
        "order_line_no": l.order_line.map(|(_, n)| n),
        "batch": l.batch,
        "expiry": opt_day(l.expiry),
        "base_qty": l.base_qty.thousandths(),
        "header_discount": money(l.header_discount),
        "tax": money(l.tax),
        "other_costs": money(l.other_costs),
        "landed": money(l.landed),
        "lot_id": opt_id(l.lot.as_ref()),
    })
}

pub(crate) fn supplier_return_json(r: &SupplierReturn) -> Value {
    json!({
        "id": id(&r.id),
        "number": r.number,
        "branch_id": id(&r.branch),
        "supplier_id": id(&r.supplier),
        "location_id": id(&r.location),
        "posted": r.posted,
        "tax_mode": r.tax_mode.as_str(),
        "tax_rate_bp": r.tax_rate_bp,
        "credit": money(r.credit),
        "note": r.note,
        "user_id": id(&r.user),
        "posted_at": ts(r.posted_at),
        "lines": r.lines.iter().map(|l| json!({
            "line_no": l.line_no,
            "variant_id": id(&l.variant),
            "unit_id": id(&l.unit),
            "factor": l.factor.thousandths(),
            "qty": l.qty.thousandths(),
            "price": l.price.minor(),
            "receipt_id": l.receipt_line.map(|(r, _)| r.to_string()),
            "receipt_line_no": l.receipt_line.map(|(_, n)| n),
            "base_qty": l.base_qty.thousandths(),
            "value": money(l.value),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn transfer_json(t: &Transfer) -> Value {
    let q = |v: Option<sultan_core::catalog::Qty>| v.map(sultan_core::catalog::Qty::thousandths);
    json!({
        "id": id(&t.id),
        "number": t.number,
        "from_branch_id": id(&t.from_branch),
        "from_location_id": opt_id(t.from_location.as_ref()),
        "to_branch_id": id(&t.to_branch),
        "to_location_id": opt_id(t.to_location.as_ref()),
        "status": t.status.as_str(),
        "note": t.note,
        "user_id": id(&t.user),
        "shipped_at": ts(t.shipped_at),
        "received_at": ts(t.received_at),
        "lines": t.lines.iter().map(|l| json!({
            "line_no": l.line_no,
            "variant_id": id(&l.variant),
            "unit_id": id(&l.unit),
            "factor": l.factor.thousandths(),
            "qty": l.qty.thousandths(),
            "shipped_qty": q(l.shipped_qty),
            "shipped_value": money(l.shipped_value),
            "received_qty": q(l.received_qty),
            "received_value": money(l.received_value),
            "lost_qty": q(l.lost_qty),
            "lost_value": money(l.lost_value),
            "lots": l.lots.iter().map(|(b, e, n)| json!({
                "batch": b, "expiry": day(*e), "qty": n.thousandths(),
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn internal_transfer_json(t: &InternalTransfer) -> Value {
    json!({
        "id": id(&t.id),
        "number": t.number,
        "branch_id": id(&t.branch),
        "from_location_id": id(&t.from_location),
        "to_location_id": id(&t.to_location),
        "note": t.note,
        "user_id": id(&t.user),
        "posted_at": t.posted_at.unix_millis(),
        "lines": t.lines.iter().map(|l| json!({
            "line_no": l.line_no,
            "variant_id": id(&l.variant),
            "unit_id": id(&l.unit),
            "factor": l.factor.thousandths(),
            "qty": l.qty.thousandths(),
            "base_qty": l.base_qty.thousandths(),
            "value": money(l.value),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn adjustment_json(a: &Adjustment) -> Value {
    json!({
        "id": id(&a.id),
        "number": a.number,
        "branch_id": id(&a.branch),
        "location_id": id(&a.location),
        "note": a.note,
        "user_id": id(&a.user),
        "posted_at": a.posted_at.unix_millis(),
        "lines": a.lines.iter().map(|l| json!({
            "line_no": l.line_no,
            "variant_id": id(&l.variant),
            "unit_id": id(&l.unit),
            "factor": l.factor.thousandths(),
            "qty": l.qty.thousandths(),
            "reason_id": id(&l.reason),
            "note": l.note,
            "unit_cost": money(l.unit_cost),
            "base_qty": l.base_qty.thousandths(),
            "value": money(l.value),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn opening_json(o: &OpeningStock) -> Value {
    json!({
        "id": id(&o.id),
        "number": o.number,
        "branch_id": id(&o.branch),
        "location_id": id(&o.location),
        "note": o.note,
        "user_id": id(&o.user),
        "posted_at": o.posted_at.unix_millis(),
        "lines": o.lines.iter().map(|l| json!({
            "line_no": l.line_no,
            "variant_id": id(&l.variant),
            "unit_id": id(&l.unit),
            "factor": l.factor.thousandths(),
            "qty": l.qty.thousandths(),
            "unit_cost": l.unit_cost.minor(),
            "batch": l.batch,
            "expiry": opt_day(l.expiry),
            "base_qty": l.base_qty.thousandths(),
            "value": money(l.value),
            "lot_id": opt_id(l.lot.as_ref()),
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn opname_json(o: &Opname) -> Value {
    let q = |v: Option<sultan_core::catalog::Qty>| v.map(sultan_core::catalog::Qty::thousandths);
    json!({
        "id": id(&o.id),
        "number": o.number,
        "branch_id": id(&o.branch),
        "location_id": id(&o.location),
        "full": o.full,
        "status": o.status,
        "uncounted": o.uncounted.map(sultan_core::stock::Uncounted::as_str),
        "note": o.note,
        "user_id": id(&o.user),
        "started_at": o.started_at.unix_millis(),
        "posted_at": ts(o.posted_at),
        "lines": o.lines.iter().map(|l| json!({
            "variant_id": id(&l.variant),
            "snapshot": l.snapshot.thousandths(),
            "since": l.since.thousandths(),
            "counted": q(l.counted),
            "variance": q(l.variance),
            "value": money(l.value),
        })).collect::<Vec<_>>(),
    })
}
