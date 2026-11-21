-- Spec 2.7 §17 reading 10, decided 2026-11-21 (option a): see ../sqlite/.

ALTER TABLE sales_settings ADD COLUMN cash_since BIGINT NULL;

UPDATE sales_settings SET cash_since = UNIX_TIMESTAMP() * 1000 WHERE id = 1;
