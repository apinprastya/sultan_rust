//! The stock movement ledger (spec 2.6 D9): append-only rows in base units, written by sales,
//! post-voids and returns in their own transaction. The movements come from `core::sale`
//! (`sale_movements`, `void_movements`, `return_movements`); this module only stores and sums
//! them. Negative stock is allowed and there is no costing yet (2.8).

use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, LocationId, RegisterId, VariantId};
use sultan_core::sale::{StockMovement, StockMovementKind};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};

/// A stored stock movement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StockMovementRecord {
    /// `UUIDv7`.
    pub id: Uuid,
    /// The branch whose stock moved.
    pub branch: BranchId,
    /// The location, when the branch has one (spec 2.6 §19, from S2).
    pub location: Option<LocationId>,
    /// Variant, kind and signed base quantity.
    pub movement: StockMovement,
    /// `sale` (a sale or its post-void) or `return`.
    pub doc_kind: String,
    /// The sale or return.
    pub doc_id: Uuid,
    /// When it happened.
    pub occurred_at: Timestamp,
}

fn parse_kind(key: &str) -> Result<StockMovementKind> {
    [
        StockMovementKind::Sale,
        StockMovementKind::SaleVoid,
        StockMovementKind::Return,
        StockMovementKind::WriteOff,
    ]
    .into_iter()
    .find(|k| k.as_str() == key)
    .ok_or_else(|| DbError::Corrupt(format!("unknown stock movement kind `{key}`")))
}

fn map(row: &dyn Row) -> Result<StockMovementRecord> {
    Ok(StockMovementRecord {
        id: sql::id(row, "id")?,
        branch: sql::id(row, "branch_id")?,
        location: sql::opt_id(row, "location_id")?,
        movement: StockMovement {
            variant: sql::id(row, "variant_id")?,
            kind: parse_kind(&row.text("kind")?)?,
            base_qty: sql::qty(row, "base_qty")?,
        },
        doc_kind: row.text("doc_kind")?,
        doc_id: sql::id(row, "doc_id")?,
        occurred_at: sql::ts(row, "occurred_at")?,
    })
}

/// The stock location a register sells from: its default location, else the branch's
/// receiving default, else any non-archived location of the branch; `None` without one.
pub(super) async fn register_location(
    tx: &mut Tx,
    register: RegisterId,
) -> Result<Option<LocationId>> {
    let own = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT r.default_location_id FROM register r \
         JOIN stock_location l ON l.id = r.default_location_id \
         WHERE r.id = ? AND l.archived = 0",
        args![register],
        |row| sql::id::<LocationId>(row, "default_location_id"),
    )
    .await?;
    if own.is_some() {
        return Ok(own);
    }
    sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT l.id FROM stock_location l JOIN register r ON r.branch_id = l.branch_id \
         WHERE r.id = ? AND l.archived = 0 ORDER BY l.receiving_default DESC, l.id LIMIT 1",
        args![register],
        |row| sql::id(row, "id"),
    )
    .await
}

/// Appends `movements` of document `doc_id` at `branch` / `location`.
pub(super) async fn insert(
    tx: &mut Tx,
    branch: BranchId,
    location: Option<LocationId>,
    doc_kind: &str,
    doc_id: Uuid,
    movements: &[StockMovement],
    at: Timestamp,
) -> Result<()> {
    for m in movements {
        sql::execute(
            Conn::Tx(&mut *tx),
            "INSERT INTO stock_movement (id, branch_id, location_id, variant_id, kind, base_qty, \
             doc_kind, doc_id, occurred_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            args![
                Uuid::now_v7(),
                branch,
                location,
                m.variant,
                m.kind.as_str(),
                m.base_qty,
                doc_kind,
                doc_id,
                at,
            ],
        )
        .await?;
    }
    Ok(())
}

/// The movements of a sale (with its post-void) or a return, in the order they were written.
///
/// # Errors
/// The database error.
pub async fn document_movements<'c>(
    conn: impl Into<Conn<'c>>,
    doc_id: Uuid,
) -> Result<Vec<StockMovementRecord>> {
    sql::fetch_all(
        conn.into(),
        "SELECT id, branch_id, location_id, variant_id, kind, base_qty, doc_kind, doc_id, \
         occurred_at FROM stock_movement WHERE doc_id = ? ORDER BY id",
        args![doc_id],
        map,
    )
    .await
}

/// The stock on hand of `variant` at `branch` (all its locations): the sum of the ledger, in base
/// units. May be negative (D9).
///
/// # Errors
/// The database error.
pub async fn stock_on_hand<'c>(
    conn: impl Into<Conn<'c>>,
    branch: BranchId,
    variant: VariantId,
) -> Result<Qty> {
    sql::fetch_optional(
        conn.into(),
        "SELECT CAST(COALESCE(SUM(base_qty), 0) AS SIGNED) AS total FROM stock_movement \
         WHERE variant_id = ? AND branch_id = ?",
        args![variant, branch],
        |row| sql::qty(row, "total"),
    )
    .await
    .map(Option::unwrap_or_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_parse() {
        for kind in [
            StockMovementKind::Sale,
            StockMovementKind::SaleVoid,
            StockMovementKind::Return,
            StockMovementKind::WriteOff,
        ] {
            assert_eq!(parse_kind(kind.as_str()).unwrap(), kind);
        }
        assert!(parse_kind("transfer").is_err());
    }
}
