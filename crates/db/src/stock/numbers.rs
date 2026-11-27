//! Inventory document numbers (spec 2.8 I19): `<BRANCH>-<PREFIX>-<YYMMDD>-<NN>` per branch,
//! prefix and branch-local day, allocated in the posting transaction.

use sultan_core::doc_number::{BranchTimeZone, LocalDay, stock_number};
use sultan_core::ids::BranchId;
use sultan_core::stock::DocKind;
use sultan_core::time::Timestamp;

use super::{StockDbError, StockResult};
use crate::conn::{Conn, Tx};
use crate::error::DbError;
use crate::sql::{self, args};

/// A document number with the day and sequence it is built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Numbered {
    pub number: String,
    pub day: i64,
    pub seq: i64,
}

/// The table that holds documents of `kind`.
pub(crate) const fn table(kind: DocKind) -> &'static str {
    match kind {
        DocKind::PurchaseOrder => "purchase_order",
        DocKind::GoodsReceipt => "goods_receipt",
        DocKind::SupplierReturn => "supplier_return",
        DocKind::Transfer => "stock_transfer",
        DocKind::InternalTransfer => "internal_transfer",
        DocKind::Adjustment => "stock_adjustment",
        DocKind::Opname => "opname",
        DocKind::OpeningStock => "opening_stock",
    }
}

/// The branch's code and time zone.
pub(crate) async fn branch_place(
    tx: &mut Tx,
    branch: BranchId,
) -> StockResult<(String, BranchTimeZone)> {
    let (code, tz) = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        "SELECT code, timezone FROM branch WHERE id = ?",
        args![branch],
        |row| Ok((row.text("code")?, row.text("timezone")?)),
    )
    .await?
    .ok_or(StockDbError::NotFound("branch"))?;
    let tz = tz
        .parse()
        .map_err(|e| DbError::Corrupt(format!("branch time zone: {e}")))?;
    Ok((code, tz))
}

/// The next number of `kind` at `branch` on the branch-local day of `at`. The table's unique key
/// `(branch_id, local_day, seq)` turns a lost race into a retryable conflict.
pub(crate) async fn allocate(
    tx: &mut Tx,
    kind: DocKind,
    branch: BranchId,
    at: Timestamp,
) -> StockResult<Numbered> {
    let (code, tz) = branch_place(tx, branch).await?;
    let day = LocalDay::of(at, tz);
    let last = sql::fetch_optional(
        Conn::Tx(&mut *tx),
        &format!(
            "SELECT MAX(seq) AS seq FROM {} WHERE branch_id = ? AND local_day = ?",
            table(kind)
        ),
        args![branch, day.key()],
        |row| row.opt_i64("seq"),
    )
    .await?
    .flatten()
    .unwrap_or(0);
    let seq = u32::try_from(last + 1)
        .map_err(|_| DbError::Corrupt(format!("document sequence {last} out of range")))?;
    Ok(Numbered {
        number: stock_number(&code, kind, day, seq),
        day: day.key(),
        seq: i64::from(seq),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_has_its_table() {
        let t = DocKind::ALL.map(table);
        assert_eq!(t.len(), 8);
        assert!(t.contains(&"stock_adjustment"));
        assert!(t.contains(&"opname"));
    }
}
