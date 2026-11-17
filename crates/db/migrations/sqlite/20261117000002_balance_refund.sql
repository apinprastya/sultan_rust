-- Spec 2.6 §19 item 10 (phase S2): the customer balance ledger gets the kind `refund` (+), the
-- part of a return or post-void refunded to the balance. Keep in step with ../mysql/.
--
-- SQLite cannot change a CHECK constraint, so the table is rebuilt: create the new table, copy
-- every entry, drop the old one (which also drops its append-only triggers; dropping fires no
-- trigger), rename, and create the index and the triggers again. Nothing references the table,
-- so the foreign keys stay intact. The copy is an INSERT, which the triggers allow.

CREATE TABLE customer_balance_entry_new (
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
                    'payout', 'merge_out', 'merge_in', 'refund')),
    CHECK (amount <> 0),
    CHECK (kind NOT IN ('change_saved', 'payment_reversed', 'refund') OR amount > 0),
    CHECK (kind NOT IN ('payment', 'change_saved_reversed') OR amount < 0)
);

INSERT INTO customer_balance_entry_new (id, customer_id, kind, amount, branch_id, register_id,
                                        user_id, cashier_session_id, document_id, occurred_at)
SELECT id, customer_id, kind, amount, branch_id, register_id, user_id, cashier_session_id,
       document_id, occurred_at
FROM customer_balance_entry;

DROP TABLE customer_balance_entry;

ALTER TABLE customer_balance_entry_new RENAME TO customer_balance_entry;

CREATE INDEX ix_customer_balance_entry_customer ON customer_balance_entry (customer_id, id);
CREATE INDEX ix_customer_balance_entry_document ON customer_balance_entry (document_id);

CREATE TRIGGER customer_balance_entry_no_update BEFORE UPDATE ON customer_balance_entry
BEGIN
    SELECT RAISE(ABORT, 'customer_balance_entry is append-only');
END;

CREATE TRIGGER customer_balance_entry_no_delete BEFORE DELETE ON customer_balance_entry
BEGIN
    SELECT RAISE(ABORT, 'customer_balance_entry is append-only');
END;
