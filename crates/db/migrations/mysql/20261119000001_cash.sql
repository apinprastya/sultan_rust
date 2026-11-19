-- Spec 2.7 Cash management, phase K2 (MySQL 8.0.19+ / InnoDB). Keep in step with ../sqlite/:
-- only the DDL differs. IDs: BINARY(16) UUIDv7. Money: BIGINT minor units. Booleans:
-- TINYINT(1). Times: BIGINT unix millis. JSON: JSON. Days: BIGINT YYYYMMDD in the branch's time
-- zone. Text compares byte-wise (utf8mb4_0900_bin, see 20261108000002_binary_collation.sql).
--
-- Cash movements and Z reports are branch transactional data (spec 2.7 §1, §8), stored without
-- foreign keys to branches, registers, sessions or users, like sales (see ../sqlite/).

-- Spec 2.7 §1, K1: cash in / out reasons (HQ master data).
CREATE TABLE cash_reason (
    id            BINARY(16)   NOT NULL PRIMARY KEY,
    name          VARCHAR(200) NOT NULL,
    direction     VARCHAR(8)   NOT NULL,
    note_required TINYINT(1)   NOT NULL,
    active        TINYINT(1)   NOT NULL,
    sort_order    BIGINT       NOT NULL,
    archived_at   BIGINT,
    created_at    BIGINT       NOT NULL,
    updated_at    BIGINT       NOT NULL,
    CHECK (direction IN ('in', 'out'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_cash_reason_sort ON cash_reason (direction, sort_order, id);

-- Seeds (spec 2.7 §2, core::cash::SEED_REASONS), fixed IDs …0401–…0406.
INSERT INTO cash_reason (id, name, direction, note_required, active, sort_order, archived_at,
                         created_at, updated_at) VALUES
    (X'00000000000070008000000000000401', 'Tambahan modal / uang kecil', 'in', 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000402', 'Lainnya', 'in', 1, 1, 90, NULL, 0, 0),
    (X'00000000000070008000000000000403', 'Belanja keperluan toko', 'out', 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000404', 'Ongkos kirim / parkir', 'out', 0, 1, 20, NULL, 0, 0),
    (X'00000000000070008000000000000405', 'Kebersihan / keamanan', 'out', 0, 1, 30, NULL, 0, 0),
    (X'00000000000070008000000000000406', 'Lainnya', 'out', 1, 1, 90, NULL, 0, 0);

-- Spec 2.7 §1, §2: a cash in / out / safe drop (see ../sqlite/ for the column notes).
CREATE TABLE cash_movement (
    id                 BINARY(16)   NOT NULL PRIMARY KEY,
    number             VARCHAR(64)  NOT NULL UNIQUE,
    kind               VARCHAR(8)   NOT NULL,
    amount             BIGINT       NOT NULL,
    reason_id          BINARY(16),
    note               VARCHAR(1000),
    branch_id          BINARY(16)   NOT NULL,
    register_id        BINARY(16)   NOT NULL,
    cashier_session_id BINARY(16)   NOT NULL,
    user_id            BINARY(16)   NOT NULL,
    approved_by        BINARY(16),
    reverses           BINARY(16)   UNIQUE,
    local_day          BIGINT       NOT NULL,
    seq                BIGINT       NOT NULL,
    posted_at          BIGINT       NOT NULL,
    request_hash       BINARY(32)   NOT NULL,
    created_at         BIGINT       NOT NULL,
    CHECK (kind IN ('in', 'out', 'drop')),
    CHECK (amount > 0),
    CHECK ((kind = 'drop') = (reason_id IS NULL)),
    CHECK (reverses IS NULL OR note IS NOT NULL),
    CHECK (seq > 0),
    UNIQUE KEY ux_cash_movement_day_seq (register_id, local_day, seq)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_cash_movement_branch ON cash_movement (branch_id, id);
CREATE INDEX ix_cash_movement_register ON cash_movement (register_id, id);
CREATE INDEX ix_cash_movement_session ON cash_movement (cashier_session_id, id);
CREATE INDEX ix_cash_movement_reason ON cash_movement (reason_id);
CREATE INDEX ix_cash_movement_posted_at ON cash_movement (posted_at, id);

CREATE TRIGGER cash_movement_no_update BEFORE UPDATE ON cash_movement FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'cash_movement is append-only';

CREATE TRIGGER cash_movement_no_delete BEFORE DELETE ON cash_movement FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'cash_movement is append-only';

-- Spec 2.7 §2a (K8): nota photos, at most three per movement (photo_no 1–3, unique per movement).
CREATE TABLE cash_movement_photo (
    id          BINARY(16) NOT NULL PRIMARY KEY,
    movement_id BINARY(16) NOT NULL,
    photo_no    BIGINT     NOT NULL,
    sha256      BINARY(32) NOT NULL,
    size        BIGINT     NOT NULL,
    added_by    BINARY(16) NOT NULL,
    added_at    BIGINT     NOT NULL,
    CONSTRAINT fk_cash_movement_photo_movement FOREIGN KEY (movement_id)
        REFERENCES cash_movement (id),
    CHECK (photo_no BETWEEN 1 AND 3),
    CHECK (size > 0),
    UNIQUE KEY ux_cash_movement_photo_no (movement_id, photo_no)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_cash_movement_photo_sha256 ON cash_movement_photo (sha256);

CREATE TRIGGER cash_movement_photo_no_update BEFORE UPDATE ON cash_movement_photo FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'cash_movement_photo is append-only';

CREATE TRIGGER cash_movement_photo_no_delete BEFORE DELETE ON cash_movement_photo FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'cash_movement_photo is append-only';

-- Spec 2.7 §6: the Z report of a branch's business day (see ../sqlite/).
CREATE TABLE z_report (
    id              BINARY(16)  NOT NULL PRIMARY KEY,
    number          VARCHAR(64) NOT NULL UNIQUE,
    branch_id       BINARY(16)  NOT NULL,
    business_day    BIGINT      NOT NULL,
    seq             BIGINT      NOT NULL,
    supplementary   TINYINT(1)  NOT NULL,
    session_count   BIGINT      NOT NULL,
    sales_count     BIGINT      NOT NULL,
    net_sales       BIGINT      NOT NULL,
    expected_cash   BIGINT      NOT NULL,
    counted_cash    BIGINT      NOT NULL,
    over_short      BIGINT      NOT NULL,
    cash_to_deposit BIGINT      NOT NULL,
    figures         JSON        NOT NULL,
    run_at          BIGINT      NOT NULL,
    user_id         BINARY(16)  NOT NULL,
    approved_by     BINARY(16),
    created_at      BIGINT      NOT NULL,
    CHECK (seq > 0),
    CHECK (supplementary = (seq > 1)),
    CHECK (session_count > 0),
    UNIQUE KEY ux_z_report_day_seq (branch_id, business_day, seq)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_z_report_branch ON z_report (branch_id, id);
CREATE INDEX ix_z_report_day ON z_report (business_day, id);

CREATE TRIGGER z_report_no_update BEFORE UPDATE ON z_report FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'z_report is append-only';

CREATE TRIGGER z_report_no_delete BEFORE DELETE ON z_report FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'z_report is append-only';

-- Spec 2.7 §3, §4, §6 on the cashier session (see ../sqlite/).
ALTER TABLE cashier_session
    ADD COLUMN float_kept       BIGINT,
    ADD COLUMN opening_proposed BIGINT,
    ADD COLUMN missing_nota     BIGINT,
    ADD COLUMN z_report_id      BINARY(16),
    ADD KEY ix_cashier_session_day (local_day, z_report_id),
    ADD KEY ix_cashier_session_z (z_report_id);

-- Drawer opens and X reads are audit entries on the session (K12, K22), counted per session.
CREATE INDEX ix_audit_log_target ON audit_log (target, action);

-- Spec 2.7 §2a: the company's "missing nota" threshold (Rp, 0 = off).
ALTER TABLE sales_settings
    ADD COLUMN nota_flag_above BIGINT NOT NULL DEFAULT 100000,
    ADD CONSTRAINT ck_sales_settings_nota_flag_above CHECK (nota_flag_above >= 0);

-- Spec 2.7 §7: new grants for installs whose seeded roles already exist (see ../sqlite/).
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'cash.drop', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'cash.drop');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'day.close', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'day.close');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'cash.out', 50000 FROM role
WHERE id = X'00000000000070008000000000000003'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000003' AND perm = 'cash.out');
