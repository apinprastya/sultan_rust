//! The stock movements of sales, returns and post-voids (spec 2.6 D9): base units at the
//! branch's default location, negative stock allowed, no costing (2.8). Only the values: the
//! ledger table comes with S2.

use super::SaleError;
use super::returns::ReturnDisposition;
use crate::catalog::{ComponentSnapshot, ProductType, Qty, explode_bundle};
use crate::ids::VariantId;

/// The kind of a stock ledger entry written by sales (D9, §7, §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StockMovementKind {
    /// `sale` (−).
    Sale,
    /// `sale_void` (+): a post-void puts the sale's stock back.
    SaleVoid,
    /// `return` (+).
    Return,
    /// `write_off` (−): a returned item that is not restocked.
    WriteOff,
}

impl StockMovementKind {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Sale => "sale",
            Self::SaleVoid => "sale_void",
            Self::Return => "return",
            Self::WriteOff => "write_off",
        }
    }
}

/// One stock movement in base units (signed: − out, + in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StockMovement {
    /// The variant whose stock moves.
    pub variant: VariantId,
    /// The kind.
    pub kind: StockMovementKind,
    /// Base units, signed.
    pub base_qty: Qty,
}

/// A sold or returned line as the stock needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StockLine<'a> {
    /// The variant sold.
    pub variant: VariantId,
    /// Its product type: stock and weighed move their own stock, a bundle moves its components,
    /// a service moves nothing.
    pub product_type: ProductType,
    /// The quantity in the line's unit (0 for a price label without a derived quantity).
    pub qty: Qty,
    /// The unit's factor to the base unit.
    pub factor: Qty,
    /// A bundle's component snapshot.
    pub components: &'a [ComponentSnapshot],
}

/// The base quantities a line moves (positive), per variant.
fn base_quantities(line: &StockLine<'_>) -> Result<Vec<(VariantId, Qty)>, SaleError> {
    let overflow = || SaleError::InvalidQty {
        reason: "does not fit in base units",
    };
    let qty = line.qty.times_factor(line.factor).ok_or_else(overflow)?;
    if qty == Qty::ZERO {
        return Ok(Vec::new());
    }
    match line.product_type {
        ProductType::Bundle => Ok(explode_bundle(qty, line.components)
            .map_err(|_| overflow())?
            .into_iter()
            .filter(|c| c.base_qty != Qty::ZERO)
            .map(|c| (c.variant, c.base_qty))
            .collect()),
        t if t.has_stock() => Ok(vec![(line.variant, qty)]),
        _ => Ok(Vec::new()),
    }
}

fn negate(q: Qty) -> Result<Qty, SaleError> {
    q.checked_neg().ok_or(SaleError::InvalidQty {
        reason: "does not fit in base units",
    })
}

/// The `sale` movements of the sold lines (−, base units), in line order. Bundles move each
/// stocked component (`qty × factor × component qty × component factor`); services and lines
/// without a quantity move nothing.
///
/// # Errors
/// [`SaleError::InvalidQty`] when a quantity does not fit.
pub fn sale_movements(lines: &[StockLine<'_>]) -> Result<Vec<StockMovement>, SaleError> {
    let mut out = Vec::new();
    for line in lines {
        for (variant, qty) in base_quantities(line)? {
            out.push(StockMovement {
                variant,
                kind: StockMovementKind::Sale,
                base_qty: negate(qty)?,
            });
        }
    }
    Ok(out)
}

/// The `sale_void` movements that put a voided sale's `sale` movements back (§7).
///
/// # Errors
/// [`SaleError::InvalidQty`] for a quantity that cannot be negated.
pub fn void_movements(sale: &[StockMovement]) -> Result<Vec<StockMovement>, SaleError> {
    sale.iter()
        .map(|m| {
            Ok(StockMovement {
                kind: StockMovementKind::SaleVoid,
                base_qty: negate(m.base_qty)?,
                ..*m
            })
        })
        .collect()
}

/// The movements of a returned line (D9): `return` (+); a write-off adds `write_off` (−) of the
/// same quantity, so the stock does not change.
///
/// # Errors
/// [`SaleError::InvalidQty`] when a quantity does not fit.
pub fn return_movements(
    line: &StockLine<'_>,
    disposition: &ReturnDisposition,
) -> Result<Vec<StockMovement>, SaleError> {
    let mut out = Vec::new();
    for (variant, qty) in base_quantities(line)? {
        out.push(StockMovement {
            variant,
            kind: StockMovementKind::Return,
            base_qty: qty,
        });
        if matches!(disposition, ReturnDisposition::WriteOff { .. }) {
            out.push(StockMovement {
                variant,
                kind: StockMovementKind::WriteOff,
                base_qty: negate(qty)?,
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::UnitId;

    fn q(t: i64) -> Qty {
        Qty::from_thousandths(t)
    }

    fn line(
        variant: VariantId,
        product_type: ProductType,
        qty: i64,
        factor: i64,
    ) -> StockLine<'static> {
        StockLine {
            variant,
            product_type,
            qty: q(qty),
            factor: q(factor),
            components: &[],
        }
    }

    /// Spec §15 case 17.
    #[test]
    fn bundle_sale_and_return() {
        let kit = VariantId::generate();
        let a = VariantId::generate();
        let b = VariantId::generate();
        let svc = VariantId::generate();
        let components = [
            ComponentSnapshot {
                variant: a,
                product_type: ProductType::Stock,
                unit: UnitId::generate(),
                qty: q(2_000),
                factor: q(6_000), // 2 boxes of 6
            },
            ComponentSnapshot {
                variant: b,
                product_type: ProductType::Weighed,
                unit: UnitId::generate(),
                qty: q(500),
                factor: Qty::ONE,
            },
            ComponentSnapshot {
                variant: svc,
                product_type: ProductType::Service,
                unit: UnitId::generate(),
                qty: Qty::ONE,
                factor: Qty::ONE,
            },
        ];
        let bundle = StockLine {
            components: &components,
            ..line(kit, ProductType::Bundle, 3_000, 1_000)
        };
        let sale = sale_movements(&[bundle]).unwrap();
        let got: Vec<_> = sale
            .iter()
            .map(|m| (m.variant, m.kind, m.base_qty))
            .collect();
        assert_eq!(
            got,
            [
                (a, StockMovementKind::Sale, q(-36_000)),
                (b, StockMovementKind::Sale, q(-1_500)),
            ]
        );
        // Return with restock → reversed.
        let back = return_movements(&bundle, &ReturnDisposition::Restock).unwrap();
        let got: Vec<_> = back
            .iter()
            .map(|m| (m.variant, m.kind, m.base_qty))
            .collect();
        assert_eq!(
            got,
            [
                (a, StockMovementKind::Return, q(36_000)),
                (b, StockMovementKind::Return, q(1_500)),
            ]
        );
        // Post-void puts the sale back.
        let void = void_movements(&sale).unwrap();
        assert!(
            void.iter()
                .zip(&sale)
                .all(|(v, s)| v.kind == StockMovementKind::SaleVoid
                    && v.base_qty.checked_add(s.base_qty) == Some(Qty::ZERO))
        );
    }

    #[test]
    fn plain_lines_and_write_off() {
        let v = VariantId::generate();
        let w = VariantId::generate();
        let lines = [
            line(v, ProductType::Stock, 2_000, 24_000), // 2 boxes of 24
            line(w, ProductType::Weighed, 1_255, 1_000),
            line(VariantId::generate(), ProductType::Service, 1_000, 1_000),
            line(w, ProductType::Weighed, 0, 1_000), // price label without qty
        ];
        let sale = sale_movements(&lines).unwrap();
        let got: Vec<_> = sale.iter().map(|m| (m.variant, m.base_qty)).collect();
        assert_eq!(got, [(v, q(-48_000)), (w, q(-1_255))]);

        let off = ReturnDisposition::WriteOff {
            reason: "broken".into(),
        };
        let back = return_movements(&lines[0], &off).unwrap();
        let got: Vec<_> = back.iter().map(|m| (m.kind.as_str(), m.base_qty)).collect();
        assert_eq!(got, [("return", q(48_000)), ("write_off", q(-48_000))]);
        assert!(return_movements(&lines[2], &off).unwrap().is_empty());

        assert_eq!(StockMovementKind::Sale.as_str(), "sale");
        assert_eq!(StockMovementKind::SaleVoid.as_str(), "sale_void");

        // Overflow.
        let huge = line(v, ProductType::Stock, i64::MAX, 2_000);
        assert_eq!(sale_movements(&[huge]).unwrap_err().code(), "invalid_qty");
        let min = StockMovement {
            variant: v,
            kind: StockMovementKind::Sale,
            base_qty: q(i64::MIN),
        };
        assert!(void_movements(&[min]).is_err());
        let comps = [ComponentSnapshot {
            variant: w,
            product_type: ProductType::Stock,
            unit: UnitId::generate(),
            qty: q(i64::MAX),
            factor: q(2_000),
        }];
        let kit = StockLine {
            components: &comps,
            ..line(v, ProductType::Bundle, 1_000, 1_000)
        };
        assert!(sale_movements(&[kit]).is_err());
    }
}
