//! Route handlers under `/api/v1` (spec 2.1 §8).
//!
//! - Phase 4: pairing, the lock-screen user list, login, logout and unlock.
//! - Phase 5: master data (`/branches`, `/registers`, `/locations`, `/users`, `/roles`), pairing
//!   codes and unpairing, the permission catalog and the audit log.
//! - Phase 6: cashier sessions, manager overrides (and the demo endpoint for the `X-Override`
//!   guard).
//! - M3 (spec 2.5): customers, groups, membership tiers, the customer balance and its settings
//!   ([`customers`], [`customer_groups`], [`customer_balance`]).
//! - C3 (spec 2.2): the catalog: products, branch blocks, images, categories, brands, tax
//!   classes, lookup and search, catalog settings and code generators ([`catalog`]).
//! - R3 (spec 2.3): price lists, entries, tax rates, pricing settings ([`pricing`]) and the quote
//!   and lookup price ([`pricing_quote`]).
//!
//! Master-data writes follow one pattern (`docs/plan.md` §1, spec 2.1 §1, §7):
//! - `PUT /{resource}/{uuid}` creates (`201`) or overwrites (`200`) the record with a
//!   client-generated `UUIDv7`. A replay of the same body changes nothing: no write, no audit
//!   entry, no outbox event, and the same body comes back (`200`).
//! - `DELETE /{resource}/{uuid}` archives (soft delete) and answers the archived record.
//! - Writes need [`RequireHq`](crate::extract::RequireHq) and the resource's permission. The
//!   change, its audit entry and its outbox event are one transaction.

pub mod audit;
pub mod auth;
pub mod branches;
pub mod cashier_sessions;
pub mod catalog;
pub mod customer_balance;
pub mod customer_groups;
pub mod customers;
pub mod locations;
pub mod overrides;
pub mod permissions;
pub mod pricing;
pub mod pricing_quote;
pub mod product_images;
pub mod products;
pub mod registers;
pub mod roles;
pub mod taxonomy;
pub mod users;

use std::str::FromStr;

use axum::http::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};
use sultan_core::access::LoginAttempts;
use sultan_core::doc_number::BranchTimeZone;
use sultan_core::time::Timestamp;
use sultan_db::Saved;
use sultan_db::audit::AuditEntry;
use uuid::Uuid;

use crate::error::ApiError;
use crate::extract::UserSession;

/// Lockout state as audit JSON (spec 2.1 §7 before/after).
fn attempts_json(attempts: &LoginAttempts) -> Value {
    json!({
        "failures": attempts.failures,
        "locked_until": attempts.locked_until.map(Timestamp::unix_millis),
    })
}

/// Parses an ID field of a request; `400 invalid_body` naming the field otherwise.
fn parse_id<T: FromStr>(value: &str, field: &str) -> Result<T, ApiError> {
    value
        .parse()
        .map_err(|_| ApiError::invalid_body(format!("`{field}` is not a UUID")))
}

/// Parses an optional ID field.
fn parse_opt_id<T: FromStr>(value: Option<&str>, field: &str) -> Result<Option<T>, ApiError> {
    value.map(|v| parse_id(v, field)).transpose()
}

/// Parses the `{id}` of a `PUT`: a client-generated `UUIDv7` (`docs/plan.md` §1).
fn parse_put_id<T: From<Uuid>>(value: &str) -> Result<T, ApiError> {
    let uuid: Uuid = parse_id(value, "id")?;
    if uuid.get_version_num() == 7 {
        Ok(T::from(uuid))
    } else {
        Err(ApiError::invalid_body("`id` must be a UUIDv7"))
    }
}

/// `201` for a new record, `200` for an overwrite.
const fn saved_status(saved: Saved) -> StatusCode {
    match saved {
        Saved::Created => StatusCode::CREATED,
        Saved::Updated => StatusCode::OK,
    }
}

/// Serializes a response type for an audit before/after field.
fn to_json<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

/// An audit entry for a change made by `session` (spec 2.1 §7): time, the session's branch and
/// register, the acting user, action, target and before/after.
fn audit_entry(
    session: &UserSession,
    now: Timestamp,
    action: &str,
    target: String,
    before: Option<Value>,
    after: Option<Value>,
) -> AuditEntry {
    let mut entry = AuditEntry::new(now, action);
    entry.branch = Some(session.session.branch);
    entry.register = session.session.register;
    entry.user = Some(session.user.id);
    entry.target = Some(target);
    entry.before = before;
    entry.after = after;
    entry
}

/// A required text field: trimmed, not empty, at most `max` characters. Public so `sultan init`
/// applies the same rules as `PUT` (plan Phase 7).
///
/// # Errors
/// `422 invalid_field` naming the field.
pub fn text(field: &str, value: &str, max: usize) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > max {
        return Err(ApiError::invalid_field(
            field,
            format!("`{field}` must be 1 to {max} characters"),
        ));
    }
    Ok(value.to_owned())
}

/// A code printed in document numbers (`JKT01`, `R2`): 1–10 ASCII letters or digits
/// (spec 2.1 §1). Public so `sultan init` applies the same rule as `PUT /branches/{id}`.
///
/// # Errors
/// `422 invalid_field` naming the field.
pub fn code(field: &str, value: &str) -> Result<String, ApiError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 10 || !value.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ApiError::invalid_field(
            field,
            format!("`{field}` must be 1 to 10 letters or digits"),
        ));
    }
    Ok(value.to_owned())
}

/// An IANA time zone name (`Asia/Jakarta`), returned in canonical form (Phase 9: document dates
/// use the branch's local day). Public so `sultan init` applies the same rule as
/// `PUT /branches/{id}`.
///
/// # Errors
/// `422 invalid_field` naming the field.
pub fn timezone(field: &str, value: &str) -> Result<String, ApiError> {
    value
        .parse::<BranchTimeZone>()
        .map(|tz| tz.name().to_owned())
        .map_err(|e| ApiError::invalid_field(field, format!("`{field}`: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sultan_core::ids::UserId;

    #[test]
    fn parse_id_names_the_field() {
        let id = UserId::generate();
        assert_eq!(super::parse_id::<UserId>(&id.to_string(), "x").unwrap(), id);
        let err = super::parse_id::<UserId>("nope", "user_id").unwrap_err();
        assert_eq!(err.code(), "invalid_body");
        assert_eq!(err.status().as_u16(), 400);
        assert_eq!(parse_opt_id::<UserId>(None, "x").unwrap(), None);
    }

    #[test]
    fn put_ids_must_be_uuid_v7() {
        let v7 = Uuid::now_v7();
        let cases = [
            (v7.to_string(), true),
            ("550e8400-e29b-41d4-a716-446655440000".to_owned(), false),
            (Uuid::nil().to_string(), false),
            ("nope".to_owned(), false),
        ];
        for (value, ok) in cases {
            assert_eq!(parse_put_id::<UserId>(&value).is_ok(), ok, "{value}");
        }
    }

    #[test]
    fn field_rules() {
        let cases = [
            (text("name", "  Toko  ", 10).ok(), Some("Toko")),
            (text("name", "   ", 10).ok(), None),
            (text("name", "abcdefghijk", 10).ok(), None),
            (code("code", " JKT01 ").ok(), Some("JKT01")),
            (code("code", "JKT-01").ok(), None),
            (code("code", "").ok(), None),
            (code("code", "ABCDEFGHIJK").ok(), None),
        ];
        for (got, expected) in cases {
            assert_eq!(got.as_deref(), expected);
        }
        let err = code("code", "?").unwrap_err();
        assert_eq!((err.status().as_u16(), err.code()), (422, "invalid_field"));
        assert_eq!(
            timezone("timezone", " Asia/Makassar ").unwrap(),
            "Asia/Makassar"
        );
        let err = timezone("timezone", "WIB").unwrap_err();
        assert_eq!((err.status().as_u16(), err.code()), (422, "invalid_field"));
        assert_eq!(
            err.detail(),
            "`timezone`: `WIB` is not an IANA time zone, e.g. Asia/Jakarta"
        );
    }

    #[test]
    fn saved_maps_to_status() {
        assert_eq!(saved_status(Saved::Created), StatusCode::CREATED);
        assert_eq!(saved_status(Saved::Updated), StatusCode::OK);
    }
}
