//! Cash management as the register and the back office see it (spec 2.7, phase K3; the JSON of
//! `crates/server/src/routes/{cash_reasons,cash_movements,cash_reports,z_reports}.rs`).
//!
//! Money is minor units (`i64`), times unix ms, business days `YYYY-MM-DD`, IDs UUID strings. The
//! answers are read with `#[serde(default)]` so later server fields never break an older client,
//! and an older server's close report (without the cash and float blocks) still reads.

use serde::{Deserialize, Serialize};

/// `cash.in`: cash into the drawer (overridable).
pub const CASH_IN: &str = "cash.in";
/// `cash.out`: cash out of the drawer, limited per movement (overridable).
pub const CASH_OUT: &str = "cash.out";
/// `cash.drop`: a safe drop (overridable).
pub const CASH_DROP: &str = "cash.drop";
/// `day.close`: run a Z (overridable at the till).
pub const DAY_CLOSE: &str = "day.close";
/// `session.view_expected`: the expected cash, and the X report (overridable for an X).
pub const SESSION_VIEW_EXPECTED: &str = "session.view_expected";

/// A cash in / out reason (`GET /cash-reasons`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CashReason {
    /// ID.
    pub id: String,
    /// Name shown and printed.
    pub name: String,
    /// `in` or `out`.
    pub direction: String,
    /// A note is required with it.
    pub note_required: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position.
    pub sort: i32,
    /// Archived.
    pub archived: bool,
}

/// Body of `PUT /cash-reasons/{id}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashReasonBody {
    /// Name, 1–60 characters.
    pub name: String,
    /// `in` or `out`; fixed once created.
    pub direction: String,
    /// A note is required with it.
    pub note_required: bool,
    /// Offered at the register.
    pub active: bool,
    /// Position.
    pub sort: i32,
}

/// Body of `PUT /cash-movements/{id}`: a cash in / out / drop, or the reversal of one
/// (`reverses` and a note; kind, amount and reason may be left out).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashMovementRequest {
    /// `in`, `out` or `drop`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whole Rupiah above 0.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<i64>,
    /// The reason (in and out).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_id: Option<String>,
    /// The note (at most 200 characters).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The movement this one reverses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverses: Option<String>,
}

/// A cash movement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CashMovement {
    /// ID.
    pub id: String,
    /// `JKT01-R2-K-261119-01`.
    pub number: String,
    /// `in`, `out` or `drop`.
    pub kind: String,
    /// The amount.
    pub amount: i64,
    /// What it did to the expected cash (signed).
    pub effect: i64,
    /// The reason.
    pub reason_id: Option<String>,
    /// The reason's name.
    pub reason_name: Option<String>,
    /// The note.
    pub note: Option<String>,
    /// Branch.
    pub branch_id: String,
    /// Register.
    pub register_id: String,
    /// Cashier session.
    pub cashier_session_id: String,
    /// The cashier.
    pub user_id: String,
    /// The approving manager.
    pub approved_by: Option<String>,
    /// The movement it reverses.
    pub reverses: Option<String>,
    /// Its number.
    pub reverses_number: Option<String>,
    /// The movement that reverses it.
    pub reversed_by: Option<String>,
    /// Posted at.
    pub posted_at: i64,
    /// Its nota photos.
    pub photo_ids: Vec<String>,
    /// A cash out missing its nota (§2a).
    pub missing_nota: bool,
}

/// One page of `GET /cash-movements`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CashMovementPage {
    /// Newest first.
    pub items: Vec<CashMovement>,
    /// Cursor of the next page.
    pub next_before_id: Option<String>,
}

/// Filters and paging of `GET /cash-movements`; unset fields are left out of the query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashQuery {
    /// Only this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// Only this register.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub register_id: Option<String>,
    /// Only this cashier session.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cashier_session_id: Option<String>,
    /// Only this kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Only this reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_id: Option<String>,
    /// Posted at or after (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// Posted before (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// Only cash outs missing a nota.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_nota: Option<bool>,
    /// Page cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    /// Page size (server default 50, at most 200).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A count and an amount.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CountAmount {
    /// How many.
    pub count: u32,
    /// The sum.
    pub amount: i64,
}

/// One reason's line in a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReasonLine {
    /// The reason.
    pub reason_id: String,
    /// Its name.
    pub name: String,
    /// `in` or `out`.
    pub direction: String,
    /// Its order.
    pub sort: i32,
    /// Movements, reversals not counted.
    pub count: u32,
    /// Σ amounts minus reversals.
    pub amount: i64,
    /// Reversals.
    pub reversals: u32,
}

/// The cash block of a close report, an X or a Z (spec 2.7 §4).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CashReport {
    /// Cash in.
    pub cash_in: CountAmount,
    /// Cash out.
    pub cash_out: CountAmount,
    /// Safe drops.
    pub drops: CountAmount,
    /// Reversals.
    pub reversals: CountAmount,
    /// Per reason.
    pub reasons: Vec<ReasonLine>,
    /// No-sale drawer opens.
    pub drawer_opens: u32,
    /// X reads.
    pub x_reports: u32,
    /// Cash outs missing a nota.
    pub missing_nota: u32,
}

/// Sales and refunds of one tender type in the sales block.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SalesTender {
    /// The tender type (`None` for the exchange tender).
    pub tender_type_id: Option<String>,
    /// Its kind.
    pub kind: String,
    /// Sales.
    pub sales: CountAmount,
    /// Refunds.
    pub refunds: CountAmount,
}

/// The sales block of a close report, an X or a Z (spec 2.6 §10, with spec 2.7's cash).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SalesReport {
    /// Cash at open.
    pub opening_cash: i64,
    /// Cash received.
    pub cash_received: i64,
    /// Change given.
    pub change_given: i64,
    /// Cash refunds.
    pub cash_refunds: i64,
    /// Cash rounding.
    pub cash_rounding: i64,
    /// Cash in.
    pub cash_in: i64,
    /// Cash out.
    pub cash_out: i64,
    /// Safe drops.
    pub drops: i64,
    /// Balance payouts.
    pub payouts: i64,
    /// Expected cash.
    pub expected_cash: i64,
    /// Per tender type.
    pub tenders: Vec<SalesTender>,
    /// Line voids.
    pub line_voids: CountAmount,
    /// Cart voids.
    pub cart_voids: CountAmount,
    /// Post-voids.
    pub post_voids: CountAmount,
    /// Returns.
    pub returns: CountAmount,
    /// Line discounts.
    pub line_discounts: CountAmount,
    /// Cart discounts.
    pub cart_discounts: CountAmount,
    /// Price overrides.
    pub price_overrides: CountAmount,
    /// Saved change.
    pub saved_change: i64,
    /// Paid with balances.
    pub balance_paid: i64,
}

/// The float of a closed session (spec 2.7 §3).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FloatInfo {
    /// Cash kept for the next session (`None` before 2.7).
    pub float_kept: Option<i64>,
    /// Taken to the safe.
    pub to_safe: Option<i64>,
    /// The proposed opening cash, when another amount was typed.
    pub opening_proposed: Option<i64>,
    /// `opening_cash − opening_proposed`.
    pub float_difference: i64,
}

/// An X report (`GET /cashier-sessions/{id}/x-report`): never a count or an over/short.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct XReport {
    /// The session.
    pub session_id: String,
    /// Its number.
    pub number: String,
    /// Branch code.
    pub branch_code: String,
    /// Branch name.
    pub branch_name: String,
    /// Branch time zone.
    pub timezone: String,
    /// Register code.
    pub register_code: String,
    /// The cashier.
    pub cashier_name: String,
    /// Opened at.
    pub opened_at: i64,
    /// Read at.
    pub read_at: i64,
    /// The approving manager.
    pub approved_by: Option<String>,
    /// Cash at open.
    pub opening_cash: i64,
    /// Expected cash now.
    pub expected_cash: i64,
    /// The sales block.
    pub sales: SalesReport,
    /// The cash block.
    pub cash: CashReport,
    /// The proposed opening cash.
    pub opening_proposed: Option<i64>,
    /// The float difference.
    pub float_difference: i64,
}

/// A printable text: a slip, an X, a close report or a Z (until 2.11 prints it, the screen shows
/// it).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CashText {
    /// A reprint (COPY).
    pub copy: bool,
    /// The lines.
    pub lines: Vec<String>,
    /// The text.
    pub text: String,
    /// Lines printed double size.
    pub large: Vec<usize>,
}

/// `GET /registers/me/float`: the opening cash the open screen proposes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProposedFloat {
    /// This register.
    pub register_id: String,
    /// The float kept by its last closed session, 0 without one.
    pub proposed_float: i64,
    /// Whether the last closed session kept a float.
    pub has_proposal: bool,
}

/// Body of `POST /z-reports`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZRunRequest {
    /// The business day, `YYYY-MM-DD`.
    pub day: String,
    /// The branch (back office); a register runs its own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
}

/// A sale number and when it was posted.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SaleRef {
    /// The number.
    pub number: String,
    /// Posted at.
    pub posted_at: i64,
}

/// Tax per rate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaxLine {
    /// The rate in basis points.
    pub rate_bp: u16,
    /// Σ base.
    pub base: i64,
    /// Σ tax.
    pub tax: i64,
}

/// The totals of a register or of the branch in a Z.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZTotals {
    /// Sessions.
    pub sessions: u32,
    /// Sales.
    pub sales_count: u32,
    /// Gross sales.
    pub gross_sales: i64,
    /// Net sales.
    pub net_sales: i64,
    /// The first sale.
    pub first_sale: Option<SaleRef>,
    /// The last sale.
    pub last_sale: Option<SaleRef>,
    /// Tax per rate.
    pub tax: Vec<TaxLine>,
    /// Σ expected cash.
    pub expected_cash: i64,
    /// Σ counted cash.
    pub counted_cash: i64,
    /// Σ over/short.
    pub over_short: i64,
    /// Σ float kept.
    pub float_kept: i64,
    /// Σ to safe.
    pub to_safe: i64,
    /// Σ float differences.
    pub float_difference: i64,
    /// Σ drops + Σ to safe.
    pub cash_to_deposit: i64,
    /// The summed sales block.
    pub sales: SalesReport,
    /// The summed cash block.
    pub cash: CashReport,
}

/// One session in a Z.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZSessionLine {
    /// The number.
    pub number: String,
    /// The register code.
    pub register: String,
    /// The cashier.
    pub cashier: String,
    /// Opened at.
    pub opened_at: i64,
    /// Closed at.
    pub closed_at: i64,
    /// Force-closed.
    pub forced: bool,
    /// Opening cash.
    pub opening_cash: i64,
    /// Expected cash.
    pub expected_cash: i64,
    /// Counted cash.
    pub counted_cash: i64,
    /// Over/short.
    pub over_short: i64,
    /// Float kept.
    pub float_kept: i64,
    /// To safe.
    pub to_safe: i64,
    /// Float difference.
    pub float_difference: i64,
}

/// A register's subtotals in a Z.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZRegister {
    /// The register code.
    pub register: String,
    /// Its totals.
    pub totals: ZTotals,
}

/// The stored snapshot of a Z.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZSnapshot {
    /// `JKT01-Z-261119-01`.
    pub number: String,
    /// The business day.
    pub day: String,
    /// The sequence within the day.
    pub seq: u32,
    /// A later Z of the same day.
    pub supplementary: bool,
    /// Run at.
    pub run_at: i64,
    /// The sessions.
    pub sessions: Vec<ZSessionLine>,
    /// Per register.
    pub registers: Vec<ZRegister>,
    /// Branch totals.
    pub branch: ZTotals,
}

/// A Z report (`POST /z-reports`, `GET /z-reports/{id}`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZReport {
    /// ID.
    pub id: String,
    /// The branch.
    pub branch_id: String,
    /// Who ran it.
    pub user_id: String,
    /// Their name.
    pub run_by_name: String,
    /// The approving manager.
    pub approved_by: Option<String>,
    /// Their name.
    pub approved_by_name: Option<String>,
    /// The sessions it covers.
    pub session_ids: Vec<String>,
    /// The snapshot.
    pub report: ZSnapshot,
}

/// A Z in a list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZSummary {
    /// ID.
    pub id: String,
    /// Number.
    pub number: String,
    /// Branch.
    pub branch_id: String,
    /// Business day.
    pub day: String,
    /// Sequence.
    pub seq: u32,
    /// Supplementary.
    pub supplementary: bool,
    /// Sessions.
    pub session_count: u32,
    /// Sales.
    pub sales_count: u32,
    /// Net sales.
    pub net_sales: i64,
    /// Σ expected cash.
    pub expected_cash: i64,
    /// Σ counted cash.
    pub counted_cash: i64,
    /// Σ over/short.
    pub over_short: i64,
    /// Cash to deposit.
    pub cash_to_deposit: i64,
    /// Run at.
    pub run_at: i64,
    /// Who ran it.
    pub user_id: String,
    /// Who approved it.
    pub approved_by: Option<String>,
}

/// One page of `GET /z-reports`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ZReportPage {
    /// Newest first.
    pub items: Vec<ZSummary>,
    /// Cursor of the next page.
    pub next_before_id: Option<String>,
}

/// Filters and paging of `GET /z-reports`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZQuery {
    /// Only this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// Business day on or after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// Business day on or before.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Page cursor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    /// Page size.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// A day still to close (`GET /z-reports/pending`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PendingDay {
    /// The business day.
    pub day: String,
    /// Sessions still open.
    pub open: u32,
    /// Closed sessions in no Z.
    pub closed: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries_and_requests_leave_out_unset_fields() {
        let q = CashQuery {
            cashier_session_id: Some("s".into()),
            missing_nota: Some(true),
            ..CashQuery::default()
        };
        assert_eq!(
            serde_json::to_value(&q).unwrap(),
            serde_json::json!({"cashier_session_id": "s", "missing_nota": true})
        );
        let reversal = CashMovementRequest {
            reverses: Some("m".into()),
            note: Some("wrong".into()),
            ..CashMovementRequest::default()
        };
        assert_eq!(
            serde_json::to_value(&reversal).unwrap(),
            serde_json::json!({"reverses": "m", "note": "wrong"})
        );
        let z = ZRunRequest {
            day: "2026-11-19".into(),
            branch_id: None,
        };
        assert_eq!(
            serde_json::to_value(&z).unwrap(),
            serde_json::json!({"day": "2026-11-19"})
        );
        // Answers with fields missing still read.
        let x: XReport = serde_json::from_value(serde_json::json!({"number": "N"})).unwrap();
        assert_eq!((x.number.as_str(), x.cash.x_reports), ("N", 0));
        let page: ZReportPage = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(page.items.is_empty());
    }
}
