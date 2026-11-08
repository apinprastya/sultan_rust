-- Idempotency-Key on action POSTs (docs/plan.md §1, spec 2.1 §9 case 17).
-- One row per (caller, key): the first request's method, path and body hash, and its response.
-- `status` is NULL while that first request is still running. Rows are purged after the
-- retention window (24 h, see crates/server/src/idempotency.rs).
CREATE TABLE idempotency_key (
    caller_hash  BLOB    NOT NULL,
    idem_key     TEXT    NOT NULL,
    method       TEXT    NOT NULL,
    path         TEXT    NOT NULL,
    request_hash BLOB    NOT NULL,
    status       INTEGER,
    content_type TEXT,
    body         TEXT,
    created_at   INTEGER NOT NULL,
    PRIMARY KEY (caller_hash, idem_key)
);
CREATE INDEX ix_idempotency_key_created_at ON idempotency_key (created_at);
