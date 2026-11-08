//! Repository tests. Every test runs on in-memory `SQLite`, and again on `MySQL` when
//! `SULTAN_TEST_MYSQL_URL` is set (see `crates/db/README.md`).

mod common;

mod access;
mod cashier;
mod catalog;
mod catalog_admin;
mod catalog_fixtures;
mod catalog_lookup;
mod consistency;
mod devices_auth;
mod idempotency;
mod org;
mod outbox_audit;
