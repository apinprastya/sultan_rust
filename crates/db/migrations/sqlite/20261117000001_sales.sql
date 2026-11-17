-- Spec 2.6 Sales, phase S2 (SQLite). Keep in step with ../mysql/: only the DDL differs.
-- IDs: 16-byte UUIDv7 BLOBs. Money: INTEGER minor units. Quantities: INTEGER thousandths.
-- Booleans: INTEGER 0/1. Times: INTEGER unix millis. JSON: TEXT.
--
-- Sales and returns are branch transactional data (spec 2.6 §11): written with their ledgers,
-- audit entry and outbox event in one transaction and pushed up to HQ. Like
-- customer_balance_entry, they record the branch, register, session, user, customer and
-- variants as they were where the document was made, without foreign keys to those tables: a
-- node receiving synced documents may not hold every row (and HQ receives them in any order).
-- Only the parts of one document reference each other.

-- Spec 2.6 D4, §1: tender types (HQ master data). kind is fixed; `exchange` is not a tender type
-- kind (§19 item 9). archived_at = deleted (soft delete); active = offered at the register.
CREATE TABLE tender_type (
    id              BLOB    NOT NULL PRIMARY KEY,
    name            TEXT    NOT NULL,
    kind            TEXT    NOT NULL,
    needs_reference INTEGER NOT NULL,
    opens_drawer    INTEGER NOT NULL,
    active          INTEGER NOT NULL,
    sort_order      INTEGER NOT NULL,
    archived_at     INTEGER,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL,
    CHECK (kind IN ('cash', 'card', 'qris', 'transfer', 'ewallet', 'customer_balance', 'other')),
    CHECK (sort_order >= 0)
);
CREATE INDEX ix_tender_type_sort ON tender_type (sort_order, id);

-- Seeds (spec 2.6 §19, from S2): Cash so a fresh install can sell, the customer balance so saved
-- change can be spent (2.5 §11), and the two most common non-cash tenders in Indonesia. Fixed IDs
-- (…0301–…0304) so every node seeds the same rows. HQ renames, deactivates or archives them.
INSERT INTO tender_type (id, name, kind, needs_reference, opens_drawer, active, sort_order,
                         archived_at, created_at, updated_at) VALUES
    (X'00000000000070008000000000000301', 'Tunai', 'cash', 0, 1, 1, 0, NULL, 0, 0),
    (X'00000000000070008000000000000302', 'Kartu debit', 'card', 1, 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000303', 'QRIS', 'qris', 1, 0, 1, 20, NULL, 0, 0),
    (X'00000000000070008000000000000304', 'Saldo pelanggan', 'customer_balance', 0, 0, 1, 30,
     NULL, 0, 0);

-- Spec 2.6 §1: a sale is created posted (D1) and may later be voided (§7).
-- number = <BRANCH>-<REGISTER>-<YYMMDD>-<NNNN> from (local_day, seq): the branch-local posting
-- day and the per-register, per-day sequence. UNIQUE (register_id, local_day, seq) catches two
-- racing posts (the loser retries, 2.1 §5). An imported old-Sultan sale (§14) keeps its old number
-- in legacy_number (and as number) and has no local_day / seq; it is read-only.
-- return_count: posted returns of this sale. A return bumps it while the sale is posted and a
-- post-void needs it at 0, both as guarded UPDATEs, so a void and a return cannot both win.
-- request_hash: a hash of the posted body (computed by the server), so replaying the same
-- PUT /sales/{id} returns the stored sale and a different body is a conflict.
CREATE TABLE sale (
    id                 BLOB    NOT NULL PRIMARY KEY,
    number             TEXT    NOT NULL UNIQUE,
    legacy_number      TEXT,
    status             TEXT    NOT NULL,
    branch_id          BLOB    NOT NULL,
    register_id        BLOB,
    cashier_session_id BLOB,
    user_id            BLOB,
    customer_id        BLOB,
    local_day          INTEGER,
    seq                INTEGER,
    tax_invoice        INTEGER NOT NULL,
    tax_inclusive      INTEGER NOT NULL,
    subtotal           INTEGER NOT NULL,
    line_discount      INTEGER NOT NULL,
    cart_discount      INTEGER NOT NULL,
    tax                INTEGER NOT NULL,
    total              INTEGER NOT NULL,
    cash_rounding      INTEGER NOT NULL,
    change_given       INTEGER NOT NULL,
    change_saved       INTEGER NOT NULL,
    line_count         INTEGER NOT NULL,
    return_count       INTEGER NOT NULL DEFAULT 0,
    posted_at          INTEGER NOT NULL,
    voided_at          INTEGER,
    request_hash       BLOB,
    created_at         INTEGER NOT NULL,
    CHECK (status IN ('posted', 'voided')),
    CHECK ((status = 'voided') = (voided_at IS NOT NULL)),
    CHECK ((local_day IS NULL) = (seq IS NULL)),
    CHECK (legacy_number IS NOT NULL OR seq IS NOT NULL),
    CHECK (total >= 0),
    CHECK (return_count >= 0)
);
CREATE UNIQUE INDEX ux_sale_day_seq ON sale (register_id, local_day, seq);
CREATE INDEX ix_sale_branch ON sale (branch_id, id);
CREATE INDEX ix_sale_register ON sale (register_id, id);
CREATE INDEX ix_sale_session ON sale (cashier_session_id, id);
CREATE INDEX ix_sale_customer ON sale (customer_id, posted_at, id);
CREATE INDEX ix_sale_posted_at ON sale (posted_at, id);
CREATE INDEX ix_sale_legacy_number ON sale (legacy_number);

-- One line of a sale (§1, §6), numbered from 1. The values are those posted (LineTotals of
-- core::sale): gross, line discount, cart discount share, amount, tax and total. name is the
-- receipt name as sold. components: a bundle's component snapshot (JSON), for its stock.
-- returned_qty / refunded: what posted returns took from the line so far (§8), kept by the return
-- in the same transaction with a guarded UPDATE (never above qty / total).
CREATE TABLE sale_line (
    sale_id             BLOB    NOT NULL REFERENCES sale (id),
    line_no             INTEGER NOT NULL,
    variant_id          BLOB    NOT NULL,
    unit_id             BLOB    NOT NULL,
    name                TEXT    NOT NULL,
    product_type        TEXT    NOT NULL,
    factor              INTEGER NOT NULL,
    components          TEXT,
    qty                 INTEGER NOT NULL,
    unit_price          INTEGER NOT NULL,
    price_mode          TEXT    NOT NULL,
    resolved_price      INTEGER,
    price_list_id       BLOB,
    override_by         BLOB,
    scale_label         TEXT,
    label_amount        INTEGER,
    gross               INTEGER NOT NULL,
    line_discount       INTEGER NOT NULL,
    after_line          INTEGER NOT NULL,
    cart_discount_share INTEGER NOT NULL,
    amount              INTEGER NOT NULL,
    tax_rate_bp         INTEGER NOT NULL,
    tax                 INTEGER NOT NULL,
    net                 INTEGER NOT NULL,
    total               INTEGER NOT NULL,
    returned_qty        INTEGER NOT NULL DEFAULT 0,
    refunded            INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (sale_id, line_no),
    CHECK (line_no > 0),
    CHECK (product_type IN ('stock', 'weighed', 'service', 'bundle')),
    CHECK (price_mode IN ('listed', 'open_price', 'overridden')),
    CHECK (scale_label IS NULL OR scale_label IN ('weight', 'price')),
    CHECK ((COALESCE(scale_label, '') = 'price') = (label_amount IS NOT NULL)),
    CHECK (qty >= 0),
    CHECK (returned_qty BETWEEN 0 AND qty),
    CHECK (refunded BETWEEN 0 AND total),
    CHECK (tax_rate_bp BETWEEN 0 AND 65535)
);
CREATE INDEX ix_sale_line_variant ON sale_line (variant_id);

-- The tenders of a sale (§1, §5), in order. tender_type_id is NULL only for an exchange tender
-- (§19 item 9). cash_received is set on cash tenders.
CREATE TABLE sale_tender (
    sale_id        BLOB    NOT NULL REFERENCES sale (id),
    tender_no      INTEGER NOT NULL,
    tender_type_id BLOB,
    kind           TEXT    NOT NULL,
    amount         INTEGER NOT NULL,
    cash_received  INTEGER,
    change_given   INTEGER NOT NULL,
    change_saved   INTEGER NOT NULL,
    reference      TEXT,
    PRIMARY KEY (sale_id, tender_no),
    CHECK (kind IN ('cash', 'card', 'qris', 'transfer', 'ewallet', 'customer_balance', 'other',
                    'exchange')),
    CHECK ((kind = 'exchange') = (tender_type_id IS NULL)),
    CHECK (amount >= 0),
    CHECK (change_given >= 0 AND change_saved >= 0)
);

-- Spec 2.6 §7: the post-void of a sale. The voiding user's session (D10: the current one) pays
-- the money back, so its report shows it; the sale's own session is never touched.
CREATE TABLE sale_void (
    sale_id                BLOB    NOT NULL PRIMARY KEY REFERENCES sale (id),
    branch_id              BLOB    NOT NULL,
    register_id            BLOB    NOT NULL,
    cashier_session_id     BLOB    NOT NULL,
    user_id                BLOB    NOT NULL,
    reason                 TEXT    NOT NULL,
    reason_text            TEXT,
    refund                 INTEGER NOT NULL,
    change_saved_reversed  INTEGER NOT NULL,
    payment_reversed       INTEGER NOT NULL,
    to_balance             INTEGER NOT NULL,
    cash                   INTEGER NOT NULL,
    cash_rounding          INTEGER NOT NULL,
    voided_at              INTEGER NOT NULL,
    CHECK (reason IN ('customer_changed_mind', 'wrong_item', 'price_dispute', 'other')),
    CHECK ((reason = 'other') = (reason_text IS NOT NULL))
);
CREATE INDEX ix_sale_void_session ON sale_void (cashier_session_id);

-- How a post-void paid back (D11): cash or customer balance.
CREATE TABLE sale_void_tender (
    sale_id        BLOB    NOT NULL REFERENCES sale_void (sale_id),
    tender_no      INTEGER NOT NULL,
    tender_type_id BLOB,
    kind           TEXT    NOT NULL,
    amount         INTEGER NOT NULL,
    PRIMARY KEY (sale_id, tender_no),
    CHECK (kind IN ('cash', 'customer_balance')),
    CHECK (amount > 0)
);

-- Spec 2.6 §1, §8: a return (by receipt: sale_id set; without receipt: NULL). `return` is
-- reserved in MySQL, hence sale_return. Numbers like sales: <BRANCH>-<REGISTER>-RT-<YYMMDD>-<NNNN>.
-- refund = Σ line refunds; its tenders (cash, balance, exchange) sum to refund + cash_rounding.
-- exchange_sale_id: the new sale of an exchange (D12), posted in the same transaction.
CREATE TABLE sale_return (
    id                 BLOB    NOT NULL PRIMARY KEY,
    number             TEXT    NOT NULL UNIQUE,
    status             TEXT    NOT NULL,
    sale_id            BLOB REFERENCES sale (id),
    branch_id          BLOB    NOT NULL,
    register_id        BLOB    NOT NULL,
    cashier_session_id BLOB    NOT NULL,
    user_id            BLOB    NOT NULL,
    customer_id        BLOB,
    local_day          INTEGER NOT NULL,
    seq                INTEGER NOT NULL,
    reason             TEXT,
    refund             INTEGER NOT NULL,
    cash_rounding      INTEGER NOT NULL,
    exchange_sale_id   BLOB REFERENCES sale (id),
    posted_at          INTEGER NOT NULL,
    request_hash       BLOB,
    created_at         INTEGER NOT NULL,
    CHECK (status IN ('posted', 'voided')),
    CHECK (refund >= 0),
    UNIQUE (register_id, local_day, seq)
);
CREATE INDEX ix_sale_return_sale ON sale_return (sale_id);
CREATE INDEX ix_sale_return_branch ON sale_return (branch_id, id);
CREATE INDEX ix_sale_return_register ON sale_return (register_id, id);
CREATE INDEX ix_sale_return_session ON sale_return (cashier_session_id, id);
CREATE INDEX ix_sale_return_customer ON sale_return (customer_id, posted_at, id);
CREATE INDEX ix_sale_return_posted_at ON sale_return (posted_at, id);

-- One returned line (§8): from a sale line (sale_line_no) or scanned without a receipt.
-- disposition restock | write_off (D9), write_off with a reason.
CREATE TABLE sale_return_line (
    return_id    BLOB    NOT NULL REFERENCES sale_return (id),
    line_no      INTEGER NOT NULL,
    sale_line_no INTEGER,
    variant_id   BLOB    NOT NULL,
    unit_id      BLOB    NOT NULL,
    name         TEXT    NOT NULL,
    product_type TEXT    NOT NULL,
    factor       INTEGER NOT NULL,
    components   TEXT,
    qty          INTEGER NOT NULL,
    refund       INTEGER NOT NULL,
    disposition  TEXT    NOT NULL,
    write_off_reason TEXT,
    PRIMARY KEY (return_id, line_no),
    CHECK (line_no > 0),
    CHECK (product_type IN ('stock', 'weighed', 'service', 'bundle')),
    CHECK (qty > 0),
    CHECK (refund >= 0),
    CHECK (disposition IN ('restock', 'write_off')),
    CHECK ((disposition = 'write_off') = (write_off_reason IS NOT NULL))
);

-- How a return paid back (D11, D12): cash, customer balance, or the exchange tender of the new
-- sale (tender_type_id NULL).
CREATE TABLE sale_return_tender (
    return_id      BLOB    NOT NULL REFERENCES sale_return (id),
    tender_no      INTEGER NOT NULL,
    tender_type_id BLOB,
    kind           TEXT    NOT NULL,
    amount         INTEGER NOT NULL,
    PRIMARY KEY (return_id, tender_no),
    CHECK (kind IN ('cash', 'customer_balance', 'exchange')),
    CHECK ((kind = 'exchange') = (tender_type_id IS NULL)),
    CHECK (amount > 0)
);

-- Spec 2.6 §4: held carts. Branch-local (not synced, no outbox), not documents, never dropped
-- automatically. payload is the cart as the register had it (JSON); item_count and total feed the
-- resume list. taken_by / taken_at: resumed (POST /held-carts/{id}/take); a guarded UPDATE lets
-- only one register take it (409 cart_taken).
CREATE TABLE held_cart (
    id                BLOB    NOT NULL PRIMARY KEY,
    branch_id         BLOB    NOT NULL,
    register_id       BLOB    NOT NULL,
    user_id           BLOB    NOT NULL,
    customer_id       BLOB,
    note              TEXT,
    payload           TEXT    NOT NULL,
    item_count        INTEGER NOT NULL,
    total             INTEGER NOT NULL,
    held_at           INTEGER NOT NULL,
    taken_by          BLOB,
    taken_register_id BLOB,
    taken_at          INTEGER,
    CHECK ((taken_by IS NULL) = (taken_at IS NULL))
);
CREATE INDEX ix_held_cart_branch ON held_cart (branch_id, taken_at, held_at);

-- Spec 2.6 D9: the stock ledger, append-only (triggers below, like audit_log). base_qty in base
-- units (thousandths), signed: sale −, sale_void +, return +, write_off −. location_id is the
-- register's default location, else the branch's receiving default; NULL when the branch has no
-- location (§19, from S2). Negative stock is allowed. On hand = Σ base_qty (no cache yet, 2.8).
CREATE TABLE stock_movement (
    id          BLOB    NOT NULL PRIMARY KEY,
    branch_id   BLOB    NOT NULL,
    location_id BLOB,
    variant_id  BLOB    NOT NULL,
    kind        TEXT    NOT NULL,
    base_qty    INTEGER NOT NULL,
    doc_kind    TEXT    NOT NULL,
    doc_id      BLOB    NOT NULL,
    occurred_at INTEGER NOT NULL,
    CHECK (kind IN ('sale', 'sale_void', 'return', 'write_off')),
    CHECK (kind NOT IN ('sale', 'write_off') OR base_qty < 0),
    CHECK (kind NOT IN ('sale_void', 'return') OR base_qty > 0),
    CHECK (doc_kind IN ('sale', 'return'))
);
CREATE INDEX ix_stock_movement_variant ON stock_movement (variant_id, branch_id, location_id);
CREATE INDEX ix_stock_movement_doc ON stock_movement (doc_id);

CREATE TRIGGER stock_movement_no_update BEFORE UPDATE ON stock_movement
BEGIN
    SELECT RAISE(ABORT, 'stock_movement is append-only');
END;

CREATE TRIGGER stock_movement_no_delete BEFORE DELETE ON stock_movement
BEGIN
    SELECT RAISE(ABORT, 'stock_movement is append-only');
END;

-- Spec 2.6 §11: new permissions for installs whose seeded roles already exist (a fresh database
-- has no roles yet; seed::seed_defaults creates them with these grants). sale.reprint: Owner
-- (…0001) and Manager (…0002); tender.manage: Owner.
INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'sale.reprint', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'sale.reprint');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'sale.reprint', NULL FROM role
WHERE id = X'00000000000070008000000000000002'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000002' AND perm = 'sale.reprint');

INSERT INTO role_permission (role_id, perm, limit_value)
SELECT id, 'tender.manage', NULL FROM role
WHERE id = X'00000000000070008000000000000001'
  AND NOT EXISTS (SELECT 1 FROM role_permission WHERE role_id = X'00000000000070008000000000000001' AND perm = 'tender.manage');
