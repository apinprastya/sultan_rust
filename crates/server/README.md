# sultan-server

REST/JSON API of a Sultan POS node (`axum`, `/api/v1`, problem+json errors), see `docs/plan.md` §1
and `docs/spec/2.1-organization-access.md`.

- `src/routes/`: one module per resource (spec 2.1 §8).
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
