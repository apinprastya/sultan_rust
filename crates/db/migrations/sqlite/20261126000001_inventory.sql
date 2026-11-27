-- Spec 2.8 Inventory, phase N2 (SQLite). Keep in step with ../mysql/: only the DDL differs.
-- IDs: 16-byte UUIDv7 BLOBs. Money: INTEGER minor units. Quantities: INTEGER thousandths of a
-- base unit. Booleans: INTEGER 0/1. Times: INTEGER unix millis. Days: INTEGER YYYYMMDD in the
-- branch's time zone.
--
-- Suppliers, supplier items and stock reasons are HQ master data. Documents, balances, costs,
-- lots and reorder rules belong to the branch (spec 2.8 §1). Like sales, branch documents record
-- branches, locations, variants and users without foreign keys to those tables (a node receiving
-- synced documents may not hold every row); only the parts of one document reference each other.
-- Documents are numbered <BRANCH>-<PREFIX>-<YYMMDD>-<NN> per branch, prefix and branch-local day
-- (local_day, seq); UNIQUE (branch_id, local_day, seq) per document table catches a race (the
-- loser retries, 2.1 §5).

-- 1. The stock ledger (spec 2.8 §2). SQLite cannot change a CHECK, so the table is rebuilt with
--    the new kinds and columns: value (signed Rupiah, NULL = no cost, so every row from before
--    2.8), lot_id (the expiry lot, NULL = no lot) and doc_line (the document line). doc_kind
--    names the document table. A 2.6 row without a location goes to its branch's receiving
--    default, else to any live location of the branch, else to a location "Stok lama" created
--    for it (spec 2.8 §4). The append-only triggers come back with it.

INSERT INTO stock_location (id, branch_id, name, receiving_default, archived, created_at,
                            updated_at)
SELECT randomblob(16), b.id, 'Stok lama', 1, 0, 0, 0
FROM branch b
WHERE EXISTS (SELECT 1 FROM stock_movement m WHERE m.branch_id = b.id AND m.location_id IS NULL)
  AND NOT EXISTS (SELECT 1 FROM stock_location l WHERE l.branch_id = b.id AND l.archived = 0);

CREATE TABLE stock_movement_new (
    id          BLOB    NOT NULL PRIMARY KEY,
    branch_id   BLOB    NOT NULL,
    location_id BLOB,
    variant_id  BLOB    NOT NULL,
    kind        TEXT    NOT NULL,
    base_qty    INTEGER NOT NULL,
    value       INTEGER,
    lot_id      BLOB,
    doc_kind    TEXT    NOT NULL,
    doc_id      BLOB    NOT NULL,
    doc_line    INTEGER,
    occurred_at INTEGER NOT NULL,
    CHECK (kind IN ('sale', 'sale_void', 'return', 'write_off', 'receipt', 'receipt_void',
                    'supplier_return', 'transfer_out', 'transfer_in', 'transit_loss',
                    'internal_out', 'internal_in', 'adjust', 'count', 'opening')),
    CHECK (kind NOT IN ('sale', 'write_off', 'receipt_void', 'supplier_return', 'transfer_out',
                        'transit_loss', 'internal_out') OR base_qty < 0),
    CHECK (kind NOT IN ('sale_void', 'return', 'receipt', 'transfer_in', 'internal_in',
                        'opening') OR base_qty > 0),
    CHECK (base_qty <> 0),
    CHECK (doc_kind IN ('sale', 'return', 'goods_receipt', 'supplier_return', 'transfer',
                        'internal_transfer', 'adjustment', 'opname', 'opening_stock'))
);

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
ALTER TABLE stock_movement_new RENAME TO stock_movement;
CREATE INDEX ix_stock_movement_variant ON stock_movement (variant_id, branch_id, location_id);
CREATE INDEX ix_stock_movement_doc ON stock_movement (doc_id);
CREATE INDEX ix_stock_movement_place ON stock_movement (branch_id, location_id, variant_id, id);
CREATE INDEX ix_stock_movement_lot ON stock_movement (lot_id);

CREATE TRIGGER stock_movement_no_update BEFORE UPDATE ON stock_movement
BEGIN
    SELECT RAISE(ABORT, 'stock_movement is append-only');
END;

CREATE TRIGGER stock_movement_no_delete BEFORE DELETE ON stock_movement
BEGIN
    SELECT RAISE(ABORT, 'stock_movement is append-only');
END;

-- 2. Cached on hand per (branch, location, variant) = the sum of the ledger (§4), updated in the
--    posting transaction. A movement without a location (a branch with no location at all) is
--    kept under the all-zero location ID.
CREATE TABLE stock_balance (
    branch_id   BLOB    NOT NULL,
    location_id BLOB    NOT NULL,
    variant_id  BLOB    NOT NULL,
    qty         INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    PRIMARY KEY (branch_id, location_id, variant_id)
);
CREATE INDEX ix_stock_balance_variant ON stock_balance (variant_id, branch_id);

INSERT INTO stock_balance (branch_id, location_id, variant_id, qty, updated_at)
SELECT branch_id, COALESCE(location_id, X'00000000000000000000000000000000'), variant_id, SUM(base_qty), 0
FROM stock_movement
GROUP BY branch_id, COALESCE(location_id, X'00000000000000000000000000000000'), variant_id;

-- 3. The moving average per (branch, variant) (§5): qty over all locations, value, and the
--    reference cost ref_value / ref_qty (both NULL = no cost yet). Stock from before 2.8 has no
--    cost.
CREATE TABLE stock_cost (
    branch_id  BLOB    NOT NULL,
    variant_id BLOB    NOT NULL,
    qty        INTEGER NOT NULL,
    value      INTEGER NOT NULL,
    ref_value  INTEGER,
    ref_qty    INTEGER,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (branch_id, variant_id),
    CHECK ((ref_value IS NULL) = (ref_qty IS NULL)),
    CHECK (ref_qty IS NULL OR ref_qty > 0)
);

INSERT INTO stock_cost (branch_id, variant_id, qty, value, ref_value, ref_qty, updated_at)
SELECT branch_id, variant_id, SUM(base_qty), 0, NULL, NULL, 0
FROM stock_movement
GROUP BY branch_id, variant_id;

-- 4. Expiry lots (§9): quantity per (branch, location, variant, batch, expiry). "No lot" is not a
--    row: it is on hand minus the lots, and may go negative. batch '' = none typed.
CREATE TABLE stock_lot (
    id          BLOB    NOT NULL PRIMARY KEY,
    branch_id   BLOB    NOT NULL,
    location_id BLOB    NOT NULL,
    variant_id  BLOB    NOT NULL,
    batch       TEXT    NOT NULL,
    expiry      INTEGER NOT NULL,
    qty         INTEGER NOT NULL,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (branch_id, location_id, variant_id, batch, expiry)
);
CREATE INDEX ix_stock_lot_variant ON stock_lot (variant_id, location_id);
CREATE INDEX ix_stock_lot_expiry ON stock_lot (branch_id, expiry);

-- 5. Suppliers and their items (§3, HQ master data). Archived, never deleted. preferred_variant
--    = variant_id when preferred, else NULL: UNIQUE keeps one preferred supplier per variant.
--    last_price is per base unit, set by every posted receipt; last_unit_id is the receipt
--    line's unit (the purchase unit of suggestions, §10).
CREATE TABLE supplier (
    id          BLOB    NOT NULL PRIMARY KEY,
    code        TEXT    NOT NULL UNIQUE,
    name        TEXT    NOT NULL,
    address     TEXT,
    phone       TEXT,
    email       TEXT,
    tax_id      TEXT,
    pkp         INTEGER NOT NULL,
    terms_days  INTEGER NOT NULL,
    lead_days   INTEGER NOT NULL,
    notes       TEXT,
    active      INTEGER NOT NULL,
    archived_at INTEGER,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (terms_days BETWEEN 0 AND 365),
    CHECK (lead_days BETWEEN 0 AND 365)
);

CREATE TABLE supplier_item (
    supplier_id       BLOB    NOT NULL REFERENCES supplier (id),
    variant_id        BLOB    NOT NULL,
    supplier_code     TEXT,
    last_price        INTEGER,
    last_unit_id      BLOB,
    preferred         INTEGER NOT NULL,
    preferred_variant BLOB    UNIQUE,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    PRIMARY KEY (supplier_id, variant_id),
    UNIQUE (supplier_id, supplier_code),
    CHECK ((preferred = 1) = (preferred_variant IS NOT NULL)),
    CHECK (preferred_variant IS NULL OR preferred_variant = variant_id)
);
CREATE INDEX ix_supplier_item_variant ON supplier_item (variant_id);

-- 6. Stock reasons (§7, HQ master data), seeded with fixed IDs …0501–…0506
--    (core::stock::SEED_REASONS). The direction is fixed once an adjustment line used it.
CREATE TABLE stock_reason (
    id            BLOB    NOT NULL PRIMARY KEY,
    name          TEXT    NOT NULL,
    direction     TEXT    NOT NULL,
    note_required INTEGER NOT NULL,
    active        INTEGER NOT NULL,
    sort_order    INTEGER NOT NULL,
    archived_at   INTEGER,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    CHECK (direction IN ('in', 'out', 'both'))
);
CREATE INDEX ix_stock_reason_sort ON stock_reason (sort_order, id);

INSERT INTO stock_reason (id, name, direction, note_required, active, sort_order, archived_at,
                          created_at, updated_at) VALUES
    (X'00000000000070008000000000000501', 'Rusak', 'out', 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000502', 'Kedaluwarsa', 'out', 0, 1, 20, NULL, 0, 0),
    (X'00000000000070008000000000000503', 'Hilang', 'out', 0, 1, 30, NULL, 0, 0),
    (X'00000000000070008000000000000504', 'Sampel', 'out', 0, 1, 40, NULL, 0, 0),
    (X'00000000000070008000000000000505', 'Pemakaian sendiri', 'out', 0, 1, 50, NULL, 0, 0),
    (X'00000000000070008000000000000506', 'Koreksi', 'both', 1, 1, 90, NULL, 0, 0);

-- 7. The company's inventory settings (§13, HQ data, one row).
CREATE TABLE inventory_settings (
    id                   INTEGER NOT NULL PRIMARY KEY,
    pkp                  INTEGER NOT NULL,
    standard_ppn_rate_bp INTEGER NOT NULL,
    expiry_warn_days     INTEGER NOT NULL,
    velocity_days        INTEGER NOT NULL,
    cover_days           INTEGER NOT NULL,
    updated_at           INTEGER NOT NULL,
    CHECK (id = 1),
    CHECK (standard_ppn_rate_bp BETWEEN 0 AND 10000),
    CHECK (expiry_warn_days BETWEEN 0 AND 365),
    CHECK (velocity_days BETWEEN 1 AND 365),
    CHECK (cover_days BETWEEN 0 AND 365)
);

INSERT INTO inventory_settings (id, pkp, standard_ppn_rate_bp, expiry_warn_days, velocity_days,
                                cover_days, updated_at)
VALUES (1, 0, 1100, 30, 30, 14, 0);

-- 8. Product fields (§9): track_expiry, and expiry_warn_days (NULL = the company setting).
ALTER TABLE product ADD COLUMN track_expiry INTEGER NOT NULL DEFAULT 0;
ALTER TABLE product ADD COLUMN expiry_warn_days INTEGER
    CHECK (expiry_warn_days IS NULL OR expiry_warn_days BETWEEN 0 AND 365);

-- 9. Reorder rules per (branch, variant) (§10), base units.
CREATE TABLE reorder_rule (
    branch_id  BLOB    NOT NULL,
    variant_id BLOB    NOT NULL,
    min_qty    INTEGER NOT NULL,
    max_qty    INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (branch_id, variant_id),
    CHECK (min_qty >= 0 AND max_qty >= min_qty)
);

-- 10. Purchase orders (§6): draft → ordered → closed, or → cancelled. Numbered when ordered.
--     Lines keep the ordered and the received quantity in base units.
CREATE TABLE purchase_order (
    id            BLOB    NOT NULL PRIMARY KEY,
    number        TEXT    UNIQUE,
    branch_id     BLOB    NOT NULL,
    supplier_id   BLOB    NOT NULL REFERENCES supplier (id),
    location_id   BLOB    NOT NULL,
    status        TEXT    NOT NULL,
    order_date    INTEGER,
    expected_date INTEGER,
    note          TEXT,
    local_day     INTEGER,
    seq           INTEGER,
    user_id       BLOB    NOT NULL,
    approved_by   BLOB,
    ordered_at    INTEGER,
    ordered_by    BLOB,
    closed_at     INTEGER,
    closed_by     BLOB,
    cancelled_at  INTEGER,
    cancelled_by  BLOB,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    CHECK (status IN ('draft', 'ordered', 'closed', 'cancelled')),
    CHECK ((status = 'draft') = (number IS NULL) OR status = 'cancelled'),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_purchase_order_branch ON purchase_order (branch_id, status, id);
CREATE INDEX ix_purchase_order_supplier ON purchase_order (supplier_id, id);

CREATE TABLE purchase_order_line (
    order_id     BLOB    NOT NULL REFERENCES purchase_order (id),
    line_no      INTEGER NOT NULL,
    variant_id   BLOB    NOT NULL,
    unit_id      BLOB    NOT NULL,
    factor       INTEGER NOT NULL,
    qty          INTEGER NOT NULL,
    price        INTEGER NOT NULL,
    base_qty     INTEGER NOT NULL,
    received_qty INTEGER NOT NULL,
    PRIMARY KEY (order_id, line_no),
    CHECK (factor > 0 AND qty > 0 AND price >= 0 AND base_qty > 0 AND received_qty >= 0)
);
CREATE INDEX ix_purchase_order_line_variant ON purchase_order_line (variant_id);

-- 11. Goods receipts with the supplier's invoice (§6): draft → posted → voided. invoice_active =
--     invoice_no while posted, so UNIQUE (supplier_id, invoice_active) refuses a second posted
--     receipt with the same invoice number (NULLs do not collide). The totals and each line's
--     landed cost are stored when posted. lot_id: the lot a tracked line went into.
CREATE TABLE goods_receipt (
    id              BLOB    NOT NULL PRIMARY KEY,
    number          TEXT    UNIQUE,
    branch_id       BLOB    NOT NULL,
    supplier_id     BLOB    NOT NULL REFERENCES supplier (id),
    location_id     BLOB    NOT NULL,
    status          TEXT    NOT NULL,
    invoice_no      TEXT    NOT NULL,
    invoice_date    INTEGER NOT NULL,
    invoice_active  TEXT,
    payment         TEXT    NOT NULL,
    due_date        INTEGER,
    tax_mode        TEXT    NOT NULL,
    tax_rate_bp     INTEGER NOT NULL,
    header_discount INTEGER NOT NULL,
    other_costs     INTEGER NOT NULL,
    pkp             INTEGER,
    net             INTEGER,
    tax             INTEGER,
    total           INTEGER,
    landed          INTEGER,
    note            TEXT,
    local_day       INTEGER,
    seq             INTEGER,
    user_id         BLOB    NOT NULL,
    approved_by     BLOB,
    posted_at       INTEGER,
    posted_by       BLOB,
    voided_at       INTEGER,
    voided_by       BLOB,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    CHECK (status IN ('draft', 'posted', 'voided')),
    CHECK (payment IN ('credit', 'cash')),
    CHECK (tax_mode IN ('included', 'added', 'none')),
    CHECK (tax_rate_bp BETWEEN 0 AND 10000),
    CHECK (header_discount >= 0 AND other_costs >= 0),
    CHECK ((status = 'posted') = (invoice_active IS NOT NULL)),
    CHECK ((status = 'draft') = (number IS NULL)),
    UNIQUE (supplier_id, invoice_active),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_goods_receipt_branch ON goods_receipt (branch_id, status, id);
CREATE INDEX ix_goods_receipt_supplier ON goods_receipt (supplier_id, id);

CREATE TABLE goods_receipt_line (
    receipt_id      BLOB    NOT NULL REFERENCES goods_receipt (id),
    line_no         INTEGER NOT NULL,
    variant_id      BLOB    NOT NULL,
    unit_id         BLOB    NOT NULL,
    factor          INTEGER NOT NULL,
    qty             INTEGER NOT NULL,
    price           INTEGER NOT NULL,
    discount        INTEGER NOT NULL,
    order_id        BLOB,
    order_line_no   INTEGER,
    batch           TEXT,
    expiry          INTEGER,
    base_qty        INTEGER NOT NULL,
    header_discount INTEGER,
    tax             INTEGER,
    other_costs     INTEGER,
    landed          INTEGER,
    lot_id          BLOB,
    PRIMARY KEY (receipt_id, line_no),
    CHECK (factor > 0 AND qty > 0 AND price >= 0 AND discount >= 0 AND base_qty > 0),
    CHECK ((order_id IS NULL) = (order_line_no IS NULL))
);
CREATE INDEX ix_goods_receipt_line_variant ON goods_receipt_line (variant_id);
CREATE INDEX ix_goods_receipt_line_order ON goods_receipt_line (order_id, order_line_no);

-- 12. Payables (§6, I3): one per posted receipt; 2.9 records payments. status voided = the
--     receipt was voided.
CREATE TABLE payable (
    id          BLOB    NOT NULL PRIMARY KEY,
    branch_id   BLOB    NOT NULL,
    supplier_id BLOB    NOT NULL REFERENCES supplier (id),
    receipt_id  BLOB    NOT NULL UNIQUE REFERENCES goods_receipt (id),
    amount      INTEGER NOT NULL,
    due_date    INTEGER NOT NULL,
    paid        INTEGER NOT NULL,
    status      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    voided_at   INTEGER,
    CHECK (status IN ('open', 'voided')),
    CHECK (amount >= 0)
);
CREATE INDEX ix_payable_branch ON payable (branch_id, status, due_date);
CREATE INDEX ix_payable_supplier ON payable (supplier_id, status, due_date);

-- 13. Supplier returns (§6): draft → posted. credit = Σ qty × price in the tax mode, for 2.9.
CREATE TABLE supplier_return (
    id          BLOB    NOT NULL PRIMARY KEY,
    number      TEXT    UNIQUE,
    branch_id   BLOB    NOT NULL,
    supplier_id BLOB    NOT NULL REFERENCES supplier (id),
    location_id BLOB    NOT NULL,
    status      TEXT    NOT NULL,
    tax_mode    TEXT    NOT NULL,
    tax_rate_bp INTEGER NOT NULL,
    credit      INTEGER,
    note        TEXT,
    local_day   INTEGER,
    seq         INTEGER,
    user_id     BLOB    NOT NULL,
    approved_by BLOB,
    posted_at   INTEGER,
    posted_by   BLOB,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (status IN ('draft', 'posted')),
    CHECK (tax_mode IN ('included', 'added', 'none')),
    CHECK ((status = 'draft') = (number IS NULL)),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_supplier_return_branch ON supplier_return (branch_id, status, id);

CREATE TABLE supplier_return_line (
    return_id       BLOB    NOT NULL REFERENCES supplier_return (id),
    line_no         INTEGER NOT NULL,
    variant_id      BLOB    NOT NULL,
    unit_id         BLOB    NOT NULL,
    factor          INTEGER NOT NULL,
    qty             INTEGER NOT NULL,
    price           INTEGER NOT NULL,
    receipt_id      BLOB,
    receipt_line_no INTEGER,
    base_qty        INTEGER NOT NULL,
    value           INTEGER,
    PRIMARY KEY (return_id, line_no),
    CHECK (factor > 0 AND qty > 0 AND price >= 0 AND base_qty > 0),
    CHECK ((receipt_id IS NULL) = (receipt_line_no IS NULL))
);
CREATE INDEX ix_supplier_return_line_receipt ON supplier_return_line (receipt_id,
                                                                      receipt_line_no);

-- 14. Transfers between branches (§8): requested → shipped → received, or → cancelled; may start
--     shipped. Numbered by the branch that creates it (branch_id). Lines: the requested qty in
--     the line's unit; shipped / received / lost in base units with their values. The lots that
--     travel with a shipped line are stock_transfer_lot rows.
CREATE TABLE stock_transfer (
    id               BLOB    NOT NULL PRIMARY KEY,
    number           TEXT    NOT NULL UNIQUE,
    branch_id        BLOB    NOT NULL,
    from_branch_id   BLOB    NOT NULL,
    from_location_id BLOB,
    to_branch_id     BLOB    NOT NULL,
    to_location_id   BLOB,
    status           TEXT    NOT NULL,
    note             TEXT,
    local_day        INTEGER NOT NULL,
    seq              INTEGER NOT NULL,
    user_id          BLOB    NOT NULL,
    approved_by      BLOB,
    requested_at     INTEGER,
    requested_by     BLOB,
    shipped_at       INTEGER,
    shipped_by       BLOB,
    received_at      INTEGER,
    received_by      BLOB,
    cancelled_at     INTEGER,
    cancelled_by     BLOB,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    CHECK (status IN ('requested', 'shipped', 'received', 'cancelled')),
    CHECK (from_branch_id <> to_branch_id),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_stock_transfer_from ON stock_transfer (from_branch_id, status, id);
CREATE INDEX ix_stock_transfer_to ON stock_transfer (to_branch_id, status, id);

CREATE TABLE stock_transfer_line (
    transfer_id    BLOB    NOT NULL REFERENCES stock_transfer (id),
    line_no        INTEGER NOT NULL,
    variant_id     BLOB    NOT NULL,
    unit_id        BLOB    NOT NULL,
    factor         INTEGER NOT NULL,
    qty            INTEGER NOT NULL,
    shipped_qty    INTEGER,
    shipped_value  INTEGER,
    received_qty   INTEGER,
    received_value INTEGER,
    lost_qty       INTEGER,
    lost_value     INTEGER,
    PRIMARY KEY (transfer_id, line_no),
    CHECK (factor > 0 AND qty >= 0),
    CHECK (shipped_qty IS NULL OR shipped_qty >= 0),
    CHECK (received_qty IS NULL OR (received_qty >= 0 AND received_qty <= shipped_qty))
);

CREATE TABLE stock_transfer_lot (
    transfer_id BLOB    NOT NULL REFERENCES stock_transfer (id),
    line_no     INTEGER NOT NULL,
    batch       TEXT    NOT NULL,
    expiry      INTEGER NOT NULL,
    qty         INTEGER NOT NULL,
    PRIMARY KEY (transfer_id, line_no, batch, expiry),
    CHECK (qty > 0)
);

-- 15. Internal transfers (§8), adjustments and opening stock (§7): posted at once (a draft may be
--     kept). Lines in the line's unit with the base quantity and the movement value.
CREATE TABLE internal_transfer (
    id               BLOB    NOT NULL PRIMARY KEY,
    number           TEXT    UNIQUE,
    branch_id        BLOB    NOT NULL,
    from_location_id BLOB    NOT NULL,
    to_location_id   BLOB    NOT NULL,
    status           TEXT    NOT NULL,
    note             TEXT,
    local_day        INTEGER,
    seq              INTEGER,
    user_id          BLOB    NOT NULL,
    approved_by      BLOB,
    posted_at        INTEGER,
    posted_by        BLOB,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL,
    CHECK (status IN ('draft', 'posted')),
    CHECK (from_location_id <> to_location_id),
    CHECK ((status = 'draft') = (number IS NULL)),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_internal_transfer_branch ON internal_transfer (branch_id, status, id);

CREATE TABLE internal_transfer_line (
    transfer_id BLOB    NOT NULL REFERENCES internal_transfer (id),
    line_no     INTEGER NOT NULL,
    variant_id  BLOB    NOT NULL,
    unit_id     BLOB    NOT NULL,
    factor      INTEGER NOT NULL,
    qty         INTEGER NOT NULL,
    base_qty    INTEGER NOT NULL,
    value       INTEGER,
    PRIMARY KEY (transfer_id, line_no),
    CHECK (factor > 0 AND qty > 0 AND base_qty > 0)
);

CREATE TABLE stock_adjustment (
    id          BLOB    NOT NULL PRIMARY KEY,
    number      TEXT    UNIQUE,
    branch_id   BLOB    NOT NULL,
    location_id BLOB    NOT NULL,
    status      TEXT    NOT NULL,
    note        TEXT,
    local_day   INTEGER,
    seq         INTEGER,
    user_id     BLOB    NOT NULL,
    approved_by BLOB,
    posted_at   INTEGER,
    posted_by   BLOB,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (status IN ('draft', 'posted')),
    CHECK ((status = 'draft') = (number IS NULL)),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_stock_adjustment_branch ON stock_adjustment (branch_id, status, id);

CREATE TABLE stock_adjustment_line (
    adjustment_id BLOB    NOT NULL REFERENCES stock_adjustment (id),
    line_no       INTEGER NOT NULL,
    variant_id    BLOB    NOT NULL,
    unit_id       BLOB    NOT NULL,
    factor        INTEGER NOT NULL,
    qty           INTEGER NOT NULL,
    reason_id     BLOB    NOT NULL REFERENCES stock_reason (id),
    note          TEXT,
    unit_cost     INTEGER,
    base_qty      INTEGER NOT NULL,
    value         INTEGER,
    PRIMARY KEY (adjustment_id, line_no),
    CHECK (factor > 0 AND qty <> 0 AND base_qty <> 0),
    CHECK (unit_cost IS NULL OR unit_cost >= 0)
);
CREATE INDEX ix_stock_adjustment_line_reason ON stock_adjustment_line (reason_id);

CREATE TABLE opening_stock (
    id          BLOB    NOT NULL PRIMARY KEY,
    number      TEXT    UNIQUE,
    branch_id   BLOB    NOT NULL,
    location_id BLOB    NOT NULL,
    status      TEXT    NOT NULL,
    note        TEXT,
    local_day   INTEGER,
    seq         INTEGER,
    user_id     BLOB    NOT NULL,
    approved_by BLOB,
    posted_at   INTEGER,
    posted_by   BLOB,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (status IN ('draft', 'posted')),
    CHECK ((status = 'draft') = (number IS NULL)),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_opening_stock_branch ON opening_stock (branch_id, status, id);

CREATE TABLE opening_stock_line (
    opening_id BLOB    NOT NULL REFERENCES opening_stock (id),
    line_no    INTEGER NOT NULL,
    variant_id BLOB    NOT NULL,
    unit_id    BLOB    NOT NULL,
    factor     INTEGER NOT NULL,
    qty        INTEGER NOT NULL,
    unit_cost  INTEGER NOT NULL,
    batch      TEXT,
    expiry     INTEGER,
    base_qty   INTEGER NOT NULL,
    value      INTEGER,
    lot_id     BLOB,
    PRIMARY KEY (opening_id, line_no),
    CHECK (factor > 0 AND qty > 0 AND unit_cost >= 0 AND base_qty > 0)
);

-- 16. Opname (§11): counting → posted, or → cancelled. snapshot_mark is a UUIDv7 taken at the
--     start: movements with a larger ID came after the snapshot. The scope of a cycle count is
--     its variants and categories; lines hold the snapshot per variant and, once posted, the
--     counted quantity, the variance and its value. Counts are the entries of every counter
--     (a scan adds, a typed quantity replaces). opname_lock (location, variant) keeps one open
--     opname per location and variant (409 opname_open); posting or cancelling frees it.
CREATE TABLE opname (
    id            BLOB    NOT NULL PRIMARY KEY,
    number        TEXT    NOT NULL UNIQUE,
    branch_id     BLOB    NOT NULL,
    location_id   BLOB    NOT NULL,
    scope         TEXT    NOT NULL,
    status        TEXT    NOT NULL,
    snapshot_mark BLOB    NOT NULL,
    uncounted     TEXT,
    note          TEXT,
    local_day     INTEGER NOT NULL,
    seq           INTEGER NOT NULL,
    user_id       BLOB    NOT NULL,
    approved_by   BLOB,
    started_at    INTEGER NOT NULL,
    posted_at     INTEGER,
    posted_by     BLOB,
    cancelled_at  INTEGER,
    cancelled_by  BLOB,
    created_at    INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    CHECK (scope IN ('full', 'cycle')),
    CHECK (status IN ('counting', 'posted', 'cancelled')),
    CHECK (uncounted IS NULL OR uncounted IN ('set_zero', 'leave')),
    UNIQUE (branch_id, local_day, seq)
);
CREATE INDEX ix_opname_branch ON opname (branch_id, status, id);

CREATE TABLE opname_scope (
    opname_id BLOB NOT NULL REFERENCES opname (id),
    kind      TEXT NOT NULL,
    ref_id    BLOB NOT NULL,
    PRIMARY KEY (opname_id, kind, ref_id),
    CHECK (kind IN ('variant', 'category'))
);

CREATE TABLE opname_line (
    opname_id    BLOB    NOT NULL REFERENCES opname (id),
    variant_id   BLOB    NOT NULL,
    snapshot_qty INTEGER NOT NULL,
    counted_qty  INTEGER,
    variance     INTEGER,
    value        INTEGER,
    PRIMARY KEY (opname_id, variant_id)
);

CREATE TABLE opname_count (
    id         BLOB    NOT NULL PRIMARY KEY,
    opname_id  BLOB    NOT NULL REFERENCES opname (id),
    variant_id BLOB    NOT NULL,
    unit_id    BLOB    NOT NULL,
    factor     INTEGER NOT NULL,
    qty        INTEGER NOT NULL,
    base_qty   INTEGER NOT NULL,
    replaces   INTEGER NOT NULL,
    user_id    BLOB    NOT NULL,
    counted_at INTEGER NOT NULL,
    CHECK (factor > 0)
);
CREATE INDEX ix_opname_count_line ON opname_count (opname_id, variant_id, id);

CREATE TRIGGER opname_count_no_update BEFORE UPDATE ON opname_count
BEGIN
    SELECT RAISE(ABORT, 'opname_count is append-only');
END;

CREATE TRIGGER opname_count_no_delete BEFORE DELETE ON opname_count
BEGIN
    SELECT RAISE(ABORT, 'opname_count is append-only');
END;

CREATE TABLE opname_lock (
    location_id BLOB NOT NULL,
    variant_id  BLOB NOT NULL,
    opname_id   BLOB NOT NULL REFERENCES opname (id),
    PRIMARY KEY (location_id, variant_id)
);
CREATE INDEX ix_opname_lock_opname ON opname_lock (opname_id);

-- 17. Spec 2.8 §12: new grants for installs whose seeded roles already exist (a fresh database
--     has no roles yet; seed::seed_defaults creates them, and rewrites Owner (…0001) to the
--     whole catalogue on every start). Manager (…0002): supplier.manage, purchase.order,
--     purchase.receive, stock.transfer, stock.count. Stock clerk (…0004): purchase.receive,
--     stock.transfer, stock.count, and no more stock.adjust (2.8 gives it to Owner and Manager;
--     nothing used it before 2.8).
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
