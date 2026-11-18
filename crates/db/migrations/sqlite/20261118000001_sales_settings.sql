-- Spec 2.6 §8, §9 (phase S6): the sales settings deferred from S3. Keep in step with ../mysql/.
--
-- sales_settings is one company row (HQ data, like pricing_settings): return_days is the return
-- window of a return by receipt (default 30, 0 = no limit). branch_sales_settings holds the
-- receipt width per branch (32, 42 or 48 columns); a branch without a row prints 32 columns.

CREATE TABLE sales_settings (
    id          INTEGER NOT NULL PRIMARY KEY,
    return_days INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    CHECK (id = 1),
    CHECK (return_days BETWEEN 0 AND 3650)
);

INSERT INTO sales_settings (id, return_days, updated_at) VALUES (1, 30, 0);

CREATE TABLE branch_sales_settings (
    branch_id     BLOB    NOT NULL PRIMARY KEY REFERENCES branch (id),
    receipt_width INTEGER NOT NULL,
    updated_at    INTEGER NOT NULL,
    CHECK (receipt_width IN (32, 42, 48))
);
