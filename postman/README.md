# Postman collection: Sultan POS API

`sultan-api.postman_collection.json` tests the main flows of spec 2.1 (`docs/spec/2.1-organization-access.md`),
of the catalog, spec 2.2 (`docs/spec/2.2-catalog.md`), of pricing, spec 2.3
(`docs/spec/2.3-pricing.md`), of customers, spec 2.5 (`docs/spec/2.5-customers.md`), of sales, spec 2.6
(`docs/spec/2.6-sales.md`), and of cash management, spec 2.7 (`docs/spec/2.7-cash-management.md`),
against a running node. Requests and bodies follow the OpenAPI document served at
`GET /api/v1/openapi.json` (import that URL into Postman to browse every endpoint).

Flow (194 requests, each with test scripts). Spec 2.1 (requests 01–35):
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

**Catalog (folder "Catalog (spec 2.2)", requests 36–76, phases C3 and C3b):** create a category and a brand →
tax classes (device token) → `PUT` a product with Size × Color options, units pcs + box(24) and
EAN-13 barcodes (default SKUs `<code>-S-RED`…, `factor_thousandths`) → replay (`200`, same body) →
read it with the device token → duplicate barcode on another product (`409 duplicate_barcode` with
the conflicting `product_id` / `variant_id`) → an invalid product lists every error (`422`,
`errors`) → lookup by barcode with the device token (branch from the token; with the register's display
fields `product_name`, `variant_label`, `unit_name`, `matched_code`, `plu`, phase C5, and the
product's sellable `units`, spec 2.6 S4) → lookup the box
barcode → search → block the product at the register's branch → read the blocks → lookup
`not_sold_here` → search hides it → unblock → archive (`Idempotency-Key`) → lookup `archived` →
the owner lists archived products → a device token may not (`403`, `catalog.view_archived`) →
unarchive → upload an 800×400 PNG (stored 512×256 JPEG ≤ 256 KB) → download it with its `ETag` →
`304` with `If-None-Match` → a non-image is `422 invalid_image` → catalog settings → overlapping
scale prefix rejected → generate a product code with an `Idempotency-Key` → the retry replays the
same code → generate an internal EAN-13 → a device token cannot write (`401`) → delete the image →
who am I (`GET /auth/me`: the user and permission keys the back office reads, phase C4; with
`permissions_anywhere`, C3b) → **C3b:** search in pages of 2 (`next_after_variant_id`, then
`after_variant_id`) → a category with products cannot be deleted (`409 in_use`, `reason:
has_products`) → delete the never-used product (`204`) → it is gone (`404`) → delete the category
and the brand (`204`) → deleting the brand again is `404`.

**Customers (folder "Customers (spec 2.5)", requests 77–106, phase M3):** the cashier logs in
again at the register → create a customer group and a membership tier (`PUT`) → the cashier signs
up a customer at the till (phone normalized to `+62…`, a random code `K7QH-29XM`) → replay the
sign-up (`200`, same body) → the same phone for another customer is `409 phone_taken` with the
first customer's `customer_code` and `masked_phone` → the cashier may not set a tier
(`permission_denied`) → lookup by phone and by the lower-case member code → a device token alone is
`401` → the cashier may not search by name (`customer.search`) → the owner searches → the owner
sets the group and tier (`tier_applies`) → a second customer → merge it into the first
(`Idempotency-Key`) → the retry replays → merging again is `409 merged` → the merged customer's
code finds the survivor → the balance (0, no entries) → the owner logs in by PIN at the register →
a payout without an open cashier session is `409 no_open_session` → open a session → a payout above
the balance is `422 insufficient_balance` (`Idempotency-Key`) → the retry replays → close the
session (`cash_out` 0) → balance settings → a negative limit is `422` → erase the customer
(`Idempotency-Key`) → a second erase is `404` → the erased customer is not found at the till.
A successful payout needs a balance, which grows from saved change at a sale (spec 2.6); until
then it is covered by the server's tests (`crates/server/tests/api/customer_balance.rs`).

**Pricing (folder "Pricing (spec 2.3)", requests 107–122, phase R3):** create a product with pcs
and box(24) → a default-list entry with a quantity tier (10 000, from 12 pcs 9 000; the seeded
default list has the fixed ID `00000000-0000-7000-8000-000000000201`) → an overlapping entry is
`409 price_overlap` (with `entry_id`) → a rule on the default list is `422 invalid_rule` → create
the branch's list (the branch of request 03) → a second active list for it is
`409 price_list_conflict` → branch price 11 000 → the product's prices in every list → lookup
with the device token answers `unit_price` 11 000 → quote 12 pcs without a customer (branch price,
11% tax included: 13 081) → a quote with an unknown customer is `422 invalid_reference` → tax rates
of `Standard` → pricing settings → archive the branch list (`Idempotency-Key`) → the quote falls
back to the default tier (9 000) → delete the entry. A quote **with** a customer (contract, tier,
group) is covered by the server tests (`crates/server/tests/api/pricing_quote.rs`); the
pricing folder runs before customer price lists exist in the collection.

**Sales (folder "Sales (spec 2.6)", requests 123–147, 130a and 138a, phases S3–S5):** create a product and price it
10 000 a pcs in the default list → the seeded tender types with the device token (Tunai, Kartu
debit, QRIS, Saldo pelanggan) → the owner opens a cashier session at the register → a sale posted
with stale totals is `409 cart_stale` and carries the fresh quote (`quote.totals`) → post the sale
with those totals (`PUT /sales/{id}`, 2 pcs, cash 50 000, change 30 000) → the same `PUT` again
answers the stored sale (`200`) → the receipt text → the receipt in Indonesian (`?lang=id`:
`Petugas`, `Kembali`) → a reprint (`copy=true`, 42 columns: COPY,
audited) → find the sale by its number → the session's sales → hold a cart → the branch's held
carts → take it (`Idempotency-Key`) → taking it again is `409 cart_taken` → a removed cart line
(`POST /cart-voids`) → open the drawer (`POST /drawer/open`, `drawer.open`, audited; S5) → return 1 pcs by receipt (refund 10 000 in cash, number `…-RT-…`) → the sale
with a return cannot be voided (`409 has_returns`) → a second sale → post-void it
(`Idempotency-Key`, 20 000 back in cash) → close the session: the report's `sales` block (cash
received 100 000, change 60 000, refunds 30 000, one return, one post-void, one line void) and
expected cash 10 000 → the stored report reads the same → the owner lists the branch's returns →
create a tender type (`tender.manage`, HQ) → archive it. The owner holds every permission, so no
override is asked for; overrides, exchanges, the customer balance and the other refusals are
covered by the server's tests (`crates/server/tests/api/sales.rs`, `sales_returns.rs`).

**Cash (folder "Cash (spec 2.7)", requests 148–176 and 159a–h, phases K3 and K6):** the cash reasons with the device
token → the register's proposed float (`GET /registers/me/float`) → the owner opens a session
(200 000) → a cash in (`PUT /cash-movements/{id}`, number `…-K-…`) → the same `PUT` replays
(`200`) → another body with the same ID is `409 conflict` → a cash out with a reason and a note →
a safe drop → a drop with a reason is `422 invalid_field` → reverse the cash in → reversing it
again is `409 already_reversed` → the session's movements → **nota photo (K6):** the
800×400 PNG fixture as the cash out's photo (`PUT /cash-movements/{id}/photos/{photo_id}`, raw
body; a JPEG ≤ 400 KB with its SHA-256) → the same photo again (`200`) → a body that is not a
picture is `422 invalid_image` → read it (`image/jpeg`, the `ETag`) → `304` with
`If-None-Match` → the cash out lists the photo (`photo_ids`, `missing_nota` false) → the sales
settings carry `nota_flag_above` (100 000) → a negative one is `422 invalid_field` → the
cash-out slip in Indonesian →
its reprint (COPY, free in its session) → the X report (expected cash 70 000, no count, audited)
→ its text → close keeping 50 000 as float (both blocks: expected 70 000, the 2.1 `cash_out`
130 000 with the drop, `cash.drops`, `cash.reversals`, two X reads, 20 000 to the safe) → the
close report text → an X of the closed session is `409 session_closed` → the days still to close
list today → run the Z of the day for the branch (`Idempotency-Key`; it covers every session of
the collection that day) → the retry replays it → another run is `409 nothing_to_close` → a
future day is `422 invalid_field` → the branch's Z reports → the Z text (COPY, audited) → today
is no longer pending → create a cash reason (HQ, `tender.manage`) → archive it. The owner holds
every permission, so no override is asked for; the overrides, a cashier's limits, an open
session blocking the Z and the supplementary Z are covered by the server's tests
(`crates/server/tests/api/cash.rs`, `cash_z.rs`).

The uploads (the product image and the nota photo) read `postman/fixtures/product-800x400.png`,
so pass `--working-dir postman`.

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

S5 (2026-11-18) ran the whole collection against `sultan serve` on a fresh SQLite database
(149/149 requests passed).
S6 (2026-11-18) added the return receipt (139a–b), the names and `status` filter of `GET /sales`
and the sales settings (145a–f), and ran it again the same way (157/157 requests passed).
K3 (2026-11-21) added the folder "Cash (spec 2.7)" and ran the whole collection twice in a row
against one `sultan serve` on a fresh SQLite database (186/186 requests passed both times).
K5 (2026-11-25) added the cashier and register names of `GET /cashier-sessions` (165a) and ran it
again the same way (187/187 requests passed).
K6 (2026-11-25) added the nota photo requests 159a–h and ran the whole collection twice in a row
the same way (194/194 requests passed both times; the photo is stored once, `0600`, under
`attachments/` next to `sultan.db`).

The run needs no Postman login; it only prints a note that results are not published to the cloud.

Collection variables: `baseUrl`, `ownerUsername`, `ownerPassword`, `ownerPin` (the approver PIN of
the override step), `cashierPin`, `cashierRoleId` (the seeded Cashier role,
`00000000-0000-7000-8000-000000000003`).

The events WebSocket (`GET /api/v1/events`) is not part of the collection; it is covered by the
server's tests (`crates/server/tests/api/events.rs`, `catalog_lookup.rs` for `catalog.changed`).

On MySQL, run the same collection against `sultan serve` on a `mysql://` database (see
`crates/db/README.md`, "manual smoke test"); C3 ran it on both backends (66/66 requests passed).
