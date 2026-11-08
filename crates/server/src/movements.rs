//! **Seam for the sales phase (spec 2.6 / 2.7):** the money movements of a cashier session that
//! feed its close report (spec 2.1 §5: expected cash, over/short, totals per tender).
//!
//! Cash sales, refunds and cash in/out do not exist yet, so the server's default source,
//! [`NoSales`], reports none: expected cash is the opening cash and every tender total is empty.
//! When sales arrive, implement [`SessionMovements`] over the sale and payment tables (one
//! [`Movement`] per tender of each payment, refund and cash in/out) and install it with
//! [`AppState::with_movements`](crate::AppState::with_movements). The close handler reads the
//! movements inside its close transaction, so the report and the close are consistent.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use sultan_core::cashier_session::Movement;
use sultan_core::ids::CashierSessionId;
use sultan_db::{DbError, Tx};

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
