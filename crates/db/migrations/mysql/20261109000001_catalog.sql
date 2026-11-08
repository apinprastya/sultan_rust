-- Spec 2.2 Catalog, phase C2 (MySQL 8.0.19+ / InnoDB). Keep in step with ../sqlite/.
-- IDs: BINARY(16) UUIDv7. Quantities and unit factors: BIGINT thousandths (core::catalog::Qty).
-- Booleans: TINYINT(1). JSON: JSON. Text compares byte-wise: every table uses utf8mb4_0900_bin
-- (see 20261108000002_binary_collation.sql), so SKUs, barcodes and codes are case-sensitive.
--
-- Company-wide uniqueness (spec 2.2 §3, §5, §6) uses columns the repository keeps equal to the
-- value while the row is active and NULL otherwise, under a UNIQUE key (any number of NULLs):
--   variant.sku_active  = sku  while the variant and its product are not archived;
--   variant.plu_active  = plu  likewise (only weighed variants have a PLU);
--   barcode.code_active = code while the barcode is not archived (archiving a variant keeps its
--                              barcodes reserved until they are removed explicitly).
-- The repository checks these first to report the conflicting variant; the keys are the safety
-- net against races.

CREATE TABLE tax_class (
    id         BINARY(16)   NOT NULL PRIMARY KEY,
    name       VARCHAR(100) NOT NULL,
    archived   TINYINT(1)   NOT NULL,
    created_at BIGINT       NOT NULL,
    updated_at BIGINT       NOT NULL,
    UNIQUE KEY ux_tax_class_name (name)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO tax_class (id, name, archived, created_at, updated_at) VALUES
    (X'00000000000070008000000000000101', 'Standard', 0, 0, 0),
    (X'00000000000070008000000000000102', 'Exempt', 0, 0, 0);

CREATE TABLE category (
    id         BINARY(16)  NOT NULL PRIMARY KEY,
    parent_id  BINARY(16),
    name       VARCHAR(60) NOT NULL,
    code       VARCHAR(8)  NOT NULL,
    archived   TINYINT(1)  NOT NULL,
    created_at BIGINT      NOT NULL,
    updated_at BIGINT      NOT NULL,
    KEY ix_category_parent (parent_id),
    CONSTRAINT fk_category_parent FOREIGN KEY (parent_id) REFERENCES category (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE brand (
    id         BINARY(16)   NOT NULL PRIMARY KEY,
    name       VARCHAR(120) NOT NULL,
    name_key   VARCHAR(255) NOT NULL,
    archived   TINYINT(1)   NOT NULL,
    created_at BIGINT       NOT NULL,
    updated_at BIGINT       NOT NULL,
    UNIQUE KEY ux_brand_name_key (name_key)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE product (
    id                       BINARY(16)   NOT NULL PRIMARY KEY,
    code                     VARCHAR(32)  NOT NULL,
    name                     VARCHAR(200) NOT NULL,
    receipt_name             VARCHAR(64)  NOT NULL,
    type                     VARCHAR(16)  NOT NULL,
    tracking                 VARCHAR(16)  NOT NULL,
    category_id              BINARY(16),
    brand_id                 BINARY(16),
    tax_class_id             BINARY(16)   NOT NULL,
    base_unit_id             BINARY(16)   NOT NULL,
    default_sale_unit_id     BINARY(16)   NOT NULL,
    default_purchase_unit_id BINARY(16)   NOT NULL,
    sellable                 TINYINT(1)   NOT NULL,
    purchasable              TINYINT(1)   NOT NULL,
    open_price               TINYINT(1)   NOT NULL,
    requires_note            TINYINT(1)   NOT NULL,
    tags                     JSON         NOT NULL,
    search_key               VARCHAR(512) NOT NULL,
    archived_at              BIGINT,
    created_at               BIGINT       NOT NULL,
    updated_at               BIGINT       NOT NULL,
    UNIQUE KEY ux_product_code (code),
    KEY ix_product_search_key (search_key),
    KEY ix_product_category (category_id),
    KEY ix_product_brand (brand_id),
    KEY ix_product_type (type),
    CONSTRAINT ck_product_type CHECK (type IN ('stock', 'weighed', 'service', 'bundle')),
    CONSTRAINT ck_product_tracking CHECK (tracking IN ('none', 'batch', 'serial')),
    CONSTRAINT fk_product_category FOREIGN KEY (category_id) REFERENCES category (id),
    CONSTRAINT fk_product_brand FOREIGN KEY (brand_id) REFERENCES brand (id),
    CONSTRAINT fk_product_tax_class FOREIGN KEY (tax_class_id) REFERENCES tax_class (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE product_unit (
    id          BINARY(16)  NOT NULL PRIMARY KEY,
    product_id  BINARY(16)  NOT NULL,
    position    BIGINT      NOT NULL,
    name        VARCHAR(24) NOT NULL,
    short_name  VARCHAR(8)  NOT NULL,
    kind        VARCHAR(16) NOT NULL,
    factor      BIGINT      NOT NULL,
    sellable    TINYINT(1)  NOT NULL,
    purchasable TINYINT(1)  NOT NULL,
    archived    TINYINT(1)  NOT NULL,
    KEY ix_product_unit_product (product_id),
    CONSTRAINT ck_product_unit_kind CHECK (kind IN ('integer', 'decimal')),
    CONSTRAINT ck_product_unit_factor CHECK (factor > 0),
    CONSTRAINT fk_product_unit_product FOREIGN KEY (product_id) REFERENCES product (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE product_option (
    id         BINARY(16)  NOT NULL PRIMARY KEY,
    product_id BINARY(16)  NOT NULL,
    position   BIGINT      NOT NULL,
    name       VARCHAR(40) NOT NULL,
    KEY ix_product_option_product (product_id),
    CONSTRAINT fk_product_option_product FOREIGN KEY (product_id) REFERENCES product (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE product_option_value (
    id        BINARY(16)  NOT NULL PRIMARY KEY,
    option_id BINARY(16)  NOT NULL,
    position  BIGINT      NOT NULL,
    name      VARCHAR(40) NOT NULL,
    code      VARCHAR(8)  NOT NULL,
    KEY ix_product_option_value_option (option_id),
    CONSTRAINT fk_option_value_option FOREIGN KEY (option_id) REFERENCES product_option (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE variant (
    id         BINARY(16)  NOT NULL PRIMARY KEY,
    product_id BINARY(16)  NOT NULL,
    position   BIGINT      NOT NULL,
    sku        VARCHAR(32) NOT NULL,
    sku_active VARCHAR(32),
    plu        BIGINT,
    plu_active BIGINT,
    archived   TINYINT(1)  NOT NULL,
    used_at    BIGINT,
    UNIQUE KEY ux_variant_sku_active (sku_active),
    UNIQUE KEY ux_variant_plu_active (plu_active),
    KEY ix_variant_product (product_id),
    KEY ix_variant_sku (sku),
    KEY ix_variant_plu (plu),
    CONSTRAINT ck_variant_sku_active CHECK (sku_active IS NULL OR sku_active = sku),
    CONSTRAINT ck_variant_plu_active CHECK (plu_active IS NULL OR plu_active = plu),
    CONSTRAINT fk_variant_product FOREIGN KEY (product_id) REFERENCES product (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE variant_option_value (
    variant_id      BINARY(16) NOT NULL,
    position        BIGINT     NOT NULL,
    option_value_id BINARY(16) NOT NULL,
    PRIMARY KEY (variant_id, position),
    KEY ix_variant_option_value_value (option_value_id),
    CONSTRAINT fk_vov_variant FOREIGN KEY (variant_id) REFERENCES variant (id),
    CONSTRAINT fk_vov_value FOREIGN KEY (option_value_id) REFERENCES product_option_value (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE barcode (
    id                        BINARY(16)  NOT NULL PRIMARY KEY,
    product_id                BINARY(16)  NOT NULL,
    variant_id                BINARY(16)  NOT NULL,
    unit_id                   BINARY(16)  NOT NULL,
    position                  BIGINT      NOT NULL,
    code                      VARCHAR(64) NOT NULL,
    code_active               VARCHAR(64),
    allow_invalid_check_digit TINYINT(1)  NOT NULL,
    archived                  TINYINT(1)  NOT NULL,
    UNIQUE KEY ux_barcode_code_active (code_active),
    KEY ix_barcode_code (code),
    KEY ix_barcode_product (product_id),
    KEY ix_barcode_variant (variant_id),
    CONSTRAINT ck_barcode_code_active CHECK (code_active IS NULL OR code_active = code),
    CONSTRAINT fk_barcode_product FOREIGN KEY (product_id) REFERENCES product (id),
    CONSTRAINT fk_barcode_variant FOREIGN KEY (variant_id) REFERENCES variant (id),
    CONSTRAINT fk_barcode_unit FOREIGN KEY (unit_id) REFERENCES product_unit (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE bundle_component (
    bundle_id  BINARY(16) NOT NULL,
    position   BIGINT     NOT NULL,
    variant_id BINARY(16) NOT NULL,
    unit_id    BINARY(16) NOT NULL,
    qty        BIGINT     NOT NULL,
    PRIMARY KEY (bundle_id, variant_id, unit_id),
    KEY ix_bundle_component_variant (variant_id),
    CONSTRAINT ck_bundle_component_qty CHECK (qty > 0),
    CONSTRAINT fk_bundle_component_bundle FOREIGN KEY (bundle_id) REFERENCES product (id),
    CONSTRAINT fk_bundle_component_variant FOREIGN KEY (variant_id) REFERENCES variant (id),
    CONSTRAINT fk_bundle_component_unit FOREIGN KEY (unit_id) REFERENCES product_unit (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE product_branch_block (
    product_id BINARY(16) NOT NULL,
    branch_id  BINARY(16) NOT NULL,
    PRIMARY KEY (product_id, branch_id),
    KEY ix_product_branch_block_branch (branch_id),
    CONSTRAINT fk_branch_block_product FOREIGN KEY (product_id) REFERENCES product (id),
    CONSTRAINT fk_branch_block_branch FOREIGN KEY (branch_id) REFERENCES branch (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE product_image (
    product_id BINARY(16)  NOT NULL PRIMARY KEY,
    mime       VARCHAR(64) NOT NULL,
    width      BIGINT      NOT NULL,
    height     BIGINT      NOT NULL,
    byte_size  BIGINT      NOT NULL,
    data       MEDIUMBLOB  NOT NULL,
    updated_at BIGINT      NOT NULL,
    CONSTRAINT fk_product_image_product FOREIGN KEY (product_id) REFERENCES product (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE catalog_settings (
    id                      BIGINT      NOT NULL PRIMARY KEY,
    product_code_prefix     VARCHAR(32) NOT NULL,
    internal_barcode_prefix VARCHAR(8)  NOT NULL,
    scale_layouts           JSON        NOT NULL,
    unit_names              JSON        NOT NULL,
    updated_at              BIGINT      NOT NULL,
    CONSTRAINT ck_catalog_settings_single CHECK (id = 1)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO catalog_settings (id, product_code_prefix, internal_barcode_prefix, scale_layouts,
                              unit_names, updated_at)
VALUES (1, 'P', '29',
        '[{"prefix":21,"plu_digits":5,"value_digits":5,"kind":"weight"},{"prefix":22,"plu_digits":5,"value_digits":5,"kind":"price"}]',
        '["pcs","box","pack","renteng","lusin","kg","gram","liter","ml","meter"]', 0);

CREATE TABLE catalog_counter (
    name       VARCHAR(32) NOT NULL PRIMARY KEY,
    next_value BIGINT      NOT NULL
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO catalog_counter (name, next_value) VALUES ('product_code', 1), ('internal_barcode', 1);

-- Spec 2.2 §11: new permissions for installs whose seeded roles already exist (see ../sqlite/).
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
