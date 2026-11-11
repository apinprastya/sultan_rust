//! HTTP integration tests (spec 2.1 §9, server cases 11–18; plans Phase 4–6; spec 2.2 §14
//! server cases 15–22, phase C3; spec 2.3 §12 server cases 20–24, phase R3; spec 2.5 §15 server
//! cases 10–20, phase M3). Every test runs on
//! in-memory `SQLite`, and again on `MySQL` when `SULTAN_TEST_MYSQL_URL` is set.

mod common;

mod audit;
mod auth;
mod cashier_sessions;
mod catalog;
mod catalog_access;
mod catalog_delete;
mod catalog_fixtures;
mod catalog_images;
mod catalog_lookup;
mod customer_balance;
mod customers;
mod delegation;
mod events;
mod guards;
mod idempotency;
mod openapi;
mod org;
mod overrides;
mod pricing;
mod pricing_quote;
mod registers;
mod roles;
mod session_history;
mod users;
