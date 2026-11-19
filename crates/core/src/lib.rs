//! Sultan POS pure domain logic.
//!
//! This crate has no IO, no database and no async: every function takes the state it needs
//! (including the current time) as arguments, so it can be unit-tested exhaustively.
//! See `docs/plan.md` §1 and the specs in `docs/spec/` (2.1 access, 2.2 catalog, 2.3 pricing,
//! 2.5 customers, 2.6 sales, 2.7 cash management).

pub mod access;
pub mod cash;
pub mod cashier_session;
pub mod catalog;
pub mod customer;
pub mod doc_number;
pub mod ids;
pub mod money;
pub mod pricing;
pub mod sale;
pub mod tax;
pub mod time;
