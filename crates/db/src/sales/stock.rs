//! The stock movements of sales, post-voids and returns (spec 2.6 D9, spec 2.8 §2): written
//! through the inventory ledger ([`crate::stock::post_moves`]) in the document's transaction, so
//! the cached balance, the average cost and the lots stay in step. The quantities come from
//! `core::sale` (`sale_movements`, `void_movements`, `return_movements`); since 2.8 each row also
//! has a value and its document line:
//!
//! - a `sale` is valued at the average and takes the lots of a tracked product FEFO;
//! - a `sale_void` puts back exactly what the sale took (each row's quantity, value and lot);
//! - a restocked `return` comes back at the value the sale line took (proportionally, into the
//!   lots it came from, else no lot), or at the average without a receipt or without a recorded
//!   value; a written-off return and its `write_off` use no lot, the write-off valued at the
//!   average.
//!
//! The location is the register's default (2.6 §19 item 22); negative stock is allowed.

use sultan_core::catalog::Qty;
use sultan_core::ids::{BranchId, LocationId, RegisterId, VariantId};
use sultan_core::money::Money;
use sultan_core::sale::{
    ReturnDisposition, StockMovement, StockMovementKind, return_movements, sale_movements,
};
use sultan_core::stock::{MovementKind, spread};
use sultan_core::time::Timestamp;
use uuid::Uuid;

use super::returns::NewReturn;
use super::sale::NewSale;
use super::{SaleDbError, SaleResult};
use crate::conn::{Conn, Tx};
use crate::error::{DbError, Result};
use crate::sql::{self, Row, args};
use crate::stock::{
    self as ledger, DocRef, LotRule, Move, MovementRecord, StockDbError, Valuation,
};

/// A stored stock movement of a sale or return.
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
    /// The value in Rupiah, signed (spec 2.8 §2); `None` = no cost (and every row from before
    /// 2.8).
    pub value: Option<Money>,
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
        value: sql::opt_money(row, "value")?,
        doc_kind: row.text("doc_kind")?,
        doc_id: sql::id(row, "doc_id")?,
        occurred_at: sql::ts(row, "occurred_at")?,
    })
}

impl From<StockDbError> for SaleDbError {
    fn from(e: StockDbError) -> Self {
        match e {
            StockDbError::Db(db) => Self::Db(db),
            other => Self::invalid("movements", other.to_string()),
        }
    }
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

/// The document line of each movement: the movements the lines produce one by one, when they
/// add up to `flat` (what the caller computed); otherwise no line numbers.
fn with_lines(
    per_line: Vec<(u32, Vec<StockMovement>)>,
    flat: &[StockMovement],
) -> Vec<(Option<u32>, StockMovement)> {
    let numbered: Vec<(Option<u32>, StockMovement)> = per_line
        .into_iter()
        .flat_map(|(no, ms)| ms.into_iter().map(move |m| (Some(no), m)))
        .collect();
    if numbered.iter().map(|(_, m)| *m).eq(flat.iter().copied()) {
        numbered
    } else {
        flat.iter().map(|m| (None, *m)).collect()
    }
}

fn line_no(i: usize) -> u32 {
    u32::try_from(i + 1).unwrap_or(u32::MAX)
}

/// Appends a sale's `sale` movements at `location`: at the average, FEFO.
pub(super) async fn insert_sale(
    tx: &mut Tx,
    sale: &NewSale,
    location: Option<LocationId>,
) -> SaleResult<()> {
    let per_line = sale
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| Ok((line_no(i), sale_movements(&[l.stock_line()])?)))
        .collect::<SaleResult<Vec<_>>>()?;
    let moves: Vec<Move> = with_lines(per_line, &sale.movements)
        .into_iter()
        .map(|(line, m)| Move {
            branch: sale.branch,
            location,
            variant: m.variant,
            kind: MovementKind::from(m.kind),
            qty: m.base_qty,
            valuation: Valuation::Average,
            lot: LotRule::Fefo,
            doc: DocRef {
                kind: "sale",
                id: *sale.id.as_uuid(),
                line,
            },
            at: sale.posted_at,
        })
        .collect();
    ledger::post_moves(tx, &moves).await?;
    Ok(())
}

fn negate(m: Option<Money>) -> Option<Money> {
    m.and_then(|v| Money::ZERO.checked_sub(v))
}

/// Appends the `sale_void` movements that put back exactly what the sale's `sale` movements
/// took: the same quantity, value, lot and location.
pub(super) async fn put_back_sale(tx: &mut Tx, sale: Uuid, at: Timestamp) -> SaleResult<()> {
    let original = ledger::document_movements(Conn::Tx(&mut *tx), sale).await?;
    let moves = original
        .iter()
        .filter(|m| m.kind == MovementKind::Sale)
        .map(|m| {
            Ok(Move {
                branch: m.branch,
                location: m.location,
                variant: m.variant,
                kind: MovementKind::SaleVoid,
                qty: m
                    .qty
                    .checked_neg()
                    .ok_or_else(|| SaleDbError::invalid("movements", "a quantity does not fit"))?,
                valuation: Valuation::PutBack(negate(m.value)),
                lot: m.lot.map_or(LotRule::None, LotRule::Lot),
                doc: DocRef {
                    kind: "sale",
                    id: sale,
                    line: m.doc_line,
                },
                at,
            })
        })
        .collect::<SaleResult<Vec<_>>>()?;
    ledger::post_moves(tx, &moves).await?;
    Ok(())
}

/// The parts a restocked return of `qty` takes from the sale line's `sale` rows of `variant`:
/// back into their lots in the order taken (each at most what it gave), the rest without a lot,
/// with the sale line's value spread over the parts; `None` value when any row had no cost.
fn restock_parts(rows: &[&MovementRecord], qty: Qty) -> Option<Vec<(LotRule, Qty, Option<Money>)>> {
    let sold: i128 = rows.iter().map(|r| -i128::from(r.qty.thousandths())).sum();
    if sold <= 0 {
        return None;
    }
    let value: Option<i128> = rows
        .iter()
        .map(|r| r.value.map(|v| -i128::from(v.minor())))
        .sum();
    let mut parts = Vec::new();
    let mut rest = i128::from(qty.thousandths());
    for r in rows.iter().filter(|r| r.lot.is_some()) {
        if rest <= 0 {
            break;
        }
        let take = rest.min(-i128::from(r.qty.thousandths()));
        parts.push((r.lot.map_or(LotRule::None, LotRule::Lot), take));
        rest -= take;
    }
    if rest > 0 {
        parts.push((LotRule::None, rest));
    }
    let total = value.map(|v| {
        let n = v * i128::from(qty.thousandths());
        // Half away from zero, like every value in the ledger.
        let q = n / sold;
        if (n % sold).abs() * 2 >= sold {
            q + n.signum()
        } else {
            q
        }
    });
    let values: Vec<Option<Money>> = match total.and_then(|t| i64::try_from(t).ok()) {
        Some(t) if t >= 0 => {
            let weights: Vec<Money> = parts
                .iter()
                .map(|(_, q)| Money::from_minor(i64::try_from(*q).unwrap_or(i64::MAX)))
                .collect();
            spread(Money::from_minor(t), &weights)
                .ok()?
                .into_iter()
                .map(Some)
                .collect()
        }
        _ => vec![None; parts.len()],
    };
    Some(
        parts
            .into_iter()
            .zip(values)
            .map(|((lot, q), v)| (lot, Qty::from_thousandths(i64::try_from(q).unwrap_or(0)), v))
            .collect(),
    )
}

/// Appends a return's `return` / `write_off` movements at `location` (spec 2.8 §2).
pub(super) async fn insert_return(
    tx: &mut Tx,
    r: &NewReturn,
    location: Option<LocationId>,
) -> SaleResult<()> {
    let per_line = r
        .lines
        .iter()
        .enumerate()
        .map(|(i, l)| {
            Ok((
                line_no(i),
                return_movements(&l.stock_line(), &l.disposition)?,
            ))
        })
        .collect::<SaleResult<Vec<_>>>()?;
    let sale_rows = match r.sale {
        Some(sale) => ledger::document_movements(Conn::Tx(&mut *tx), *sale.as_uuid()).await?,
        None => Vec::new(),
    };
    let doc = |line| DocRef {
        kind: "return",
        id: *r.id.as_uuid(),
        line,
    };
    let base = |variant: VariantId, kind, qty, valuation, lot, line| Move {
        branch: r.branch,
        location,
        variant,
        kind,
        qty,
        valuation,
        lot,
        doc: doc(line),
        at: r.posted_at,
    };
    let mut moves = Vec::new();
    for (line, m) in with_lines(per_line, &r.movements) {
        let ret = line.and_then(|no| r.lines.get(no as usize - 1));
        let write_off =
            ret.is_some_and(|l| matches!(l.disposition, ReturnDisposition::WriteOff { .. }));
        if m.kind == StockMovementKind::WriteOff {
            moves.push(base(
                m.variant,
                MovementKind::WriteOff,
                m.base_qty,
                Valuation::Average,
                LotRule::None,
                line,
            ));
        } else if write_off {
            // Not a restock: it comes in at the average, without a lot, so the `return` +
            // `write_off` pair nets to zero quantity and value (§2, §20 reading 12).
            moves.push(base(
                m.variant,
                MovementKind::Return,
                m.base_qty,
                Valuation::Average,
                LotRule::None,
                line,
            ));
        } else {
            let rows: Vec<&MovementRecord> = match ret.and_then(|l| l.sale_line) {
                Some(sl) => sale_rows
                    .iter()
                    .filter(|s| {
                        s.kind == MovementKind::Sale
                            && s.variant == m.variant
                            && s.doc_line == Some(sl)
                    })
                    .collect(),
                None => Vec::new(),
            };
            match restock_parts(&rows, m.base_qty) {
                Some(parts) => {
                    for (lot, qty, value) in parts {
                        moves.push(base(
                            m.variant,
                            MovementKind::Return,
                            qty,
                            Valuation::PutBack(value),
                            lot,
                            line,
                        ));
                    }
                }
                None => moves.push(base(
                    m.variant,
                    MovementKind::from(m.kind),
                    m.base_qty,
                    Valuation::PutBack(None),
                    LotRule::None,
                    line,
                )),
            }
        }
    }
    ledger::post_moves(tx, &moves).await?;
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
        "SELECT id, branch_id, location_id, variant_id, kind, base_qty, value, doc_kind, doc_id, \
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

    fn row(qty: i64, value: Option<i64>, lot: bool) -> MovementRecord {
        MovementRecord {
            id: Uuid::nil(),
            branch: BranchId::from_uuid(Uuid::nil()),
            location: None,
            variant: VariantId::from_uuid(Uuid::nil()),
            kind: MovementKind::Sale,
            qty: Qty::from_thousandths(qty),
            value: value.map(Money::from_minor),
            lot: lot.then(sultan_core::ids::LotId::generate),
            doc_kind: "sale".into(),
            doc_id: Uuid::nil(),
            doc_line: Some(1),
            occurred_at: Timestamp::from_unix_millis(0),
        }
    }

    #[test]
    fn restock_follows_the_sale_line() {
        let q = Qty::from_thousandths;
        // 3 sold at 1 000 (one lot of 2, one without): a return of 2 goes back into the lot.
        let a = row(-2_000, Some(-700), true);
        let b = row(-1_000, Some(-300), false);
        let parts = restock_parts(&[&a, &b], q(2_000)).unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(
            (parts[0].1, parts[0].2),
            (q(2_000), Some(Money::from_minor(667)))
        );
        assert_eq!(parts[0].0, LotRule::Lot(a.lot.unwrap()));
        // 3 back: 2 into the lot, 1 without.
        let parts = restock_parts(&[&a, &b], q(3_000)).unwrap();
        let got: Vec<_> = parts.iter().map(|p| (p.1, p.2)).collect();
        assert_eq!(
            got,
            [
                (q(2_000), Some(Money::from_minor(667))),
                (q(1_000), Some(Money::from_minor(333)))
            ]
        );
        // A row without a cost: no value.
        let c = row(-1_000, None, false);
        let parts = restock_parts(&[&c], q(1_000)).unwrap();
        assert_eq!(parts[0].2, None);
        // Nothing sold on the line: none.
        assert!(restock_parts(&[], q(1_000)).is_none());
    }

    #[test]
    fn lines_only_when_they_add_up() {
        let v = VariantId::generate();
        let m = |n| StockMovement {
            variant: v,
            kind: StockMovementKind::Sale,
            base_qty: Qty::from_thousandths(n),
        };
        let per_line = vec![(1, vec![m(-1)]), (2, vec![]), (3, vec![m(-2)])];
        assert_eq!(
            with_lines(per_line.clone(), &[m(-1), m(-2)]),
            [(Some(1), m(-1)), (Some(3), m(-2))]
        );
        assert_eq!(with_lines(per_line, &[m(-2)]), [(None, m(-2))]);
        assert_eq!(
            negate(Some(Money::from_minor(5))),
            Some(Money::from_minor(-5))
        );
        assert_eq!(negate(None), None);
    }
}
