-- Spec 2.1 Organization & access (MySQL 8.0.19+ / InnoDB / utf8mb4). Keep in step with ../sqlite/.
-- IDs: BINARY(16) UUIDv7. Money: BIGINT minor units. Timestamps: BIGINT unix millis.
-- Booleans: TINYINT(1). JSON: JSON. Token columns hold SHA-256 hashes only (BINARY(32)).

CREATE TABLE company (
    id         BINARY(16)   NOT NULL PRIMARY KEY,
    name       VARCHAR(200) NOT NULL,
    created_at BIGINT       NOT NULL,
    updated_at BIGINT       NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE branch (
    id             BINARY(16)   NOT NULL PRIMARY KEY,
    company_id     BINARY(16)   NOT NULL,
    code           VARCHAR(32)  NOT NULL,
    name           VARCHAR(200) NOT NULL,
    address        TEXT         NOT NULL,
    npwp           VARCHAR(32),
    receipt_header TEXT         NOT NULL,
    receipt_footer TEXT         NOT NULL,
    archived       TINYINT(1)   NOT NULL,
    created_at     BIGINT       NOT NULL,
    updated_at     BIGINT       NOT NULL,
    UNIQUE KEY ux_branch_code (code),
    CONSTRAINT fk_branch_company FOREIGN KEY (company_id) REFERENCES company (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE stock_location (
    id                BINARY(16)   NOT NULL PRIMARY KEY,
    branch_id         BINARY(16)   NOT NULL,
    name              VARCHAR(200) NOT NULL,
    receiving_default TINYINT(1)   NOT NULL,
    archived          TINYINT(1)   NOT NULL,
    created_at        BIGINT       NOT NULL,
    updated_at        BIGINT       NOT NULL,
    UNIQUE KEY ux_stock_location_name (branch_id, name),
    CONSTRAINT fk_stock_location_branch FOREIGN KEY (branch_id) REFERENCES branch (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE register (
    id                  BINARY(16)   NOT NULL PRIMARY KEY,
    branch_id           BINARY(16)   NOT NULL,
    code                VARCHAR(32)  NOT NULL,
    name                VARCHAR(200) NOT NULL,
    default_location_id BINARY(16),
    archived            TINYINT(1)   NOT NULL,
    created_at          BIGINT       NOT NULL,
    updated_at          BIGINT       NOT NULL,
    UNIQUE KEY ux_register_code (branch_id, code),
    CONSTRAINT fk_register_branch FOREIGN KEY (branch_id) REFERENCES branch (id),
    CONSTRAINT fk_register_location FOREIGN KEY (default_location_id) REFERENCES stock_location (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE register_pairing_code (
    code_hash   BINARY(32) NOT NULL PRIMARY KEY,
    register_id BINARY(16) NOT NULL,
    created_at  BIGINT     NOT NULL,
    expires_at  BIGINT     NOT NULL,
    consumed_at BIGINT,
    CONSTRAINT fk_pairing_code_register FOREIGN KEY (register_id) REFERENCES register (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE app_user (
    id            BINARY(16)   NOT NULL PRIMARY KEY,
    username      VARCHAR(100) NOT NULL,
    display_name  VARCHAR(200) NOT NULL,
    pin_hash      VARCHAR(255),
    password_hash VARCHAR(255),
    active        TINYINT(1)   NOT NULL,
    created_at    BIGINT       NOT NULL,
    updated_at    BIGINT       NOT NULL,
    UNIQUE KEY ux_app_user_username (username)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE role (
    id         BINARY(16)   NOT NULL PRIMARY KEY,
    name       VARCHAR(100) NOT NULL,
    archived   TINYINT(1)   NOT NULL,
    created_at BIGINT       NOT NULL,
    updated_at BIGINT       NOT NULL,
    UNIQUE KEY ux_role_name (name)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE role_permission (
    role_id     BINARY(16)  NOT NULL,
    perm        VARCHAR(64) NOT NULL,
    limit_value BIGINT,
    PRIMARY KEY (role_id, perm),
    CONSTRAINT fk_role_permission_role FOREIGN KEY (role_id) REFERENCES role (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

-- branch_id NULL = all branches.
CREATE TABLE user_role (
    user_id   BINARY(16) NOT NULL,
    role_id   BINARY(16) NOT NULL,
    branch_id BINARY(16),
    KEY ix_user_role_user (user_id),
    KEY ix_user_role_role (role_id),
    CONSTRAINT fk_user_role_user FOREIGN KEY (user_id) REFERENCES app_user (id),
    CONSTRAINT fk_user_role_role FOREIGN KEY (role_id) REFERENCES role (id),
    CONSTRAINT fk_user_role_branch FOREIGN KEY (branch_id) REFERENCES branch (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE device_token (
    token_hash  BINARY(32) NOT NULL PRIMARY KEY,
    register_id BINARY(16) NOT NULL,
    created_at  BIGINT     NOT NULL,
    revoked_at  BIGINT,
    KEY ix_device_token_register (register_id),
    CONSTRAINT fk_device_token_register FOREIGN KEY (register_id) REFERENCES register (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE auth_session (
    token_hash  BINARY(32) NOT NULL PRIMARY KEY,
    user_id     BINARY(16) NOT NULL,
    register_id BINARY(16),
    branch_id   BINARY(16) NOT NULL,
    created_at  BIGINT     NOT NULL,
    last_seen   BIGINT     NOT NULL,
    expires_at  BIGINT     NOT NULL,
    revoked_at  BIGINT,
    KEY ix_auth_session_user (user_id),
    CONSTRAINT fk_auth_session_user FOREIGN KEY (user_id) REFERENCES app_user (id),
    CONSTRAINT fk_auth_session_register FOREIGN KEY (register_id) REFERENCES register (id),
    CONSTRAINT fk_auth_session_branch FOREIGN KEY (branch_id) REFERENCES branch (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

-- core::access::LoginAttempts per (user, branch).
CREATE TABLE login_attempt (
    user_id      BINARY(16) NOT NULL,
    branch_id    BINARY(16) NOT NULL,
    failures     BIGINT     NOT NULL,
    locked_until BIGINT,
    PRIMARY KEY (user_id, branch_id),
    CONSTRAINT fk_login_attempt_user FOREIGN KEY (user_id) REFERENCES app_user (id),
    CONSTRAINT fk_login_attempt_branch FOREIGN KEY (branch_id) REFERENCES branch (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE override_token (
    token_hash      BINARY(32)   NOT NULL PRIMARY KEY,
    permission      VARCHAR(64)  NOT NULL,
    requested_value BIGINT,
    register_id     BINARY(16)   NOT NULL,
    context_ref     VARCHAR(200) NOT NULL,
    requester_id    BINARY(16)   NOT NULL,
    approver_id     BINARY(16)   NOT NULL,
    created_at      BIGINT       NOT NULL,
    expires_at      BIGINT       NOT NULL,
    consumed_at     BIGINT,
    CONSTRAINT fk_override_register FOREIGN KEY (register_id) REFERENCES register (id),
    CONSTRAINT fk_override_requester FOREIGN KEY (requester_id) REFERENCES app_user (id),
    CONSTRAINT fk_override_approver FOREIGN KEY (approver_id) REFERENCES app_user (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

-- MySQL has no partial indexes. The "one open session per register / per user" safety net
-- (spec 2.1 §5) uses generated columns that hold the id only while the session is open and NULL
-- once closed; a UNIQUE key allows any number of NULLs.
CREATE TABLE cashier_session (
    id               BINARY(16)  NOT NULL PRIMARY KEY,
    user_id          BINARY(16)  NOT NULL,
    register_id      BINARY(16)  NOT NULL,
    opening_cash     BIGINT      NOT NULL,
    opened_at        BIGINT      NOT NULL,
    closed_at        BIGINT,
    close_kind       VARCHAR(16),
    forced_by        BINARY(16),
    counted_cash     BIGINT,
    expected_cash    BIGINT,
    over_short       BIGINT,
    transactions     BIGINT,
    open_register_id BINARY(16) AS (CASE WHEN closed_at IS NULL THEN register_id END) STORED,
    open_user_id     BINARY(16) AS (CASE WHEN closed_at IS NULL THEN user_id END) STORED,
    UNIQUE KEY ux_cashier_session_open_register (open_register_id),
    UNIQUE KEY ux_cashier_session_open_user (open_user_id),
    KEY ix_cashier_session_register (register_id),
    KEY ix_cashier_session_user (user_id),
    CONSTRAINT ck_cashier_session_kind CHECK (close_kind IN ('normal', 'forced')),
    CONSTRAINT ck_cashier_session_closed CHECK ((closed_at IS NULL) = (close_kind IS NULL)),
    CONSTRAINT fk_cashier_session_user FOREIGN KEY (user_id) REFERENCES app_user (id),
    CONSTRAINT fk_cashier_session_register FOREIGN KEY (register_id) REFERENCES register (id),
    CONSTRAINT fk_cashier_session_forced_by FOREIGN KEY (forced_by) REFERENCES app_user (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TABLE cashier_session_count (
    session_id   BINARY(16) NOT NULL,
    denomination BIGINT     NOT NULL,
    quantity     BIGINT     NOT NULL,
    PRIMARY KEY (session_id, denomination),
    CONSTRAINT fk_session_count_session FOREIGN KEY (session_id) REFERENCES cashier_session (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

-- Append-only (spec 2.1 §7): no foreign keys, so history never blocks master-data changes.
CREATE TABLE audit_log (
    id          BINARY(16)   NOT NULL PRIMARY KEY,
    occurred_at BIGINT       NOT NULL,
    branch_id   BINARY(16),
    register_id BINARY(16),
    user_id     BINARY(16),
    action      VARCHAR(64)  NOT NULL,
    target      VARCHAR(255),
    before_json JSON,
    after_json  JSON,
    KEY ix_audit_log_occurred_at (occurred_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;

CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'audit_log is append-only';

CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'audit_log is append-only';

-- docs/plan.md §1: every master-data write appends here in the same transaction.
CREATE TABLE outbox (
    event_id   BINARY(16)  NOT NULL PRIMARY KEY,
    kind       VARCHAR(64) NOT NULL,
    payload    JSON        NOT NULL,
    created_at BIGINT      NOT NULL,
    synced_at  BIGINT,
    KEY ix_outbox_synced_at (synced_at)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci;
