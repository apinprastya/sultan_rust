-- Spec 2.7 §4, §17 reading 5: the close report is stored and never changes, but a cash reason's
-- name and sort order may (PUT /cash-reasons/{id}). The close stores the reasons its report shows
-- (name, direction, sort as at the close), and a stored report or a later Z reads them from here
-- instead of from cash_reason. Written once, in the close transaction; no foreign keys (2.7 §17
-- reading 3). A session closed before this migration has no rows and shows the live names.
CREATE TABLE cashier_session_reason (
    cashier_session_id BLOB    NOT NULL,
    reason_id          BLOB    NOT NULL,
    name               TEXT    NOT NULL,
    direction          TEXT    NOT NULL,
    sort_order         INTEGER NOT NULL,
    PRIMARY KEY (cashier_session_id, reason_id),
    CHECK (direction IN ('in', 'out'))
);
