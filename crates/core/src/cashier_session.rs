//! Cashier session: whose money is in the drawer (spec 2.1 §5).
//!
//! State machine: `Open → Closed (normal | forced)`. A close cannot be undone.

use std::collections::BTreeMap;

use crate::ids::{CashierSessionId, RegisterId, UserId};
use crate::money::Money;
use crate::time::Timestamp;

/// Errors of the cashier-session rules, with stable problem+json codes (spec 2.1 §8).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionError {
    /// The user has no open session on this register (spec 2.1 §5).
    #[error("no open cashier session for this user on this register")]
    NoOpenSession,
    /// A session is already open on the register or for the user (spec 2.1 §5).
    #[error("a cashier session is already open ({conflict:?})")]
    SessionAlreadyOpen {
        /// Which rule was hit, and the open session.
        conflict: OpenConflict,
    },
    /// The session was already closed; a close cannot be undone (spec 2.1 §5).
    #[error("cashier session {session} is already closed")]
    AlreadyClosed {
        /// The session.
        session: CashierSessionId,
    },
    /// A normal close by someone other than the session's cashier; use a force close
    /// (spec 2.1 §5, `session.open_close` is for one's own session).
    #[error("only the session's cashier can close it; use force close")]
    NotSessionOwner,
    /// A total does not fit in `i64` minor units.
    #[error("amount overflow")]
    AmountOverflow,
}

impl SessionError {
    /// The stable error code used in problem+json responses (spec 2.1 §8).
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NoOpenSession => "no_open_session",
            Self::SessionAlreadyOpen { .. } => "session_already_open",
            Self::AlreadyClosed { .. } => "session_closed",
            Self::NotSessionOwner => "permission_denied",
            Self::AmountOverflow => "amount_overflow",
        }
    }
}

/// Which "at most one open session" rule an open request hit (spec 2.1 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OpenConflict {
    /// The register already has an open session.
    Register(CashierSessionId),
    /// The user already has an open session (on any register).
    User(CashierSessionId),
}

/// How a session was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CloseKind {
    /// Closed by its own cashier.
    Normal,
    /// Force-closed by a manager holding `session.force_close` (spec 2.1 §5).
    Forced {
        /// The manager.
        by: UserId,
    },
}

/// The lifecycle state of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionStatus {
    /// Money is in the drawer; sales are allowed.
    Open,
    /// Counted and closed.
    Closed {
        /// When it was closed.
        at: Timestamp,
        /// Normal or forced.
        kind: CloseKind,
    },
}

/// A tender (payment method). Only what the close report needs today; the sales section (2.6)
/// will extend it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tender {
    /// Cash in the drawer. The only tender that affects expected cash.
    Cash,
    /// Debit or credit card.
    Card,
    /// QRIS.
    Qris,
    /// Bank transfer.
    Transfer,
}

impl Tender {
    /// Every tender, in report order.
    pub const ALL: [Self; 4] = [Self::Cash, Self::Card, Self::Qris, Self::Transfer];

    /// The stable key used in the API and the database, e.g. `cash`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Cash => "cash",
            Self::Card => "card",
            Self::Qris => "qris",
            Self::Transfer => "transfer",
        }
    }
}

/// A tender key that is not in [`Tender::ALL`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tender `{0}`")]
pub struct UnknownTender(pub String);

impl std::str::FromStr for Tender {
    type Err = UnknownTender;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|t| t.as_str() == s)
            .ok_or_else(|| UnknownTender(s.to_owned()))
    }
}

/// A money movement during a session, as input to the close report.
///
/// Amounts are positive. A cash sale amount is the cash kept (tendered minus change).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Movement {
    /// A completed sale paid (partly) with `tender`. A split payment is one movement per tender.
    Sale {
        /// Payment method.
        tender: Tender,
        /// Amount paid with it.
        amount: Money,
    },
    /// A refund paid out with `tender`.
    Refund {
        /// Payment method.
        tender: Tender,
        /// Amount refunded.
        amount: Money,
    },
    /// Petty cash put into the drawer.
    CashIn(Money),
    /// Petty cash taken out of the drawer.
    CashOut(Money),
}

/// Totals for one tender in the close report.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct TenderTotals {
    /// Sum of sales.
    pub sales: Money,
    /// Sum of refunds.
    pub refunds: Money,
    /// Sales minus refunds.
    pub net: Money,
    /// Number of sale and refund movements.
    pub count: u32,
}

/// The blind-close report (spec 2.1 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseReport {
    /// Cash in the drawer at open.
    pub opening_cash: Money,
    /// Cash sales.
    pub cash_sales: Money,
    /// Cash refunds.
    pub cash_refunds: Money,
    /// Petty cash in.
    pub cash_in: Money,
    /// Petty cash out.
    pub cash_out: Money,
    /// `opening_cash + cash_sales − cash_refunds + cash_in − cash_out`.
    pub expected_cash: Money,
    /// What the cashier counted.
    pub counted_cash: Money,
    /// `counted_cash − expected_cash`: positive = over, negative = short.
    pub over_short: Money,
    /// Totals per tender that had at least one sale or refund.
    pub tenders: BTreeMap<Tender, TenderTotals>,
    /// Number of sales and refunds.
    pub transactions: u32,
}

/// Computes the close report (spec 2.1 §5).
///
/// # Errors
/// [`SessionError::AmountOverflow`] if a total does not fit in `i64`.
pub fn close_report(
    opening_cash: Money,
    counted_cash: Money,
    movements: &[Movement],
) -> Result<CloseReport, SessionError> {
    fn add(a: Money, b: Money) -> Result<Money, SessionError> {
        a.checked_add(b).ok_or(SessionError::AmountOverflow)
    }
    fn sub(a: Money, b: Money) -> Result<Money, SessionError> {
        a.checked_sub(b).ok_or(SessionError::AmountOverflow)
    }

    let mut cash_in = Money::ZERO;
    let mut cash_out = Money::ZERO;
    let mut tenders: BTreeMap<Tender, TenderTotals> = BTreeMap::new();
    let mut transactions = 0_u32;

    for movement in movements {
        match *movement {
            Movement::Sale { tender, amount } => {
                let t = tenders.entry(tender).or_default();
                t.sales = add(t.sales, amount)?;
                t.net = add(t.net, amount)?;
                t.count += 1;
                transactions += 1;
            }
            Movement::Refund { tender, amount } => {
                let t = tenders.entry(tender).or_default();
                t.refunds = add(t.refunds, amount)?;
                t.net = sub(t.net, amount)?;
                t.count += 1;
                transactions += 1;
            }
            Movement::CashIn(amount) => cash_in = add(cash_in, amount)?,
            Movement::CashOut(amount) => cash_out = add(cash_out, amount)?,
        }
    }

    let cash = tenders.get(&Tender::Cash).copied().unwrap_or_default();
    let expected_cash = sub(
        add(sub(add(opening_cash, cash.sales)?, cash.refunds)?, cash_in)?,
        cash_out,
    )?;
    let over_short = sub(counted_cash, expected_cash)?;

    Ok(CloseReport {
        opening_cash,
        cash_sales: cash.sales,
        cash_refunds: cash.refunds,
        cash_in,
        cash_out,
        expected_cash,
        counted_cash,
        over_short,
        tenders,
        transactions,
    })
}

/// A cashier session bound to (user, register) (spec 2.1 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CashierSession {
    id: CashierSessionId,
    user: UserId,
    register: RegisterId,
    opening_cash: Money,
    opened_at: Timestamp,
    status: SessionStatus,
}

impl CashierSession {
    /// Opens a session after checking the "one per register, one per user" rule against
    /// `existing` (spec 2.1 §5, test case 9). Closed sessions in `existing` are ignored.
    ///
    /// # Errors
    /// [`SessionError::SessionAlreadyOpen`] (register conflicts are reported first).
    pub fn open(
        id: CashierSessionId,
        user: UserId,
        register: RegisterId,
        opening_cash: Money,
        opened_at: Timestamp,
        existing: &[Self],
    ) -> Result<Self, SessionError> {
        ensure_can_open(user, register, existing)?;
        Ok(Self {
            id,
            user,
            register,
            opening_cash,
            opened_at,
            status: SessionStatus::Open,
        })
    }

    /// Rebuilds a session from stored fields (e.g. a database row) without checks.
    #[must_use]
    pub const fn from_parts(
        id: CashierSessionId,
        user: UserId,
        register: RegisterId,
        opening_cash: Money,
        opened_at: Timestamp,
        status: SessionStatus,
    ) -> Self {
        Self {
            id,
            user,
            register,
            opening_cash,
            opened_at,
            status,
        }
    }

    /// Session ID.
    #[must_use]
    pub const fn id(&self) -> CashierSessionId {
        self.id
    }

    /// The cashier whose money is in the drawer.
    #[must_use]
    pub const fn user(&self) -> UserId {
        self.user
    }

    /// The register.
    #[must_use]
    pub const fn register(&self) -> RegisterId {
        self.register
    }

    /// Cash in the drawer at open.
    #[must_use]
    pub const fn opening_cash(&self) -> Money {
        self.opening_cash
    }

    /// When the session was opened.
    #[must_use]
    pub const fn opened_at(&self) -> Timestamp {
        self.opened_at
    }

    /// Current status.
    #[must_use]
    pub const fn status(&self) -> SessionStatus {
        self.status
    }

    /// Whether the session is open.
    #[must_use]
    pub const fn is_open(&self) -> bool {
        matches!(self.status, SessionStatus::Open)
    }

    /// Blind close by the session's own cashier (spec 2.1 §5): the cashier has entered
    /// `counted_cash`; returns the close report. On error the session stays open.
    ///
    /// # Errors
    /// [`SessionError::AlreadyClosed`], [`SessionError::NotSessionOwner`] if `by` is not the
    /// session's cashier, or [`SessionError::AmountOverflow`].
    pub fn close(
        &mut self,
        by: UserId,
        counted_cash: Money,
        movements: &[Movement],
        at: Timestamp,
    ) -> Result<CloseReport, SessionError> {
        self.ensure_open()?;
        if by != self.user {
            return Err(SessionError::NotSessionOwner);
        }
        self.finish(counted_cash, movements, at, CloseKind::Normal)
    }

    /// Force close of an abandoned session by a manager (spec 2.1 §5). The `session.force_close`
    /// permission check and the audit entry are the caller's job. The report is marked forced
    /// through [`SessionStatus::Closed`].
    ///
    /// # Errors
    /// [`SessionError::AlreadyClosed`] or [`SessionError::AmountOverflow`].
    pub fn force_close(
        &mut self,
        by: UserId,
        counted_cash: Money,
        movements: &[Movement],
        at: Timestamp,
    ) -> Result<CloseReport, SessionError> {
        self.ensure_open()?;
        self.finish(counted_cash, movements, at, CloseKind::Forced { by })
    }

    fn ensure_open(&self) -> Result<(), SessionError> {
        if self.is_open() {
            Ok(())
        } else {
            Err(SessionError::AlreadyClosed { session: self.id })
        }
    }

    fn finish(
        &mut self,
        counted_cash: Money,
        movements: &[Movement],
        at: Timestamp,
        kind: CloseKind,
    ) -> Result<CloseReport, SessionError> {
        let report = close_report(self.opening_cash, counted_cash, movements)?;
        self.status = SessionStatus::Closed { at, kind };
        Ok(report)
    }
}

/// Checks that `user` may open a session on `register` given the `existing` sessions: at most one
/// open session per register and one per user (spec 2.1 §5). Closed sessions are ignored.
///
/// # Errors
/// [`SessionError::SessionAlreadyOpen`]; a register conflict is reported before a user conflict.
pub fn ensure_can_open(
    user: UserId,
    register: RegisterId,
    existing: &[CashierSession],
) -> Result<(), SessionError> {
    let open = || existing.iter().filter(|s| s.is_open());
    let conflict = open()
        .find(|s| s.register == register)
        .map(|s| OpenConflict::Register(s.id))
        .or_else(|| {
            open()
                .find(|s| s.user == user)
                .map(|s| OpenConflict::User(s.id))
        });
    match conflict {
        Some(conflict) => Err(SessionError::SessionAlreadyOpen { conflict }),
        None => Ok(()),
    }
}

/// Finds the open session of `user` on `register`. Selling, refunds, cash in/out and drawer open
/// call this first (spec 2.1 §5, test case 10).
///
/// # Errors
/// [`SessionError::NoOpenSession`] if there is none.
pub fn require_open_session(
    user: UserId,
    register: RegisterId,
    sessions: &[CashierSession],
) -> Result<&CashierSession, SessionError> {
    sessions
        .iter()
        .find(|s| s.is_open() && s.user == user && s.register == register)
        .ok_or(SessionError::NoOpenSession)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(minor: i64) -> Money {
        Money::from_minor(minor)
    }

    const T: Timestamp = Timestamp::from_unix_millis(1_000);

    fn open_session(user: UserId, register: RegisterId) -> CashierSession {
        CashierSession::open(
            CashierSessionId::generate(),
            user,
            register,
            m(500_000),
            T,
            &[],
        )
        .unwrap()
    }

    #[test]
    fn tender_keys_round_trip() {
        for tender in Tender::ALL {
            assert_eq!(tender.as_str().parse::<Tender>(), Ok(tender));
        }
        let keys: Vec<_> = Tender::ALL.iter().map(|t| t.as_str()).collect();
        assert_eq!(keys, ["cash", "card", "qris", "transfer"]);
        let err = "gold".parse::<Tender>().unwrap_err();
        assert_eq!(err.to_string(), "unknown tender `gold`");
    }

    /// Spec 2.1 §9 case 8.
    #[test]
    fn expected_cash_and_over_short() {
        use Movement::{CashIn, CashOut, Refund, Sale};
        let mixed = [
            Sale {
                tender: Tender::Cash,
                amount: m(100_000),
            },
            Sale {
                tender: Tender::Card,
                amount: m(250_000),
            },
            Sale {
                tender: Tender::Qris,
                amount: m(40_000),
            },
            Sale {
                tender: Tender::Cash,
                amount: m(60_000),
            },
            Refund {
                tender: Tender::Cash,
                amount: m(20_000),
            },
            Refund {
                tender: Tender::Card,
                amount: m(50_000),
            },
            CashIn(m(30_000)),
            CashOut(m(15_000)),
        ];
        // expected = 500_000 + 160_000 − 20_000 + 30_000 − 15_000 = 655_000
        #[rustfmt::skip]
        let cases: [(&str, &[Movement], i64, i64, i64); 5] = [
            ("exact", &mixed, 655_000, 655_000, 0),
            ("over", &mixed, 660_000, 655_000, 5_000),
            ("short", &mixed, 650_000, 655_000, -5_000),
            ("no movements", &[], 500_000, 500_000, 0),
            ("non-cash only", &[Sale { tender: Tender::Transfer, amount: m(9_000) }], 499_000, 500_000, -1_000),
        ];
        for (name, movements, counted, expected, over_short) in cases {
            let r = close_report(m(500_000), m(counted), movements).unwrap();
            assert_eq!(r.expected_cash, m(expected), "{name}");
            assert_eq!(r.over_short, m(over_short), "{name}");
            assert_eq!(r.counted_cash, m(counted), "{name}");
        }

        let r = close_report(m(500_000), m(655_000), &mixed).unwrap();
        assert_eq!(
            (
                r.opening_cash,
                r.cash_sales,
                r.cash_refunds,
                r.cash_in,
                r.cash_out
            ),
            (m(500_000), m(160_000), m(20_000), m(30_000), m(15_000))
        );
        assert_eq!(r.transactions, 6);
        let t = |sales, refunds, net, count| TenderTotals {
            sales: m(sales),
            refunds: m(refunds),
            net: m(net),
            count,
        };
        assert_eq!(
            r.tenders,
            BTreeMap::from([
                (Tender::Cash, t(160_000, 20_000, 140_000, 3)),
                (Tender::Card, t(250_000, 50_000, 200_000, 2)),
                (Tender::Qris, t(40_000, 0, 40_000, 1)),
            ])
        );
    }

    #[test]
    fn overflow_is_reported() {
        let huge = m(i64::MAX);
        let cases: [&[Movement]; 4] = [
            &[
                Movement::Sale {
                    tender: Tender::Card,
                    amount: huge,
                },
                Movement::Sale {
                    tender: Tender::Card,
                    amount: huge,
                },
            ],
            &[
                Movement::Refund {
                    tender: Tender::Card,
                    amount: huge,
                },
                Movement::Refund {
                    tender: Tender::Card,
                    amount: huge,
                },
            ],
            &[Movement::CashIn(huge), Movement::CashIn(huge)],
            &[Movement::CashOut(huge), Movement::CashOut(huge)],
        ];
        for movements in cases {
            assert_eq!(
                close_report(Money::ZERO, Money::ZERO, movements),
                Err(SessionError::AmountOverflow)
            );
        }
        // Expected cash itself overflows.
        assert_eq!(
            close_report(huge, Money::ZERO, &[Movement::CashIn(m(1))]),
            Err(SessionError::AmountOverflow)
        );
        // Over/short overflows.
        assert_eq!(
            close_report(m(i64::MIN + 1), huge, &[]),
            Err(SessionError::AmountOverflow)
        );
    }

    /// Spec 2.1 §9 case 9.
    #[test]
    fn second_open_session_is_rejected() {
        let alice = UserId::generate();
        let bob = UserId::generate();
        let r1 = RegisterId::generate();
        let r2 = RegisterId::generate();
        let existing = open_session(alice, r1);
        let mut closed = open_session(bob, r2);
        closed.close(bob, m(500_000), &[], T).unwrap();
        let sessions = [existing.clone(), closed];

        let cases = [
            (
                "same register, other user",
                bob,
                r1,
                Err(OpenConflict::Register(existing.id())),
            ),
            (
                "same user, other register",
                alice,
                r2,
                Err(OpenConflict::User(existing.id())),
            ),
            (
                "same user and register",
                alice,
                r1,
                Err(OpenConflict::Register(existing.id())),
            ),
            (
                "free register and user (closed session ignored)",
                bob,
                r2,
                Ok(()),
            ),
        ];
        for (name, user, register, expected) in cases {
            let result = CashierSession::open(
                CashierSessionId::generate(),
                user,
                register,
                Money::ZERO,
                T,
                &sessions,
            );
            match expected {
                Ok(()) => assert!(result.unwrap().is_open(), "{name}"),
                Err(conflict) => assert_eq!(
                    result,
                    Err(SessionError::SessionAlreadyOpen { conflict }),
                    "{name}"
                ),
            }
        }
    }

    /// Spec 2.1 §9 case 10.
    #[test]
    fn sale_requires_own_open_session_on_this_register() {
        let alice = UserId::generate();
        let bob = UserId::generate();
        let r1 = RegisterId::generate();
        let r2 = RegisterId::generate();
        let session = open_session(alice, r1);
        let mut closed = open_session(bob, r2);
        closed.close(bob, Money::ZERO, &[], T).unwrap();
        let sessions = [session.clone(), closed];

        assert_eq!(require_open_session(alice, r1, &sessions), Ok(&session));
        let cases = [
            ("no sessions at all", alice, r1, &[][..]),
            (
                "other user's session on this register",
                bob,
                r1,
                &sessions[..],
            ),
            ("own session on another register", alice, r2, &sessions[..]),
            ("own session is closed", bob, r2, &sessions[..]),
        ];
        for (name, user, register, sessions) in cases {
            let err = require_open_session(user, register, sessions).unwrap_err();
            assert_eq!(err, SessionError::NoOpenSession, "{name}");
            assert_eq!(err.code(), "no_open_session");
        }
    }

    #[test]
    fn close_state_machine() {
        let alice = UserId::generate();
        let manager = UserId::generate();
        let register = RegisterId::generate();
        let closed_at = Timestamp::from_unix_millis(9_000);

        // Normal close by someone else is refused; the session stays open.
        let mut s = open_session(alice, register);
        assert_eq!(
            s.close(manager, Money::ZERO, &[], closed_at),
            Err(SessionError::NotSessionOwner)
        );
        assert!(s.is_open());

        // An overflow leaves the session open too.
        assert_eq!(
            s.close(alice, m(i64::MIN), &[], closed_at),
            Err(SessionError::AmountOverflow)
        );
        assert!(s.is_open());

        let report = s.close(alice, m(500_000), &[], closed_at).unwrap();
        assert_eq!(report.over_short, Money::ZERO);
        assert_eq!(
            s.status(),
            SessionStatus::Closed {
                at: closed_at,
                kind: CloseKind::Normal
            }
        );
        // A close cannot be undone or repeated.
        let again = SessionError::AlreadyClosed { session: s.id() };
        assert_eq!(
            s.close(alice, Money::ZERO, &[], closed_at),
            Err(again.clone())
        );
        assert_eq!(
            s.force_close(manager, Money::ZERO, &[], closed_at),
            Err(again)
        );

        // Force close by a manager is marked as forced.
        let mut s = open_session(alice, register);
        let report = s.force_close(manager, m(490_000), &[], closed_at).unwrap();
        assert_eq!(report.over_short, m(-10_000));
        assert_eq!(
            s.status(),
            SessionStatus::Closed {
                at: closed_at,
                kind: CloseKind::Forced { by: manager }
            }
        );
    }

    #[test]
    fn accessors_and_from_parts() {
        let id = CashierSessionId::generate();
        let user = UserId::generate();
        let register = RegisterId::generate();
        let s = CashierSession::from_parts(id, user, register, m(1), T, SessionStatus::Open);
        assert_eq!(
            (
                s.id(),
                s.user(),
                s.register(),
                s.opening_cash(),
                s.opened_at()
            ),
            (id, user, register, m(1), T)
        );
        assert_eq!(s.status(), SessionStatus::Open);
    }

    #[test]
    fn error_codes_and_messages() {
        let id = CashierSessionId::generate();
        let cases = [
            (SessionError::NoOpenSession, "no_open_session"),
            (
                SessionError::SessionAlreadyOpen {
                    conflict: OpenConflict::User(id),
                },
                "session_already_open",
            ),
            (
                SessionError::AlreadyClosed { session: id },
                "session_closed",
            ),
            (SessionError::NotSessionOwner, "permission_denied"),
            (SessionError::AmountOverflow, "amount_overflow"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code);
            assert!(!error.to_string().is_empty());
        }
        assert_eq!(
            SessionError::AlreadyClosed { session: id }.to_string(),
            format!("cashier session {id} is already closed")
        );
    }
}
