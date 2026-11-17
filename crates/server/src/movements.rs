//! **The sales seam (spec 2.6 §10):** the money movements of a cashier session that feed its
//! close report (spec 2.1 §5: expected cash, over/short, totals per tender).
//!
//! The server's default source is [`SalesMovements`] (phase S3): the session's sales, returns
//! and post-voids (`sultan_db::sales::session_events`) as [`Movement`]s. A cash tender counts the
//! cash that stays in the drawer, `cash_received − change_given` (so the cash rounding and the
//! saved change are in it, spec 2.6 §19 item 19); card, QRIS and transfer tenders count their
//! amount; refunds paid in cash lower the expected cash. E-wallet, other, customer-balance and
//! exchange tenders move no cash and have no column in the 2.1 tender table: they appear in the
//! report's `sales` block (`core::sale::session_report`) instead. Expected cash is therefore
//! `opening_cash + cash received − change given − cash refunds + cash_in − cash_out − payouts`
//! (§10). [`NoSales`] reports nothing (tests). Cash in/out do not exist yet (2.7). The close
//! handler reads the movements inside its close transaction, so the report and the close are
//! consistent.
//!
//! **Balance payouts** (spec 2.5 §11, phase M3) are not part of the seam: they already exist.
//! [`session_movements`] adds the cash a session paid out of its drawer for customer balance
//! payouts (`sultan_db::customers::session_payouts`) as one [`Movement::CashOut`], so they lower
//! the expected cash and appear in the close report's `cash_out`, next to whatever the installed
//! source reports.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use sultan_core::cashier_session::{Movement, Tender};
use sultan_core::ids::CashierSessionId;
use sultan_core::money::Money;
use sultan_core::sale::{SessionEvent, TenderKind};
use sultan_db::{DbError, Tx, customers, sales};

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

/// The sales, returns and post-voids of a session (see the module docs).
#[derive(Debug, Clone, Copy, Default)]
pub struct SalesMovements;

impl SessionMovements for SalesMovements {
    fn movements<'a>(&'a self, tx: &'a mut Tx, session: CashierSessionId) -> MovementsFuture<'a> {
        Box::pin(async move {
            let events = sales::session_events(&mut *tx, session).await?;
            Ok(to_movements(&events))
        })
    }
}

/// The 2.1 tender of a tender kind, if it has one.
const fn tender(kind: TenderKind) -> Option<Tender> {
    match kind {
        TenderKind::Cash => Some(Tender::Cash),
        TenderKind::Card => Some(Tender::Card),
        TenderKind::Qris => Some(Tender::Qris),
        TenderKind::Transfer => Some(Tender::Transfer),
        _ => None,
    }
}

/// The session events as close-report movements (see the module docs).
#[must_use]
pub fn to_movements(events: &[SessionEvent]) -> Vec<Movement> {
    let mut out = Vec::new();
    for event in events {
        match *event {
            SessionEvent::Tender {
                key,
                amount,
                change_saved,
                ..
            } => {
                if let Some(tender) = tender(key.kind) {
                    let amount = if tender == Tender::Cash {
                        amount.saturating_add(change_saved)
                    } else {
                        amount
                    };
                    out.push(Movement::Sale { tender, amount });
                }
            }
            SessionEvent::Refund { key, amount } => {
                if let Some(tender) = tender(key.kind) {
                    out.push(Movement::Refund { tender, amount });
                }
            }
            SessionEvent::CashIn(a) => out.push(Movement::CashIn(a)),
            SessionEvent::CashOut(a) | SessionEvent::Payout(a) => out.push(Movement::CashOut(a)),
            _ => {}
        }
    }
    out
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

    #[test]
    fn events_become_movements() {
        use sultan_core::sale::TenderKey;
        let m = Money::from_minor;
        let key = |kind| TenderKey {
            kind,
            tender_type: None,
        };
        let tender = |kind, amount, saved| SessionEvent::Tender {
            key: key(kind),
            amount: m(amount),
            cash_received: None,
            change_given: m(0),
            change_saved: m(saved),
        };
        let events = [
            tender(TenderKind::Cash, 63_500, 700),
            tender(TenderKind::Qris, 50_000, 0),
            tender(TenderKind::Card, 1_000, 0),
            tender(TenderKind::Transfer, 2_000, 0),
            tender(TenderKind::CustomerBalance, 2_000, 0),
            SessionEvent::Refund {
                key: key(TenderKind::Cash),
                amount: m(5_000),
            },
            SessionEvent::Refund {
                key: key(TenderKind::CustomerBalance),
                amount: m(1_000),
            },
            SessionEvent::CashIn(m(10)),
            SessionEvent::CashOut(m(20)),
            SessionEvent::Payout(m(30)),
            SessionEvent::LineVoid(m(99)),
        ];
        assert_eq!(
            to_movements(&events),
            vec![
                Movement::Sale {
                    tender: Tender::Cash,
                    amount: m(64_200)
                },
                Movement::Sale {
                    tender: Tender::Qris,
                    amount: m(50_000)
                },
                Movement::Sale {
                    tender: Tender::Card,
                    amount: m(1_000)
                },
                Movement::Sale {
                    tender: Tender::Transfer,
                    amount: m(2_000)
                },
                Movement::Refund {
                    tender: Tender::Cash,
                    amount: m(5_000)
                },
                Movement::CashIn(m(10)),
                Movement::CashOut(m(20)),
                Movement::CashOut(m(30)),
            ]
        );
    }

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
