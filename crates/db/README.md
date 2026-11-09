# sultan-db

Persistence for Sultan POS: sqlx migrations and repositories for SQLite and MySQL
(`docs/plan.md` Phase 3, `docs/spec/2.1-organization-access.md`).

- `migrations/sqlite`, `migrations/mysql`: the schema, one folder per backend (only the DDL
  differs), embedded with `sqlx::migrate!` and applied by `Db::migrate()` (`build.rs` rebuilds the
  crate when a migration changes).
- `src/`: one module per area (`org`, `users`, `roles`, `seed`, `devices`, `auth`, `overrides`,
  `cashier_sessions`, `catalog`, `audit`, `outbox`, `idempotency`). The queries are shared between backends
  and use the runtime query API, so building never needs a `DATABASE_URL`.

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
- The migration creates triggers that make `audit_log` append-only. With binary logging on
  (the default), a user without `SUPER` needs `log_bin_trust_function_creators = 1` to create
  them.
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
