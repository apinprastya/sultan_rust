//! API errors as `application/problem+json` (RFC 9457, `docs/plan.md` §1).
//!
//! Every error carries a stable machine-readable `code` (spec 2.1 §8) that the UI translates
//! (ID/EN); `detail` is a developer-facing English message. Codes used so far:
//!
//! | Status | Code | When |
//! |---|---|---|
//! | 400 | `invalid_body` | malformed JSON, missing field or a field that is not a UUID |
//! | 400 | `invalid_query` | a query parameter has the wrong type |
//! | 400 | `invalid_pairing_code` | pairing code unknown, used or expired (spec 2.1 §2) |
//! | 400 | `invalid_idempotency_key` | `Idempotency-Key` empty, too long or not visible ASCII |
//! | 401 | `invalid_device_token` | `X-Register-Token` missing, unknown or revoked (spec 2.1 §2) |
//! | 401 | `invalid_session` | bearer token missing, unknown, revoked or idle-expired (§4) |
//! | 401 | `invalid_credentials` | wrong PIN/password, unknown or inactive user (§4) |
//! | 403 | `permission_denied` / `needs_override` | permission check failed (§3); `permission`, and `limit` for an override |
//! | 403 | `permission_denied` + `reason: grant_exceeds_own` | assigning or editing a role that grants more than the caller holds (§3) |
//! | 403 | `self_approval` / `override_limit_exceeded` | manager override approval rules (§6); the latter with `permission` and the approver's `limit` |
//! | 403 | `invalid_override` | `X-Override` token unknown, used, expired or bound to something else (§6) |
//! | 403 | `register_session_required` | the endpoint needs a PIN session at a register (§4, §5) |
//! | 403 | `read_only_replica` | master-data write on a branch node (§1) |
//! | 404 | `not_found` | unknown route or resource |
//! | 405 | `method_not_allowed` | known route, wrong method |
//! | 409 | `conflict` | duplicate code, username or role name |
//! | 409 | `last_owner` | the change would leave no active Owner for all branches (§3) |
//! | 409 | `session_already_open` | register or user already has an open cashier session; `conflict`, `session_id` (§5) |
//! | 409 | `session_closed` | the cashier session is already closed (§5) |
//! | 409 | `no_open_session` | no open cashier session of this user on this register (§5) |
//! | 409 | `idempotency_in_progress` | the first request with this `Idempotency-Key` is still running |
//! | 422 | `invalid_field` | a field fails validation; `field` names it |
//! | 422 | `invalid_reference` | a referenced record (branch, role, location) does not exist |
//! | 422 | `invalid_pin` / `weak_password` / `invalid_grant` | credential or grant rules (§3, §4) |
//! | 422 | `idempotency_key_reused` | the `Idempotency-Key` was used for a different request (§9 case 17) |
//! | 422 | `amount_overflow` | a cash total does not fit in 64 bits |
//! | 423 | `locked_out` | too many failures; `locked_until` (unix ms) is included (§4) |
//! | 500 | `internal` | database or other unexpected error (details are not exposed) |

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};
use sultan_core::access::{AccessError, Limit};
use sultan_core::cashier_session::{OpenConflict, SessionError};
use sultan_db::DbError;

/// The media type of every error body.
pub const PROBLEM_JSON: &str = "application/problem+json";

/// An error response: HTTP status, stable code, detail and optional extension members.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    detail: String,
    extensions: Map<String, Value>,
}

impl ApiError {
    /// A new error.
    #[must_use]
    pub fn new(status: StatusCode, code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            status,
            code,
            detail: detail.into(),
            extensions: Map::new(),
        }
    }

    /// Adds an extension member to the problem body (e.g. `locked_until`).
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extensions.insert(key.to_owned(), value.into());
        self
    }

    /// The HTTP status.
    #[must_use]
    pub const fn status(&self) -> StatusCode {
        self.status
    }

    /// The stable error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.code
    }

    /// The human-readable detail.
    #[must_use]
    pub fn detail(&self) -> &str {
        &self.detail
    }

    /// `400 invalid_body`.
    #[must_use]
    pub fn invalid_body(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_body", detail)
    }

    /// `422 invalid_field`: the body parsed, but `field` breaks a rule.
    #[must_use]
    pub fn invalid_field(field: &str, detail: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_field", detail).with("field", field)
    }

    /// `400 invalid_pairing_code`: unknown, already used or expired (spec 2.1 §2).
    #[must_use]
    pub fn invalid_pairing_code() -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_pairing_code",
            "the pairing code is unknown, already used or expired",
        )
    }

    /// `401 invalid_device_token` (spec 2.1 §2).
    #[must_use]
    pub fn invalid_device_token() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_device_token",
            "missing, unknown or revoked X-Register-Token",
        )
    }

    /// `401 invalid_session` (spec 2.1 §4).
    #[must_use]
    pub fn invalid_session() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_session",
            "missing, unknown, revoked or expired session token",
        )
    }

    /// `401 invalid_credentials`. Deliberately the same for an unknown user and a wrong secret.
    #[must_use]
    pub fn invalid_credentials() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_credentials",
            "wrong user, PIN or password",
        )
    }

    /// `403 read_only_replica`: master data is HQ-owned (spec 2.1 §1).
    #[must_use]
    pub fn read_only_replica() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "read_only_replica",
            "master data is edited at HQ only; this branch node holds a read-only replica",
        )
    }

    /// `403 register_session_required`: the endpoint needs a PIN session bound to this register.
    #[must_use]
    pub fn register_session_required() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "register_session_required",
            "this endpoint needs a PIN session at a register (X-Register-Token + bearer)",
        )
    }

    /// `403 invalid_override`: the `X-Override` token cannot be used for this action (spec 2.1 §6).
    #[must_use]
    pub fn invalid_override() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "invalid_override",
            "the override token is unknown, used, expired or was approved for another action, \
             value, register, document or requester",
        )
    }

    /// `404 not_found`.
    #[must_use]
    pub fn not_found(what: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{what} not found"),
        )
    }

    /// `405 method_not_allowed`.
    #[must_use]
    pub fn method_not_allowed() -> Self {
        Self::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "method not allowed on this resource",
        )
    }

    /// `500 internal`. The cause is not exposed to the client.
    #[must_use]
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal server error",
        )
    }

    fn body(&self) -> Value {
        let mut body = json!({
            "type": "about:blank",
            "title": self.status.canonical_reason().unwrap_or("Error"),
            "status": self.status.as_u16(),
            "detail": self.detail,
            "code": self.code,
        });
        if let Value::Object(members) = &mut body {
            members.extend(self.extensions.clone());
        }
        body
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, self.body().to_string()).into_response();
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(PROBLEM_JSON));
        if self.code == "invalid_session" {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}

impl From<AccessError> for ApiError {
    fn from(error: AccessError) -> Self {
        let status = match &error {
            AccessError::LockedOut { .. } => StatusCode::LOCKED,
            AccessError::LastOwner => StatusCode::CONFLICT,
            AccessError::InvalidGrant { .. } => StatusCode::UNPROCESSABLE_ENTITY,
            AccessError::PermissionDenied { .. }
            | AccessError::NeedsOverride { .. }
            | AccessError::SelfApproval
            | AccessError::OverrideLimitExceeded { .. }
            | AccessError::GrantExceedsOwn { .. } => StatusCode::FORBIDDEN,
        };
        let api = Self::new(status, error.code(), error.to_string());
        match error {
            AccessError::LockedOut { until } => api.with("locked_until", until.unix_millis()),
            AccessError::PermissionDenied { permission } => {
                api.with("permission", permission.as_str())
            }
            AccessError::OverrideLimitExceeded {
                permission, limit, ..
            } => api
                .with("permission", permission.as_str())
                .with("limit", limit_value(limit)),
            AccessError::NeedsOverride { permission, limit }
            | AccessError::GrantExceedsOwn { permission, limit } => {
                let api = api.with("permission", permission.as_str());
                let api = match limit {
                    Some(limit) => api.with("limit", limit_value(limit)),
                    None => api,
                };
                if matches!(error, AccessError::GrantExceedsOwn { .. }) {
                    api.with("reason", "grant_exceeds_own")
                } else {
                    api
                }
            }
            _ => api,
        }
    }
}

/// A limit as its stored number: basis points or minor units (as in `GET /roles`).
const fn limit_value(limit: Limit) -> i64 {
    match limit {
        Limit::MaxPct(bp) => bp as i64,
        Limit::MaxAmount(amount) => amount.minor(),
    }
}

impl From<SessionError> for ApiError {
    fn from(error: SessionError) -> Self {
        let status = match &error {
            SessionError::NoOpenSession
            | SessionError::SessionAlreadyOpen { .. }
            | SessionError::AlreadyClosed { .. } => StatusCode::CONFLICT,
            SessionError::NotSessionOwner => StatusCode::FORBIDDEN,
            SessionError::AmountOverflow => StatusCode::UNPROCESSABLE_ENTITY,
        };
        let api = Self::new(status, error.code(), error.to_string());
        match error {
            SessionError::SessionAlreadyOpen { conflict } => {
                let (rule, id) = match conflict {
                    OpenConflict::Register(id) => ("register", id),
                    OpenConflict::User(id) => ("user", id),
                };
                api.with("conflict", rule)
                    .with("session_id", id.to_string())
            }
            SessionError::AlreadyClosed { session } => api.with("session_id", session.to_string()),
            _ => api,
        }
    }
}

impl From<DbError> for ApiError {
    fn from(error: DbError) -> Self {
        match error {
            DbError::NotFound(what) => Self::not_found(what),
            DbError::Conflict(detail) => Self::new(StatusCode::CONFLICT, "conflict", detail),
            DbError::MissingReference(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_reference",
                "a referenced record does not exist",
            ),
            _ => Self::internal(),
        }
    }
}

impl From<JsonRejection> for ApiError {
    fn from(rejection: JsonRejection) -> Self {
        Self::new(rejection.status(), "invalid_body", rejection.body_text())
    }
}

impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            rejection.body_text(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use sultan_core::access::Permission;
    use sultan_core::time::Timestamp;

    async fn render(error: ApiError) -> (StatusCode, Option<String>, Value) {
        let response = error.into_response();
        let status = response.status();
        let www = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .map(|v| v.to_str().unwrap().to_owned());
        assert_eq!(response.headers()[header::CONTENT_TYPE], PROBLEM_JSON);
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, www, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn problem_body_shape() {
        let (status, www, body) = render(ApiError::invalid_session()).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(www.as_deref(), Some("Bearer"));
        assert_eq!(
            body,
            json!({
                "type": "about:blank",
                "title": "Unauthorized",
                "status": 401,
                "detail": "missing, unknown, revoked or expired session token",
                "code": "invalid_session",
            })
        );

        let (_, www, body) = render(ApiError::not_found("user").with("id", "x")).await;
        assert_eq!(www, None);
        assert_eq!(body["detail"], "user not found");
        assert_eq!(body["id"], "x");

        // A status without a canonical reason still renders.
        let odd = StatusCode::from_u16(599).unwrap();
        let (_, _, body) = render(ApiError::new(odd, "odd", "x")).await;
        assert_eq!(body["title"], "Error");
    }

    #[test]
    fn constructors() {
        let cases = [
            (ApiError::invalid_body("x"), 400, "invalid_body"),
            (ApiError::invalid_field("f", "x"), 422, "invalid_field"),
            (
                ApiError::invalid_pairing_code(),
                400,
                "invalid_pairing_code",
            ),
            (
                ApiError::invalid_device_token(),
                401,
                "invalid_device_token",
            ),
            (ApiError::invalid_session(), 401, "invalid_session"),
            (ApiError::invalid_credentials(), 401, "invalid_credentials"),
            (ApiError::read_only_replica(), 403, "read_only_replica"),
            (
                ApiError::register_session_required(),
                403,
                "register_session_required",
            ),
            (ApiError::invalid_override(), 403, "invalid_override"),
            (ApiError::not_found("x"), 404, "not_found"),
            (ApiError::method_not_allowed(), 405, "method_not_allowed"),
            (ApiError::internal(), 500, "internal"),
        ];
        for (error, status, code) in cases {
            assert_eq!(error.status().as_u16(), status, "{code}");
            assert_eq!(error.code(), code);
        }
    }

    #[test]
    fn access_errors_map_to_status_and_code() {
        use sultan_core::access::{Limit, Requested};
        let p = Permission::UserUnlock;
        let cases = [
            (AccessError::PermissionDenied { permission: p }, 403),
            (
                AccessError::NeedsOverride {
                    permission: p,
                    limit: None,
                },
                403,
            ),
            (AccessError::SelfApproval, 403),
            (
                AccessError::OverrideLimitExceeded {
                    permission: Permission::SaleDiscount,
                    limit: Limit::MaxPct(1),
                    requested: Requested::Pct(2),
                },
                403,
            ),
            (
                AccessError::LockedOut {
                    until: Timestamp::from_unix_millis(9),
                },
                423,
            ),
            (AccessError::LastOwner, 409),
            (
                AccessError::InvalidGrant {
                    permission: p,
                    reason: "x",
                },
                422,
            ),
        ];
        for (error, status) in cases {
            let code = error.code();
            let api = ApiError::from(error);
            assert_eq!(api.status().as_u16(), status, "{code}");
            assert_eq!(api.code(), code);
        }
        let locked = ApiError::from(AccessError::LockedOut {
            until: Timestamp::from_unix_millis(9),
        });
        assert_eq!(locked.extensions["locked_until"], 9);
        let denied = ApiError::from(AccessError::PermissionDenied { permission: p });
        assert_eq!(denied.extensions["permission"], "user.unlock");

        let needs = ApiError::from(AccessError::NeedsOverride {
            permission: Permission::SaleDiscount,
            limit: Some(Limit::MaxPct(1_000)),
        });
        assert_eq!(needs.extensions["limit"], 1_000);
        let exceeded = ApiError::from(AccessError::OverrideLimitExceeded {
            permission: Permission::SaleDiscount,
            limit: Limit::MaxPct(2_000),
            requested: Requested::Pct(2_500),
        });
        assert_eq!(exceeded.extensions["limit"], 2_000);
        assert_eq!(exceeded.extensions["permission"], "sale.discount");
        assert!(!needs.extensions.contains_key("reason"));
        let grant = ApiError::from(AccessError::GrantExceedsOwn {
            permission: Permission::CashOut,
            limit: Some(Limit::MaxAmount(sultan_core::money::Money::from_minor(5))),
        });
        assert_eq!(
            (grant.status().as_u16(), grant.code()),
            (403, "permission_denied")
        );
        assert_eq!(grant.extensions["reason"], "grant_exceeds_own");
        assert_eq!(grant.extensions["limit"], 5);
        let unlimited = ApiError::from(AccessError::GrantExceedsOwn {
            permission: Permission::UserManage,
            limit: None,
        });
        assert!(!unlimited.extensions.contains_key("limit"));
    }

    #[test]
    fn session_errors_map_to_status_and_code() {
        use sultan_core::ids::CashierSessionId;
        let id = CashierSessionId::generate();
        let cases = [
            (SessionError::NoOpenSession, 409, "no_open_session"),
            (
                SessionError::SessionAlreadyOpen {
                    conflict: OpenConflict::Register(id),
                },
                409,
                "session_already_open",
            ),
            (
                SessionError::AlreadyClosed { session: id },
                409,
                "session_closed",
            ),
            (SessionError::NotSessionOwner, 403, "permission_denied"),
            (SessionError::AmountOverflow, 422, "amount_overflow"),
        ];
        for (error, status, code) in cases {
            let api = ApiError::from(error);
            assert_eq!((api.status().as_u16(), api.code()), (status, code));
        }
        let user = ApiError::from(SessionError::SessionAlreadyOpen {
            conflict: OpenConflict::User(id),
        });
        assert_eq!(user.extensions["conflict"], "user");
        assert_eq!(user.extensions["session_id"], id.to_string());
        let closed = ApiError::from(SessionError::AlreadyClosed { session: id });
        assert_eq!(closed.extensions["session_id"], id.to_string());
    }

    #[test]
    fn db_errors_hide_internals() {
        let cases = [
            (DbError::NotFound("role"), 404, "not_found"),
            (DbError::Conflict("dup".into()), 409, "conflict"),
            (DbError::Corrupt("secret detail".into()), 500, "internal"),
            (
                DbError::MissingReference("secret fk".into()),
                422,
                "invalid_reference",
            ),
        ];
        for (error, status, code) in cases {
            let api = ApiError::from(error);
            assert_eq!((api.status().as_u16(), api.code()), (status, code));
            assert!(!api.detail.contains("secret"));
        }
    }
}
