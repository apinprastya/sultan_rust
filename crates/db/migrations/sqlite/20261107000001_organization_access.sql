-- Spec 2.1 Organization & access (SQLite). Keep in step with ../mysql/: only the DDL differs.
-- IDs: 16-byte UUIDv7 BLOBs. Money: INTEGER minor units. Timestamps: INTEGER unix millis.
-- Booleans: INTEGER 0/1. JSON: TEXT. Token columns hold SHA-256 hashes only (32 bytes).

CREATE TABLE company (
    id         BLOB    NOT NULL PRIMARY KEY,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE branch (
    id             BLOB    NOT NULL PRIMARY KEY,
    company_id     BLOB    NOT NULL REFERENCES company (id),
    code           TEXT    NOT NULL UNIQUE,
    name           TEXT    NOT NULL,
    address        TEXT    NOT NULL,
    npwp           TEXT,
    receipt_header TEXT    NOT NULL,
    receipt_footer TEXT    NOT NULL,
    archived       INTEGER NOT NULL,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);

CREATE TABLE stock_location (
    id                BLOB    NOT NULL PRIMARY KEY,
    branch_id         BLOB    NOT NULL REFERENCES branch (id),
    name              TEXT    NOT NULL,
    receiving_default INTEGER NOT NULL,
    archived          INTEGER NOT NULL,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    UNIQUE (branch_id, name)
);

CREATE TABLE register (
    id                  BLOB    NOT NULL PRIMARY KEY,
    branch_id           BLOB    NOT NULL REFERENCES branch (id),
    code                TEXT    NOT NULL,
    name                TEXT    NOT NULL,
    default_location_id BLOB REFERENCES stock_location (id),
    archived            INTEGER NOT NULL,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    UNIQUE (branch_id, code)
);

CREATE TABLE register_pairing_code (
    code_hash   BLOB    NOT NULL PRIMARY KEY,
    register_id BLOB    NOT NULL REFERENCES register (id),
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    consumed_at INTEGER
);

CREATE TABLE app_user (
    id            BLOB    NOT NULL PRIMARY KEY,
    username      TEXT    NOT NULL UNIQUE,
    display_name  TEXT    NOT NULL,
    pin_hash      TEXT,
    password_hash TEXT,
    active        INTEGER NOT NULL,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL
);

CREATE TABLE role (
    id         BLOB    NOT NULL PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    archived   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE role_permission (
    role_id     BLOB    NOT NULL REFERENCES role (id),
    perm        TEXT    NOT NULL,
    limit_value INTEGER,
    PRIMARY KEY (role_id, perm)
);

-- branch_id NULL = all branches.
CREATE TABLE user_role (
    user_id   BLOB NOT NULL REFERENCES app_user (id),
    role_id   BLOB NOT NULL REFERENCES role (id),
    branch_id BLOB REFERENCES branch (id)
);
CREATE INDEX ix_user_role_user ON user_role (user_id);
CREATE INDEX ix_user_role_role ON user_role (role_id);

CREATE TABLE device_token (
    token_hash  BLOB    NOT NULL PRIMARY KEY,
    register_id BLOB    NOT NULL REFERENCES register (id),
    created_at  INTEGER NOT NULL,
    revoked_at  INTEGER
);
CREATE INDEX ix_device_token_register ON device_token (register_id);

CREATE TABLE auth_session (
    token_hash  BLOB    NOT NULL PRIMARY KEY,
    user_id     BLOB    NOT NULL REFERENCES app_user (id),
    register_id BLOB REFERENCES register (id),
    branch_id   BLOB    NOT NULL REFERENCES branch (id),
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    revoked_at  INTEGER
);
CREATE INDEX ix_auth_session_user ON auth_session (user_id);

-- core::access::LoginAttempts per (user, branch).
CREATE TABLE login_attempt (
    user_id      BLOB    NOT NULL REFERENCES app_user (id),
    branch_id    BLOB    NOT NULL REFERENCES branch (id),
    failures     INTEGER NOT NULL,
    locked_until INTEGER,
    PRIMARY KEY (user_id, branch_id)
);

CREATE TABLE override_token (
    token_hash      BLOB    NOT NULL PRIMARY KEY,
    permission      TEXT    NOT NULL,
    requested_value INTEGER,
    register_id     BLOB    NOT NULL REFERENCES register (id),
    context_ref     TEXT    NOT NULL,
    requester_id    BLOB    NOT NULL REFERENCES app_user (id),
    approver_id     BLOB    NOT NULL REFERENCES app_user (id),
    created_at      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL,
    consumed_at     INTEGER
);

CREATE TABLE cashier_session (
    id            BLOB    NOT NULL PRIMARY KEY,
    user_id       BLOB    NOT NULL REFERENCES app_user (id),
    register_id   BLOB    NOT NULL REFERENCES register (id),
    opening_cash  INTEGER NOT NULL,
    opened_at     INTEGER NOT NULL,
    closed_at     INTEGER,
    close_kind    TEXT CHECK (close_kind IN ('normal', 'forced')),
    forced_by     BLOB REFERENCES app_user (id),
    counted_cash  INTEGER,
    expected_cash INTEGER,
    over_short    INTEGER,
    transactions  INTEGER,
    CHECK ((closed_at IS NULL) = (close_kind IS NULL))
);
-- Safety net for spec 2.1 §5 next to core::cashier_session::ensure_can_open:
-- at most one open session per register and per user (partial unique indexes).
CREATE UNIQUE INDEX ux_cashier_session_open_register ON cashier_session (register_id)
    WHERE closed_at IS NULL;
CREATE UNIQUE INDEX ux_cashier_session_open_user ON cashier_session (user_id)
    WHERE closed_at IS NULL;

CREATE TABLE cashier_session_count (
    session_id   BLOB    NOT NULL REFERENCES cashier_session (id),
    denomination INTEGER NOT NULL,
    quantity     INTEGER NOT NULL,
    PRIMARY KEY (session_id, denomination)
);

-- Append-only (spec 2.1 §7): no foreign keys, so history never blocks master-data changes.
CREATE TABLE audit_log (
    id          BLOB    NOT NULL PRIMARY KEY,
    occurred_at INTEGER NOT NULL,
    branch_id   BLOB,
    register_id BLOB,
    user_id     BLOB,
    action      TEXT    NOT NULL,
    target      TEXT,
    before_json TEXT,
    after_json  TEXT
);
CREATE INDEX ix_audit_log_occurred_at ON audit_log (occurred_at);

CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

-- docs/plan.md §1: every master-data write appends here in the same transaction.
CREATE TABLE outbox (
    event_id   BLOB    NOT NULL PRIMARY KEY,
    kind       TEXT    NOT NULL,
    payload    TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    synced_at  INTEGER
);
CREATE INDEX ix_outbox_synced_at ON outbox (synced_at);
