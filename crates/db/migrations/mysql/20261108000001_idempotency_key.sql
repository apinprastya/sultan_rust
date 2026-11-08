-- Idempotency-Key on action POSTs (docs/plan.md §1, spec 2.1 §9 case 17).
-- One row per (caller, key): the first request's method, path and body hash, and its response.
-- `status` is NULL while that first request is still running. Rows are purged after the
-- retention window (24 h, see crates/server/src/idempotency.rs).
CREATE TABLE idempotency_key (
    caller_hash  BINARY(32)   NOT NULL,
    idem_key     VARCHAR(255) NOT NULL,
    method       VARCHAR(16)  NOT NULL,
    path         VARCHAR(512) NOT NULL,
    request_hash BINARY(32)   NOT NULL,
    status       BIGINT,
    content_type VARCHAR(255),
    body         MEDIUMTEXT,
    created_at   BIGINT       NOT NULL,
    PRIMARY KEY (caller_hash, idem_key),
    KEY ix_idempotency_key_created_at (created_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;
