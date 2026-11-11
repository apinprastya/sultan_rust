//! Client errors. A problem+json answer (`crates/server/src/error.rs`) becomes [`Problem`] with its
//! stable `code` and extension members, so the UI can react to `locked_out` (`locked_until`),
//! `needs_override` (`permission`, `limit`), `session_already_open` (`conflict`, `session_id`) and
//! the others without parsing text.

use serde::Deserialize;
use serde_json::{Map, Value};

/// A failed service call.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ClientError {
    /// The server answered with a problem+json error.
    #[error("{} ({})", .0.detail, .0.code)]
    Api(Problem),
    /// The server could not be reached, or the connection broke.
    #[error("cannot reach the server: {0}")]
    Network(String),
    /// The server answered something this client does not understand.
    #[error("unexpected answer from the server: {0}")]
    Unexpected(String),
}

impl ClientError {
    /// The problem code, for [`ClientError::Api`].
    #[must_use]
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api(p) => Some(&p.code),
            _ => None,
        }
    }

    /// The problem, for [`ClientError::Api`].
    #[must_use]
    pub const fn problem(&self) -> Option<&Problem> {
        match self {
            Self::Api(p) => Some(p),
            _ => None,
        }
    }
}

/// An RFC 9457 problem as the Sultan server writes it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Problem {
    /// HTTP status.
    pub status: u16,
    /// Stable machine-readable code, e.g. `locked_out`.
    pub code: String,
    /// Developer-facing English detail.
    #[serde(default)]
    pub detail: String,
    /// Every other member (`type`, `title` and the extensions).
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Problem {
    /// A problem without extensions (used by the fake service).
    #[must_use]
    pub fn new(status: u16, code: &str, detail: &str) -> Self {
        Self {
            status,
            code: code.to_owned(),
            detail: detail.to_owned(),
            extra: Map::new(),
        }
    }

    /// Adds an extension member.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.insert(key.to_owned(), value.into());
        self
    }

    fn int(&self, key: &str) -> Option<i64> {
        self.extra.get(key).and_then(Value::as_i64)
    }

    fn text(&self, key: &str) -> Option<&str> {
        self.extra.get(key).and_then(Value::as_str)
    }

    /// `locked_out`: end of the lockout (unix ms).
    #[must_use]
    pub fn locked_until(&self) -> Option<i64> {
        self.int("locked_until")
    }

    /// `needs_override` / `permission_denied`: the permission key.
    #[must_use]
    pub fn permission(&self) -> Option<&str> {
        self.text("permission")
    }

    /// `needs_override` / `override_limit_exceeded`: the limit (basis points or minor units).
    #[must_use]
    pub fn limit(&self) -> Option<i64> {
        self.int("limit")
    }

    /// `session_already_open`: `register` or `user`.
    #[must_use]
    pub fn conflict(&self) -> Option<&str> {
        self.text("conflict")
    }

    /// `session_already_open` / `session_closed`: the session.
    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.text("session_id")
    }

    /// `invalid_field`: the field.
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.text("field")
    }

    /// `in_use` (spec 2.2 §9): why the row cannot be deleted (`used`, `bundle_component`,
    /// `has_products`, `has_children`).
    #[must_use]
    pub fn reason(&self) -> Option<&str> {
        self.text("reason")
    }

    /// `phone_taken` (spec 2.5 §7): the code of the customer holding the phone, e.g.
    /// `K7QH-29XM`, so the cashier can pick that customer instead.
    #[must_use]
    pub fn customer_code(&self) -> Option<&str> {
        self.text("customer_code")
    }

    /// `phone_taken`: that customer's masked phone, e.g. `0812****789`.
    #[must_use]
    pub fn masked_phone(&self) -> Option<&str> {
        self.text("masked_phone")
    }

    /// `insufficient_balance` / `balance_not_zero` (spec 2.5 §11): the customer balance.
    #[must_use]
    pub fn balance(&self) -> Option<i64> {
        self.int("balance")
    }

    /// `balance_limit` (spec 2.5 §11): the most that can still be saved.
    #[must_use]
    pub fn max(&self) -> Option<i64> {
        self.int("max")
    }
}

/// One entry of a catalog rule failure's `errors` (spec 2.2 §12).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct FieldError {
    /// Stable code, e.g. `invalid_code`.
    pub code: String,
    /// The body field, e.g. `variants.sku`, when there is one.
    #[serde(default)]
    pub field: Option<String>,
    /// English message.
    #[serde(default)]
    pub message: String,
}

/// The conflict of a `409 duplicate_*` answer: who already uses the value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Duplicate {
    /// The product that has it.
    pub product_id: String,
    /// Its variant (`None` for a product code).
    pub variant_id: Option<String>,
    /// The value (code, SKU, barcode, or the PLU as text).
    pub value: String,
}

impl Problem {
    /// Catalog rule failures: every `errors` entry, or one built from `code` / `field` when the
    /// answer has no list (e.g. `invalid_field` from a body check, or a `409 duplicate_*`).
    #[must_use]
    pub fn errors(&self) -> Vec<FieldError> {
        let listed: Vec<FieldError> = self
            .extra
            .get("errors")
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        if listed.is_empty() {
            vec![FieldError {
                code: self.code.clone(),
                field: self.field().map(str::to_owned),
                message: self.detail.clone(),
            }]
        } else {
            listed
        }
    }

    /// `409 duplicate_code|_sku|_barcode|_plu`: the conflicting product, variant and value.
    #[must_use]
    pub fn duplicate(&self) -> Option<Duplicate> {
        if !self.code.starts_with("duplicate_") {
            return None;
        }
        let product_id = self.text("product_id")?.to_owned();
        let value = match self.extra.get("value") {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        Some(Duplicate {
            product_id,
            variant_id: self.text("variant_id").map(str::to_owned),
            value,
        })
    }
}

impl From<Problem> for ClientError {
    fn from(problem: Problem) -> Self {
        Self::Api(problem)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_extensions_are_typed() {
        let body = serde_json::json!({
            "type": "about:blank", "title": "Locked", "status": 423, "code": "locked_out",
            "detail": "locked", "locked_until": 1_800_000_300_000_i64,
            "permission": "sale.discount", "limit": 1000, "conflict": "register",
            "session_id": "s1", "field": "opening_cash",
        });
        let p: Problem = serde_json::from_value(body).unwrap();
        assert_eq!(p.locked_until(), Some(1_800_000_300_000));
        assert_eq!(p.permission(), Some("sale.discount"));
        assert_eq!(p.limit(), Some(1000));
        assert_eq!(p.conflict(), Some("register"));
        assert_eq!(p.session_id(), Some("s1"));
        assert_eq!(p.field(), Some("opening_cash"));
        assert_eq!(p.extra["title"], "Locked");

        let empty = Problem::new(401, "invalid_credentials", "wrong");
        let cases: [Option<i64>; 2] = [empty.locked_until(), empty.limit()];
        assert_eq!(cases, [None, None]);
        assert_eq!(empty.permission(), None);
    }

    #[test]
    fn catalog_errors_and_duplicates() {
        let body = serde_json::json!({
            "status": 422, "code": "invalid_code", "detail": "a; b", "field": "variants.sku",
            "errors": [
                {"code": "invalid_code", "field": "variants.sku", "message": "a"},
                {"code": "invalid_field", "message": "b"},
            ],
        });
        let p: Problem = serde_json::from_value(body).unwrap();
        let errors = p.errors();
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].field.as_deref(), Some("variants.sku"));
        assert_eq!(errors[1].field, None);
        assert_eq!(p.duplicate(), None);

        let single = Problem::new(422, "invalid_field", "bad").with("field", "name");
        let errors = single.errors();
        assert_eq!(
            (errors[0].code.as_str(), errors[0].field.as_deref()),
            ("invalid_field", Some("name"))
        );

        let dup = Problem::new(409, "duplicate_barcode", "taken")
            .with("product_id", "p1")
            .with("variant_id", "v1")
            .with("value", "899")
            .with("field", "barcodes.code");
        let d = dup.duplicate().unwrap();
        assert_eq!(
            (
                d.product_id.as_str(),
                d.variant_id.as_deref(),
                d.value.as_str()
            ),
            ("p1", Some("v1"), "899")
        );
        let plu = Problem::new(409, "duplicate_plu", "taken")
            .with("product_id", "p1")
            .with("variant_id", Value::Null)
            .with("value", 12);
        assert_eq!(plu.duplicate().unwrap().value, "12");
        assert_eq!(plu.duplicate().unwrap().variant_id, None);
        let odd = Problem::new(409, "duplicate_code", "x").with("product_id", "p");
        assert_eq!(odd.duplicate().unwrap().value, "");
        assert_eq!(Problem::new(409, "duplicate_code", "x").duplicate(), None);
    }

    #[test]
    fn errors_expose_code_and_message() {
        let api = ClientError::from(Problem::new(409, "session_closed", "closed"));
        #[rustfmt::skip]
        let cases = [
            (api.clone(), Some("session_closed"), "closed (session_closed)"),
            (ClientError::Network("refused".into()), None, "cannot reach the server: refused"),
            (ClientError::Unexpected("HTTP 502".into()), None,
                "unexpected answer from the server: HTTP 502"),
        ];
        for (err, code, text) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.problem().is_some(), code.is_some());
            assert_eq!(err.to_string(), text);
        }
    }
}
