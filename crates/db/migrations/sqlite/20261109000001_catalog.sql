-- Spec 2.2 Catalog, phase C2 (SQLite). Keep in step with ../mysql/: only the DDL differs.
-- IDs: 16-byte UUIDv7 BLOBs. Quantities and unit factors: INTEGER thousandths (core::catalog::Qty).
-- Booleans: INTEGER 0/1. JSON: TEXT. Text compares byte-wise (SQLite's BINARY collation).
--
-- Company-wide uniqueness (spec 2.2 §3, §5, §6) uses columns the repository keeps equal to the
-- value while the row is active and NULL otherwise, under a UNIQUE index (any number of NULLs):
--   variant.sku_active  = sku  while the variant and its product are not archived;
--   variant.plu_active  = plu  likewise (only weighed variants have a PLU);
--   barcode.code_active = code while the barcode is not archived (archiving a variant keeps its
--                              barcodes reserved until they are removed explicitly).
-- The repository checks these first to report the conflicting variant; the indexes are the
-- safety net against races.

-- Tax classes: rates and inclusive/exclusive rules arrive with 2.3. The two seeded classes have
-- fixed IDs so every node has the same rows.
CREATE TABLE tax_class (
    id         BLOB    NOT NULL PRIMARY KEY,
    name       TEXT    NOT NULL UNIQUE,
    archived   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

INSERT INTO tax_class (id, name, archived, created_at, updated_at) VALUES
    (X'00000000000070008000000000000101', 'Standard', 0, 0, 0),
    (X'00000000000070008000000000000102', 'Exempt', 0, 0, 0);

CREATE TABLE category (
    id         BLOB    NOT NULL PRIMARY KEY,
    parent_id  BLOB REFERENCES category (id),
    name       TEXT    NOT NULL,
    code       TEXT    NOT NULL,
    archived   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX ix_category_parent ON category (parent_id);

-- name_key = core::catalog::search_key(name): brand names are unique ignoring case and accents.
CREATE TABLE brand (
    id         BLOB    NOT NULL PRIMARY KEY,
    name       TEXT    NOT NULL,
    name_key   TEXT    NOT NULL UNIQUE,
    archived   INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

-- base/default unit IDs point at product_unit rows of the same product (no foreign key: the
-- units reference the product, so the product row is written first).
-- used_at on variant: set by later phases (sales, stock movements, price lists) through
-- catalog::mark_used; a product is "used" once any variant is (spec 2.2 §3 "after first use").
CREATE TABLE product (
    id                       BLOB    NOT NULL PRIMARY KEY,
    code                     TEXT    NOT NULL UNIQUE,
    name                     TEXT    NOT NULL,
    receipt_name             TEXT    NOT NULL,
    type                     TEXT    NOT NULL,
    tracking                 TEXT    NOT NULL,
    category_id              BLOB REFERENCES category (id),
    brand_id                 BLOB REFERENCES brand (id),
    tax_class_id             BLOB    NOT NULL REFERENCES tax_class (id),
    base_unit_id             BLOB    NOT NULL,
    default_sale_unit_id     BLOB    NOT NULL,
    default_purchase_unit_id BLOB    NOT NULL,
    sellable                 INTEGER NOT NULL,
    purchasable              INTEGER NOT NULL,
    open_price               INTEGER NOT NULL,
    requires_note            INTEGER NOT NULL,
    tags                     TEXT    NOT NULL,
    search_key               TEXT    NOT NULL,
    archived_at              INTEGER,
    created_at               INTEGER NOT NULL,
    updated_at               INTEGER NOT NULL,
    CHECK (type IN ('stock', 'weighed', 'service', 'bundle')),
    CHECK (tracking IN ('none', 'batch', 'serial'))
);
CREATE INDEX ix_product_search_key ON product (search_key);
CREATE INDEX ix_product_category ON product (category_id);
CREATE INDEX ix_product_brand ON product (brand_id);
CREATE INDEX ix_product_type ON product (type);

CREATE TABLE product_unit (
    id          BLOB    NOT NULL PRIMARY KEY,
    product_id  BLOB    NOT NULL REFERENCES product (id),
    position    INTEGER NOT NULL,
    name        TEXT    NOT NULL,
    short_name  TEXT    NOT NULL,
    kind        TEXT    NOT NULL,
    factor      INTEGER NOT NULL,
    sellable    INTEGER NOT NULL,
    purchasable INTEGER NOT NULL,
    archived    INTEGER NOT NULL,
    CHECK (kind IN ('integer', 'decimal')),
    CHECK (factor > 0)
);
CREATE INDEX ix_product_unit_product ON product_unit (product_id);

CREATE TABLE product_option (
    id         BLOB    NOT NULL PRIMARY KEY,
    product_id BLOB    NOT NULL REFERENCES product (id),
    position   INTEGER NOT NULL,
    name       TEXT    NOT NULL
);
CREATE INDEX ix_product_option_product ON product_option (product_id);

CREATE TABLE product_option_value (
    id        BLOB    NOT NULL PRIMARY KEY,
    option_id BLOB    NOT NULL REFERENCES product_option (id),
    position  INTEGER NOT NULL,
    name      TEXT    NOT NULL,
    code      TEXT    NOT NULL
);
CREATE INDEX ix_product_option_value_option ON product_option_value (option_id);

CREATE TABLE variant (
    id         BLOB    NOT NULL PRIMARY KEY,
    product_id BLOB    NOT NULL REFERENCES product (id),
    position   INTEGER NOT NULL,
    sku        TEXT    NOT NULL,
    sku_active TEXT UNIQUE,
    plu        INTEGER,
    plu_active INTEGER UNIQUE,
    archived   INTEGER NOT NULL,
    used_at    INTEGER,
    CHECK (sku_active IS NULL OR sku_active = sku),
    CHECK (plu_active IS NULL OR plu_active = plu)
);
CREATE INDEX ix_variant_product ON variant (product_id);
CREATE INDEX ix_variant_sku ON variant (sku);
CREATE INDEX ix_variant_plu ON variant (plu);

-- One value per option of the product, in option order (position).
CREATE TABLE variant_option_value (
    variant_id      BLOB    NOT NULL REFERENCES variant (id),
    position        INTEGER NOT NULL,
    option_value_id BLOB    NOT NULL REFERENCES product_option_value (id),
    PRIMARY KEY (variant_id, position)
);
CREATE INDEX ix_variant_option_value_value ON variant_option_value (option_value_id);

CREATE TABLE barcode (
    id                        BLOB    NOT NULL PRIMARY KEY,
    product_id                BLOB    NOT NULL REFERENCES product (id),
    variant_id                BLOB    NOT NULL REFERENCES variant (id),
    unit_id                   BLOB    NOT NULL REFERENCES product_unit (id),
    position                  INTEGER NOT NULL,
    code                      TEXT    NOT NULL,
    code_active               TEXT UNIQUE,
    allow_invalid_check_digit INTEGER NOT NULL,
    archived                  INTEGER NOT NULL,
    CHECK (code_active IS NULL OR code_active = code)
);
CREATE INDEX ix_barcode_code ON barcode (code);
CREATE INDEX ix_barcode_product ON barcode (product_id);
CREATE INDEX ix_barcode_variant ON barcode (variant_id);

CREATE TABLE bundle_component (
    bundle_id  BLOB    NOT NULL REFERENCES product (id),
    position   INTEGER NOT NULL,
    variant_id BLOB    NOT NULL REFERENCES variant (id),
    unit_id    BLOB    NOT NULL REFERENCES product_unit (id),
    qty        INTEGER NOT NULL,
    PRIMARY KEY (bundle_id, variant_id, unit_id),
    CHECK (qty > 0)
);
CREATE INDEX ix_bundle_component_variant ON bundle_component (variant_id);

-- Spec 2.2 §10: a row = the product is NOT sold at that branch.
CREATE TABLE product_branch_block (
    product_id BLOB NOT NULL REFERENCES product (id),
    branch_id  BLOB NOT NULL REFERENCES branch (id),
    PRIMARY KEY (product_id, branch_id)
);
CREATE INDEX ix_product_branch_block_branch ON product_branch_block (branch_id);

-- Spec 2.2 §8: one image per product, stored in the database (resized by the server).
CREATE TABLE product_image (
    product_id BLOB    NOT NULL PRIMARY KEY REFERENCES product (id),
    mime       TEXT    NOT NULL,
    width      INTEGER NOT NULL,
    height     INTEGER NOT NULL,
    byte_size  INTEGER NOT NULL,
    data       BLOB    NOT NULL,
    updated_at INTEGER NOT NULL
);

-- Company catalog settings: one row. scale_layouts is a JSON array of
-- {"prefix", "plu_digits", "value_digits", "kind": "weight"|"price"}; unit_names a JSON array.
CREATE TABLE catalog_settings (
    id                      INTEGER NOT NULL PRIMARY KEY,
    product_code_prefix     TEXT    NOT NULL,
    internal_barcode_prefix TEXT    NOT NULL,
    scale_layouts           TEXT    NOT NULL,
    unit_names              TEXT    NOT NULL,
    updated_at              INTEGER NOT NULL,
    CHECK (id = 1)
);

INSERT INTO catalog_settings (id, product_code_prefix, internal_barcode_prefix, scale_layouts,
                              unit_names, updated_at)
VALUES (1, 'P', '29',
        '[{"prefix":21,"plu_digits":5,"value_digits":5,"kind":"weight"},{"prefix":22,"plu_digits":5,"value_digits":5,"kind":"price"}]',
        '["pcs","box","pack","renteng","lusin","kg","gram","liter","ml","meter"]', 0);

-- Spec 2.2 §7: generator counters (HQ only).
CREATE TABLE catalog_counter (
    name       TEXT    NOT NULL PRIMARY KEY,
    next_value INTEGER NOT NULL
);

INSERT INTO catalog_counter (name, next_value) VALUES ('product_code', 1), ('internal_barcode', 1);

-- Spec 2.2 §11: new permissions for installs whose seeded roles already exist (a fresh database
-- has no roles yet; seed::seed_defaults creates them with these grants). Owner is rewritten to
-- the whole catalog by the seed anyway. Manager (…0002) and Stock clerk (…0004) get
-- catalog.manage; Manager also catalog.view_archived.
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'catalog.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'catalog.manage');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'catalog.view_archived', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'catalog.view_archived');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'catalog.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'catalog.manage');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'catalog.view_archived', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'catalog.view_archived');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'catalog.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000004'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000004' AND perm = 'catalog.manage');
