-- Phase 9 (decided 2026-11-08, spec 2.1 §1, §5). Keep in step with ../mysql/.
--
-- 1. branch.timezone: the IANA zone document dates use (default Asia/Jakarta).
-- 2. cashier_session.number = <BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>, with the branch-local open
--    day (local_day = YYYYMMDD) and the per-register, per-day sequence (seq, from 1) it is built
--    from. UNIQUE (register_id, local_day, seq) keeps numbers unique when two opens race.
-- 3. The full close report, so a past report can be shown and reprinted exactly
--    (GET /cashier-sessions/{id}): the cash figures and the per-tender totals.
--
-- Backfill: every branch is Asia/Jakarta when this runs (the column is new), which is UTC+7 with
-- no DST since 1964, so the local day is computed with a fixed +7 h offset. Sequences follow
-- open time per register and local day. Closed sessions were closed while sales did not exist
-- (crates/server/src/movements.rs, NoSales), so their cash sales, refunds and cash in/out were 0
-- and they have no tender totals.
-- (No collation change is needed here: SQLite already compares TEXT byte-wise.)

ALTER TABLE branch ADD COLUMN timezone TEXT NOT NULL DEFAULT 'Asia/Jakarta';

ALTER TABLE cashier_session ADD COLUMN local_day INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cashier_session ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cashier_session ADD COLUMN number TEXT NOT NULL DEFAULT '';
ALTER TABLE cashier_session ADD COLUMN cash_sales INTEGER;
ALTER TABLE cashier_session ADD COLUMN cash_refunds INTEGER;
ALTER TABLE cashier_session ADD COLUMN cash_in INTEGER;
ALTER TABLE cashier_session ADD COLUMN cash_out INTEGER;

UPDATE cashier_session
SET local_day = CAST(strftime('%Y%m%d', (opened_at + 25200000) / 1000, 'unixepoch') AS INTEGER);

UPDATE cashier_session
SET seq = (SELECT x.rn
           FROM (SELECT id, ROW_NUMBER() OVER (PARTITION BY register_id, local_day
                                               ORDER BY opened_at, id) AS rn
                 FROM cashier_session) x
           WHERE x.id = cashier_session.id);

UPDATE cashier_session
SET number = (SELECT b.code || '-' || r.code || '-S-'
                     || substr(CAST(cashier_session.local_day AS TEXT), 3) || '-'
                     || printf('%02d', cashier_session.seq)
              FROM register r JOIN branch b ON b.id = r.branch_id
              WHERE r.id = cashier_session.register_id);

UPDATE cashier_session SET cash_sales = 0, cash_refunds = 0, cash_in = 0, cash_out = 0
WHERE closed_at IS NOT NULL;

CREATE UNIQUE INDEX ux_cashier_session_day_seq ON cashier_session (register_id, local_day, seq);
CREATE INDEX ix_cashier_session_opened_at ON cashier_session (opened_at);

CREATE TABLE cashier_session_tender (
    session_id BLOB    NOT NULL REFERENCES cashier_session (id),
    tender     TEXT    NOT NULL,
    sales      INTEGER NOT NULL,
    refunds    INTEGER NOT NULL,
    net        INTEGER NOT NULL,
    tx_count   INTEGER NOT NULL,
    PRIMARY KEY (session_id, tender)
);
