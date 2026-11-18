-- Spec 2.6 §8, §9 (phase S6): the sales settings deferred from S3 (see ../sqlite/).

CREATE TABLE sales_settings (
    id          BIGINT NOT NULL PRIMARY KEY,
    return_days BIGINT NOT NULL,
    updated_at  BIGINT NOT NULL,
    CONSTRAINT ck_sales_settings_single CHECK (id = 1),
    CONSTRAINT ck_sales_settings_return_days CHECK (return_days BETWEEN 0 AND 3650)
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;

INSERT INTO sales_settings (id, return_days, updated_at) VALUES (1, 30, 0);

CREATE TABLE branch_sales_settings (
    branch_id     BINARY(16) NOT NULL PRIMARY KEY,
    receipt_width BIGINT     NOT NULL,
    updated_at    BIGINT     NOT NULL,
    CONSTRAINT fk_branch_sales_settings_branch FOREIGN KEY (branch_id) REFERENCES branch (id),
    CONSTRAINT ck_branch_sales_settings_width CHECK (receipt_width IN (32, 42, 48))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
