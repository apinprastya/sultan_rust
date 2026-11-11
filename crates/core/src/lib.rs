//! Sultan POS pure domain logic.
//!
//! This crate has no IO, no database and no async: every function takes the state it needs
//! (including the current time) as arguments, so it can be unit-tested exhaustively.
//! See `docs/plan.md` §1, `docs/spec/2.1-organization-access.md` and `docs/spec/2.2-catalog.md`.

pub mod access;
pub mod cashier_session;
pub mod catalog;
pub mod customer;
pub mod doc_number;
pub mod ids;
pub mod money;
pub mod pricing;
pub mod tax;
pub mod time;
