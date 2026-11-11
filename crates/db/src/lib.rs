//! Sultan POS persistence layer: sqlx migrations and repositories for `SQLite` and `MySQL`
//! (`docs/plan.md` Phase 3, `docs/spec/2.1-organization-access.md`).
//!
//! # Shape
//! - [`Db`] is a pool for one of the two backends; [`Tx`] is a transaction on it. Both convert
//!   into a [`Conn`], which every repository function accepts, so the same function works on a
//!   pool or inside a transaction: `org::get_branch(&db, id)` or `org::get_branch(&mut tx, id)`.
//! - Master-data writes (branches, registers, locations, users, roles, grants, assignments) take
//!   `&mut Tx` and append an [`outbox`] row **in the same transaction** (`docs/plan.md` §1).
//! - SQL is shared between backends (`?` placeholders, portable statements). Only the migrations
//!   (`migrations/{sqlite,mysql}`) and the few upserts in [`auth`] differ.
//! - Customers, groups, tiers and the customer balance ledger (spec 2.5) live in [`customers`].
//! - Rows map to and from the domain types of `sultan_core`. No access rule lives here: the
//!   server checks permissions, lockout, override approval and the Owner invariant with `core`.
//!
//! # Storage conventions (`docs/plan.md` §1)
//! IDs are 16-byte `UUIDv7` values (`BLOB` / `BINARY(16)`), money is `BIGINT` minor units, times
//! are `BIGINT` unix milliseconds ([`Timestamp`](sultan_core::time::Timestamp)), booleans are
//! integers and JSON is `TEXT` / `JSON`. Token columns store SHA-256 hashes only ([`TokenHash`]);
//! hashing is the caller's job.
//!
//! Every function takes the current time as an argument, like `core`, so nothing here reads the
//! clock.

mod conn;
mod error;
mod sql;

pub mod audit;
pub mod auth;
pub mod cashier_sessions;
pub mod catalog;
pub mod customers;
pub mod devices;
pub mod idempotency;
pub mod org;
pub mod outbox;
pub mod overrides;
pub mod roles;
pub mod seed;
pub mod users;

pub use conn::{Backend, Conn, Db, Tx, secure_sqlite_files};
pub use error::{DbError, Result};

/// A SHA-256 hash of a secret token (device token, session token, pairing code, override token).
/// Only hashes are stored (spec 2.1 §2, §4).
pub type TokenHash = [u8; 32];

/// Whether an upsert created a new row or updated an existing one. Lets an idempotent
/// `PUT /resource/{uuid}` answer `201` or `200` (spec 2.1 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Saved {
    /// No row with this ID existed.
    Created,
    /// The row existed and was overwritten.
    Updated,
}
