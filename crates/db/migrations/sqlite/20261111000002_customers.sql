-- Spec 2.5 Customers & membership, phase M2 (SQLite). Keep in step with ../mysql/: only the DDL
-- differs. IDs: 16-byte UUIDv7 BLOBs. Money: INTEGER minor units. Booleans: INTEGER 0/1.
-- Dates (tier_valid_until): INTEGER YYYYMMDD in the selling branch's time zone.
--
-- Phone uniqueness (spec 2.5 §2): a phone is unique among customers that are not archived. Here
-- a partial unique index does it. MySQL has no partial indexes and uses a generated column
-- instead (see ../mysql/). Erased and merged customers are archived, so they never hold a phone.

CREATE TABLE customer_group (
    id          BLOB    NOT NULL PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    archived_at INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- tier_rank: higher = better (spec 2.5 §4). Not `rank`, which MySQL 8 reserves.
CREATE TABLE membership_tier (
    id          BLOB    NOT NULL PRIMARY KEY,
    name        TEXT    NOT NULL UNIQUE,
    tier_rank   INTEGER NOT NULL UNIQUE,
    archived_at INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL
);

-- search_key = core::catalog::search_key(name + " " + company_name), for the name search (§6).
-- balance: the sum of the customer's customer_balance_entry rows, kept by the repository in the
-- same transaction as each entry (§11). Never written otherwise.
CREATE TABLE customer (
    id                  BLOB    NOT NULL PRIMARY KEY,
    code                TEXT    NOT NULL UNIQUE,
    name                TEXT    NOT NULL,
    phone               TEXT,
    phone_conflict      TEXT,
    email               TEXT,
    birth_month         INTEGER,
    birth_day           INTEGER,
    birth_year          INTEGER,
    address             TEXT,
    company_name        TEXT,
    tax_id              TEXT,
    tax_address         TEXT,
    notes               TEXT,
    group_id            BLOB REFERENCES customer_group (id),
    tier_id             BLOB REFERENCES membership_tier (id),
    tier_valid_until    INTEGER,
    manual_tier         INTEGER NOT NULL,
    signed_up_branch_id BLOB REFERENCES branch (id),
    signed_up_at        INTEGER NOT NULL,
    search_key          TEXT    NOT NULL,
    balance             INTEGER NOT NULL DEFAULT 0,
    archived_at         INTEGER,
    merged_into_id      BLOB REFERENCES customer (id),
    erased_at           INTEGER,
    created_at          INTEGER NOT NULL,
    updated_at          INTEGER NOT NULL,
    CHECK (birth_month IS NULL OR birth_month BETWEEN 1 AND 12),
    CHECK (birth_day IS NULL OR birth_day BETWEEN 1 AND 31),
    CHECK ((birth_month IS NULL) = (birth_day IS NULL)),
    CHECK (birth_year IS NULL OR birth_month IS NOT NULL),
    CHECK (merged_into_id IS NULL OR archived_at IS NOT NULL),
    CHECK (erased_at IS NULL OR archived_at IS NOT NULL)
);
CREATE UNIQUE INDEX ux_customer_phone_active ON customer (phone) WHERE archived_at IS NULL;
CREATE INDEX ix_customer_phone ON customer (phone);
CREATE INDEX ix_customer_phone_conflict ON customer (phone_conflict);
CREATE INDEX ix_customer_search_key ON customer (search_key);
CREATE INDEX ix_customer_merged_into ON customer (merged_into_id);
CREATE INDEX ix_customer_group ON customer (group_id);
CREATE INDEX ix_customer_tier ON customer (tier_id);
CREATE INDEX ix_customer_balance ON customer (balance);

-- Signs (spec 2.5 §11): change_saved and payment_reversed add, payment and change_saved_reversed
-- subtract. payout is usually negative but positive when it writes off a negative balance, and
-- merge_out/merge_in move a balance of either sign.
-- Spec 2.5 §11: the append-only balance ledger. Entries are created where they happen (a branch,
-- a register) and synced up like sales; the id is the client's UUIDv7, so a replay is a no-op.
-- Only customer_id has a foreign key: like audit_log, the branch, register, user, cashier session
-- and document (sale or payout, tables of later phases) are recorded as they were where the entry
-- was made, and a node receiving synced entries may not hold those rows.
CREATE TABLE customer_balance_entry (
    id                 BLOB    NOT NULL PRIMARY KEY,
    customer_id        BLOB    NOT NULL REFERENCES customer (id),
    kind               TEXT    NOT NULL,
    amount             INTEGER NOT NULL,
    branch_id          BLOB,
    register_id        BLOB,
    user_id            BLOB,
    cashier_session_id BLOB,
    document_id        BLOB,
    occurred_at        INTEGER NOT NULL,
    CHECK (kind IN ('change_saved', 'payment', 'change_saved_reversed', 'payment_reversed',
                    'payout', 'merge_out', 'merge_in')),
    CHECK (amount <> 0),
    CHECK (kind NOT IN ('change_saved', 'payment_reversed') OR amount > 0),
    CHECK (kind NOT IN ('payment', 'change_saved_reversed') OR amount < 0)
);
CREATE INDEX ix_customer_balance_entry_customer ON customer_balance_entry (customer_id, id);

CREATE TRIGGER customer_balance_entry_no_update BEFORE UPDATE ON customer_balance_entry
BEGIN
    SELECT RAISE(ABORT, 'customer_balance_entry is append-only');
END;

CREATE TRIGGER customer_balance_entry_no_delete BEFORE DELETE ON customer_balance_entry
BEGIN
    SELECT RAISE(ABORT, 'customer_balance_entry is append-only');
END;

-- Spec 2.5 §11: company settings for saved change, one row. 0 per sale turns saving off.
CREATE TABLE customer_balance_settings (
    id                 INTEGER NOT NULL PRIMARY KEY,
    max_saved_per_sale INTEGER NOT NULL,
    max_balance        INTEGER NOT NULL,
    updated_at         INTEGER NOT NULL,
    CHECK (id = 1),
    CHECK (max_saved_per_sale >= 0),
    CHECK (max_balance >= 0)
);

INSERT INTO customer_balance_settings (id, max_saved_per_sale, max_balance, updated_at)
VALUES (1, 5000, 100000, 0);

-- Spec 2.5 §12: new permissions for installs whose seeded roles already exist (a fresh database
-- has no roles yet; seed::seed_defaults creates them with these grants). Owner (…0001) gets all
-- of them (the seed rewrites Owner to the whole catalog anyway); Manager (…0002) all but
-- customer.erase; Cashier (…0003) customer.create.
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
