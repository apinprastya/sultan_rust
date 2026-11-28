//! Inventory as the back office and the till see it (spec 2.8, phase N3; the JSON of
//! `crates/server/src/routes/{stock,suppliers,purchasing,stock_docs}.rs`).
//!
//! Quantities are thousandths (`*_thousandths`, base units unless a line says "in the unit"),
//! money is minor units (`i64`), days `YYYY-MM-DD`, times unix ms, IDs UUID strings. Answers are
//! read with `#[serde(default)]`, so later server fields never break an older client.
//!
//! **Cost fields** ([`Cost`]) are `Option<Option<i64>>`: `None` = left out by the server because
//! the caller lacks `report.profit` (spec 2.8 §12), `Some(None)` = shown but "no cost yet" (§5),
//! `Some(Some(v))` = the value. [`shown`] reads them that way.

use serde::{Deserialize, Deserializer, Serialize};

/// A cost or value field: `None` hidden, `Some(None)` no cost yet, `Some(Some(v))` the value.
pub type Cost = Option<Option<i64>>;

/// Reads a cost field that is present (a value or `null`) as `Some(..)`; an absent field stays
/// `None` through `#[serde(default)]`.
///
/// # Errors
/// The deserializer's error.
pub fn shown<'de, D: Deserializer<'de>>(d: D) -> Result<Cost, D::Error> {
    Option::<i64>::deserialize(d).map(Some)
}

/// The seeded stock reasons (spec 2.8 §7): …0501 Rusak, …0502 Kedaluwarsa, …0503 Hilang,
/// …0504 Sampel, …0505 Pemakaian sendiri, …0506 Koreksi (both ways, note required).
pub const SEED_STOCK_REASONS: [&str; 6] = [
    "00000000-0000-7000-8000-000000000501",
    "00000000-0000-7000-8000-000000000502",
    "00000000-0000-7000-8000-000000000503",
    "00000000-0000-7000-8000-000000000504",
    "00000000-0000-7000-8000-000000000505",
    "00000000-0000-7000-8000-000000000506",
];

/// A supplier (HQ data, §3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Supplier {
    /// ID.
    pub id: String,
    /// Code (upper case).
    pub code: String,
    /// Name.
    pub name: String,
    /// Address.
    pub address: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// NPWP.
    pub tax_id: Option<String>,
    /// PKP.
    pub pkp: bool,
    /// Payment terms in days.
    pub terms_days: u32,
    /// Lead time in days.
    pub lead_days: u32,
    /// Notes.
    pub notes: Option<String>,
    /// Offered for new documents.
    pub active: bool,
    /// Archived.
    pub archived: bool,
}

/// Body of `PUT /suppliers/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplierBody {
    /// 1–20 characters, stored upper case.
    pub code: String,
    /// 1–120 characters.
    pub name: String,
    /// Address.
    pub address: Option<String>,
    /// Phone.
    pub phone: Option<String>,
    /// E-mail.
    pub email: Option<String>,
    /// NPWP.
    pub tax_id: Option<String>,
    /// PKP.
    pub pkp: bool,
    /// Payment terms in days (0 = cash).
    pub terms_days: u32,
    /// Lead time in days.
    pub lead_days: u32,
    /// Notes.
    pub notes: Option<String>,
    /// Offered for new documents.
    pub active: bool,
}

impl SupplierBody {
    /// A body with a code and a name; cash terms, 7 lead days, active.
    #[must_use]
    pub fn new(code: &str, name: &str) -> Self {
        Self {
            code: code.to_owned(),
            name: name.to_owned(),
            address: None,
            phone: None,
            email: None,
            tax_id: None,
            pkp: false,
            terms_days: 0,
            lead_days: 7,
            notes: None,
            active: true,
        }
    }
}

/// A supplier item (§3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupplierItem {
    /// The supplier.
    pub supplier_id: String,
    /// The variant.
    pub variant_id: String,
    /// The supplier's item code.
    pub supplier_code: Option<String>,
    /// The last price per base unit ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub last_price: Cost,
    /// The unit of the last receipt line.
    pub last_unit_id: Option<String>,
    /// The variant's preferred supplier.
    pub preferred: bool,
}

/// Body of `PUT /suppliers/{id}/items/{variant_id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplierItemBody {
    /// The supplier's item code.
    pub supplier_code: Option<String>,
    /// The variant's preferred supplier.
    pub preferred: bool,
}

/// A stock reason (HQ data, §7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StockReason {
    /// ID.
    pub id: String,
    /// Name.
    pub name: String,
    /// `in`, `out` or `both`.
    pub direction: String,
    /// A line with it needs a note.
    pub note_required: bool,
    /// Offered for new lines.
    pub active: bool,
    /// Position.
    pub sort: i32,
    /// Archived.
    pub archived: bool,
}

/// Body of `PUT /stock-reasons/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockReasonBody {
    /// 1–60 characters.
    pub name: String,
    /// `in`, `out` or `both`.
    pub direction: String,
    /// A line with it needs a note.
    pub note_required: bool,
    /// Offered for new lines.
    pub active: bool,
    /// Position.
    pub sort: i32,
}

/// The inventory settings (§13, `GET|PUT /settings/inventory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct InventorySettings {
    /// The company is PKP: input PPN stays out of the cost.
    pub pkp: bool,
    /// The receipts' default PPN rate in basis points.
    pub standard_ppn_rate_bp: u16,
    /// Lots expiring within these days are listed.
    pub expiry_warn_days: u32,
    /// The velocity period.
    pub velocity_days: u32,
    /// Cover days of a suggestion.
    pub cover_days: u32,
}

impl Default for InventorySettings {
    fn default() -> Self {
        Self {
            pkp: false,
            standard_ppn_rate_bp: 1_100,
            expiry_warn_days: 30,
            velocity_days: 30,
            cover_days: 14,
        }
    }
}

/// A ledger row (§2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Movement {
    /// ID, in posting order.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: Option<String>,
    /// The variant.
    pub variant_id: String,
    /// The kind (`sale`, `receipt`, `transfer_out`, …).
    pub kind: String,
    /// Base units, signed.
    pub qty_thousandths: i64,
    /// Signed value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
    /// The lot.
    pub lot_id: Option<String>,
    /// The document kind.
    pub doc_kind: String,
    /// The document.
    pub doc_id: String,
    /// The document line.
    pub doc_line: Option<u32>,
    /// Unix ms.
    pub occurred_at: i64,
}

/// A (location, variant) a posting left below zero (§4: a warning, never a refusal).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NegativeStock {
    /// The location.
    pub location_id: Option<String>,
    /// The variant.
    pub variant_id: String,
    /// On hand there now.
    pub qty_thousandths: i64,
}

/// Query of `GET /stock/on-hand`; unset fields are left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnHandQuery {
    /// The branch (default: the session's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// One location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// One variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,
    /// Products of this category.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    /// Products of this brand.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brand_id: Option<String>,
    /// Text: in the name, or exactly the code, SKU or a barcode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub q: Option<String>,
    /// `negative`, `low` or `expiring`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    /// The `next_cursor` of the previous page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Page size (default 50).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One row of the on-hand list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnHandRow {
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: Option<String>,
    /// The variant.
    pub variant_id: String,
    /// Its product.
    pub product_id: Option<String>,
    /// The product code.
    pub product_code: String,
    /// The product name.
    pub product_name: String,
    /// The SKU.
    pub sku: String,
    /// On hand at the location.
    pub qty_thousandths: i64,
    /// On order at the branch.
    pub on_order_thousandths: i64,
    /// In transit to or from the branch.
    pub in_transit_thousandths: i64,
    /// The reorder min.
    pub min_thousandths: Option<i64>,
    /// The reorder max.
    pub max_thousandths: Option<i64>,
    /// Unit cost per base unit ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub unit_cost: Cost,
    /// On hand × unit cost ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
}

/// A page of the on-hand list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OnHandPage {
    /// The rows.
    pub rows: Vec<OnHandRow>,
    /// The next page's cursor; `None` when this page was not full.
    pub next_cursor: Option<String>,
}

/// Query of `GET /stock/card/{variant_id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CardQuery {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// One location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// From (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// Until (unix ms, exclusive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// The `next_cursor` of the previous page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// Page size (default 100).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A stock card row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardRow {
    /// The movement.
    pub movement: Movement,
    /// On hand after it.
    pub balance_thousandths: i64,
}

/// A page of a stock card.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CardPage {
    /// Oldest first.
    pub rows: Vec<CardRow>,
    /// The next page's cursor.
    pub next_cursor: Option<String>,
}

/// Query of `GET /stock/lots`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LotQuery {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// One location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// One variant.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant_id: Option<String>,
    /// Only the Expiring list.
    pub expiring: bool,
}

/// A lot with stock.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Lot {
    /// ID.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// The variant.
    pub variant_id: String,
    /// Batch.
    pub batch: String,
    /// `YYYY-MM-DD`.
    pub expiry: String,
    /// Expired.
    pub expired: bool,
    /// Base units.
    pub qty_thousandths: i64,
    /// Value at the average ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
}

/// Query of `GET /payables`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayableQuery {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// One supplier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplier_id: Option<String>,
    /// `open` (default) or `all`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// A payable (read-only until 2.9).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Payable {
    /// ID.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The receipt.
    pub receipt_id: String,
    /// The receipt's number.
    pub receipt_number: Option<String>,
    /// Amount.
    pub amount: i64,
    /// `YYYY-MM-DD`.
    pub due_date: String,
    /// Paid.
    pub paid: bool,
    /// Voided.
    pub voided: bool,
}

/// The till's on hand of a variant (I18).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TillStock {
    /// The variant.
    pub variant_id: String,
    /// The branch.
    pub branch_id: String,
    /// On hand at the branch, base units.
    pub qty_thousandths: i64,
}

/// Query of the document lists (`branch_id`, `supplier_id`, `status`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StockDocQuery {
    /// The branch (default: the session's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// One supplier (purchasing lists).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supplier_id: Option<String>,
    /// One status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// A line of a body: variant, unit and quantity in the unit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QtyLine {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
}

/// A quantity for one document line (ship, receive).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LineQty {
    /// The line.
    pub line_no: u32,
    /// Quantity in the line's unit.
    pub qty_thousandths: i64,
}

/// A purchase order line in a body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurchaseOrderLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit.
    pub price: i64,
}

/// Body of `PUT /purchase-orders/{id}` (a draft).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PurchaseOrderBody {
    /// The branch (default: the session's).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The supplier.
    pub supplier_id: String,
    /// The location to receive into.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_date: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_date: Option<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<PurchaseOrderLineBody>,
}

/// A purchase order line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PurchaseOrderLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub price: Cost,
    /// Ordered base units.
    pub base_qty_thousandths: i64,
    /// Received base units.
    pub received_qty_thousandths: i64,
}

/// A purchase order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PurchaseOrder {
    /// ID.
    pub id: String,
    /// Number once ordered.
    pub number: Option<String>,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    pub location_id: String,
    /// `draft`, `ordered`, `closed` or `cancelled`.
    pub status: String,
    /// `YYYY-MM-DD`.
    pub order_date: Option<String>,
    /// `YYYY-MM-DD`.
    pub expected_date: Option<String>,
    /// Note.
    pub note: Option<String>,
    /// Who wrote it.
    pub user_id: String,
    /// Unix ms.
    pub ordered_at: Option<i64>,
    /// Unix ms.
    pub closed_at: Option<i64>,
    /// Unix ms.
    pub cancelled_at: Option<i64>,
    /// The lines.
    pub lines: Vec<PurchaseOrderLine>,
}

/// An action on a purchase order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OrderAction {
    /// `POST …/order`: number a draft.
    Order,
    /// `POST …/close`: close the rest.
    Close,
    /// `POST …/cancel`: nothing received yet.
    Cancel,
}

impl OrderAction {
    /// The path segment.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Order => "order",
            Self::Close => "close",
            Self::Cancel => "cancel",
        }
    }
}

/// A goods receipt line in a body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit before discount.
    pub price: i64,
    /// Line discount.
    pub discount: i64,
    /// The PO it fills.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_id: Option<String>,
    /// The PO line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order_line_no: Option<u32>,
    /// Batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
    /// Expiry `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,
}

/// Body of `PUT /goods-receipts/{id}` (a draft).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReceiptBody {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// The invoice number.
    pub invoice_no: String,
    /// `YYYY-MM-DD`.
    pub invoice_date: String,
    /// `credit` (default) or `cash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payment: Option<String>,
    /// `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_date: Option<String>,
    /// `included`, `added` or `none` (default).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_mode: Option<String>,
    /// Basis points; default the company's.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_rate_bp: Option<u16>,
    /// Header discount.
    pub header_discount: i64,
    /// Other costs.
    pub other_costs: i64,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<ReceiptLineBody>,
}

/// A goods receipt line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReceiptLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit (invoice).
    pub price: i64,
    /// Line discount.
    pub discount: i64,
    /// The PO.
    pub order_id: Option<String>,
    /// The PO line.
    pub order_line_no: Option<u32>,
    /// Batch.
    pub batch: Option<String>,
    /// `YYYY-MM-DD`.
    pub expiry: Option<String>,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Its header discount share (posted).
    pub header_discount: Option<i64>,
    /// Its PPN (posted).
    pub tax: Option<i64>,
    /// Its other costs share (posted).
    pub other_costs: Option<i64>,
    /// Landed cost ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub landed: Cost,
    /// New unit cost per line unit ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub unit_cost: Cost,
    /// The lot.
    pub lot_id: Option<String>,
}

/// A PO line received beyond the order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OrderLineRef {
    /// The PO.
    pub order_id: String,
    /// The line.
    pub line_no: u32,
}

/// A goods receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoodsReceipt {
    /// ID.
    pub id: String,
    /// Number once posted.
    pub number: Option<String>,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    pub location_id: String,
    /// `draft`, `posted` or `voided`.
    pub status: String,
    /// The invoice number.
    pub invoice_no: String,
    /// `YYYY-MM-DD`.
    pub invoice_date: String,
    /// `credit` or `cash`.
    pub payment: String,
    /// `YYYY-MM-DD`.
    pub due_date: Option<String>,
    /// The tax mode.
    pub tax_mode: String,
    /// Basis points.
    pub tax_rate_bp: u16,
    /// Header discount.
    pub header_discount: i64,
    /// Other costs.
    pub other_costs: i64,
    /// PKP when posted.
    pub pkp: Option<bool>,
    /// Σ line nets (posted).
    pub net: Option<i64>,
    /// PPN (posted).
    pub tax: Option<i64>,
    /// The invoice total (posted).
    pub total: Option<i64>,
    /// Σ landed ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub landed: Cost,
    /// Note.
    pub note: Option<String>,
    /// Who entered it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: Option<i64>,
    /// Unix ms.
    pub voided_at: Option<i64>,
    /// The approving manager of a void.
    pub approved_by: Option<String>,
    /// The lines.
    pub lines: Vec<ReceiptLine>,
    /// The ledger rows (single reads and action answers).
    pub movements: Vec<Movement>,
    /// PO lines received beyond the order (posting answer).
    pub over_received: Vec<OrderLineRef>,
    /// Locations and variants left below zero (posting answers).
    pub negative_after: Vec<NegativeStock>,
}

/// A supplier return line in a body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplierReturnLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit.
    pub price: i64,
    /// The receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    /// The receipt line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_line_no: Option<u32>,
}

/// Body of `PUT /supplier-returns/{id}` (a draft).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SupplierReturnBody {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// `included`, `added` or `none`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_mode: Option<String>,
    /// Basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tax_rate_bp: Option<u16>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<SupplierReturnLineBody>,
}

/// A supplier return line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupplierReturnLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Price per unit.
    pub price: i64,
    /// The receipt.
    pub receipt_id: Option<String>,
    /// The receipt line.
    pub receipt_line_no: Option<u32>,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Value at the average ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
}

/// A supplier return.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SupplierReturn {
    /// ID.
    pub id: String,
    /// Number once posted.
    pub number: Option<String>,
    /// The branch.
    pub branch_id: String,
    /// The supplier.
    pub supplier_id: String,
    /// The location.
    pub location_id: String,
    /// `draft` or `posted`.
    pub status: String,
    /// The tax mode.
    pub tax_mode: String,
    /// Basis points.
    pub tax_rate_bp: u16,
    /// The supplier credit (posted).
    pub credit: Option<i64>,
    /// Note.
    pub note: Option<String>,
    /// Who entered it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: Option<i64>,
    /// The lines.
    pub lines: Vec<SupplierReturnLine>,
    /// The ledger rows.
    pub movements: Vec<Movement>,
    /// Left below zero (posting answer).
    pub negative_after: Vec<NegativeStock>,
}

/// Body of `PUT /transfers/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransferBody {
    /// The sending branch.
    pub from_branch_id: String,
    /// The receiving branch.
    pub to_branch_id: String,
    /// The receiver's location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_location_id: Option<String>,
    /// Ship at once from this location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ship_from_location_id: Option<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<QtyLine>,
}

/// Body of `POST /transfers/{id}/ship` and `/receive`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveBody {
    /// The sender's location (ship) or the receiver's (receive).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// Quantities per line; a line left out moves in full.
    pub lines: Vec<LineQty>,
}

/// An action on a transfer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferAction {
    /// `POST …/ship` (the sender).
    Ship,
    /// `POST …/receive` (the receiver).
    Receive,
    /// `POST …/cancel` (a request only).
    Cancel,
}

impl TransferAction {
    /// The path segment.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ship => "ship",
            Self::Receive => "receive",
            Self::Cancel => "cancel",
        }
    }
}

/// A lot travelling with a transfer line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TravelLot {
    /// Batch.
    pub batch: String,
    /// `YYYY-MM-DD`.
    pub expiry: String,
    /// Base units.
    pub qty_thousandths: i64,
}

/// A transfer line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TransferLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Requested quantity in the unit.
    pub qty_thousandths: i64,
    /// Shipped base units.
    pub shipped_qty_thousandths: Option<i64>,
    /// Received base units.
    pub received_qty_thousandths: Option<i64>,
    /// Lost base units.
    pub lost_qty_thousandths: Option<i64>,
    /// Shipped value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub shipped_value: Cost,
    /// Received value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub received_value: Cost,
    /// Lost value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub lost_value: Cost,
    /// The lots that travel.
    pub lots: Vec<TravelLot>,
}

/// A transfer between branches.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Transfer {
    /// ID.
    pub id: String,
    /// Number (the creating branch's).
    pub number: String,
    /// The sender.
    pub from_branch_id: String,
    /// The sender's location.
    pub from_location_id: Option<String>,
    /// The receiver.
    pub to_branch_id: String,
    /// The receiver's location.
    pub to_location_id: Option<String>,
    /// `requested`, `shipped`, `received` or `cancelled`.
    pub status: String,
    /// Note.
    pub note: Option<String>,
    /// Who created it.
    pub user_id: String,
    /// Unix ms.
    pub shipped_at: Option<i64>,
    /// Unix ms.
    pub received_at: Option<i64>,
    /// The lines.
    pub lines: Vec<TransferLine>,
    /// The ledger rows.
    pub movements: Vec<Movement>,
    /// Left below zero (action answers).
    pub negative_after: Vec<NegativeStock>,
}

/// Body of `PUT /internal-transfers/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InternalTransferBody {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// From.
    pub from_location_id: String,
    /// To.
    pub to_location_id: String,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<QtyLine>,
}

/// An internal transfer line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InternalLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Value moved ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
}

/// An internal transfer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct InternalTransfer {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// From.
    pub from_location_id: String,
    /// To.
    pub to_location_id: String,
    /// Note.
    pub note: Option<String>,
    /// Who posted it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: i64,
    /// The lines.
    pub lines: Vec<InternalLine>,
    /// The ledger rows.
    pub movements: Vec<Movement>,
    /// Left below zero (posting answer).
    pub negative_after: Vec<NegativeStock>,
}

/// An adjustment line in a body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjustmentLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Signed quantity in the unit.
    pub qty_thousandths: i64,
    /// The stock reason.
    pub reason_id: String,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Unit cost for an increase without a cost yet.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit_cost: Option<i64>,
}

/// Body of `PUT /adjustments/{id}` (posts at once).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdjustmentBody {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<AdjustmentLineBody>,
}

/// An adjustment line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AdjustmentLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Signed quantity in the unit.
    pub qty_thousandths: i64,
    /// The reason.
    pub reason_id: String,
    /// The note.
    pub note: Option<String>,
    /// The typed unit cost ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub unit_cost: Cost,
    /// Signed base units.
    pub base_qty_thousandths: i64,
    /// Value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
}

/// A posted adjustment.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Adjustment {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// Note.
    pub note: Option<String>,
    /// Who posted it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: i64,
    /// The lines.
    pub lines: Vec<AdjustmentLine>,
    /// The ledger rows.
    pub movements: Vec<Movement>,
    /// Left below zero (posting answer).
    pub negative_after: Vec<NegativeStock>,
}

/// An opening stock line in a body.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpeningLineBody {
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// Quantity in the unit (> 0).
    pub qty_thousandths: i64,
    /// Unit cost per line unit.
    pub unit_cost: i64,
    /// Batch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<String>,
    /// Expiry `YYYY-MM-DD`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expiry: Option<String>,
}

/// Body of `PUT /opening-stock/{id}` (posts at once).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpeningBody {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The lines.
    pub lines: Vec<OpeningLineBody>,
}

/// An opening stock line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpeningLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The unit's factor.
    pub factor_thousandths: i64,
    /// Quantity in the unit.
    pub qty_thousandths: i64,
    /// Unit cost ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub unit_cost: Cost,
    /// Batch.
    pub batch: Option<String>,
    /// `YYYY-MM-DD`.
    pub expiry: Option<String>,
    /// Base units.
    pub base_qty_thousandths: i64,
    /// Value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
    /// The lot.
    pub lot_id: Option<String>,
}

/// A posted opening stock document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpeningStock {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// Note.
    pub note: Option<String>,
    /// Who posted it.
    pub user_id: String,
    /// Unix ms.
    pub posted_at: i64,
    /// The lines.
    pub lines: Vec<OpeningLine>,
    /// The ledger rows.
    pub movements: Vec<Movement>,
}

/// Body of `PUT /opnames/{id}` (start).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpnameBody {
    /// The branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// The location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location_id: Option<String>,
    /// `full` or `cycle`.
    pub scope: String,
    /// A cycle count's variants.
    pub variant_ids: Vec<String>,
    /// A cycle count's categories.
    pub category_ids: Vec<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// One count entry of `POST /opnames/{id}/counts`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CountEntry {
    /// The client's ID: an entry is stored once.
    pub id: String,
    /// The variant.
    pub variant_id: String,
    /// Any unit of its product.
    pub unit_id: String,
    /// A scan adds; a typed quantity replaces.
    pub qty_thousandths: i64,
    /// A typed quantity.
    pub replace: bool,
}

/// An opname line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpnameLine {
    /// The variant.
    pub variant_id: String,
    /// On hand at the start.
    pub snapshot_thousandths: i64,
    /// Movements since the start.
    pub since_thousandths: i64,
    /// Counted.
    pub counted_thousandths: Option<i64>,
    /// counted − (snapshot + since).
    pub variance_thousandths: Option<i64>,
    /// The posted value ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub value: Cost,
}

/// An opname.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Opname {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// The branch.
    pub branch_id: String,
    /// The location.
    pub location_id: String,
    /// A full count.
    pub full: bool,
    /// `counting`, `posted` or `cancelled`.
    pub status: String,
    /// `set_zero` or `leave` once posted.
    pub uncounted: Option<String>,
    /// Note.
    pub note: Option<String>,
    /// Who started it.
    pub user_id: String,
    /// Unix ms.
    pub started_at: i64,
    /// Unix ms.
    pub posted_at: Option<i64>,
    /// The lines.
    pub lines: Vec<OpnameLine>,
    /// The ledger rows.
    pub movements: Vec<Movement>,
    /// Left below zero (posting answer).
    pub negative_after: Vec<NegativeStock>,
}

/// A reorder rule (base units).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReorderRule {
    /// The branch.
    pub branch_id: String,
    /// The variant.
    pub variant_id: String,
    /// Min.
    pub min_thousandths: i64,
    /// Max.
    pub max_thousandths: i64,
}

/// Body of `PUT /reorder-rules/{branch_id}/{variant_id}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReorderRuleBody {
    /// Min (base units).
    pub min_thousandths: i64,
    /// Max.
    pub max_thousandths: i64,
}

/// A suggested PO line or a low-stock row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SuggestedLine {
    /// The variant.
    pub variant_id: String,
    /// Branch on hand.
    pub on_hand_thousandths: i64,
    /// On order.
    pub on_order_thousandths: i64,
    /// Min.
    pub min_thousandths: Option<i64>,
    /// Max.
    pub max_thousandths: Option<i64>,
    /// The purchase unit.
    pub unit_id: Option<String>,
    /// Whole purchase units.
    pub units: i64,
    /// The same in base units.
    pub base_qty_thousandths: i64,
    /// The last price per purchase unit ([`Cost`]).
    #[serde(deserialize_with = "shown", skip_serializing_if = "Option::is_none")]
    pub price: Cost,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_read_hidden_null_and_values() {
        let hidden: OnHandRow = serde_json::from_str(r#"{"variant_id":"v"}"#).unwrap();
        assert_eq!((hidden.unit_cost, hidden.value), (None, None));
        let none: OnHandRow = serde_json::from_str(r#"{"unit_cost":null,"value":5}"#).unwrap();
        assert_eq!((none.unit_cost, none.value), (Some(None), Some(Some(5))));
        // Round trip keeps the three states.
        let text = serde_json::to_string(&none).unwrap();
        assert!(text.contains(r#""unit_cost":null"#), "{text}");
        let back: OnHandRow = serde_json::from_str(&text).unwrap();
        assert_eq!(back, none);
        assert!(
            !serde_json::to_string(&hidden)
                .unwrap()
                .contains("unit_cost")
        );
    }

    #[test]
    fn defaults_and_actions() {
        let s = InventorySettings::default();
        assert_eq!((s.standard_ppn_rate_bp, s.cover_days), (1_100, 14));
        let b = SupplierBody::new("S1", "Satu");
        assert_eq!((b.lead_days, b.active), (7, true));
        assert_eq!(
            [OrderAction::Order, OrderAction::Close, OrderAction::Cancel].map(OrderAction::as_str),
            ["order", "close", "cancel"]
        );
        assert_eq!(
            [
                TransferAction::Ship,
                TransferAction::Receive,
                TransferAction::Cancel
            ]
            .map(TransferAction::as_str),
            ["ship", "receive", "cancel"]
        );
        let q = serde_json::to_value(OnHandQuery {
            filter: Some("low".into()),
            ..OnHandQuery::default()
        })
        .unwrap();
        assert_eq!(q, serde_json::json!({ "filter": "low" }));
    }
}
