-- Spec 2.7 §4, §17 reading 5: the cash reasons of a close report as at the close (see ../sqlite/).
CREATE TABLE cashier_session_reason (
    cashier_session_id BINARY(16)   NOT NULL,
    reason_id          BINARY(16)   NOT NULL,
    name               VARCHAR(200) NOT NULL,
    direction          VARCHAR(8)   NOT NULL,
    sort_order         BIGINT       NOT NULL,
    PRIMARY KEY (cashier_session_id, reason_id),
    CHECK (direction IN ('in', 'out'))
) ENGINE = InnoDB DEFAULT CHARSET = utf8mb4 COLLATE = utf8mb4_0900_bin;
