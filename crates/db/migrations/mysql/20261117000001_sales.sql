-- Spec 2.6 Sales, phase S2 (MySQL 8.0.19+ / InnoDB). Keep in step with ../sqlite/: only the DDL
-- differs. IDs: BINARY(16) UUIDv7. Money: BIGINT minor units. Quantities: BIGINT thousandths.
-- Booleans: TINYINT(1). Times: BIGINT unix millis. JSON: JSON. Text compares byte-wise
-- (utf8mb4_0900_bin, see 20261108000002_binary_collation.sql).
--
-- Sales and returns are branch transactional data (spec 2.6 §11): written with their ledgers,
-- audit entry and outbox event in one transaction and pushed up to HQ. Like
-- customer_balance_entry, they record the branch, register, session, user, customer and
-- variants as they were where the document was made, without foreign keys to those tables: a
-- node receiving synced documents may not hold every row (and HQ receives them in any order).
-- Only the parts of one document reference each other. JSON columns (components, payload) are
-- JSON here and TEXT on SQLite.

-- Spec 2.6 D4, §1: tender types (HQ master data). kind is fixed; `exchange` is not a tender type
-- kind (§19 item 9). archived_at = deleted (soft delete); active = offered at the register.
CREATE TABLE tender_type (
    id              BINARY(16) NOT NULL PRIMARY KEY,
    name            VARCHAR(200) NOT NULL,
    kind            VARCHAR(32) NOT NULL,
    needs_reference TINYINT(1) NOT NULL,
    opens_drawer    TINYINT(1) NOT NULL,
    active          TINYINT(1) NOT NULL,
    sort_order      BIGINT NOT NULL,
    archived_at     BIGINT,
    created_at      BIGINT NOT NULL,
    updated_at      BIGINT NOT NULL,
    CHECK (kind IN ('cash', 'card', 'qris', 'transfer', 'ewallet', 'customer_balance', 'other')),
    CHECK (sort_order >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_tender_type_sort ON tender_type (sort_order, id);

-- Seeds (spec 2.6 §19, from S2): Cash so a fresh install can sell, the customer balance so saved
-- change can be spent (2.5 §11), and the two most common non-cash tenders in Indonesia. Fixed IDs
-- (…0301–…0304) so every node seeds the same rows. HQ renames, deactivates or archives them.
INSERT INTO tender_type (id, name, kind, needs_reference, opens_drawer, active, sort_order,
                         archived_at, created_at, updated_at) VALUES
    (X'00000000000070008000000000000301', 'Cash', 'cash', 0, 1, 1, 0, NULL, 0, 0),
    (X'00000000000070008000000000000302', 'Debit card', 'card', 1, 0, 1, 10, NULL, 0, 0),
    (X'00000000000070008000000000000303', 'QRIS', 'qris', 1, 0, 1, 20, NULL, 0, 0),
    (X'00000000000070008000000000000304', 'Customer balance', 'customer_balance', 0, 0, 1, 30,
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
    id                 BINARY(16) NOT NULL PRIMARY KEY,
    number             VARCHAR(64) NOT NULL UNIQUE,
    legacy_number      VARCHAR(64),
    status             VARCHAR(16) NOT NULL,
    branch_id          BINARY(16) NOT NULL,
    register_id        BINARY(16),
    cashier_session_id BINARY(16),
    user_id            BINARY(16),
    customer_id        BINARY(16),
    local_day          BIGINT,
    seq                BIGINT,
    tax_invoice        TINYINT(1) NOT NULL,
    tax_inclusive      TINYINT(1) NOT NULL,
    subtotal           BIGINT NOT NULL,
    line_discount      BIGINT NOT NULL,
    cart_discount      BIGINT NOT NULL,
    tax                BIGINT NOT NULL,
    total              BIGINT NOT NULL,
    cash_rounding      BIGINT NOT NULL,
    change_given       BIGINT NOT NULL,
    change_saved       BIGINT NOT NULL,
    line_count         BIGINT NOT NULL,
    return_count       BIGINT NOT NULL DEFAULT 0,
    posted_at          BIGINT NOT NULL,
    voided_at          BIGINT,
    request_hash       BINARY(32),
    created_at         BIGINT NOT NULL,
    CHECK (status IN ('posted', 'voided')),
    CHECK ((status = 'voided') = (voided_at IS NOT NULL)),
    CHECK ((local_day IS NULL) = (seq IS NULL)),
    CHECK (legacy_number IS NOT NULL OR seq IS NOT NULL),
    CHECK (total >= 0),
    CHECK (return_count >= 0)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
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
    sale_id             BINARY(16) NOT NULL,
    line_no             BIGINT NOT NULL,
    variant_id          BINARY(16) NOT NULL,
    unit_id             BINARY(16) NOT NULL,
    name                VARCHAR(200) NOT NULL,
    product_type        VARCHAR(16) NOT NULL,
    factor              BIGINT NOT NULL,
    components          JSON,
    qty                 BIGINT NOT NULL,
    unit_price          BIGINT NOT NULL,
    price_mode          VARCHAR(16) NOT NULL,
    resolved_price      BIGINT,
    price_list_id       BINARY(16),
    override_by         BINARY(16),
    scale_label         VARCHAR(16),
    label_amount        BIGINT,
    gross               BIGINT NOT NULL,
    line_discount       BIGINT NOT NULL,
    after_line          BIGINT NOT NULL,
    cart_discount_share BIGINT NOT NULL,
    amount              BIGINT NOT NULL,
    tax_rate_bp         BIGINT NOT NULL,
    tax                 BIGINT NOT NULL,
    net                 BIGINT NOT NULL,
    total               BIGINT NOT NULL,
    returned_qty        BIGINT NOT NULL DEFAULT 0,
    refunded            BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (sale_id, line_no),
    CHECK (line_no > 0),
    CHECK (product_type IN ('stock', 'weighed', 'service', 'bundle')),
    CHECK (price_mode IN ('listed', 'open_price', 'overridden')),
    CHECK (scale_label IS NULL OR scale_label IN ('weight', 'price')),
    CHECK ((COALESCE(scale_label, '') = 'price') = (label_amount IS NOT NULL)),
    CHECK (qty >= 0),
    CHECK (returned_qty BETWEEN 0 AND qty),
    CHECK (refunded BETWEEN 0 AND total),
    CHECK (tax_rate_bp BETWEEN 0 AND 65535),
    CONSTRAINT fk_sale_line_sale FOREIGN KEY (sale_id) REFERENCES sale (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_sale_line_variant ON sale_line (variant_id);

-- The tenders of a sale (§1, §5), in order. tender_type_id is NULL only for an exchange tender
-- (§19 item 9). cash_received is set on cash tenders.
CREATE TABLE sale_tender (
    sale_id        BINARY(16) NOT NULL,
    tender_no      BIGINT NOT NULL,
    tender_type_id BINARY(16),
    kind           VARCHAR(32) NOT NULL,
    amount         BIGINT NOT NULL,
    cash_received  BIGINT,
    change_given   BIGINT NOT NULL,
    change_saved   BIGINT NOT NULL,
    reference      VARCHAR(64),
    PRIMARY KEY (sale_id, tender_no),
    CHECK (kind IN ('cash', 'card', 'qris', 'transfer', 'ewallet', 'customer_balance', 'other',
                    'exchange')),
    CHECK ((kind = 'exchange') = (tender_type_id IS NULL)),
    CHECK (amount >= 0),
    CHECK (change_given >= 0 AND change_saved >= 0),
    CONSTRAINT fk_sale_tender_sale FOREIGN KEY (sale_id) REFERENCES sale (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- Spec 2.6 §7: the post-void of a sale. The voiding user's session (D10: the current one) pays
-- the money back, so its report shows it; the sale's own session is never touched.
CREATE TABLE sale_void (
    sale_id                BINARY(16) NOT NULL PRIMARY KEY,
    branch_id              BINARY(16) NOT NULL,
    register_id            BINARY(16) NOT NULL,
    cashier_session_id     BINARY(16) NOT NULL,
    user_id                BINARY(16) NOT NULL,
    reason                 VARCHAR(200) NOT NULL,
    reason_text            VARCHAR(200),
    refund                 BIGINT NOT NULL,
    change_saved_reversed  BIGINT NOT NULL,
    payment_reversed       BIGINT NOT NULL,
    to_balance             BIGINT NOT NULL,
    cash                   BIGINT NOT NULL,
    cash_rounding          BIGINT NOT NULL,
    voided_at              BIGINT NOT NULL,
    CHECK (reason IN ('customer_changed_mind', 'wrong_item', 'price_dispute', 'other')),
    CHECK ((reason = 'other') = (reason_text IS NOT NULL)),
    CONSTRAINT fk_sale_void_sale FOREIGN KEY (sale_id) REFERENCES sale (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_sale_void_session ON sale_void (cashier_session_id);

-- How a post-void paid back (D11): cash or customer balance.
CREATE TABLE sale_void_tender (
    sale_id        BINARY(16) NOT NULL,
    tender_no      BIGINT NOT NULL,
    tender_type_id BINARY(16),
    kind           VARCHAR(32) NOT NULL,
    amount         BIGINT NOT NULL,
    PRIMARY KEY (sale_id, tender_no),
    CHECK (kind IN ('cash', 'customer_balance')),
    CHECK (amount > 0),
    CONSTRAINT fk_sale_void_tender_void FOREIGN KEY (sale_id) REFERENCES sale_void (sale_id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- Spec 2.6 §1, §8: a return (by receipt: sale_id set; without receipt: NULL). `return` is
-- reserved in MySQL, hence sale_return. Numbers like sales: <BRANCH>-<REGISTER>-RT-<YYMMDD>-<NNNN>.
-- refund = Σ line refunds; its tenders (cash, balance, exchange) sum to refund + cash_rounding.
-- exchange_sale_id: the new sale of an exchange (D12), posted in the same transaction.
CREATE TABLE sale_return (
    id                 BINARY(16) NOT NULL PRIMARY KEY,
    number             VARCHAR(64) NOT NULL UNIQUE,
    status             VARCHAR(16) NOT NULL,
    sale_id            BINARY(16),
    branch_id          BINARY(16) NOT NULL,
    register_id        BINARY(16) NOT NULL,
    cashier_session_id BINARY(16) NOT NULL,
    user_id            BINARY(16) NOT NULL,
    customer_id        BINARY(16),
    local_day          BIGINT NOT NULL,
    seq                BIGINT NOT NULL,
    reason             VARCHAR(200),
    refund             BIGINT NOT NULL,
    cash_rounding      BIGINT NOT NULL,
    exchange_sale_id   BINARY(16),
    posted_at          BIGINT NOT NULL,
    request_hash       BINARY(32),
    created_at         BIGINT NOT NULL,
    CHECK (status IN ('posted', 'voided')),
    CHECK (refund >= 0),
    UNIQUE (register_id, local_day, seq),
    CONSTRAINT fk_sale_return_sale FOREIGN KEY (sale_id) REFERENCES sale (id),
    CONSTRAINT fk_sale_return_exchange FOREIGN KEY (exchange_sale_id) REFERENCES sale (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_sale_return_sale ON sale_return (sale_id);
CREATE INDEX ix_sale_return_branch ON sale_return (branch_id, id);
CREATE INDEX ix_sale_return_register ON sale_return (register_id, id);
CREATE INDEX ix_sale_return_session ON sale_return (cashier_session_id, id);
CREATE INDEX ix_sale_return_customer ON sale_return (customer_id, posted_at, id);
CREATE INDEX ix_sale_return_posted_at ON sale_return (posted_at, id);

-- One returned line (§8): from a sale line (sale_line_no) or scanned without a receipt.
-- disposition restock | write_off (D9), write_off with a reason.
CREATE TABLE sale_return_line (
    return_id    BINARY(16) NOT NULL,
    line_no      BIGINT NOT NULL,
    sale_line_no BIGINT,
    variant_id   BINARY(16) NOT NULL,
    unit_id      BINARY(16) NOT NULL,
    name         VARCHAR(200) NOT NULL,
    product_type VARCHAR(16) NOT NULL,
    factor       BIGINT NOT NULL,
    components   JSON,
    qty          BIGINT NOT NULL,
    refund       BIGINT NOT NULL,
    disposition  VARCHAR(16) NOT NULL,
    write_off_reason VARCHAR(200),
    PRIMARY KEY (return_id, line_no),
    CHECK (line_no > 0),
    CHECK (product_type IN ('stock', 'weighed', 'service', 'bundle')),
    CHECK (qty > 0),
    CHECK (refund >= 0),
    CHECK (disposition IN ('restock', 'write_off')),
    CHECK ((disposition = 'write_off') = (write_off_reason IS NOT NULL)),
    CONSTRAINT fk_sale_return_line_return FOREIGN KEY (return_id) REFERENCES sale_return (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- How a return paid back (D11, D12): cash, customer balance, or the exchange tender of the new
-- sale (tender_type_id NULL).
CREATE TABLE sale_return_tender (
    return_id      BINARY(16) NOT NULL,
    tender_no      BIGINT NOT NULL,
    tender_type_id BINARY(16),
    kind           VARCHAR(32) NOT NULL,
    amount         BIGINT NOT NULL,
    PRIMARY KEY (return_id, tender_no),
    CHECK (kind IN ('cash', 'customer_balance', 'exchange')),
    CHECK ((kind = 'exchange') = (tender_type_id IS NULL)),
    CHECK (amount > 0),
    CONSTRAINT fk_sale_return_tender_return FOREIGN KEY (return_id) REFERENCES sale_return (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

-- Spec 2.6 §4: held carts. Branch-local (not synced, no outbox), not documents, never dropped
-- automatically. payload is the cart as the register had it (JSON); item_count and total feed the
-- resume list. taken_by / taken_at: resumed (POST /held-carts/{id}/take); a guarded UPDATE lets
-- only one register take it (409 cart_taken).
CREATE TABLE held_cart (
    id                BINARY(16) NOT NULL PRIMARY KEY,
    branch_id         BINARY(16) NOT NULL,
    register_id       BINARY(16) NOT NULL,
    user_id           BINARY(16) NOT NULL,
    customer_id       BINARY(16),
    note              VARCHAR(200),
    payload           JSON NOT NULL,
    item_count        BIGINT NOT NULL,
    total             BIGINT NOT NULL,
    held_at           BIGINT NOT NULL,
    taken_by          BINARY(16),
    taken_register_id BINARY(16),
    taken_at          BIGINT,
    CHECK ((taken_by IS NULL) = (taken_at IS NULL))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_held_cart_branch ON held_cart (branch_id, taken_at, held_at);

-- Spec 2.6 D9: the stock ledger, append-only (triggers below, like audit_log). base_qty in base
-- units (thousandths), signed: sale −, sale_void +, return +, write_off −. location_id is the
-- register's default location, else the branch's receiving default; NULL when the branch has no
-- location (§19, from S2). Negative stock is allowed. On hand = Σ base_qty (no cache yet, 2.8).
CREATE TABLE stock_movement (
    id          BINARY(16) NOT NULL PRIMARY KEY,
    branch_id   BINARY(16) NOT NULL,
    location_id BINARY(16),
    variant_id  BINARY(16) NOT NULL,
    kind        VARCHAR(32) NOT NULL,
    base_qty    BIGINT NOT NULL,
    doc_kind    VARCHAR(16) NOT NULL,
    doc_id      BINARY(16) NOT NULL,
    occurred_at BIGINT NOT NULL,
    CHECK (kind IN ('sale', 'sale_void', 'return', 'write_off')),
    CHECK (kind NOT IN ('sale', 'write_off') OR base_qty < 0),
    CHECK (kind NOT IN ('sale_void', 'return') OR base_qty > 0),
    CHECK (doc_kind IN ('sale', 'return'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
CREATE INDEX ix_stock_movement_variant ON stock_movement (variant_id, branch_id, location_id);
CREATE INDEX ix_stock_movement_doc ON stock_movement (doc_id);

CREATE TRIGGER stock_movement_no_update BEFORE UPDATE ON stock_movement FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'stock_movement is append-only';

CREATE TRIGGER stock_movement_no_delete BEFORE DELETE ON stock_movement FOR EACH ROW
    SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'stock_movement is append-only';

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
