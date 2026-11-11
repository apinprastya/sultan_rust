-- Spec 2.3 Pricing, phase R2 (SQLite). Keep in step with ../mysql/: only the DDL differs.
-- IDs: 16-byte UUIDv7 BLOBs. Money: INTEGER minor units. Quantities: INTEGER thousandths.
-- Booleans: INTEGER 0/1. Dates (valid_from, valid_to): INTEGER YYYYMMDD in the selling branch's
-- time zone, like customer.tier_valid_until.
--
-- One live list per target (spec 2.3 §2): at most one list that is active and not archived per
-- (kind, target_id). Here a partial unique index does it. MySQL has no partial indexes and uses a
-- generated column instead (see ../mysql/). The `default` list has no target; exactly one exists
-- (seeded below with a fixed ID), and a second one is refused by ux_price_list_default.
--
-- target_id has no foreign key: it points at a branch, a customer group, a membership tier or a
-- customer depending on kind. The repository checks it exists.
--
-- A rule is (rule_kind, rule_value): `pct` in basis points (−5% = −500) or `amount` in minor
-- units (−Rp 500 = −500).

CREATE TABLE price_list (
    id          BLOB    NOT NULL PRIMARY KEY,
    kind        TEXT    NOT NULL,
    target_id   BLOB,
    name        TEXT    NOT NULL,
    active      INTEGER NOT NULL,
    rule_kind   TEXT,
    rule_value  INTEGER,
    archived_at INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (kind IN ('default', 'branch', 'group', 'tier', 'customer')),
    CHECK ((kind = 'default') = (target_id IS NULL)),
    CHECK (rule_kind IS NULL OR rule_kind IN ('pct', 'amount')),
    CHECK ((rule_kind IS NULL) = (rule_value IS NULL)),
    CHECK (kind <> 'default' OR (rule_kind IS NULL AND active = 1 AND archived_at IS NULL))
);
CREATE UNIQUE INDEX ux_price_list_live ON price_list (kind, target_id)
    WHERE active = 1 AND archived_at IS NULL;
CREATE UNIQUE INDEX ux_price_list_default ON price_list (kind) WHERE kind = 'default';
CREATE INDEX ix_price_list_target ON price_list (target_id);

INSERT INTO price_list (id, kind, target_id, name, active, rule_kind, rule_value, archived_at,
                        created_at, updated_at)
VALUES (X'00000000000070008000000000000201', 'default', NULL, 'Default', 1, NULL, NULL, NULL, 0, 0);

-- An entry prices one variant + unit of a list over an optional date range (spec 2.3 §3). Entries
-- of one list for the same variant + unit must not overlap; the repository checks that with
-- core::pricing::validate_entry.
CREATE TABLE price_entry (
    id         BLOB    NOT NULL PRIMARY KEY,
    list_id    BLOB    NOT NULL REFERENCES price_list (id),
    variant_id BLOB    NOT NULL REFERENCES variant (id),
    unit_id    BLOB    NOT NULL REFERENCES product_unit (id),
    valid_from INTEGER,
    valid_to   INTEGER,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (valid_from IS NULL OR valid_to IS NULL OR valid_from <= valid_to)
);
CREATE INDEX ix_price_entry_list ON price_entry (list_id, variant_id, unit_id);
CREATE INDEX ix_price_entry_variant ON price_entry (variant_id);

-- 1–10 tiers per entry, written with the entry as one document. A tier is a fixed price (≥ 0) or
-- a rule, never both.
CREATE TABLE price_tier (
    entry_id   BLOB    NOT NULL REFERENCES price_entry (id),
    min_qty    INTEGER NOT NULL,
    price      INTEGER,
    rule_kind  TEXT,
    rule_value INTEGER,
    PRIMARY KEY (entry_id, min_qty),
    CHECK (min_qty > 0),
    CHECK (price IS NULL OR price >= 0),
    CHECK (rule_kind IS NULL OR rule_kind IN ('pct', 'amount')),
    CHECK ((rule_kind IS NULL) = (rule_value IS NULL)),
    CHECK ((price IS NULL) <> (rule_kind IS NULL))
);

-- Spec 2.3 §7: the rate history of a tax class. The rate on a sale is the one with the latest
-- valid_from on or before the sale date. One rate per class and day.
CREATE TABLE tax_rate (
    tax_class_id BLOB    NOT NULL REFERENCES tax_class (id),
    valid_from   INTEGER NOT NULL,
    rate_bp      INTEGER NOT NULL,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (tax_class_id, valid_from),
    CHECK (rate_bp BETWEEN 0 AND 65535)
);

-- Standard (…0101) 11% from 2022-04-01; Exempt (…0102) 0% (from 2000-01-01, before any sale).
INSERT INTO tax_rate (tax_class_id, valid_from, rate_bp, created_at, updated_at) VALUES
    (X'00000000000070008000000000000101', 20220401, 1100, 0, 0),
    (X'00000000000070008000000000000102', 20000101, 0, 0, 0);

-- Spec 2.3 §7, §8: company pricing settings, one row.
CREATE TABLE pricing_settings (
    id             INTEGER NOT NULL PRIMARY KEY,
    tax_inclusive  INTEGER NOT NULL,
    tax_registered INTEGER NOT NULL,
    cash_rounding  TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL,
    CHECK (id = 1),
    CHECK (cash_rounding IN ('none', 'nearest_100', 'down_100', 'nearest_500', 'down_500'))
);

INSERT INTO pricing_settings (id, tax_inclusive, tax_registered, cash_rounding, updated_at)
VALUES (1, 1, 1, 'none', 0);

-- Spec 2.3 §9: price.manage for installs whose seeded roles already exist (a fresh database has
-- no roles yet; seed::seed_defaults creates them with these grants). Owner (…0001) and Manager
-- (…0002). Existing price.override grants are kept as they are (spec 2.3 §6: no limit means
-- unlimited); only fresh installs get Manager's 20%.
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'price.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'price.manage');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'price.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'price.manage');
