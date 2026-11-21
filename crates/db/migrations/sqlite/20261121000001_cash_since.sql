-- Spec 2.7 §17 reading 10, decided 2026-11-21 (option a): the back office lists days still to
-- close (K18) only from the day cash management was installed on. `cash_since` (unix ms) is the
-- time this migration ran: the upgrade of an existing install, or the first migration run of a
-- fresh one. `NULL` lists every day. `GET /z-reports/pending` leaves out business days before the
-- branch-local day of it; a Z of an earlier day can still be run by naming the day (§6).

ALTER TABLE sales_settings ADD COLUMN cash_since INTEGER;

UPDATE sales_settings SET cash_since = CAST(strftime('%s', 'now') AS INTEGER) * 1000 WHERE id = 1;
