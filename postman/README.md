# Postman collection: Sultan POS API

`sultan-api.postman_collection.json` tests the main flows of spec 2.1 (`docs/spec/2.1-organization-access.md`)
and of the catalog, spec 2.2 (`docs/spec/2.2-catalog.md`), against a running node. Requests and bodies follow the OpenAPI document served at
`GET /api/v1/openapi.json` (import that URL into Postman to browse every endpoint).

Flow (68 requests, each with test scripts). Spec 2.1 (requests 01–35):
password login → OpenAPI document → create branch (`PUT`, client UUIDv7, time zone
`Asia/Makassar`) → replay the same `PUT` (idempotent) → a time zone that is not IANA (`WIB`) is
rejected (`invalid_field`) → create register → issue pairing code → pair (the answer names the branch: `branch_code`,
`branch_name`) → reused code rejected → create cashier user with PIN → lock-screen users → the
device's register and branch (`GET /registers/me`) → PIN login → list users → cashier denied `GET /users` and
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

**Catalog (folder "Catalog (spec 2.2)", requests 36–68, phase C3):** create a category and a brand →
tax classes (device token) → `PUT` a product with Size × Color options, units pcs + box(24) and
EAN-13 barcodes (default SKUs `<code>-S-RED`…, `factor_thousandths`) → replay (`200`, same body) →
read it with the device token → duplicate barcode on another product (`409 duplicate_barcode` with
the conflicting `product_id` / `variant_id`) → an invalid product lists every error (`422`,
`errors`) → lookup by barcode with the device token (branch from the token; with the register's display
fields `product_name`, `variant_label`, `unit_name`, `matched_code`, `plu`, phase C5) → lookup the box
barcode → search → block the product at the register's branch → read the blocks → lookup
`not_sold_here` → search hides it → unblock → archive (`Idempotency-Key`) → lookup `archived` →
the owner lists archived products → a device token may not (`403`, `catalog.view_archived`) →
unarchive → upload an 800×400 PNG (stored 512×256 JPEG ≤ 256 KB) → download it with its `ETag` →
`304` with `If-None-Match` → a non-image is `422 invalid_image` → catalog settings → overlapping
scale prefix rejected → generate a product code with an `Idempotency-Key` → the retry replays the
same code → generate an internal EAN-13 → a device token cannot write (`401`) → delete the image →
who am I (`GET /auth/me`: the user and permission keys the back office reads, phase C4).
The upload reads `postman/fixtures/product-800x400.png`, so pass `--working-dir postman`.

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
postman collection run postman/sultan-api.postman_collection.json --working-dir postman \
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
postman collection run postman/sultan-api.postman_collection.json --working-dir postman \
    --env-var baseUrl=http://127.0.0.1:8080/api/v1
```

The run needs no Postman login; it only prints a note that results are not published to the cloud.

Collection variables: `baseUrl`, `ownerUsername`, `ownerPassword`, `ownerPin` (the approver PIN of
the override step), `cashierPin`, `cashierRoleId` (the seeded Cashier role,
`00000000-0000-7000-8000-000000000003`).

The events WebSocket (`GET /api/v1/events`) is not part of the collection; it is covered by the
server's tests (`crates/server/tests/api/events.rs`, `catalog_lookup.rs` for `catalog.changed`).

On MySQL, run the same collection against `sultan serve` on a `mysql://` database (see
`crates/db/README.md`, "manual smoke test"); C3 ran it on both backends (66/66 requests passed).
