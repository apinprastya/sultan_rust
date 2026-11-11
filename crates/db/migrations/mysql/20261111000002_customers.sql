-- Spec 2.5 Customers & membership, phase M2 (MySQL 8.0.19+ / InnoDB). Keep in step with
-- ../sqlite/. IDs: BINARY(16) UUIDv7. Money: BIGINT minor units. Booleans: TINYINT(1).
-- Dates (tier_valid_until): BIGINT YYYYMMDD in the selling branch's time zone. Text compares
-- byte-wise (utf8mb4_0900_bin, see 20261108000002_binary_collation.sql).
--
-- Phone uniqueness (spec 2.5 §2): a phone is unique among customers that are not archived.
-- MySQL has no partial indexes, so `phone_active` is a STORED generated column that equals
-- `phone` while the customer is not archived and is NULL otherwise, under a UNIQUE key (a unique
-- key allows any number of NULLs). The repository never writes it. SQLite uses a partial unique
-- index instead. Erased and merged customers are archived, so they never hold a phone.

CREATE TABLE customer_group (
    id          BINARY(16)   NOT NULL PRIMARY KEY,
    name        VARCHAR(100) NOT NULL,
    archived_at BIGINT,
    created_at  BIGINT       NOT NULL,
    updated_at  BIGINT       NOT NULL,
    UNIQUE KEY ux_customer_group_name (name)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- tier_rank: higher = better (spec 2.5 §4). Not `rank`, which MySQL 8 reserves.
CREATE TABLE membership_tier (
    id          BINARY(16)   NOT NULL PRIMARY KEY,
    name        VARCHAR(100) NOT NULL,
    tier_rank   BIGINT       NOT NULL,
    archived_at BIGINT,
    created_at  BIGINT       NOT NULL,
    updated_at  BIGINT       NOT NULL,
    UNIQUE KEY ux_membership_tier_name (name),
    UNIQUE KEY ux_membership_tier_rank (tier_rank)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- search_key and balance: see ../sqlite/.
CREATE TABLE customer (
    id                  BINARY(16)   NOT NULL PRIMARY KEY,
    code                VARCHAR(16)  NOT NULL,
    name                VARCHAR(255) NOT NULL,
    phone               VARCHAR(32),
    phone_active        VARCHAR(32)  GENERATED ALWAYS AS
                            (CASE WHEN archived_at IS NULL THEN phone END) STORED,
    phone_conflict      VARCHAR(32),
    email               VARCHAR(320),
    birth_month         BIGINT,
    birth_day           BIGINT,
    birth_year          BIGINT,
    address             TEXT,
    company_name        VARCHAR(255),
    tax_id              VARCHAR(32),
    tax_address         TEXT,
    notes               TEXT,
    group_id            BINARY(16),
    tier_id             BINARY(16),
    tier_valid_until    BIGINT,
    manual_tier         TINYINT(1)   NOT NULL,
    signed_up_branch_id BINARY(16),
    signed_up_at        BIGINT       NOT NULL,
    search_key          VARCHAR(512) NOT NULL,
    balance             BIGINT       NOT NULL DEFAULT 0,
    archived_at         BIGINT,
    merged_into_id      BINARY(16),
    erased_at           BIGINT,
    created_at          BIGINT       NOT NULL,
    updated_at          BIGINT       NOT NULL,
    UNIQUE KEY ux_customer_code (code),
    UNIQUE KEY ux_customer_phone_active (phone_active),
    KEY ix_customer_phone (phone),
    KEY ix_customer_phone_conflict (phone_conflict),
    KEY ix_customer_search_key (search_key),
    KEY ix_customer_merged_into (merged_into_id),
    KEY ix_customer_group (group_id),
    KEY ix_customer_tier (tier_id),
    KEY ix_customer_balance (balance),
    CONSTRAINT ck_customer_birth_month CHECK (birth_month IS NULL OR birth_month BETWEEN 1 AND 12),
    CONSTRAINT ck_customer_birth_day CHECK (birth_day IS NULL OR birth_day BETWEEN 1 AND 31),
    CONSTRAINT ck_customer_birthday CHECK ((birth_month IS NULL) = (birth_day IS NULL)),
    CONSTRAINT ck_customer_birth_year CHECK (birth_year IS NULL OR birth_month IS NOT NULL),
    CONSTRAINT ck_customer_merged CHECK (merged_into_id IS NULL OR archived_at IS NOT NULL),
    CONSTRAINT ck_customer_erased CHECK (erased_at IS NULL OR archived_at IS NOT NULL),
    CONSTRAINT fk_customer_group FOREIGN KEY (group_id) REFERENCES customer_group (id),
    CONSTRAINT fk_customer_tier FOREIGN KEY (tier_id) REFERENCES membership_tier (id),
    CONSTRAINT fk_customer_branch FOREIGN KEY (signed_up_branch_id) REFERENCES branch (id),
    CONSTRAINT fk_customer_merged_into FOREIGN KEY (merged_into_id) REFERENCES customer (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- Signs (spec 2.5 §11): change_saved and payment_reversed add, payment and change_saved_reversed
-- subtract. payout is usually negative but positive when it writes off a negative balance, and
-- merge_out/merge_in move a balance of either sign.
-- Spec 2.5 §11: the append-only balance ledger (see ../sqlite/ for the foreign keys left out).
CREATE TABLE customer_balance_entry (
    id                 BINARY(16)  NOT NULL PRIMARY KEY,
    customer_id        BINARY(16)  NOT NULL,
    kind               VARCHAR(32) NOT NULL,
    amount             BIGINT      NOT NULL,
    branch_id          BINARY(16),
    register_id        BINARY(16),
    user_id            BINARY(16),
    cashier_session_id BINARY(16),
    document_id        BINARY(16),
    occurred_at        BIGINT      NOT NULL,
    KEY ix_customer_balance_entry_customer (customer_id, id),
    CONSTRAINT ck_balance_entry_kind CHECK (kind IN ('change_saved', 'payment',
        'change_saved_reversed', 'payment_reversed', 'payout', 'merge_out', 'merge_in')),
    CONSTRAINT ck_balance_entry_amount CHECK (amount <> 0),
    CONSTRAINT ck_balance_entry_plus
        CHECK (kind NOT IN ('change_saved', 'payment_reversed') OR amount > 0),
    CONSTRAINT ck_balance_entry_minus
        CHECK (kind NOT IN ('payment', 'change_saved_reversed') OR amount < 0),
    CONSTRAINT fk_balance_entry_customer FOREIGN KEY (customer_id) REFERENCES customer (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TRIGGER customer_balance_entry_no_update BEFORE UPDATE ON customer_balance_entry
    FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'customer_balance_entry is append-only';

CREATE TRIGGER customer_balance_entry_no_delete BEFORE DELETE ON customer_balance_entry
    FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'customer_balance_entry is append-only';

-- Spec 2.5 §11: company settings for saved change, one row. 0 per sale turns saving off.
CREATE TABLE customer_balance_settings (
    id                 BIGINT NOT NULL PRIMARY KEY,
    max_saved_per_sale BIGINT NOT NULL,
    max_balance        BIGINT NOT NULL,
    updated_at         BIGINT NOT NULL,
    CONSTRAINT ck_customer_balance_settings_single CHECK (id = 1),
    CONSTRAINT ck_customer_balance_settings_per_sale CHECK (max_saved_per_sale >= 0),
    CONSTRAINT ck_customer_balance_settings_max CHECK (max_balance >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO customer_balance_settings (id, max_saved_per_sale, max_balance, updated_at)
VALUES (1, 5000, 100000, 0);

-- Spec 2.5 §12: new permissions for installs whose seeded roles already exist (see ../sqlite/).
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.create', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'customer.create');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.search', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'customer.search');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.edit', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'customer.edit');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'customer.manage');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.erase', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'customer.erase');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.balance_payout', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'customer.balance_payout');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.create', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'customer.create');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.search', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'customer.search');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.edit', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'customer.edit');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'customer.manage');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.balance_payout', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'customer.balance_payout');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'customer.create', NULL FROM role
WHERE id = X'00000000000070008000000000000003'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000003' AND perm = 'customer.create');
