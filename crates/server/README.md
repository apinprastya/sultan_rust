# sultan-server

REST/JSON API of a Sultan POS node (`axum`, `/api/v1`, problem+json errors), see `docs/plan.md` §1
and `docs/spec/2.1-organization-access.md`.

- `src/routes/`: one module per resource (spec 2.1 §8); the catalog (spec 2.2) and customers
  (spec 2.5: `customers.rs`, `customer_groups.rs`, `customer_balance.rs`).
- `src/movements.rs`: what a cashier session's close report counts (the 2.1 block): sales,
  returns and post-voids (spec 2.6), cash in / out / safe drops (spec 2.7) and customer balance
  payouts as cash out (`session_movements`).
- Cash management (spec 2.7, phase K3): `routes/cash.rs` (the session figures of the close
  report, the X and the Z, their DTOs, the texts, the retry loop), `routes/cash_reasons.rs`,
  `routes/cash_movements.rs` (cash in / out / drop, slips, the payout slip),
  `routes/cash_reports.rs` (X report, close report text, `GET /registers/me/float`) and
  `routes/z_reports.rs` (run, list, read, text, days still to close).
- Nota photos (spec 2.7 §2a, phase K6): `routes/cash_photos.rs` (`PUT|GET
  /cash-movements/{id}/photos/{photo_id}`), processed by `src/product_image.rs` with the `NOTA`
  limits (1600×1600, ≤ 400 KB JPEG) and stored as files by `src/attachments.rs` under the node's
  data directory (`AppState::with_data_dir`; `<dir>/attachments/<2 hex>/<sha256>.jpg`, `0600`).
  A state without a data directory answers photo calls with `500 internal`. Back the
  `attachments/` directory up with the database.
- `src/routes/`: one module per resource (spec 2.1 §8); pricing (spec 2.3) is
  `routes/pricing.rs` (price lists, entries, tax rates, settings, `price.changed`) and
  `routes/pricing_quote.rs` (`POST /pricing/quote` and the lookup's `unit_price`).
- `src/access.rs`: permission checks, branch visibility and the Owner invariant over `core::access`.
- `src/openapi.rs`: the OpenAPI document (`utoipa`), served at `GET /api/v1/openapi.json`.

## Tests

`cargo test -p sultan-server` drives the router in-process (`tower::ServiceExt::oneshot`) on
in-memory SQLite; with `SULTAN_TEST_MYSQL_URL` set, every HTTP test runs again on a fresh MySQL
database (see `crates/db/README.md` for the container).

## Development server and Postman

```sh
cargo run -p sultan-server --example dev_server -- 8080
postman collection run postman/sultan-api.postman_collection.json \
    --env-var baseUrl=http://127.0.0.1:8080/api/v1
```

The dev server seeds company "PT Sultan Demo", branch `JKT01` and user `owner` (password
`owner-pass-1`, PIN `1234`) in a fresh in-memory database; see `postman/README.md`.
