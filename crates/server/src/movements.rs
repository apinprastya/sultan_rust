//! **Seam for the sales phase (spec 2.6 / 2.7):** the money movements of a cashier session that
//! feed its close report (spec 2.1 §5: expected cash, over/short, totals per tender).
//!
//! Cash sales, refunds and cash in/out do not exist yet, so the server's default source,
//! [`NoSales`], reports none: expected cash is the opening cash and every tender total is empty.
//! When sales arrive, implement [`SessionMovements`] over the sale and payment tables (one
//! [`Movement`] per tender of each payment, refund and cash in/out) and install it with
//! [`AppState::with_movements`](crate::AppState::with_movements). The close handler reads the
//! movements inside its close transaction, so the report and the close are consistent.
//!
//! **Balance payouts** (spec 2.5 §11, phase M3) are not part of the seam: they already exist.
//! [`session_movements`] adds the cash a session paid out of its drawer for customer balance
//! payouts (`sultan_db::customers::session_payouts`) as one [`Movement::CashOut`], so they lower
//! the expected cash and appear in the close report's `cash_out`, next to whatever the installed
//! source reports.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use sultan_core::cashier_session::Movement;
use sultan_core::ids::CashierSessionId;
use sultan_core::money::Money;
use sultan_db::{DbError, Tx, customers};

use crate::state::AppState;

/// A boxed future of the movements.
pub type MovementsFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Vec<Movement>, DbError>> + Send + 'a>>;

/// Where the close report gets a session's sales, refunds and cash in/out from.
pub trait SessionMovements: Send + Sync + fmt::Debug {
    /// Every movement of `session`, read inside `tx`.
    fn movements<'a>(&'a self, tx: &'a mut Tx, session: CashierSessionId) -> MovementsFuture<'a>;
}

/// The stub used until sales exist: no movements.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSales;

impl SessionMovements for NoSales {
    fn movements<'a>(&'a self, _tx: &'a mut Tx, _session: CashierSessionId) -> MovementsFuture<'a> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// Every movement of `session` that the close report counts: those of the installed source
/// ([`AppState::movements`]) plus the balance payouts of the session as cash out (see the module
/// docs), read inside `tx`.
///
/// # Errors
/// The database error.
pub async fn session_movements(
    state: &AppState,
    tx: &mut Tx,
    session: CashierSessionId,
) -> Result<Vec<Movement>, DbError> {
    let mut movements = state.movements.movements(tx, session).await?;
    let paid_out = customers::session_payouts(&mut *tx, session).await?;
    if paid_out > Money::ZERO {
        movements.push(Movement::CashOut(paid_out));
    }
    Ok(movements)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_sales_reports_nothing() {
        let db = sultan_db::Db::connect("sqlite::memory:").await.unwrap();
        let mut tx = db.begin().await.unwrap();
        let movements = NoSales
            .movements(&mut tx, CashierSessionId::generate())
            .await
            .unwrap();
        assert!(movements.is_empty());
    }
}
