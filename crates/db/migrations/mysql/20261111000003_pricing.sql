-- Spec 2.3 Pricing, phase R2 (MySQL 8.0.19+ / InnoDB). Keep in step with ../sqlite/.
-- IDs: BINARY(16) UUIDv7. Money: BIGINT minor units. Quantities: BIGINT thousandths.
-- Booleans: TINYINT(1). Dates (valid_from, valid_to): BIGINT YYYYMMDD in the selling branch's
-- time zone. Text compares byte-wise (utf8mb4_0900_bin, see 20261108000002_binary_collation.sql).
--
-- One live list per target (spec 2.3 §2): MySQL has no partial indexes, so `live_target` is a
-- STORED generated column that equals `target_id` while the list is active and not archived and
-- is NULL otherwise, under a UNIQUE key with `kind` (a unique key allows any number of NULLs).
-- `default_key` is 1 for the `default` list and NULL otherwise, under a UNIQUE key: exactly one
-- default list (seeded below). The repository never writes either column. SQLite uses partial
-- unique indexes instead. See ../sqlite/ for target_id and the rule columns.

CREATE TABLE price_list (
    id          BINARY(16)   NOT NULL PRIMARY KEY,
    kind        VARCHAR(16)  NOT NULL,
    target_id   BINARY(16),
    live_target BINARY(16)   GENERATED ALWAYS AS
                    (CASE WHEN active = 1 AND archived_at IS NULL THEN target_id END) STORED,
    default_key TINYINT      GENERATED ALWAYS AS
                    (CASE WHEN kind = 'default' THEN 1 END) STORED,
    name        VARCHAR(100) NOT NULL,
    active      TINYINT(1)   NOT NULL,
    rule_kind   VARCHAR(16),
    rule_value  BIGINT,
    archived_at BIGINT,
    created_at  BIGINT       NOT NULL,
    updated_at  BIGINT       NOT NULL,
    UNIQUE KEY ux_price_list_live (kind, live_target),
    UNIQUE KEY ux_price_list_default (default_key),
    KEY ix_price_list_target (target_id),
    CONSTRAINT ck_price_list_kind
        CHECK (kind IN ('default', 'branch', 'group', 'tier', 'customer')),
    CONSTRAINT ck_price_list_target CHECK ((kind = 'default') = (target_id IS NULL)),
    CONSTRAINT ck_price_list_rule_kind CHECK (rule_kind IS NULL OR rule_kind IN ('pct', 'amount')),
    CONSTRAINT ck_price_list_rule CHECK ((rule_kind IS NULL) = (rule_value IS NULL)),
    CONSTRAINT ck_price_list_default
        CHECK (kind <> 'default' OR (rule_kind IS NULL AND active = 1 AND archived_at IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO price_list (id, kind, target_id, name, active, rule_kind, rule_value, archived_at,
                        created_at, updated_at)
VALUES (X'00000000000070008000000000000201', 'default', NULL, 'Default', 1, NULL, NULL, NULL, 0, 0);

CREATE TABLE price_entry (
    id         BINARY(16) NOT NULL PRIMARY KEY,
    list_id    BINARY(16) NOT NULL,
    variant_id BINARY(16) NOT NULL,
    unit_id    BINARY(16) NOT NULL,
    valid_from BIGINT,
    valid_to   BIGINT,
    created_at BIGINT     NOT NULL,
    updated_at BIGINT     NOT NULL,
    KEY ix_price_entry_list (list_id, variant_id, unit_id),
    KEY ix_price_entry_variant (variant_id),
    CONSTRAINT ck_price_entry_dates
        CHECK (valid_from IS NULL OR valid_to IS NULL OR valid_from <= valid_to),
    CONSTRAINT fk_price_entry_list FOREIGN KEY (list_id) REFERENCES price_list (id),
    CONSTRAINT fk_price_entry_variant FOREIGN KEY (variant_id) REFERENCES variant (id),
    CONSTRAINT fk_price_entry_unit FOREIGN KEY (unit_id) REFERENCES product_unit (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE price_tier (
    entry_id   BINARY(16)  NOT NULL,
    min_qty    BIGINT      NOT NULL,
    price      BIGINT,
    rule_kind  VARCHAR(16),
    rule_value BIGINT,
    PRIMARY KEY (entry_id, min_qty),
    CONSTRAINT ck_price_tier_min_qty CHECK (min_qty > 0),
    CONSTRAINT ck_price_tier_price CHECK (price IS NULL OR price >= 0),
    CONSTRAINT ck_price_tier_rule_kind CHECK (rule_kind IS NULL OR rule_kind IN ('pct', 'amount')),
    CONSTRAINT ck_price_tier_rule CHECK ((rule_kind IS NULL) = (rule_value IS NULL)),
    CONSTRAINT ck_price_tier_one CHECK ((price IS NULL) <> (rule_kind IS NULL)),
    CONSTRAINT fk_price_tier_entry FOREIGN KEY (entry_id) REFERENCES price_entry (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE tax_rate (
    tax_class_id BINARY(16) NOT NULL,
    valid_from   BIGINT     NOT NULL,
    rate_bp      BIGINT     NOT NULL,
    created_at   BIGINT     NOT NULL,
    updated_at   BIGINT     NOT NULL,
    PRIMARY KEY (tax_class_id, valid_from),
    CONSTRAINT ck_tax_rate_bp CHECK (rate_bp BETWEEN 0 AND 65535),
    CONSTRAINT fk_tax_rate_class FOREIGN KEY (tax_class_id) REFERENCES tax_class (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO tax_rate (tax_class_id, valid_from, rate_bp, created_at, updated_at) VALUES
    (X'00000000000070008000000000000101', 20220401, 1100, 0, 0),
    (X'00000000000070008000000000000102', 20000101, 0, 0, 0);

CREATE TABLE pricing_settings (
    id             BIGINT      NOT NULL PRIMARY KEY,
    tax_inclusive  TINYINT(1)  NOT NULL,
    tax_registered TINYINT(1)  NOT NULL,
    cash_rounding  VARCHAR(16) NOT NULL,
    updated_at     BIGINT      NOT NULL,
    CONSTRAINT ck_pricing_settings_single CHECK (id = 1),
    CONSTRAINT ck_pricing_settings_cash_rounding
        CHECK (cash_rounding IN ('none', 'nearest_100', 'down_100', 'nearest_500', 'down_500'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO pricing_settings (id, tax_inclusive, tax_registered, cash_rounding, updated_at)
VALUES (1, 1, 1, 'none', 0);

-- Spec 2.3 §9: price.manage for installs whose seeded roles already exist (see ../sqlite/).
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'price.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'price.manage');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'price.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'price.manage');
