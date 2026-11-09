# AGENTS.md

Guide for coding agents working on Sultan POS. Read [README.md](README.md) for usage and
configuration. The design is in [docs/plan.md](docs/plan.md), and the agreed rules for each
section are in [docs/spec/](docs/spec/). If code and spec disagree, the spec wins. Ask before
changing a spec rule.

## Project

A retail point of sale in Rust. One binary (`sultan`) runs every node role (`standalone`, `hq`,
`branch`, `register`). It serves a REST/JSON API under `/api/v1`, sends WebSocket event
notifications, and stores data in SQLite or MySQL. The register UI is built with Slint.
Toolchain: Rust 1.95, edition 2024 (`rust-toolchain.toml`).

| Crate | Path | Role |
|---|---|---|
| `sultan-core` | `crates/core` | Pure domain logic (access, catalog, money, doc numbers, time). **No IO.** |
| `sultan-db` | `crates/db` | sqlx migrations and repositories for SQLite and MySQL. See [crates/db/README.md](crates/db/README.md). |
| `sultan-server` | `crates/server` | axum REST API, auth, idempotency, outbox, WebSocket events, OpenAPI (`utoipa`). |
| `sultan-client` | `crates/client` | The register's `Service` trait and the back office's `BackOffice` trait, their HTTP implementation, and a fake for UI tests. |
| `sultan-ui` | `crates/ui` | Slint register (with the barcode lookup, `src/scan.rs`), setup wizard, Start screen (`src/start.rs`) and back office (`ui/*.slint`, controllers in `src/`). |
| `sultan-app` | `crates/app` | The binary: CLI, config, setup wizard, Start screen flow (`src/start.rs`), `serve`, `init`, `register`. |

Dependencies point one way: `core` ← `db` ← `server`; `core` ← `client` ← `ui` ← `app`.
Never add IO, sqlx or axum to `core`.

## Commands

```sh
make check        # the gate: fmt-check, clippy -D warnings, tests, coverage >= 80%
make fmt          # cargo fmt --all
make lint         # cargo clippy --workspace --all-targets -- -D warnings
make test         # cargo test --workspace (SQLite in memory)
make cov          # cargo llvm-cov --workspace --fail-under-lines 80
make test-mysql   # the whole suite again on a throwaway mysql:8.4 container (needs Docker)
cargo test -p sultan-db                     # a single crate
cargo run -p sultan-ui --example screenshots -- docs/screenshots 1366x768 en
```

`make check` must pass before you call a change done. If the change touches SQL, also run
`make test-mysql` when Docker is available.

## Rules

- **Coverage:** new code needs at least 80% line coverage. Aim for about 95% in `core`, using
  table-driven unit tests.
- **Lints:** `unsafe_code` is forbidden, and `clippy::pedantic` warnings fail the build. Do not
  add `#[allow(clippy::…)]` without a comment that says why.
- **Formatting:** `rustfmt` with `max_width = 100`.
- **Money** is `i64` in minor units (Rupiah: 1). **Quantities** are fixed-point decimals with 3
  decimal places. Never use floats for either.
- **IDs** are UUIDv7 and the client creates them. Master-data writes are
  `PUT /{resource}/{uuid}`, which creates (`201`) or overwrites (`200`) the record. Replaying the
  same body changes nothing. `DELETE` archives the record (soft delete). Nothing that is
  referenced is ever hard-deleted.
- **One transaction per write.** Every write of master data stores the change, its audit entry
  and its outbox event in the same transaction (see `crates/server/src/routes/mod.rs`).
- **Documents** go `draft → posted → voided`. A posted document is never edited, only reversed.
  Ledgers (stock, payments, points) are append-only, and `audit_log` is append-only by trigger.
- **Action `POST`s** take an `Idempotency-Key` header.
- **Errors** are `application/problem+json` with a stable `code`. When you add a code, add it to
  the table at the top of `crates/server/src/error.rs` and translate it in the UI.
- **Text** compares byte-wise, so it is case-sensitive on both backends. Product codes and SKUs
  are stored upper case.
- **Document numbers and dates** use the branch's IANA time zone (default `Asia/Jakarta`), e.g.
  `JKT01-R2-S-261107-01`.

## Database and migrations

- Each schema change is a pair of files with the same name, one in
  `crates/db/migrations/sqlite/` and one in `crates/db/migrations/mysql/`, named
  `YYYYMMDDNNNNNN_<name>.sql`. Only the DDL differs between backends. Never edit a migration that
  has already been applied: add a new one.
- Queries use sqlx's runtime API (no `query!` macros), so the build never needs a
  `DATABASE_URL`. The same SQL must work on SQLite and on MySQL 8.0.19 or later.
- Repository tests live in `crates/db/tests/db/`, and API tests live in
  `crates/server/tests/api/`. They run on in-memory SQLite, and also on MySQL when
  `SULTAN_TEST_MYSQL_URL` is set.

## API

- Document each route with `utoipa` so it appears in `GET /api/v1/openapi.json`
  (`crates/server/src/openapi.rs`).
- Registers send `X-Register-Token`, and users send a bearer session token. Permission checks and
  manager overrides (`X-Override`) follow `docs/spec/2.1-organization-access.md`.
- When you change an API flow, update the Postman collection in [postman/](postman/) and run it
  with the Postman CLI against a live `sultan serve` (see [postman/README.md](postman/README.md)).

## UI

- **UI only, except HQ** (`docs/plan.md` §3): on a single app or a terminal everything must be
  reachable from a window. A bare `sultan` opens the Start screen (F1 register, F2 back office,
  F3 settings); the register's lock screen and the back office's login screen go back to it
  with F9. A CLI subcommand may exist for scripts, but never as the only way. New windows are
  opened through a hook in `sultan_app::Context` (`open_ui`, `open_setup`, `open_backoffice`,
  `open_start`) so the flow is tested without a display.
- The register is keyboard-first, and every key also has an on-screen button. Keep the key
  tables in README.md in sync with the UI.
- Every user-facing string uses `@tr()` (in Slint) and has an Indonesian translation in
  `crates/ui/translations/id/LC_MESSAGES/sultan-ui.po`. The UI supports Indonesian and English.
- UI flows are tested headlessly (`crates/ui/tests/`) against `sultan-client`'s fake service.
  Tests find elements by their accessible label, so give new controls one.
- When a screen changes, regenerate `docs/screenshots/`.

## Working style

- New domain areas are specified first: rules and edge cases go in a spec under `docs/spec/`
  (written as test cases), then you implement them. Respect the scope in `docs/plan.md` §3–4.
  For example, F&B is out of scope.
- Keep docs current: the README, crate READMEs and module doc comments describe the actual
  behaviour, including known limitations.
- Do not commit local node files (`sultan.toml`, `*.db`, `register.token`). They are gitignored
  and may hold secrets.
