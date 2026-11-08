-- Phase 9: compare and sort text byte-wise, like SQLite's default BINARY collation.
--
-- The tables were created with utf8mb4_0900_ai_ci, which ignores case and accents: on MySQL a
-- username `Ani` collided with `ani` (and `josé` with `jose`), branch code `jkt01` with `JKT01`,
-- an Idempotency-Key `KEY-A` with `key-a` (answered `422 idempotency_key_reused`), the audit filter
-- `action=user.save` also matched `USER.SAVE`, and `ORDER BY name` sorted differently from SQLite.
-- utf8mb4_0900_bin is byte-wise and NO PAD (trailing spaces count), exactly SQLite's behaviour.
-- Only character columns change; BINARY ids, BIGINT and JSON columns are untouched.

ALTER TABLE company               CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE branch                CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE stock_location        CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE register              CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE register_pairing_code CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE app_user              CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE role                  CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE role_permission       CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE user_role             CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE device_token          CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE auth_session          CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE login_attempt         CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE override_token        CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE cashier_session       CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE cashier_session_count CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE audit_log             CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE outbox                CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
ALTER TABLE idempotency_key       CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin;
