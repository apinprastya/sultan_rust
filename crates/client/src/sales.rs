//! Sales as the register and the back office see them (spec 2.6, phase S3; the JSON of
//! `crates/server/src/routes/{tender_types,sales,returns,held_carts}.rs`).
//!
//! Money is minor units (`i64`), quantities thousandths, times unix ms, IDs UUID strings. The
//! request types ([`SaleRequest`], [`ReturnRequest`], [`HoldRequest`], …) are what the register
//! posts; the answers ([`Sale`], [`SaleReturn`], [`HeldCart`], …) are read with `#[serde(default)]`
//! so later server fields never break an older client.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `sale.create`: sell, hold and resume carts.
pub const SALE_CREATE: &str = "sale.create";
/// `sale.post_void`: void a posted sale (overridable).
pub const SALE_POST_VOID: &str = "sale.post_void";
/// `sale.reprint`: reprint any receipt but the last of the session (overridable).
pub const SALE_REPRINT: &str = "sale.reprint";
/// `tender.manage`: tender types (HQ back office).
pub const TENDER_MANAGE: &str = "tender.manage";

/// A tender type (`GET /tender-types`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)] // the server's flags, one field each
pub struct TenderType {
    /// ID.
    pub id: String,
    /// Name shown and printed.
    pub name: String,
    /// `cash`, `card`, `qris`, `transfer`, `ewallet`, `customer_balance`, `other`.
    pub kind: String,
    /// A reference must be typed.
    pub needs_reference: bool,
    /// Opens the drawer.
    pub opens_drawer: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position.
    pub sort: u32,
    /// Archived.
    pub archived: bool,
}

/// Body of `PUT /tender-types/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenderTypeBody {
    /// Name, 1–40 characters.
    pub name: String,
    /// The kind; fixed once created.
    pub kind: String,
    /// A reference must be typed.
    pub needs_reference: bool,
    /// Opens the drawer.
    pub opens_drawer: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position.
    pub sort: u32,
}

/// A manual discount: `pct_bp` or `amount`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Discount {
    /// Basis points (`1000` = 10%).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pct_bp: Option<u16>,
    /// Minor units.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
}

/// One posted cart line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaleLineRequest {
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// An open price or a manual override.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub typed_price: Option<i64>,
    /// A line discount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discount: Option<Discount>,
    /// `weight` or `price`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scale_label: Option<String>,
    /// The printed amount of a `price` label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_amount: Option<i64>,
}

/// One posted tender.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenderRequest {
    /// The tender type.
    pub tender_type_id: String,
    /// The amount (cash: what was handed over).
    pub amount: i64,
    /// A reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// The totals the register showed; the server compares them with its own quote.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleTotals {
    /// Σ gross.
    pub subtotal: i64,
    /// Σ line discounts.
    pub line_discount: i64,
    /// Cart discount.
    pub cart_discount: i64,
    /// Σ tax.
    pub tax: i64,
    /// Total before cash rounding.
    pub total: i64,
}

/// Body of `PUT /sales/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaleRequest {
    /// The attached customer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    /// A tax invoice (D13).
    pub tax_invoice: bool,
    /// The lines.
    pub lines: Vec<SaleLineRequest>,
    /// A whole-cart discount.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cart_discount: Option<Discount>,
    /// The tenders.
    pub tenders: Vec<TenderRequest>,
    /// Change saved to the customer balance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save_change: Option<i64>,
    /// The totals shown.
    pub totals: SaleTotals,
}

/// A line of a posted sale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleLine {
    /// From 1.
    pub line_no: u32,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The receipt name.
    pub name: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// Unit price.
    pub unit_price: i64,
    /// `listed`, `open_price`, `overridden`.
    pub price_mode: String,
    /// The price an override replaced.
    pub resolved_price: Option<i64>,
    /// The approving manager of an override.
    pub override_by: Option<String>,
    /// `weight` or `price`.
    pub scale_label: Option<String>,
    /// Gross.
    pub gross: i64,
    /// Line discount.
    pub line_discount: i64,
    /// Share of the cart discount.
    pub cart_discount_share: i64,
    /// Amount.
    pub amount: i64,
    /// Tax rate, basis points.
    pub tax_rate_bp: u16,
    /// Tax.
    pub tax: i64,
    /// Total.
    pub total: i64,
    /// Returned so far, thousandths.
    pub returned_qty_thousandths: i64,
    /// Refunded so far.
    pub refunded: i64,
}

/// A tender of a posted sale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleTender {
    /// The tender type (`None` for an exchange).
    pub tender_type_id: Option<String>,
    /// The kind (also `exchange`).
    pub kind: String,
    /// Amount.
    pub amount: i64,
    /// Cash received.
    pub cash_received: Option<i64>,
    /// Change handed back.
    pub change_given: i64,
    /// Change saved.
    pub change_saved: i64,
    /// Reference.
    pub reference: Option<String>,
}

/// A refund tender (post-void, return).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefundTender {
    /// The tender type (`None` for an exchange).
    pub tender_type_id: Option<String>,
    /// `cash`, `customer_balance`, `exchange`.
    pub kind: String,
    /// Amount.
    pub amount: i64,
}

/// How a sale was post-voided.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleVoid {
    /// The register that paid back.
    pub register_id: String,
    /// The session that paid back.
    pub cashier_session_id: String,
    /// Who voided.
    pub user_id: String,
    /// The reason key.
    pub reason: String,
    /// The text of `other`.
    pub reason_text: Option<String>,
    /// Everything paid back.
    pub refund: i64,
    /// Saved change taken back.
    pub change_saved_reversed: i64,
    /// Balance payment given back.
    pub payment_reversed: i64,
    /// Refunded to the balance.
    pub to_balance: i64,
    /// Cash paid out.
    pub cash: i64,
    /// Cash rounding.
    pub cash_rounding: i64,
    /// Refund tenders.
    pub tenders: Vec<RefundTender>,
    /// When.
    pub voided_at: i64,
}

/// A sale header (lists, recent receipts).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleSummary {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// An imported sale's old number.
    pub legacy_number: Option<String>,
    /// `posted` or `voided`.
    pub status: String,
    /// Branch.
    pub branch_id: String,
    /// Register.
    pub register_id: Option<String>,
    /// Session.
    pub cashier_session_id: Option<String>,
    /// Cashier.
    pub user_id: Option<String>,
    /// Customer.
    pub customer_id: Option<String>,
    /// Tax invoice.
    pub tax_invoice: bool,
    /// Tax-inclusive prices.
    pub tax_inclusive: bool,
    /// Σ gross.
    pub subtotal: i64,
    /// Σ line discounts.
    pub line_discount: i64,
    /// Cart discount.
    pub cart_discount: i64,
    /// Σ tax.
    pub tax: i64,
    /// Total before cash rounding.
    pub total: i64,
    /// Cash rounding.
    pub cash_rounding: i64,
    /// What was paid.
    pub paid: i64,
    /// Change handed back.
    pub change_given: i64,
    /// Change saved.
    pub change_saved: i64,
    /// Lines.
    pub line_count: u32,
    /// Posted returns.
    pub return_count: u32,
    /// Posted (unix ms).
    pub posted_at: i64,
    /// Voided (unix ms).
    pub voided_at: Option<i64>,
}

impl SaleSummary {
    /// The totals as a [`SaleRequest`] posts them.
    #[must_use]
    pub const fn totals(&self) -> SaleTotals {
        SaleTotals {
            subtotal: self.subtotal,
            line_discount: self.line_discount,
            cart_discount: self.cart_discount,
            tax: self.tax,
            total: self.total,
        }
    }
}

/// A posted sale (`PUT` / `GET /sales/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sale {
    /// The header.
    #[serde(flatten)]
    pub summary: SaleSummary,
    /// Lines.
    pub lines: Vec<SaleLine>,
    /// Tenders.
    pub tenders: Vec<SaleTender>,
    /// The post-void.
    pub void: Option<SaleVoid>,
}

/// One page of `GET /sales`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SalePage {
    /// Newest first.
    pub items: Vec<SaleSummary>,
    /// The next page's `before_id`.
    pub next_before_id: Option<String>,
}

/// Filters of `GET /sales` and `GET /returns`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocQuery {
    /// Branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// Register.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub register_id: Option<String>,
    /// Cashier session (a cashier's own session needs no `report.sales`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cashier_session_id: Option<String>,
    /// Customer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    /// From (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// Until (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// Number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<String>,
    /// Page cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    /// Page size, 1–200.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A receipt (`GET /sales/{id}/receipt`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Receipt {
    /// The sale.
    pub sale_id: String,
    /// Its number.
    pub number: String,
    /// A reprint.
    pub copy: bool,
    /// Voided.
    pub voided: bool,
    /// The text lines.
    pub lines: Vec<String>,
    /// The text.
    pub text: String,
    /// The barcode/QR payload.
    pub barcode: String,
}

/// Body of `POST /sales/{id}/void`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoidRequest {
    /// `customer_changed_mind`, `wrong_item`, `price_dispute`, `other`.
    pub reason: String,
    /// The text of `other`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_text: Option<String>,
    /// `cash` or `balance`.
    pub refund: String,
}

/// A removed line of `POST /cart-voids`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VoidedLine {
    /// The variant.
    pub variant_id: String,
    /// The receipt name.
    pub name: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// Gross amount.
    pub amount: i64,
}

/// Body of `POST /cart-voids`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CartVoidRequest {
    /// `line` or `cart`.
    pub kind: String,
    /// The cart's ID (the override's `context_ref`).
    pub cart_id: String,
    /// The reason key.
    pub reason: String,
    /// The text of `other`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_text: Option<String>,
    /// The removed lines.
    pub lines: Vec<VoidedLine>,
}

/// The answer of `POST /cart-voids`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CartVoid {
    /// `line` or `cart`.
    pub kind: String,
    /// The session it counts in.
    pub cashier_session_id: String,
    /// Σ amounts.
    pub amount: i64,
    /// The approving manager.
    pub approved_by: Option<String>,
}

/// A returned line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnLineRequest {
    /// The sale line (by receipt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sale_line_no: Option<u32>,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// Written off with this reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub write_off_reason: Option<String>,
}

/// An exchange sale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExchangeRequest {
    /// The new sale's ID.
    pub sale_id: String,
    /// The new sale.
    pub sale: SaleRequest,
}

/// Body of `PUT /returns/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReturnRequest {
    /// The original sale (by receipt).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sale_id: Option<String>,
    /// The attached customer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// `cash` or `balance`.
    pub refund: String,
    /// The lines.
    pub lines: Vec<ReturnLineRequest>,
    /// An exchange sale.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exchange: Option<ExchangeRequest>,
}

/// A stored return line.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReturnLine {
    /// From 1.
    pub line_no: u32,
    /// The sale line.
    pub sale_line_no: Option<u32>,
    /// The variant.
    pub variant_id: String,
    /// The unit.
    pub unit_id: String,
    /// The receipt name.
    pub name: String,
    /// Quantity, thousandths.
    pub qty_thousandths: i64,
    /// Refund.
    pub refund: i64,
    /// `restock` or `write_off`.
    pub disposition: String,
    /// Why written off.
    pub write_off_reason: Option<String>,
}

/// A return's header.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReturnSummary {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// `posted`.
    pub status: String,
    /// The original sale.
    pub sale_id: Option<String>,
    /// Branch.
    pub branch_id: String,
    /// Register.
    pub register_id: String,
    /// Session.
    pub cashier_session_id: String,
    /// Cashier.
    pub user_id: String,
    /// Customer.
    pub customer_id: Option<String>,
    /// The note.
    pub reason: Option<String>,
    /// Σ refunds.
    pub refund: i64,
    /// Cash rounding.
    pub cash_rounding: i64,
    /// The exchange sale.
    pub exchange_sale_id: Option<String>,
    /// Posted (unix ms).
    pub posted_at: i64,
}

/// A posted return (`PUT` / `GET /returns/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleReturn {
    /// The header.
    #[serde(flatten)]
    pub summary: ReturnSummary,
    /// Lines.
    pub lines: Vec<ReturnLine>,
    /// Refund tenders.
    pub tenders: Vec<RefundTender>,
    /// The exchange sale.
    pub exchange_sale: Option<Sale>,
}

/// One page of `GET /returns`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReturnPage {
    /// Newest first.
    pub items: Vec<ReturnSummary>,
    /// The next page's `before_id`.
    pub next_before_id: Option<String>,
}

/// Body of `PUT /held-carts/{id}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HoldRequest {
    /// The attached customer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub customer_id: Option<String>,
    /// A note.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The cart as the register keeps it.
    pub payload: Value,
    /// Item count.
    pub item_count: u32,
    /// Total.
    pub total: i64,
}

/// A held cart.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HeldCart {
    /// ID.
    pub id: String,
    /// The register that held it.
    pub register_id: String,
    /// The cashier who held it.
    pub user_id: String,
    /// The customer.
    pub customer_id: Option<String>,
    /// The note.
    pub note: Option<String>,
    /// The cart.
    pub payload: Value,
    /// Item count.
    pub item_count: u32,
    /// Total.
    pub total: i64,
    /// Held at (unix ms).
    pub held_at: i64,
    /// Who resumed it.
    pub taken_by: Option<String>,
}

/// One entry of a customer's purchase history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryItem {
    /// `sale` or `return`.
    pub kind: String,
    /// The document.
    pub document_id: String,
    /// Its number.
    pub number: String,
    /// Branch.
    pub branch_id: String,
    /// The customer it was posted for.
    pub customer_id: String,
    /// Posted (unix ms).
    pub posted_at: i64,
    /// Paid (sale) or refunded (return).
    pub total: i64,
    /// A voided sale (shown marked VOID).
    pub voided: bool,
}

/// A page of purchase history.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistoryPage {
    /// Newest first.
    pub items: Vec<HistoryItem>,
    /// The next page's `before`.
    pub next_before_id: Option<String>,
}
