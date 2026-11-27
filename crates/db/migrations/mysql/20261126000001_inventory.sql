-- Spec 2.8 Inventory, phase N2 (MySQL 8.0.19+ / InnoDB). Keep in step with ../sqlite/: only the
-- DDL differs. IDs: BINARY(16) UUIDv7. Money: BIGINT minor units. Quantities: BIGINT thousandths
-- of a base unit. Booleans: TINYINT(1). Times: BIGINT unix millis. Days: BIGINT YYYYMMDD in the
-- branch's time zone. Text compares byte-wise (utf8mb4_0900_bin). See ../sqlite/ for the notes
-- on each table.

-- 1. The stock ledger (spec 2.8 §2), rebuilt like on SQLite (same kinds, columns and checks), so
--    a 2.6 row without a location moves to its branch's receiving default, else any live location,
--    else a location "Stok lama" created for it (§4).
INSERT INTO stock_location (id, branch_id, name, receiving_default, archived, created_at,
                            updated_at)
SELECT UUID_TO_BIN(UUID()), b.id, 'Stok lama', 1, 0, 0, 0
FROM branch b
WHERE EXISTS (SELECT 1 FROM stock_movement m WHERE m.branch_id = b.id AND m.location_id IS NULL)
  AND NOT EXISTS (SELECT 1 FROM stock_location l WHERE l.branch_id = b.id AND l.archived = 0);

CREATE TABLE stock_movement_new (
    id          BINARY(16)  NOT NULL PRIMARY KEY,
    branch_id   BINARY(16)  NOT NULL,
    location_id BINARY(16),
    variant_id  BINARY(16)  NOT NULL,
    kind        VARCHAR(32) NOT NULL,
    base_qty    BIGINT      NOT NULL,
    value       BIGINT,
    lot_id      BINARY(16),
    doc_kind    VARCHAR(32) NOT NULL,
    doc_id      BINARY(16)  NOT NULL,
    doc_line    BIGINT,
    occurred_at BIGINT      NOT NULL,
    CONSTRAINT ck_stock_movement_kind CHECK (kind IN ('sale', 'sale_void', 'return', 'write_off',
        'receipt', 'receipt_void', 'supplier_return', 'transfer_out', 'transfer_in',
        'transit_loss', 'internal_out', 'internal_in', 'adjust', 'count', 'opening')),
    CONSTRAINT ck_stock_movement_out CHECK (kind NOT IN ('sale', 'write_off', 'receipt_void',
        'supplier_return', 'transfer_out', 'transit_loss', 'internal_out') OR base_qty < 0),
    CONSTRAINT ck_stock_movement_in CHECK (kind NOT IN ('sale_void', 'return', 'receipt',
        'transfer_in', 'internal_in', 'opening') OR base_qty > 0),
    CONSTRAINT ck_stock_movement_qty CHECK (base_qty <> 0),
    CONSTRAINT ck_stock_movement_doc CHECK (doc_kind IN ('sale', 'return', 'goods_receipt',
        'supplier_return', 'transfer', 'internal_transfer', 'adjustment', 'opname',
        'opening_stock'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO stock_movement_new (id, branch_id, location_id, variant_id, kind, base_qty, value,
                                lot_id, doc_kind, doc_id, doc_line, occurred_at)
SELECT m.id, m.branch_id,
       COALESCE(m.location_id,
                (SELECT l.id FROM stock_location l
                 WHERE l.branch_id = m.branch_id AND l.archived = 0
                 ORDER BY l.receiving_default DESC, l.id LIMIT 1)),
       m.variant_id, m.kind, m.base_qty, NULL, NULL, m.doc_kind, m.doc_id, NULL, m.occurred_at
FROM stock_movement m;

DROP TABLE stock_movement;
RENAME TABLE stock_movement_new TO stock_movement;
CREATE INDEX ix_stock_movement_variant ON stock_movement (variant_id, branch_id, location_id);
CREATE INDEX ix_stock_movement_doc ON stock_movement (doc_id);
CREATE INDEX ix_stock_movement_place ON stock_movement (branch_id, location_id, variant_id, id);
CREATE INDEX ix_stock_movement_lot ON stock_movement (lot_id);

CREATE TRIGGER stock_movement_no_update BEFORE UPDATE ON stock_movement FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'stock_movement is append-only';

CREATE TRIGGER stock_movement_no_delete BEFORE DELETE ON stock_movement FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'stock_movement is append-only';

-- 2. Cached on hand (§4).
CREATE TABLE stock_balance (
    branch_id   BINARY(16) NOT NULL,
    location_id BINARY(16) NOT NULL,
    variant_id  BINARY(16) NOT NULL,
    qty         BIGINT     NOT NULL,
    updated_at  BIGINT     NOT NULL,
    PRIMARY KEY (branch_id, location_id, variant_id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_stock_balance_variant ON stock_balance (variant_id, branch_id);

INSERT INTO stock_balance (branch_id, location_id, variant_id, qty, updated_at)
SELECT branch_id, COALESCE(location_id, X'00000000000000000000000000000000'), variant_id,
       SUM(base_qty), 0
FROM stock_movement
GROUP BY branch_id, COALESCE(location_id, X'00000000000000000000000000000000'), variant_id;

-- 3. The moving average per (branch, variant) (§5).
CREATE TABLE stock_cost (
    branch_id  BINARY(16) NOT NULL,
    variant_id BINARY(16) NOT NULL,
    qty        BIGINT     NOT NULL,
    value      BIGINT     NOT NULL,
    ref_value  BIGINT,
    ref_qty    BIGINT,
    updated_at BIGINT     NOT NULL,
    PRIMARY KEY (branch_id, variant_id),
    CONSTRAINT ck_stock_cost_ref CHECK ((ref_value IS NULL) = (ref_qty IS NULL)),
    CONSTRAINT ck_stock_cost_ref_qty CHECK (ref_qty IS NULL OR ref_qty > 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO stock_cost (branch_id, variant_id, qty, value, ref_value, ref_qty, updated_at)
SELECT branch_id, variant_id, SUM(base_qty), 0, NULL, NULL, 0
FROM stock_movement
GROUP BY branch_id, variant_id;

-- 4. Expiry lots (§9).
CREATE TABLE stock_lot (
    id          BINARY(16)  NOT NULL PRIMARY KEY,
    branch_id   BINARY(16)  NOT NULL,
    location_id BINARY(16)  NOT NULL,
    variant_id  BINARY(16)  NOT NULL,
    batch       VARCHAR(160) NOT NULL,
    expiry      BIGINT      NOT NULL,
    qty         BIGINT      NOT NULL,
    created_at  BIGINT      NOT NULL,
    updated_at  BIGINT      NOT NULL,
    UNIQUE KEY ux_stock_lot (branch_id, location_id, variant_id, batch, expiry)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_stock_lot_variant ON stock_lot (variant_id, location_id);
CREATE INDEX ix_stock_lot_expiry ON stock_lot (branch_id, expiry);

-- 5. Suppliers and their items (§3, HQ master data).
CREATE TABLE supplier (
    id          BINARY(16)    NOT NULL PRIMARY KEY,
    code        VARCHAR(80)   NOT NULL,
    name        VARCHAR(480)  NOT NULL,
    address     VARCHAR(1000),
    phone       VARCHAR(100),
    email       VARCHAR(320),
    tax_id      VARCHAR(100),
    pkp         TINYINT(1)    NOT NULL,
    terms_days  BIGINT        NOT NULL,
    lead_days   BIGINT        NOT NULL,
    notes       VARCHAR(2000),
    active      TINYINT(1)    NOT NULL,
    archived_at BIGINT,
    created_at  BIGINT        NOT NULL,
    updated_at  BIGINT        NOT NULL,
    UNIQUE KEY ux_supplier_code (code),
    CONSTRAINT ck_supplier_terms CHECK (terms_days BETWEEN 0 AND 365),
    CONSTRAINT ck_supplier_lead CHECK (lead_days BETWEEN 0 AND 365)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE supplier_item (
    supplier_id       BINARY(16)  NOT NULL,
    variant_id        BINARY(16)  NOT NULL,
    supplier_code     VARCHAR(160),
    last_price        BIGINT,
    last_unit_id      BINARY(16),
    preferred         TINYINT(1)  NOT NULL,
    preferred_variant BINARY(16),
    created_at        BIGINT      NOT NULL,
    updated_at        BIGINT      NOT NULL,
    PRIMARY KEY (supplier_id, variant_id),
    UNIQUE KEY ux_supplier_item_code (supplier_id, supplier_code),
    UNIQUE KEY ux_supplier_item_preferred (preferred_variant),
    CONSTRAINT fk_supplier_item_supplier FOREIGN KEY (supplier_id) REFERENCES supplier (id),
    CONSTRAINT ck_supplier_item_preferred CHECK ((preferred = 1) = (preferred_variant IS NOT NULL)),
    CONSTRAINT ck_supplier_item_preferred_variant
        CHECK (preferred_variant IS NULL OR preferred_variant = variant_id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_supplier_item_variant ON supplier_item (variant_id);

-- 6. Stock reasons (§7, HQ master data), seeded …0501–…0506.
CREATE TABLE stock_reason (
    id            BINARY(16)   NOT NULL PRIMARY KEY,
    name          VARCHAR(240) NOT NULL,
    direction     VARCHAR(8)   NOT NULL,
    note_required TINYINT(1)   NOT NULL,
    active        TINYINT(1)   NOT NULL,
    sort_order    BIGINT       NOT NULL,
    archived_at   BIGINT,
    created_at    BIGINT       NOT NULL,
    updated_at    BIGINT       NOT NULL,
    CONSTRAINT ck_stock_reason_direction CHECK (direction IN ('in', 'out', 'both'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_stock_reason_sort ON stock_reason (sort_order, id);

INSERT INTO stock_reason (id, name, direction, note_required, active, sort_order, archived_at,
                          created_at, updated_at) VALUES
    (X'00000000000070008000000000000501', 'Rusak', 'out', 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000502', 'Kedaluwarsa', 'out', 0, 1, 20, NULL, 0, 0),
    (X'00000000000070008000000000000503', 'Hilang', 'out', 0, 1, 30, NULL, 0, 0),
    (X'00000000000070008000000000000504', 'Sampel', 'out', 0, 1, 40, NULL, 0, 0),
    (X'00000000000070008000000000000505', 'Pemakaian sendiri', 'out', 0, 1, 50, NULL, 0, 0),
    (X'00000000000070008000000000000506', 'Koreksi', 'both', 1, 1, 90, NULL, 0, 0);

-- 7. The company's inventory settings (§13).
CREATE TABLE inventory_settings (
    id                   BIGINT     NOT NULL PRIMARY KEY,
    pkp                  TINYINT(1) NOT NULL,
    standard_ppn_rate_bp BIGINT     NOT NULL,
    expiry_warn_days     BIGINT     NOT NULL,
    velocity_days        BIGINT     NOT NULL,
    cover_days           BIGINT     NOT NULL,
    updated_at           BIGINT     NOT NULL,
    CONSTRAINT ck_inventory_settings_single CHECK (id = 1),
    CONSTRAINT ck_inventory_settings_rate CHECK (standard_ppn_rate_bp BETWEEN 0 AND 10000),
    CONSTRAINT ck_inventory_settings_warn CHECK (expiry_warn_days BETWEEN 0 AND 365),
    CONSTRAINT ck_inventory_settings_velocity CHECK (velocity_days BETWEEN 1 AND 365),
    CONSTRAINT ck_inventory_settings_cover CHECK (cover_days BETWEEN 0 AND 365)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO inventory_settings (id, pkp, standard_ppn_rate_bp, expiry_warn_days, velocity_days,
                                cover_days, updated_at)
VALUES (1, 0, 1100, 30, 30, 14, 0);

-- 8. Product fields (§9).
ALTER TABLE product
    ADD COLUMN track_expiry     TINYINT(1) NOT NULL DEFAULT 0,
    ADD COLUMN expiry_warn_days BIGINT,
    ADD CONSTRAINT ck_product_expiry_warn_days
        CHECK (expiry_warn_days IS NULL OR expiry_warn_days BETWEEN 0 AND 365);

-- 9. Reorder rules (§10).
CREATE TABLE reorder_rule (
    branch_id  BINARY(16) NOT NULL,
    variant_id BINARY(16) NOT NULL,
    min_qty    BIGINT     NOT NULL,
    max_qty    BIGINT     NOT NULL,
    updated_at BIGINT     NOT NULL,
    PRIMARY KEY (branch_id, variant_id),
    CONSTRAINT ck_reorder_rule CHECK (min_qty >= 0 AND max_qty >= min_qty)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 10. Purchase orders (§6).
CREATE TABLE purchase_order (
    id            BINARY(16)    NOT NULL PRIMARY KEY,
    number        VARCHAR(64),
    branch_id     BINARY(16)    NOT NULL,
    supplier_id   BINARY(16)    NOT NULL,
    location_id   BINARY(16)    NOT NULL,
    status        VARCHAR(16)   NOT NULL,
    order_date    BIGINT,
    expected_date BIGINT,
    note          VARCHAR(2000),
    local_day     BIGINT,
    seq           BIGINT,
    user_id       BINARY(16)    NOT NULL,
    approved_by   BINARY(16),
    ordered_at    BIGINT,
    ordered_by    BINARY(16),
    closed_at     BIGINT,
    closed_by     BINARY(16),
    cancelled_at  BIGINT,
    cancelled_by  BINARY(16),
    created_at    BIGINT        NOT NULL,
    updated_at    BIGINT        NOT NULL,
    UNIQUE KEY ux_purchase_order_number (number),
    UNIQUE KEY ux_purchase_order_day_seq (branch_id, local_day, seq),
    KEY ix_purchase_order_branch (branch_id, status, id),
    KEY ix_purchase_order_supplier (supplier_id, id),
    CONSTRAINT fk_purchase_order_supplier FOREIGN KEY (supplier_id) REFERENCES supplier (id),
    CONSTRAINT ck_purchase_order_status
        CHECK (status IN ('draft', 'ordered', 'closed', 'cancelled')),
    CONSTRAINT ck_purchase_order_number
        CHECK ((status = 'draft') = (number IS NULL) OR status = 'cancelled')
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE purchase_order_line (
    order_id     BINARY(16) NOT NULL,
    line_no      BIGINT     NOT NULL,
    variant_id   BINARY(16) NOT NULL,
    unit_id      BINARY(16) NOT NULL,
    factor       BIGINT     NOT NULL,
    qty          BIGINT     NOT NULL,
    price        BIGINT     NOT NULL,
    base_qty     BIGINT     NOT NULL,
    received_qty BIGINT     NOT NULL,
    PRIMARY KEY (order_id, line_no),
    KEY ix_purchase_order_line_variant (variant_id),
    CONSTRAINT fk_purchase_order_line_order FOREIGN KEY (order_id) REFERENCES purchase_order (id),
    CONSTRAINT ck_purchase_order_line CHECK (factor > 0 AND qty > 0 AND price >= 0
        AND base_qty > 0 AND received_qty >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 11. Goods receipts (§6).
CREATE TABLE goods_receipt (
    id              BINARY(16)    NOT NULL PRIMARY KEY,
    number          VARCHAR(64),
    branch_id       BINARY(16)    NOT NULL,
    supplier_id     BINARY(16)    NOT NULL,
    location_id     BINARY(16)    NOT NULL,
    status          VARCHAR(16)   NOT NULL,
    invoice_no      VARCHAR(256)  NOT NULL,
    invoice_date    BIGINT        NOT NULL,
    invoice_active  VARCHAR(256),
    payment         VARCHAR(16)   NOT NULL,
    due_date        BIGINT,
    tax_mode        VARCHAR(16)   NOT NULL,
    tax_rate_bp     BIGINT        NOT NULL,
    header_discount BIGINT        NOT NULL,
    other_costs     BIGINT        NOT NULL,
    pkp             TINYINT(1),
    net             BIGINT,
    tax             BIGINT,
    total           BIGINT,
    landed          BIGINT,
    note            VARCHAR(2000),
    local_day       BIGINT,
    seq             BIGINT,
    user_id         BINARY(16)    NOT NULL,
    approved_by     BINARY(16),
    posted_at       BIGINT,
    posted_by       BINARY(16),
    voided_at       BIGINT,
    voided_by       BINARY(16),
    created_at      BIGINT        NOT NULL,
    updated_at      BIGINT        NOT NULL,
    UNIQUE KEY ux_goods_receipt_number (number),
    UNIQUE KEY ux_goods_receipt_invoice (supplier_id, invoice_active),
    UNIQUE KEY ux_goods_receipt_day_seq (branch_id, local_day, seq),
    KEY ix_goods_receipt_branch (branch_id, status, id),
    KEY ix_goods_receipt_supplier (supplier_id, id),
    CONSTRAINT fk_goods_receipt_supplier FOREIGN KEY (supplier_id) REFERENCES supplier (id),
    CONSTRAINT ck_goods_receipt_status CHECK (status IN ('draft', 'posted', 'voided')),
    CONSTRAINT ck_goods_receipt_payment CHECK (payment IN ('credit', 'cash')),
    CONSTRAINT ck_goods_receipt_tax_mode CHECK (tax_mode IN ('included', 'added', 'none')),
    CONSTRAINT ck_goods_receipt_rate CHECK (tax_rate_bp BETWEEN 0 AND 10000),
    CONSTRAINT ck_goods_receipt_amounts CHECK (header_discount >= 0 AND other_costs >= 0),
    CONSTRAINT ck_goods_receipt_invoice
        CHECK ((status = 'posted') = (invoice_active IS NOT NULL)),
    CONSTRAINT ck_goods_receipt_number CHECK ((status = 'draft') = (number IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE goods_receipt_line (
    receipt_id      BINARY(16)   NOT NULL,
    line_no         BIGINT       NOT NULL,
    variant_id      BINARY(16)   NOT NULL,
    unit_id         BINARY(16)   NOT NULL,
    factor          BIGINT       NOT NULL,
    qty             BIGINT       NOT NULL,
    price           BIGINT       NOT NULL,
    discount        BIGINT       NOT NULL,
    order_id        BINARY(16),
    order_line_no   BIGINT,
    batch           VARCHAR(160),
    expiry          BIGINT,
    base_qty        BIGINT       NOT NULL,
    header_discount BIGINT,
    tax             BIGINT,
    other_costs     BIGINT,
    landed          BIGINT,
    lot_id          BINARY(16),
    PRIMARY KEY (receipt_id, line_no),
    KEY ix_goods_receipt_line_variant (variant_id),
    KEY ix_goods_receipt_line_order (order_id, order_line_no),
    CONSTRAINT fk_goods_receipt_line_receipt FOREIGN KEY (receipt_id)
        REFERENCES goods_receipt (id),
    CONSTRAINT ck_goods_receipt_line CHECK (factor > 0 AND qty > 0 AND price >= 0
        AND discount >= 0 AND base_qty > 0),
    CONSTRAINT ck_goods_receipt_line_order CHECK ((order_id IS NULL) = (order_line_no IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 12. Payables (§6, I3).
CREATE TABLE payable (
    id          BINARY(16)  NOT NULL PRIMARY KEY,
    branch_id   BINARY(16)  NOT NULL,
    supplier_id BINARY(16)  NOT NULL,
    receipt_id  BINARY(16)  NOT NULL,
    amount      BIGINT      NOT NULL,
    due_date    BIGINT      NOT NULL,
    paid        TINYINT(1)  NOT NULL,
    status      VARCHAR(16) NOT NULL,
    created_at  BIGINT      NOT NULL,
    voided_at   BIGINT,
    UNIQUE KEY ux_payable_receipt (receipt_id),
    KEY ix_payable_branch (branch_id, status, due_date),
    KEY ix_payable_supplier (supplier_id, status, due_date),
    CONSTRAINT fk_payable_supplier FOREIGN KEY (supplier_id) REFERENCES supplier (id),
    CONSTRAINT fk_payable_receipt FOREIGN KEY (receipt_id) REFERENCES goods_receipt (id),
    CONSTRAINT ck_payable_status CHECK (status IN ('open', 'voided')),
    CONSTRAINT ck_payable_amount CHECK (amount >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 13. Supplier returns (§6).
CREATE TABLE supplier_return (
    id          BINARY(16)    NOT NULL PRIMARY KEY,
    number      VARCHAR(64),
    branch_id   BINARY(16)    NOT NULL,
    supplier_id BINARY(16)    NOT NULL,
    location_id BINARY(16)    NOT NULL,
    status      VARCHAR(16)   NOT NULL,
    tax_mode    VARCHAR(16)   NOT NULL,
    tax_rate_bp BIGINT        NOT NULL,
    credit      BIGINT,
    note        VARCHAR(2000),
    local_day   BIGINT,
    seq         BIGINT,
    user_id     BINARY(16)    NOT NULL,
    approved_by BINARY(16),
    posted_at   BIGINT,
    posted_by   BINARY(16),
    created_at  BIGINT        NOT NULL,
    updated_at  BIGINT        NOT NULL,
    UNIQUE KEY ux_supplier_return_number (number),
    UNIQUE KEY ux_supplier_return_day_seq (branch_id, local_day, seq),
    KEY ix_supplier_return_branch (branch_id, status, id),
    CONSTRAINT fk_supplier_return_supplier FOREIGN KEY (supplier_id) REFERENCES supplier (id),
    CONSTRAINT ck_supplier_return_status CHECK (status IN ('draft', 'posted')),
    CONSTRAINT ck_supplier_return_tax_mode CHECK (tax_mode IN ('included', 'added', 'none')),
    CONSTRAINT ck_supplier_return_number CHECK ((status = 'draft') = (number IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE supplier_return_line (
    return_id       BINARY(16) NOT NULL,
    line_no         BIGINT     NOT NULL,
    variant_id      BINARY(16) NOT NULL,
    unit_id         BINARY(16) NOT NULL,
    factor          BIGINT     NOT NULL,
    qty             BIGINT     NOT NULL,
    price           BIGINT     NOT NULL,
    receipt_id      BINARY(16),
    receipt_line_no BIGINT,
    base_qty        BIGINT     NOT NULL,
    value           BIGINT,
    PRIMARY KEY (return_id, line_no),
    KEY ix_supplier_return_line_receipt (receipt_id, receipt_line_no),
    CONSTRAINT fk_supplier_return_line_return FOREIGN KEY (return_id)
        REFERENCES supplier_return (id),
    CONSTRAINT ck_supplier_return_line CHECK (factor > 0 AND qty > 0 AND price >= 0
        AND base_qty > 0),
    CONSTRAINT ck_supplier_return_line_receipt
        CHECK ((receipt_id IS NULL) = (receipt_line_no IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 14. Transfers between branches (§8).
CREATE TABLE stock_transfer (
    id               BINARY(16)    NOT NULL PRIMARY KEY,
    number           VARCHAR(64)   NOT NULL,
    branch_id        BINARY(16)    NOT NULL,
    from_branch_id   BINARY(16)    NOT NULL,
    from_location_id BINARY(16),
    to_branch_id     BINARY(16)    NOT NULL,
    to_location_id   BINARY(16),
    status           VARCHAR(16)   NOT NULL,
    note             VARCHAR(2000),
    local_day        BIGINT        NOT NULL,
    seq              BIGINT        NOT NULL,
    user_id          BINARY(16)    NOT NULL,
    approved_by      BINARY(16),
    requested_at     BIGINT,
    requested_by     BINARY(16),
    shipped_at       BIGINT,
    shipped_by       BINARY(16),
    received_at      BIGINT,
    received_by      BINARY(16),
    cancelled_at     BIGINT,
    cancelled_by     BINARY(16),
    created_at       BIGINT        NOT NULL,
    updated_at       BIGINT        NOT NULL,
    UNIQUE KEY ux_stock_transfer_number (number),
    UNIQUE KEY ux_stock_transfer_day_seq (branch_id, local_day, seq),
    KEY ix_stock_transfer_from (from_branch_id, status, id),
    KEY ix_stock_transfer_to (to_branch_id, status, id),
    CONSTRAINT ck_stock_transfer_status
        CHECK (status IN ('requested', 'shipped', 'received', 'cancelled')),
    CONSTRAINT ck_stock_transfer_branches CHECK (from_branch_id <> to_branch_id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE stock_transfer_line (
    transfer_id    BINARY(16) NOT NULL,
    line_no        BIGINT     NOT NULL,
    variant_id     BINARY(16) NOT NULL,
    unit_id        BINARY(16) NOT NULL,
    factor         BIGINT     NOT NULL,
    qty            BIGINT     NOT NULL,
    shipped_qty    BIGINT,
    shipped_value  BIGINT,
    received_qty   BIGINT,
    received_value BIGINT,
    lost_qty       BIGINT,
    lost_value     BIGINT,
    PRIMARY KEY (transfer_id, line_no),
    CONSTRAINT fk_stock_transfer_line_transfer FOREIGN KEY (transfer_id)
        REFERENCES stock_transfer (id),
    CONSTRAINT ck_stock_transfer_line CHECK (factor > 0 AND qty >= 0),
    CONSTRAINT ck_stock_transfer_line_shipped CHECK (shipped_qty IS NULL OR shipped_qty >= 0),
    CONSTRAINT ck_stock_transfer_line_received
        CHECK (received_qty IS NULL OR (received_qty >= 0 AND received_qty <= shipped_qty))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE stock_transfer_lot (
    transfer_id BINARY(16)   NOT NULL,
    line_no     BIGINT       NOT NULL,
    batch       VARCHAR(160) NOT NULL,
    expiry      BIGINT       NOT NULL,
    qty         BIGINT       NOT NULL,
    PRIMARY KEY (transfer_id, line_no, batch, expiry),
    CONSTRAINT fk_stock_transfer_lot_transfer FOREIGN KEY (transfer_id)
        REFERENCES stock_transfer (id),
    CONSTRAINT ck_stock_transfer_lot CHECK (qty > 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 15. Internal transfers, adjustments and opening stock (§7, §8).
CREATE TABLE internal_transfer (
    id               BINARY(16)    NOT NULL PRIMARY KEY,
    number           VARCHAR(64),
    branch_id        BINARY(16)    NOT NULL,
    from_location_id BINARY(16)    NOT NULL,
    to_location_id   BINARY(16)    NOT NULL,
    status           VARCHAR(16)   NOT NULL,
    note             VARCHAR(2000),
    local_day        BIGINT,
    seq              BIGINT,
    user_id          BINARY(16)    NOT NULL,
    approved_by      BINARY(16),
    posted_at        BIGINT,
    posted_by        BINARY(16),
    created_at       BIGINT        NOT NULL,
    updated_at       BIGINT        NOT NULL,
    UNIQUE KEY ux_internal_transfer_number (number),
    UNIQUE KEY ux_internal_transfer_day_seq (branch_id, local_day, seq),
    KEY ix_internal_transfer_branch (branch_id, status, id),
    CONSTRAINT ck_internal_transfer_status CHECK (status IN ('draft', 'posted')),
    CONSTRAINT ck_internal_transfer_locations CHECK (from_location_id <> to_location_id),
    CONSTRAINT ck_internal_transfer_number CHECK ((status = 'draft') = (number IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE internal_transfer_line (
    transfer_id BINARY(16) NOT NULL,
    line_no     BIGINT     NOT NULL,
    variant_id  BINARY(16) NOT NULL,
    unit_id     BINARY(16) NOT NULL,
    factor      BIGINT     NOT NULL,
    qty         BIGINT     NOT NULL,
    base_qty    BIGINT     NOT NULL,
    value       BIGINT,
    PRIMARY KEY (transfer_id, line_no),
    CONSTRAINT fk_internal_transfer_line_transfer FOREIGN KEY (transfer_id)
        REFERENCES internal_transfer (id),
    CONSTRAINT ck_internal_transfer_line CHECK (factor > 0 AND qty > 0 AND base_qty > 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE stock_adjustment (
    id          BINARY(16)    NOT NULL PRIMARY KEY,
    number      VARCHAR(64),
    branch_id   BINARY(16)    NOT NULL,
    location_id BINARY(16)    NOT NULL,
    status      VARCHAR(16)   NOT NULL,
    note        VARCHAR(2000),
    local_day   BIGINT,
    seq         BIGINT,
    user_id     BINARY(16)    NOT NULL,
    approved_by BINARY(16),
    posted_at   BIGINT,
    posted_by   BINARY(16),
    created_at  BIGINT        NOT NULL,
    updated_at  BIGINT        NOT NULL,
    UNIQUE KEY ux_stock_adjustment_number (number),
    UNIQUE KEY ux_stock_adjustment_day_seq (branch_id, local_day, seq),
    KEY ix_stock_adjustment_branch (branch_id, status, id),
    CONSTRAINT ck_stock_adjustment_status CHECK (status IN ('draft', 'posted')),
    CONSTRAINT ck_stock_adjustment_number CHECK ((status = 'draft') = (number IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE stock_adjustment_line (
    adjustment_id BINARY(16)    NOT NULL,
    line_no       BIGINT        NOT NULL,
    variant_id    BINARY(16)    NOT NULL,
    unit_id       BINARY(16)    NOT NULL,
    factor        BIGINT        NOT NULL,
    qty           BIGINT        NOT NULL,
    reason_id     BINARY(16)    NOT NULL,
    note          VARCHAR(2000),
    unit_cost     BIGINT,
    base_qty      BIGINT        NOT NULL,
    value         BIGINT,
    PRIMARY KEY (adjustment_id, line_no),
    KEY ix_stock_adjustment_line_reason (reason_id),
    CONSTRAINT fk_stock_adjustment_line_adjustment FOREIGN KEY (adjustment_id)
        REFERENCES stock_adjustment (id),
    CONSTRAINT fk_stock_adjustment_line_reason FOREIGN KEY (reason_id)
        REFERENCES stock_reason (id),
    CONSTRAINT ck_stock_adjustment_line CHECK (factor > 0 AND qty <> 0 AND base_qty <> 0),
    CONSTRAINT ck_stock_adjustment_line_cost CHECK (unit_cost IS NULL OR unit_cost >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE opening_stock (
    id          BINARY(16)    NOT NULL PRIMARY KEY,
    number      VARCHAR(64),
    branch_id   BINARY(16)    NOT NULL,
    location_id BINARY(16)    NOT NULL,
    status      VARCHAR(16)   NOT NULL,
    note        VARCHAR(2000),
    local_day   BIGINT,
    seq         BIGINT,
    user_id     BINARY(16)    NOT NULL,
    approved_by BINARY(16),
    posted_at   BIGINT,
    posted_by   BINARY(16),
    created_at  BIGINT        NOT NULL,
    updated_at  BIGINT        NOT NULL,
    UNIQUE KEY ux_opening_stock_number (number),
    UNIQUE KEY ux_opening_stock_day_seq (branch_id, local_day, seq),
    KEY ix_opening_stock_branch (branch_id, status, id),
    CONSTRAINT ck_opening_stock_status CHECK (status IN ('draft', 'posted')),
    CONSTRAINT ck_opening_stock_number CHECK ((status = 'draft') = (number IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE opening_stock_line (
    opening_id BINARY(16)   NOT NULL,
    line_no    BIGINT       NOT NULL,
    variant_id BINARY(16)   NOT NULL,
    unit_id    BINARY(16)   NOT NULL,
    factor     BIGINT       NOT NULL,
    qty        BIGINT       NOT NULL,
    unit_cost  BIGINT       NOT NULL,
    batch      VARCHAR(160),
    expiry     BIGINT,
    base_qty   BIGINT       NOT NULL,
    value      BIGINT,
    lot_id     BINARY(16),
    PRIMARY KEY (opening_id, line_no),
    CONSTRAINT fk_opening_stock_line_opening FOREIGN KEY (opening_id)
        REFERENCES opening_stock (id),
    CONSTRAINT ck_opening_stock_line CHECK (factor > 0 AND qty > 0 AND unit_cost >= 0
        AND base_qty > 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 16. Opname (§11).
CREATE TABLE opname (
    id            BINARY(16)    NOT NULL PRIMARY KEY,
    number        VARCHAR(64)   NOT NULL,
    branch_id     BINARY(16)    NOT NULL,
    location_id   BINARY(16)    NOT NULL,
    scope         VARCHAR(16)   NOT NULL,
    status        VARCHAR(16)   NOT NULL,
    snapshot_mark BINARY(16)    NOT NULL,
    uncounted     VARCHAR(16),
    note          VARCHAR(2000),
    local_day     BIGINT        NOT NULL,
    seq           BIGINT        NOT NULL,
    user_id       BINARY(16)    NOT NULL,
    approved_by   BINARY(16),
    started_at    BIGINT        NOT NULL,
    posted_at     BIGINT,
    posted_by     BINARY(16),
    cancelled_at  BIGINT,
    cancelled_by  BINARY(16),
    created_at    BIGINT        NOT NULL,
    updated_at    BIGINT        NOT NULL,
    UNIQUE KEY ux_opname_number (number),
    UNIQUE KEY ux_opname_day_seq (branch_id, local_day, seq),
    KEY ix_opname_branch (branch_id, status, id),
    CONSTRAINT ck_opname_scope CHECK (scope IN ('full', 'cycle')),
    CONSTRAINT ck_opname_status CHECK (status IN ('counting', 'posted', 'cancelled')),
    CONSTRAINT ck_opname_uncounted CHECK (uncounted IS NULL OR uncounted IN ('set_zero', 'leave'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE opname_scope (
    opname_id BINARY(16)  NOT NULL,
    kind      VARCHAR(16) NOT NULL,
    ref_id    BINARY(16)  NOT NULL,
    PRIMARY KEY (opname_id, kind, ref_id),
    CONSTRAINT fk_opname_scope_opname FOREIGN KEY (opname_id) REFERENCES opname (id),
    CONSTRAINT ck_opname_scope_kind CHECK (kind IN ('variant', 'category'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE opname_line (
    opname_id    BINARY(16) NOT NULL,
    variant_id   BINARY(16) NOT NULL,
    snapshot_qty BIGINT     NOT NULL,
    counted_qty  BIGINT,
    variance     BIGINT,
    value        BIGINT,
    PRIMARY KEY (opname_id, variant_id),
    CONSTRAINT fk_opname_line_opname FOREIGN KEY (opname_id) REFERENCES opname (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TABLE opname_count (
    id         BINARY(16) NOT NULL PRIMARY KEY,
    opname_id  BINARY(16) NOT NULL,
    variant_id BINARY(16) NOT NULL,
    unit_id    BINARY(16) NOT NULL,
    factor     BIGINT     NOT NULL,
    qty        BIGINT     NOT NULL,
    base_qty   BIGINT     NOT NULL,
    replaces   TINYINT(1) NOT NULL,
    user_id    BINARY(16) NOT NULL,
    counted_at BIGINT     NOT NULL,
    KEY ix_opname_count_line (opname_id, variant_id, id),
    CONSTRAINT fk_opname_count_opname FOREIGN KEY (opname_id) REFERENCES opname (id),
    CONSTRAINT ck_opname_count CHECK (factor > 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

CREATE TRIGGER opname_count_no_update BEFORE UPDATE ON opname_count FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'opname_count is append-only';

CREATE TRIGGER opname_count_no_delete BEFORE DELETE ON opname_count FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'opname_count is append-only';

CREATE TABLE opname_lock (
    location_id BINARY(16) NOT NULL,
    variant_id  BINARY(16) NOT NULL,
    opname_id   BINARY(16) NOT NULL,
    PRIMARY KEY (location_id, variant_id),
    KEY ix_opname_lock_opname (opname_id),
    CONSTRAINT fk_opname_lock_opname FOREIGN KEY (opname_id) REFERENCES opname (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- 17. Spec 2.8 §12: new grants for existing installs (see ../sqlite/).
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT r.id, p.perm, NULL
FROM role r
JOIN (SELECT 'supplier.manage' AS perm UNION ALL SELECT 'purchase.order'
      UNION ALL SELECT 'purchase.receive' UNION ALL SELECT 'stock.transfer'
      UNION ALL SELECT 'stock.count') p
WHERE r.id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission x WHERE x.role_id = r.id AND x.perm = p.perm);

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT r.id, p.perm, NULL
FROM role r
JOIN (SELECT 'purchase.receive' AS perm UNION ALL SELECT 'stock.transfer'
      UNION ALL SELECT 'stock.count') p
WHERE r.id = X'00000000000070008000000000000004'
  AND NOT EXISTS (SELECT 1 FROM role_permission x WHERE x.role_id = r.id AND x.perm = p.perm);

DELETE FROM role_permission
WHERE role_id = X'00000000000070008000000000000004' AND perm = 'stock.adjust';
