//! Request and response bodies, mirroring the server's JSON (`crates/server/src/routes`). The
//! server's types are not reused because they live in the server crate and are `Serialize`-only;
//! `sultan-core` types carry no serde. Unknown fields are ignored, so the server may add some.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One lock-screen tile (`GET /registers/me/users`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockUser {
    /// User ID.
    pub id: String,
    /// Name on the tile.
    pub display_name: String,
}

/// A PIN session (`POST /auth/pin`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    /// Bearer token.
    pub token: String,
    /// The user.
    pub user_id: String,
    /// The user's display name.
    pub display_name: String,
    /// The register's branch.
    pub branch_id: String,
    /// The register.
    pub register_id: Option<String>,
    /// Idle expiry (unix ms).
    pub expires_at: i64,
}

/// The parts of a branch the register shows (`GET /branches/{id}`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Branch {
    /// Branch ID.
    pub id: String,
    /// Code, e.g. `JKT01`.
    pub code: String,
    /// Display name.
    pub name: String,
    /// IANA time zone of the branch's document dates (empty from an older server).
    #[serde(default)]
    pub timezone: String,
}

/// A cashier session (`GET /cashier-sessions/current`, `POST /cashier-sessions`,
/// `GET /cashier-sessions[/{id}]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashierSession {
    /// Session ID.
    pub id: String,
    /// Session number, e.g. `JKT01-R2-S-261107-01` (branch-local open date, spec 2.1 §5).
    #[serde(default)]
    pub number: String,
    /// The cashier.
    pub user_id: String,
    /// The register.
    pub register_id: String,
    /// The branch.
    pub branch_id: String,
    /// Cash at open (minor units).
    pub opening_cash: i64,
    /// Open time (unix ms).
    pub opened_at: i64,
    /// `open` or `closed`.
    pub status: String,
    /// Close time (unix ms), once closed.
    #[serde(default)]
    pub closed_at: Option<i64>,
    /// Expected cash: while open only for holders of `session.view_expected`, and the register
    /// UI never shows it before the close (blind close, spec 2.1 §5); once closed, the figure of
    /// the close.
    #[serde(default)]
    pub expected_cash: Option<i64>,
}

/// `GET /cashier-sessions/{id}`: a session and, once closed, its stored close report (the source
/// of a reprint, spec 2.1 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionDetail {
    /// The session.
    pub session: CashierSession,
    /// The close report as the close returned it; `None` while open.
    pub close_report: Option<CloseReport>,
}

/// Filters and paging of `GET /cashier-sessions`; unset fields are left out of the query.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionQuery {
    /// Only this branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_id: Option<String>,
    /// Only this register.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub register_id: Option<String>,
    /// Only this cashier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// `open` or `closed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Opened at or after (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<i64>,
    /// Opened before (unix ms).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub until: Option<i64>,
    /// Page cursor: `next_before_id` of the previous page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_id: Option<String>,
    /// Page size (server default 100, at most 500).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One page of `GET /cashier-sessions`, newest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPage {
    /// The sessions the caller may read.
    pub items: Vec<CashierSession>,
    /// Cursor of the next page; `None` on the last page.
    pub next_before_id: Option<String>,
}

/// Notes or coins of one denomination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Count {
    /// Face value (minor units).
    pub denomination: i64,
    /// Number counted.
    pub quantity: u32,
}

/// Body of `POST /cashier-sessions/{id}/close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseRequest {
    /// Cash counted (minor units).
    pub counted_cash: i64,
    /// Optional count per denomination; must add up to `counted_cash`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub counts: Option<Vec<Count>>,
}

/// Totals of one tender in the close report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TenderTotal {
    /// `cash`, `card`, `qris` or `transfer`.
    pub tender: String,
    /// Sum of sales.
    pub sales: i64,
    /// Sum of refunds.
    pub refunds: i64,
    /// Sales minus refunds.
    pub net: i64,
    /// Number of sales and refunds.
    pub count: u32,
}

/// The close report (spec 2.1 §5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseReport {
    /// The closed session.
    pub session: CashierSession,
    /// Whether a manager force-closed it.
    pub forced: bool,
    /// Branch code, e.g. `JKT01` (for the printout).
    #[serde(default)]
    pub branch_code: String,
    /// Branch name.
    #[serde(default)]
    pub branch_name: String,
    /// Branch IANA time zone, for printing local times.
    #[serde(default)]
    pub timezone: String,
    /// Register code, e.g. `R2`.
    #[serde(default)]
    pub register_code: String,
    /// Register name.
    #[serde(default)]
    pub register_name: String,
    /// The cashier's display name.
    #[serde(default)]
    pub cashier_name: String,
    /// The manager who force-closed it, if forced.
    #[serde(default)]
    pub forced_by_name: Option<String>,
    /// Cash at open.
    pub opening_cash: i64,
    /// Cash sales.
    pub cash_sales: i64,
    /// Cash refunds.
    pub cash_refunds: i64,
    /// Petty cash in.
    pub cash_in: i64,
    /// Petty cash out.
    pub cash_out: i64,
    /// Expected cash in the drawer.
    pub expected_cash: i64,
    /// What was counted.
    pub counted_cash: i64,
    /// `counted − expected`: positive = over, negative = short.
    pub over_short: i64,
    /// Number of sales and refunds.
    pub transactions: u32,
    /// Totals per tender.
    pub tenders: Vec<TenderTotal>,
    /// The count per denomination, if entered.
    pub counts: Vec<Count>,
}

/// Body of `POST /overrides` (spec 2.1 §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideRequest {
    /// Permission key, e.g. `sale.discount`.
    pub action: String,
    /// Requested value (basis points or minor units).
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
    /// The approving manager.
    pub approver_id: String,
    /// The manager's PIN.
    pub approver_pin: String,
}

/// An approved override (`POST /overrides`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverrideGrant {
    /// Single-use token for `X-Override`.
    pub token: String,
    /// The approved action.
    pub action: String,
    /// The approved value.
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
    /// The cashier who asked.
    pub requester_id: String,
    /// The approving manager.
    pub approver_id: String,
    /// End of validity (unix ms).
    pub expires_at: i64,
}

/// Body of `POST /demo/override-protected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedAction {
    /// Permission key.
    pub action: String,
    /// Requested value.
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
}

/// Answer of `POST /demo/override-protected`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtectedResult {
    /// The authorized action.
    pub action: String,
    /// The authorized value.
    pub value: Option<i64>,
    /// The document or cart.
    pub context_ref: String,
    /// The manager whose override was used, or `None` when the caller's own permissions sufficed.
    pub approved_by: Option<String>,
}

/// One message of the events WebSocket: `{"type": "...", "data": {...}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerEvent {
    /// Event type, e.g. `session.revoked`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Event data.
    #[serde(default)]
    pub data: Value,
}

impl ServerEvent {
    /// `session.revoked` for the socket's own login session (`this_session: true`).
    #[must_use]
    pub fn revokes_this_session(&self) -> bool {
        self.kind == "session.revoked" && self.data["this_session"].as_bool() == Some(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revoked_event_is_recognised() {
        let event = |kind: &str, data: Value| ServerEvent {
            kind: kind.to_owned(),
            data,
        };
        let cases = [
            (
                event("session.revoked", serde_json::json!({"this_session": true})),
                true,
            ),
            (
                event(
                    "session.revoked",
                    serde_json::json!({"this_session": false}),
                ),
                false,
            ),
            (event("session.revoked", Value::Null), false),
            (
                event("user.changed", serde_json::json!({"this_session": true})),
                false,
            ),
        ];
        for (event, expected) in cases {
            assert_eq!(event.revokes_this_session(), expected, "{event:?}");
        }
        let parsed: ServerEvent = serde_json::from_str(r#"{"type":"events.lagged"}"#).unwrap();
        assert_eq!(parsed.data, Value::Null);
    }

    #[test]
    fn session_query_leaves_out_unset_fields() {
        let query = SessionQuery {
            register_id: Some("r".into()),
            status: Some("closed".into()),
            limit: Some(1),
            ..SessionQuery::default()
        };
        assert_eq!(
            serde_json::to_value(&query).unwrap(),
            serde_json::json!({"register_id": "r", "status": "closed", "limit": 1})
        );
        // An older server's session without `number` still parses.
        let old: CashierSession = serde_json::from_value(serde_json::json!({
            "id": "s", "user_id": "u", "register_id": "r", "branch_id": "b",
            "opening_cash": 0, "opened_at": 1, "status": "open"
        }))
        .unwrap();
        assert_eq!((old.number.as_str(), old.closed_at), ("", None));
    }

    #[test]
    fn close_request_omits_absent_counts() {
        let body = CloseRequest {
            counted_cash: 5,
            counts: None,
        };
        assert_eq!(
            serde_json::to_value(&body).unwrap(),
            serde_json::json!({"counted_cash": 5})
        );
    }
}
