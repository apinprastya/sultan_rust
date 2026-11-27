# sultan-db

Persistence for Sultan POS: sqlx migrations and repositories for SQLite and MySQL
(`docs/plan.md` Phase 3, `docs/spec/2.1-organization-access.md`).

- `migrations/sqlite`, `migrations/mysql`: the schema, one folder per backend (only the DDL
  differs), embedded with `sqlx::migrate!` and applied by `Db::migrate()` (`build.rs` rebuilds the
  crate when a migration changes).
- `src/`: one module per area (`org`, `users`, `roles`, `seed`, `devices`, `auth`, `overrides`,
  `cashier_sessions`, `catalog`, `customers`, `pricing`, `sales`, `cash`, `stock`, `audit`,
  `outbox`, `idempotency`). The queries are
  shared between backends and use the runtime query API, so building never needs a `DATABASE_URL`.

## Sales (spec 2.6, phase S2)

`src/sales/` and `20261117000001_sales.sql` / `20261117000002_balance_refund.sql`:

| Table | What |
|---|---|
| `tender_type` | HQ master data: name, fixed `kind` (`cash`, `card`, `qris`, `transfer`, `ewallet`, `customer_balance`, `other`), `needs_reference`, `opens_drawer`, `active`, `sort_order`, `archived_at`. Seeded with fixed ids and Indonesian names: Tunai (cash, …0301), Kartu debit (…0302), QRIS (…0303), Saldo pelanggan (customer balance, …0304). |
| `sale`, `sale_line`, `sale_tender` | A posted sale (status `posted` / `voided`), its lines (the `core::sale::LineTotals` values, price mode, scale label, bundle components as JSON, and `returned_qty` / `refunded` so far) and tenders (`exchange` without a tender type). `legacy_number` marks an imported old-Sultan sale (read-only). `request_hash` makes `PUT /sales/{id}` idempotent. |
| `sale_void`, `sale_void_tender` | The post-void: the paying (current) session, reason, refund split and how it was paid back (cash or balance). |
| `sale_return`, `sale_return_line`, `sale_return_tender` | Returns (by receipt with `sale_id` + `sale_line_no`, or without), restock / write-off per line, refund tenders (cash, balance, `exchange`), the exchange's new sale. |
| `held_cart` | Branch-local held carts (JSON payload, item count, total); `taken_*` once resumed. Not synced. |
| `stock_movement` | The append-only stock ledger (D9, triggers like `audit_log`): base-unit `base_qty` signed by kind (`sale` −, `sale_void` +, `return` +, `write_off` −), branch + location, document. Since 2.8 it has a value, a lot and the document line and is written through `stock::post_moves` (see Inventory below). |

- **Numbers:** `MAX(seq) + 1` per register and branch-local day inside the posting transaction,
  guarded by `UNIQUE (register_id, local_day, seq)` on `sale` and on `sale_return`, like cashier
  session numbers below: a lost race is a retryable `DbError::Conflict`.
- **Races between documents:** a return bumps `sale.return_count` and each sale line's
  `returned_qty` / `refunded` with guarded `UPDATE`s; a post-void needs `return_count = 0`. A guard
  that fails is a lost race (retryable); the rerun reports the real refusal.
- **No foreign keys** from documents to branches, registers, sessions, users, customers or
  variants (like `customer_balance_entry`): synced documents may arrive before those rows. Only
  the parts of one document reference each other.
- **Balance kind `refund`:** SQLite cannot change a `CHECK`, so `20261117000002_balance_refund.sql`
  rebuilds `customer_balance_entry` (copy, drop, rename, recreate the index and the append-only
  triggers); MySQL drops and re-adds the two named checks.
- **Sales settings (phase S6, `20261118000001_sales_settings.sql`, `src/sales/settings.rs`):**
  `sales_settings` is one company row (`id = 1`) with `return_days` (default 30, `0` = no return
  window, at most 3650), like `pricing_settings` and `customer_balance_settings`;
  `branch_sales_settings` holds the receipt width per branch (`CHECK` 32, 42 or 48; a branch
  without a row prints 32). `put_sales_settings` writes both with one audit row and one outbox
  row `sales_settings.put` (nothing when unchanged).
- **Names in lists (S6):** sale and return headers join the customer's `name` / `code`, the
  cashier's `display_name` and the register's `code` (a derived table, so the callers' `WHERE`
  clauses stay unqualified; no query per row); the purchase history joins the name and code of the
  customer each document was posted for. `DocFilter.status` filters by `posted` / `voided`.

## Cash management (spec 2.7, phase K2)

`src/cash/` and `20261119000001_cash.sql`:

| Table / column | What |
|---|---|
| `cash_reason` | HQ master data: name, fixed `direction` (`in`, `out`), `note_required`, `active`, `sort_order`, `archived_at` (never hard-deleted). Seeded with fixed ids …0401–…0406 (`core::cash::SEED_REASONS`, `cash::SEED_REASON_IDS`). |
| `cash_movement` | A posted cash in / out / safe drop (append-only by trigger): number `<BRANCH>-<REGISTER>-K-<YYMMDD>-<NN>` from `(local_day, seq)` under `UNIQUE (register_id, local_day, seq)`, kind, amount > 0, reason (none for a drop), note, branch / register / session / user / approver, `reverses` (`UNIQUE`: reversed at most once), `request_hash` (idempotent `PUT`). |
| `cash_movement_photo` | Nota photo records (append-only by trigger): sha256, size, who, when; `photo_no` 1–3 under `UNIQUE (movement_id, photo_no)`, so a movement never has a fourth. The file itself lives on the node's disk, `<data dir>/attachments/<2 hex>/<sha256>.jpg`, written by the server (`sultan_server::attachments`, K6) before the row; one file may serve several photos. |
| `z_report` | The Z of a branch's business day (append-only by trigger): number `<BRANCH>-Z-<YYMMDD>-<NN>` under `UNIQUE (branch_id, business_day, seq)`, `supplementary` (= `seq > 1`), headline totals and the server's JSON snapshot `figures`. |
| `cashier_session` + | `float_kept`, `missing_nota` (written at the close), `opening_proposed` (at the open), `z_report_id` (the Z that covers it). |
| `cashier_session_reason` | The cash reasons of a close report as at the close (name, direction, sort; `record_close_reasons`, once), read back by `close_reasons` so a renamed reason never changes a stored report or a later Z (migration `20261121000002_session_reasons`). |
| `sales_settings` + | `nota_flag_above` (Rp, default 100 000, `0` = off); `cash_since` (unix ms, migration `20261121000001_cash_since`, K3): when cash management was installed, the time that migration ran. |

- **Posting** (`post_cash_movement`) re-checks the open session, the reason (exists, active, not
  archived, right direction) and a reversal's target, allocates the number like sales (a lost
  race is a retryable `DbError::Conflict`), and writes the audit row and the outbox event
  `cash_movement.posted` in the caller's transaction.
- **Session inputs** of `core::cash::session_figures`: `session_cash` (movements, photo counts,
  `drawer.open` / `session.x_report` audit entries on `cashier_session:<id>`, indexed by
  `audit_log (target, action)`), and `session_cash_events` (the netted `CashIn` / `CashOut` /
  `Drop` events the server's close report adds to `sales::session_events`).
- **Z:** `day_sessions` / `z_candidates` / `open_day_sessions` by business day (the session's
  `local_day`), `next_z_number`, `insert_z_report` (links each session with a guarded `UPDATE`),
  `pending_days` (K18: days with closed sessions in no Z, or past days with an open session; with
  `since`, the branch-local day of `cash_since`, earlier days are left out: spec 2.7 §17 reading
  10). `session_sales` (K3) gives a Z each session's sales documents: count, Σ gross, first and
  last sale, tax per rate, and the tax of the returns and post-voids paid in it (a return
  without a receipt at the product's tax class on the return's day, decided 2026-11-21).
- **Float:** `last_float_kept` (K3) is the float of the register's last closed session or `None`;
  the server stores an `opening_proposed` only when there is one.
- **Payout slip** (K3): `customers::get_balance_entry` and `balance_after` (the balance right
  after an entry, in ID order).
- **Grants:** the migration gives Manager `cash.drop` and `day.close`, and Cashier `cash.out`
  ≤ Rp 50 000 only when Cashier has no `cash.out` grant yet; `seed_defaults` does the same on a
  fresh install and keeps rewriting Owner.

## Inventory (spec 2.8, phase N2)

`src/stock/` and `20261126000001_inventory.sql`:

| Table | What |
|---|---|
| `stock_movement` | Rebuilt (SQLite cannot change a `CHECK`): the 15 kinds of spec 2.8 §2 plus `receipt_void`, signs checked per kind, `value` (signed Rp, `NULL` = no cost, every row from before 2.8), `lot_id`, `doc_kind` (`sale`, `return`, `goods_receipt`, `supplier_return`, `transfer`, `internal_transfer`, `adjustment`, `opname`, `opening_stock`), `doc_line`. 2.6 rows without a location moved to the branch's receiving default (else any live location, else a location "Stok lama" created for it). Append-only triggers recreated. |
| `stock_balance` | On hand per (branch, location, variant), built from the ledger by the migration and kept equal to it by every posting. A movement without a location (a branch with no location at all) is balanced under the all-zero location ID (`stock::NO_LOCATION`). |
| `stock_cost` | The moving average per (branch, variant): `qty`, `value` and the reference cost `ref_value / ref_qty` (`NULL` = no cost yet). Rows from before 2.8 have no cost. |
| `stock_lot` | Expiry lots per (branch, location, variant, batch, expiry). "No lot" is on hand minus the lots and may go negative. |
| `supplier`, `supplier_item` | HQ master data. `supplier_item.preferred_variant` (= `variant_id` when preferred) under `UNIQUE`: one preferred supplier per variant; `UNIQUE (supplier_id, supplier_code)`; `last_price` per base unit and `last_unit_id` set by posted receipts. |
| `stock_reason` | HQ master data, direction `in` / `out` / `both`, seeded …0501–…0506 (Rusak, Kedaluwarsa, Hilang, Sampel, Pemakaian sendiri, Koreksi with a note). |
| `inventory_settings` | One company row: `pkp`, `standard_ppn_rate_bp`, `expiry_warn_days`, `velocity_days`, `cover_days`. |
| `product` + | `track_expiry`, `expiry_warn_days`. |
| `reorder_rule` | Min / max per (branch, variant). |
| `purchase_order`, `_line` | `draft → ordered → closed`, or `cancelled`; numbered when ordered; lines keep `received_qty` (base units). |
| `goods_receipt`, `_line`, `payable` | Receipts with the supplier invoice; `invoice_active` (= `invoice_no` while posted) under `UNIQUE (supplier_id, invoice_active)`; per line the landed cost and lot; one payable per posted receipt. |
| `supplier_return`, `_line` | Returns to the supplier with the credit. |
| `stock_transfer`, `_line`, `_lot` | Transfers between branches with shipped / received / lost quantities and values, and the lots that travel. |
| `internal_transfer`, `stock_adjustment`, `opening_stock` (+ `_line`) | Posted at once. |
| `opname`, `opname_scope`, `opname_line`, `opname_count`, `opname_lock` | Counts: snapshot per line, append-only count entries, one lock row per (location, variant) for the one-open-opname rule. |

- **Posting** (`stock::post_moves`): checks the kind's sign, splits an outgoing FEFO movement into
  one row per lot taken, values each part from `core::stock::CostState`, appends the rows and
  updates `stock_balance`, `stock_cost` and `stock_lot` in the caller's transaction. On MySQL the
  cost row and the lots are read `FOR UPDATE`; on SQLite `BEGIN IMMEDIATE` serializes writers.
  Balances use an upsert (`ON CONFLICT … DO UPDATE` / `ON DUPLICATE KEY UPDATE … AS new`).
- **Sales and returns** post through it (`sales/stock.rs`): a sale at the average (FEFO), a
  post-void mirrors each sale row (quantity, value, lot), a restocked return comes back at the
  sale line's value (into its lots), a write-off at the average.
- **Numbers:** `<BRANCH>-<PREFIX>-<YYMMDD>-<NN>` per branch, prefix and branch-local day, as
  `MAX(seq) + 1` under `UNIQUE (branch_id, local_day, seq)` per document table.
- **Audit and outbox** are the caller's (the N3 server appends them in the same transaction), so
  these repositories only store the documents and the ledger.
- **Grants:** the migration gives Manager `supplier.manage`, `purchase.order`,
  `purchase.receive`, `stock.transfer`, `stock.count`, gives Stock clerk the last three and takes
  its `stock.adjust` away (spec 2.8 §12; nothing used it before); `seed_defaults` does the same
  on a fresh install.
- **Tests:** `tests/db/stock.rs` and `tests/db/stock_docs.rs` (spec 2.8 §17 cases 13–21 at the db
  level) check after each scenario that every cache equals the ledger (`assert_balanced`), and the
  upgrade from a 2.7 database (`*_upgrade_builds_the_balance`).

## Tests

`cargo test -p sultan-db` runs every repository test on in-memory SQLite.

The same tests (and the server API tests in `crates/server/tests/api`) also run on MySQL when
`SULTAN_TEST_MYSQL_URL` is set. Each test creates its own database (`sultan_t_<uuid>` /
`sultan_s_<uuid>`) and drops it when it passes, so the user needs `CREATE` and `DROP` privileges.

### Run the whole suite on MySQL

All in one step (starts a throwaway `mysql:8.4` container named `sultan-mysql-test` on
`127.0.0.1:33306`, runs `cargo test --workspace`, and stops the container even when tests fail):

```sh
make test-mysql                 # or: make test-mysql MYSQL_PORT=33307 if 33306 is taken
```

Or by hand, from the workspace root:

```sh
docker run -d --rm --name sultan-mysql-test -e MYSQL_ROOT_PASSWORD=test \
  -e MYSQL_DATABASE=sultan_test -p 127.0.0.1:33306:3306 mysql:8.4
# wait until it accepts connections
until docker exec sultan-mysql-test mysqladmin ping -h127.0.0.1 -uroot -ptest --silent; do sleep 2; done
until docker logs sultan-mysql-test 2>&1 | grep -q "ready for connections.*port: 3306"; do sleep 2; done
SULTAN_TEST_MYSQL_URL=mysql://root:test@127.0.0.1:33306/sultan_test cargo test --workspace
docker stop sultan-mysql-test   # --rm removes it
```

The `mysqladmin ping` alone is not enough: on its first start the image runs a temporary server
while it initialises, so also wait for the final "ready for connections ... port: 3306" line.

A manual smoke test of the binary against the same container:

```sh
docker exec sultan-mysql-test mysql -uroot -ptest -e "CREATE DATABASE sultan_smoke"
DIR=$(mktemp -d)
SULTAN_OWNER_PASSWORD=owner-pass-1 SULTAN_OWNER_PIN=1234 target/debug/sultan --config "$DIR/sultan.toml" \
  init --db mysql://root:test@127.0.0.1:33306/sultan_smoke --company "PT Sultan Demo" \
  --branch JKT01 --branch-name "Toko Jakarta" --owner owner
target/debug/sultan --config "$DIR/sultan.toml" serve --listen 127.0.0.1:8080 &
postman collection run postman/sultan-api.postman_collection.json --working-dir postman \
  --env-var baseUrl=http://127.0.0.1:8080/api/v1
kill %1 && rm -rf "$DIR"
```

## MySQL notes

- Requires MySQL 8.0.19 or later (`INSERT ... AS new ON DUPLICATE KEY UPDATE`, generated
  columns, enforced `CHECK` constraints, `utf8mb4_0900_bin`, window functions in migrations).
  Tested on 8.4.
- **Text compares byte-wise on both backends.** The tables use `utf8mb4_0900_bin`
  (`20261108000002_binary_collation.sql`). Like SQLite's default `BINARY` collation it is
  case- and accent-sensitive and keeps trailing spaces. With MySQL's default
  `utf8mb4_0900_ai_ci`, several values collided: usernames `Ani`/`ani` and `josé`/`jose`, branch
  codes `JKT01`/`jkt01`, and `Idempotency-Key`s `KEY-A`/`key-a`. Audit filters also matched
  other cases, and `ORDER BY` sorted differently (`tests/db/consistency.rs`). MySQL flags `_bin`
  character columns `BINARY`, and sqlx 0.8 will not decode those as `String`, so `sql.rs` reads
  text columns of MySQL rows unchecked (the value is still validated as UTF-8).
- **Customer phone uniqueness** (spec 2.5 §2: unique among non-archived customers,
  `20261111000002_customers.sql`). SQLite uses a partial unique index
  (`ON customer (phone) WHERE archived_at IS NULL`). MySQL has no partial indexes, so
  `customer.phone_active` is a `STORED` generated column, `CASE WHEN archived_at IS NULL THEN phone
  END`, under a `UNIQUE` key (a unique key allows any number of `NULL`s). The repository never
  writes the column; it checks the phone first to report the holder (`phone_taken`), and the
  index catches races (a retryable `DbError::Conflict`).
- **One live price list per target** (spec 2.3 §2, `20261111000003_pricing.sql`): at most one
  active, non-archived list per (kind, target). SQLite uses partial unique indexes
  (`ON price_list (kind, target_id) WHERE active = 1 AND archived_at IS NULL`, and one on `kind`
  `WHERE kind = 'default'` for the single default list). MySQL uses `STORED` generated columns
  `live_target` and `default_key` under `UNIQUE` keys, like the phone above. The repository reports
  `price_list_conflict` first; the indexes catch races.
- The migrations create triggers that make `audit_log`, `customer_balance_entry`,
  `stock_movement`, `cash_movement`, `cash_movement_photo`, `z_report` and `opname_count`
  append-only.
  With binary logging on (the default), a user without `SUPER` needs
  `log_bin_trust_function_creators = 1` to create them.
- Session numbers (`cashier_sessions::insert`) are allocated as `MAX(seq) + 1` inside the open
  transaction, guarded by `UNIQUE (register_id, local_day, seq)`. Under `REPEATABLE READ` two
  racing opens can read the same maximum. The loser gets a duplicate key error (or a deadlock),
  and the server reruns the transaction with a short jittered backoff (`DbError::is_retryable`,
  `sultan_server::retry`), which then answers `409 session_already_open`. Concurrent opens are
  tested on both backends.
- On SQLite, `Db::begin` issues `BEGIN IMMEDIATE`: a transaction takes the write lock up front
  and waits for it up to the busy timeout (5 s). A deferred transaction that read first could
  otherwise fail at its first write with `SQLITE_BUSY_SNAPSHOT`, which ignores the busy timeout.
  Readers outside transactions are not blocked (WAL).

## File permissions

On Unix, `Db::connect` keeps a SQLite database and its `-wal` / `-shm` / `-journal` side files
readable and writable by the owner only (`0600`). A new file is created with that mode before
SQLite opens it (SQLite gives side files the database's mode). Existing files with group or
other bits are tightened, each with a `tracing` warning. MySQL URLs and in-memory databases are
left alone.
