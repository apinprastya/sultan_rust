-- Phase 9 (decided 2026-11-08, spec 2.1 §1, §5). Keep in step with ../sqlite/.
--
-- 1. branch.timezone: the IANA zone document dates use (default Asia/Jakarta).
-- 2. cashier_session.number = <BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>, with the branch-local open
--    day (local_day = YYYYMMDD) and the per-register, per-day sequence (seq, from 1) it is built
--    from. UNIQUE (register_id, local_day, seq) keeps numbers unique when two opens race.
-- 3. The full close report, so a past report can be shown and reprinted exactly
--    (GET /cashier-sessions/{id}): the cash figures and the per-tender totals.
--
-- Backfill: every branch is Asia/Jakarta when this runs (the column is new), which is UTC+7 with
-- no DST since 1964, so the local day is computed with a fixed +7 h offset, independent of the
-- server's time_zone. Sequences follow open time per register and local day. Closed sessions
-- were closed while sales did not exist (crates/server/src/movements.rs, NoSales), so their cash
-- sales, refunds and cash in/out were 0 and they have no tender totals.

ALTER TABLE branch ADD COLUMN timezone VARCHAR(64) NOT NULL DEFAULT 'Asia/Jakarta';

ALTER TABLE cashier_session
    ADD COLUMN local_day    BIGINT,
    ADD COLUMN seq          BIGINT,
    ADD COLUMN number       VARCHAR(64),
    ADD COLUMN cash_sales   BIGINT,
    ADD COLUMN cash_refunds BIGINT,
    ADD COLUMN cash_in      BIGINT,
    ADD COLUMN cash_out     BIGINT;

UPDATE cashier_session
SET local_day = CAST(DATE_FORMAT(
        DATE_ADD('1970-01-01', INTERVAL FLOOR((opened_at + 25200000) / 86400000) DAY),
        '%Y%m%d') AS UNSIGNED);

UPDATE cashier_session s
JOIN (SELECT id, ROW_NUMBER() OVER (PARTITION BY register_id, local_day
                                    ORDER BY opened_at, id) AS rn
      FROM cashier_session) x ON x.id = s.id
SET s.seq = x.rn;

UPDATE cashier_session s
JOIN register r ON r.id = s.register_id
JOIN branch b ON b.id = r.branch_id
SET s.number = CONCAT(b.code, '-', r.code, '-S-', SUBSTRING(CAST(s.local_day AS CHAR), 3), '-',
                      IF(s.seq < 10, CONCAT('0', s.seq), CAST(s.seq AS CHAR)));

UPDATE cashier_session SET cash_sales = 0, cash_refunds = 0, cash_in = 0, cash_out = 0
WHERE closed_at IS NOT NULL;

ALTER TABLE cashier_session
    MODIFY local_day BIGINT      NOT NULL,
    MODIFY seq       BIGINT      NOT NULL,
    MODIFY number    VARCHAR(64) NOT NULL,
    ADD UNIQUE KEY ux_cashier_session_day_seq (register_id, local_day, seq),
    ADD KEY ix_cashier_session_opened_at (opened_at);

CREATE TABLE cashier_session_tender (
    session_id BINARY(16)  NOT NULL,
    tender     VARCHAR(16) NOT NULL,
    sales      BIGINT      NOT NULL,
    refunds    BIGINT      NOT NULL,
    net        BIGINT      NOT NULL,
    tx_count   BIGINT      NOT NULL,
    PRIMARY KEY (session_id, tender),
    CONSTRAINT fk_session_tender_session FOREIGN KEY (session_id) REFERENCES cashier_session (id)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
