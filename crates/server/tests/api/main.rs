//! HTTP integration tests (spec 2.1 §9, server cases 11–18; plans Phase 4–6). Every test runs on
//! in-memory `SQLite`, and again on `MySQL` when `SULTAN_TEST_MYSQL_URL` is set.

mod common;

mod audit;
mod auth;
mod cashier_sessions;
mod delegation;
mod events;
mod guards;
mod idempotency;
mod openapi;
mod org;
mod overrides;
mod registers;
mod roles;
mod session_history;
mod users;
