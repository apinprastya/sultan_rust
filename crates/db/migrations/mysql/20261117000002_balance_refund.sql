-- Spec 2.6 §19 item 10 (phase S2): the customer balance ledger gets the kind `refund` (+), the
-- part of a return or post-void refunded to the balance. Keep in step with ../sqlite/ (which
-- rebuilds the table, since SQLite cannot change a CHECK constraint). ALTER TABLE fires no
-- trigger, so the append-only triggers stay as they are.

ALTER TABLE customer_balance_entry
    DROP CHECK ck_balance_entry_kind,
    DROP CHECK ck_balance_entry_plus;

ALTER TABLE customer_balance_entry
    ADD CONSTRAINT ck_balance_entry_kind CHECK (kind IN ('change_saved', 'payment',
        'change_saved_reversed', 'payment_reversed', 'payout', 'merge_out', 'merge_in', 'refund')),
    ADD CONSTRAINT ck_balance_entry_plus
        CHECK (kind NOT IN ('change_saved', 'payment_reversed', 'refund') OR amount > 0),
    ADD KEY ix_customer_balance_entry_document (document_id);
