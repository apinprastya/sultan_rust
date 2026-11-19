-- Spec 2.7 Cash management, phase K2 (SQLite). Keep in step with ../mysql/: only the DDL differs.
-- IDs: 16-byte UUIDv7 BLOBs. Money: INTEGER minor units. Booleans: INTEGER 0/1. Times: INTEGER
-- unix millis. JSON: TEXT. Days: INTEGER YYYYMMDD in the branch's time zone.
--
-- Cash movements and Z reports are branch transactional data (spec 2.7 §1, §8): written with
-- their audit entry and outbox event in one transaction and pushed up to HQ. Like sales, they
-- record the branch, register, session and users without foreign keys to those tables (a node
-- receiving synced documents may not hold every row). Only the parts of one document reference
-- each other (a photo its movement).

-- Spec 2.7 §1, K1: cash in / out reasons (HQ master data). direction is fixed once created.
-- archived_at = deleted (soft delete; a reason is never hard-deleted); active = offered at the
-- register. note_required: the movement needs a note (the seeded "Lainnya").
CREATE TABLE cash_reason (
    id            BLOB    NOT NULL PRIMARY KEY,
    name          TEXT    NOT NULL,
    direction     TEXT    NOT NULL,
    note_required INTEGER NOT NULL,
    active        INTEGER NOT NULL,
    sort_order    INTEGER NOT NULL,
    archived_at   INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    CHECK (direction IN ('in', 'out'))
);
CREATE INDEX ix_cash_reason_sort ON cash_reason (direction, sort_order, id);

-- Seeds (spec 2.7 §2, core::cash::SEED_REASONS): fixed IDs (…0401–…0406) so every node seeds the
-- same rows. HQ renames, deactivates or archives them.
INSERT INTO cash_reason (id, name, direction, note_required, active, sort_order, archived_at,
                         created_at, updated_at) VALUES
    (X'00000000000070008000000000000401', 'Tambahan modal / uang kecil', 'in', 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000402', 'Lainnya', 'in', 1, 1, 90, NULL, 0, 0),
    (X'00000000000070008000000000000403', 'Belanja keperluan toko', 'out', 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000404', 'Ongkos kirim / parkir', 'out', 0, 1, 20, NULL, 0, 0),
    (X'00000000000070008000000000000405', 'Kebersihan / keamanan', 'out', 0, 1, 30, NULL, 0, 0),
    (X'00000000000070008000000000000406', 'Lainnya', 'out', 1, 1, 90, NULL, 0, 0);

-- Spec 2.7 §1, §2: a cash in / out / safe drop, created posted, never edited (append-only by the
-- triggers below). number = <BRANCH>-<REGISTER>-K-<YYMMDD>-<NN> from (local_day, seq), per
-- register and branch-local posting day; UNIQUE (register_id, local_day, seq) catches two racing
-- posts (the loser retries, 2.1 §5). A drop has no reason, in and out always have one (a reversal
-- takes its original's). reverses: the reversed movement (K10); UNIQUE, so a movement is reversed
-- at most once (NULLs do not collide). request_hash: a hash of the posted body, so replaying the
-- same PUT /cash-movements/{id} returns the stored movement and another body is a conflict.
CREATE TABLE cash_movement (
    id                 BLOB    NOT NULL PRIMARY KEY,
    number             TEXT    NOT NULL UNIQUE,
    kind               TEXT    NOT NULL,
    amount             INTEGER NOT NULL,
    reason_id          BLOB,
    note               TEXT,
    branch_id          BLOB    NOT NULL,
    register_id        BLOB    NOT NULL,
    cashier_session_id BLOB    NOT NULL,
    user_id            BLOB    NOT NULL,
    approved_by        BLOB,
    reverses           BLOB    UNIQUE,
    local_day          INTEGER NOT NULL,
    seq                INTEGER NOT NULL,
    posted_at          INTEGER NOT NULL,
    request_hash       BLOB    NOT NULL,
    created_at         INTEGER NOT NULL,
    CHECK (kind IN ('in', 'out', 'drop')),
    CHECK (amount > 0),
    CHECK ((kind = 'drop') = (reason_id IS NULL)),
    CHECK (reverses IS NULL OR note IS NOT NULL),
    CHECK (seq > 0),
    UNIQUE (register_id, local_day, seq)
);
CREATE INDEX ix_cash_movement_branch ON cash_movement (branch_id, id);
CREATE INDEX ix_cash_movement_register ON cash_movement (register_id, id);
CREATE INDEX ix_cash_movement_session ON cash_movement (cashier_session_id, id);
CREATE INDEX ix_cash_movement_reason ON cash_movement (reason_id);
CREATE INDEX ix_cash_movement_posted_at ON cash_movement (posted_at, id);

CREATE TRIGGER cash_movement_no_update BEFORE UPDATE ON cash_movement
BEGIN
    SELECT RAISE(ABORT, 'cash_movement is append-only');
END;

CREATE TRIGGER cash_movement_no_delete BEFORE DELETE ON cash_movement
BEGIN
    SELECT RAISE(ABORT, 'cash_movement is append-only');
END;

-- Spec 2.7 §2a (K8): nota photos of a cash movement. The file lives on the node's disk
-- (attachments/<2 hex>/<sha256>.jpg); this is its record. photo_no 1–3 with UNIQUE (movement_id,
-- photo_no) caps a movement at three photos even when two uploads race. Never deleted or
-- replaced in P1 (append-only).
CREATE TABLE cash_movement_photo (
    id          BLOB    NOT NULL PRIMARY KEY,
    movement_id BLOB    NOT NULL REFERENCES cash_movement (id),
    photo_no    INTEGER NOT NULL,
    sha256      BLOB    NOT NULL,
    size        INTEGER NOT NULL,
    added_by    BLOB    NOT NULL,
    added_at    INTEGER NOT NULL,
    CHECK (photo_no BETWEEN 1 AND 3),
    CHECK (size > 0),
    CHECK (length(sha256) = 32),
    UNIQUE (movement_id, photo_no)
);
CREATE INDEX ix_cash_movement_photo_sha256 ON cash_movement_photo (sha256);

CREATE TRIGGER cash_movement_photo_no_update BEFORE UPDATE ON cash_movement_photo
BEGIN
    SELECT RAISE(ABORT, 'cash_movement_photo is append-only');
END;

CREATE TRIGGER cash_movement_photo_no_delete BEFORE DELETE ON cash_movement_photo
BEGIN
    SELECT RAISE(ABORT, 'cash_movement_photo is append-only');
END;

-- Spec 2.7 §6: the Z report of a branch's business day, a snapshot never recomputed (append-only
-- by trigger). number = <BRANCH>-Z-<YYMMDD>-<NN> from (business_day, seq); a Z with seq > 1 is
-- supplementary (K16). figures: the whole report as JSON (built by the server); the headline
-- columns are the branch totals, for lists.
CREATE TABLE z_report (
    id              BLOB    NOT NULL PRIMARY KEY,
    number          TEXT    NOT NULL UNIQUE,
    branch_id       BLOB    NOT NULL,
    business_day    INTEGER NOT NULL,
    seq             INTEGER NOT NULL,
    supplementary   INTEGER NOT NULL,
    session_count   INTEGER NOT NULL,
    sales_count     INTEGER NOT NULL,
    net_sales       INTEGER NOT NULL,
    expected_cash   INTEGER NOT NULL,
    counted_cash    INTEGER NOT NULL,
    over_short      INTEGER NOT NULL,
    cash_to_deposit INTEGER NOT NULL,
    figures         TEXT    NOT NULL,
    run_at          INTEGER NOT NULL,
    user_id         BLOB    NOT NULL,
    approved_by     BLOB,
    created_at      INTEGER NOT NULL,
    CHECK (seq > 0),
    CHECK (supplementary = (seq > 1)),
    CHECK (session_count > 0),
    UNIQUE (branch_id, business_day, seq)
);
CREATE INDEX ix_z_report_branch ON z_report (branch_id, id);
CREATE INDEX ix_z_report_day ON z_report (business_day, id);

CREATE TRIGGER z_report_no_update BEFORE UPDATE ON z_report
BEGIN
    SELECT RAISE(ABORT, 'z_report is append-only');
END;

CREATE TRIGGER z_report_no_delete BEFORE DELETE ON z_report
BEGIN
    SELECT RAISE(ABORT, 'z_report is append-only');
END;

-- Spec 2.7 §3, §4, §6 on the cashier session: float_kept (decided at the close, K20),
-- opening_proposed (the proposal when the typed opening cash differs), missing_nota (cash outs
-- flagged at the close, §2a) and z_report_id (the Z that covers it; each session is in one Z).
ALTER TABLE cashier_session ADD COLUMN float_kept INTEGER;
ALTER TABLE cashier_session ADD COLUMN opening_proposed INTEGER;
ALTER TABLE cashier_session ADD COLUMN missing_nota INTEGER;
ALTER TABLE cashier_session ADD COLUMN z_report_id BLOB;
CREATE INDEX ix_cashier_session_day ON cashier_session (local_day, z_report_id);
CREATE INDEX ix_cashier_session_z ON cashier_session (z_report_id);

-- Drawer opens and X reads are audit entries on the session (K12, K22), counted per session.
CREATE INDEX ix_audit_log_target ON audit_log (target, action);

-- Spec 2.7 §2a: the company's "missing nota" threshold (Rp, 0 = off).
ALTER TABLE sales_settings ADD COLUMN nota_flag_above INTEGER NOT NULL DEFAULT 100000
    CHECK (nota_flag_above >= 0);

-- Spec 2.7 §7: new grants for installs whose seeded roles already exist (a fresh database has no
-- roles yet; seed::seed_defaults creates them with these grants, and rewrites Owner (…0001) to
-- the whole catalogue on every start). Manager (…0002): cash.drop and day.close. Cashier (…0003):
-- cash.out up to Rp 50 000 (K5), only when it has no cash.out grant yet, so an edited one stays.
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
