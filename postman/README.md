# Postman collection: Sultan POS API

`sultan-api.postman_collection.json` tests the main flows of spec 2.1 (`docs/spec/2.1-organization-access.md`)
against a running node. Requests and bodies follow the OpenAPI document served at
`GET /api/v1/openapi.json` (import that URL into Postman to browse every endpoint).

Flow (34 requests, each with test scripts):
password login → OpenAPI document → create branch (`PUT`, client UUIDv7, time zone
`Asia/Makassar`) → replay the same `PUT` (idempotent) → a time zone that is not IANA (`WIB`) is
rejected (`invalid_field`) → create register → issue pairing code → pair → reused code rejected → create cashier
user with PIN → lock-screen users → PIN login → list users → cashier denied `GET /users` and
`PUT /branches` (`permission_denied`) → last Owner cannot be archived (`last_owner`) → permission
catalog → audit queries → **cashier session and override (spec 2.1 §5–6, plan Phase 6):** open a
cashier session with an `Idempotency-Key` → retry with the same key replays the `201` → second open
rejected (`session_already_open`) → current session (blind: no expected cash) → 15% discount needs
an override (`needs_override`, demo endpoint `POST /demo/override-protected`) → the owner approves
with their PIN (`POST /overrides`) → the action succeeds with `X-Override` → the reused token is
rejected (`invalid_override`) → blind close with a per-denomination count returns the close report
→ second close rejected (`session_closed`) → **past sessions (Phase 9):** the cashier reads the
session with its stored close report (`GET /cashier-sessions/{id}`, identical to the close
response) → the owner lists the register's closed sessions one per page
(`GET /cashier-sessions?register_id=…&status=closed&limit=1`, `next_before_id`) → the next page is
empty → an unknown `status` is rejected → logout. The open and close steps also check the session
number `<BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>`.

IDs and codes are generated fresh in the first request's test script, so the collection can be run
repeatedly against the same server.

## Run

The collection assumes an Owner `owner` with password `owner-pass-1` and PIN `1234` (Owner for all
branches). Everything else it creates itself.

### Against the real binary (`sultan serve`)

```sh
cargo build -p sultan-app
DIR=$(mktemp -d)
SULTAN_OWNER_PASSWORD=owner-pass-1 SULTAN_OWNER_PIN=1234 target/debug/sultan --config "$DIR/sultan.toml" \
    init --company "PT Sultan Demo" --branch JKT01 --branch-name "Toko Jakarta" --owner owner
target/debug/sultan --config "$DIR/sultan.toml" serve --listen 127.0.0.1:8080 &
postman collection run postman/sultan-api.postman_collection.json \
    --env-var baseUrl=http://127.0.0.1:8080/api/v1
kill %1 && rm -rf "$DIR"     # SIGTERM: graceful shutdown
```

The `init` flags match the seed of the development server below; the Owner's password and PIN are
passed through the environment only because this is a script (see `README.md`).

### Against the development server

The real router on a fresh in-memory SQLite database with company "PT Sultan Demo", branch `JKT01`
and the same Owner; everything is lost when it stops.

```sh
cargo run -p sultan-server --example dev_server -- 8080 &
postman collection run postman/sultan-api.postman_collection.json \
    --env-var baseUrl=http://127.0.0.1:8080/api/v1
```

The run needs no Postman login; it only prints a note that results are not published to the cloud.

Collection variables: `baseUrl`, `ownerUsername`, `ownerPassword`, `ownerPin` (the approver PIN of
the override step), `cashierPin`, `cashierRoleId` (the seeded Cashier role,
`00000000-0000-7000-8000-000000000003`).

The events WebSocket (`GET /api/v1/events`) is not part of the collection; it is covered by the
server's tests (`crates/server/tests/api/events.rs`).
