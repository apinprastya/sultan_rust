//! Session figures for the close report and the X report, and the float between sessions
//! (spec 2.7 §3, §4, §5).

use std::collections::{BTreeMap, BTreeSet};

use super::movement::{CashKind, CashMovement, CashReason, Direction, is_missing_nota};
use super::{CashError, add, invalid, sub};
use crate::cashier_session::{Movement, Tender};
use crate::ids::{CashMovementId, CashReasonId};
use crate::money::Money;
use crate::sale::{
    CountAmount, SalesSessionReport, SessionEvent, TenderKind, TenderReport, session_report,
};
use crate::time::Timestamp;

/// One reason's line in the report (§4): its originals counted, its amount net of reversals.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReasonLine {
    /// The reason's name (a snapshot, so a Z keeps it after a rename).
    pub name: String,
    /// In or out.
    pub direction: Direction,
    /// The reason's list order.
    pub sort: i32,
    /// Movements with this reason, reversals not counted.
    pub count: u32,
    /// Σ amounts minus Σ reversed amounts.
    pub amount: Money,
    /// Reversals with this reason.
    pub reversals: u32,
}

/// The cash movements of a session, or of a Z (§4).
///
/// Per kind, `count` is the number of movements that are not reversals and `amount` their sum
/// minus the reversals of that kind, i.e. what the kind did to the drawer. `reversals` counts the
/// reversals on their own, with the sum of their amounts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CashFigures {
    /// Cash in.
    pub cash_in: CountAmount,
    /// Cash out.
    pub cash_out: CountAmount,
    /// Safe drops.
    pub drops: CountAmount,
    /// Reversals of any kind.
    pub reversals: CountAmount,
    /// Per reason (in and out).
    pub reasons: BTreeMap<CashReasonId, ReasonLine>,
}

fn push(line: &mut CountAmount, amount: Money) -> Result<(), CashError> {
    line.count = line.count.saturating_add(1);
    line.amount = add(line.amount, amount)?;
    Ok(())
}

fn merge_count(into: &mut CountAmount, other: CountAmount) -> Result<(), CashError> {
    into.count = into.count.saturating_add(other.count);
    into.amount = add(into.amount, other.amount)?;
    Ok(())
}

impl CashFigures {
    /// The figures of `movements`. `reasons` gives the names; a reason missing from it is shown
    /// by its ID.
    ///
    /// # Errors
    /// [`CashError::AmountOverflow`].
    pub fn from_movements(
        movements: &[CashMovement],
        reasons: &[CashReason],
    ) -> Result<Self, CashError> {
        let mut f = Self::default();
        for m in movements {
            let line = match m.kind {
                CashKind::In => &mut f.cash_in,
                CashKind::Out => &mut f.cash_out,
                CashKind::Drop => &mut f.drops,
            };
            if m.is_reversal() {
                line.amount = sub(line.amount, m.amount)?;
                push(&mut f.reversals, m.amount)?;
            } else {
                push(line, m.amount)?;
            }
            if let Some(id) = m.reason {
                let known = reasons.iter().find(|r| r.id == id);
                let r = f.reasons.entry(id).or_insert_with(|| ReasonLine {
                    name: known.map_or_else(|| id.to_string(), |r| r.name.clone()),
                    direction: known.map_or(
                        if m.kind == CashKind::In {
                            Direction::In
                        } else {
                            Direction::Out
                        },
                        |r| r.direction,
                    ),
                    sort: known.map_or(i32::MAX, |r| r.sort),
                    count: 0,
                    amount: Money::ZERO,
                    reversals: 0,
                });
                if m.is_reversal() {
                    r.amount = sub(r.amount, m.amount)?;
                    r.reversals = r.reversals.saturating_add(1);
                } else {
                    r.count = r.count.saturating_add(1);
                    r.amount = add(r.amount, m.amount)?;
                }
            }
        }
        Ok(f)
    }

    /// The netted cash movements as session events for [`crate::sale::session_report`] and
    /// [`close_movements`]: `CashIn`, `CashOut` and `Drop`, each left out when 0.
    #[must_use]
    pub fn events(&self) -> Vec<SessionEvent> {
        [
            SessionEvent::CashIn(self.cash_in.amount),
            SessionEvent::CashOut(self.cash_out.amount),
            SessionEvent::Drop(self.drops.amount),
        ]
        .into_iter()
        .filter(|e| {
            !matches!(
                e,
                SessionEvent::CashIn(Money::ZERO)
                    | SessionEvent::CashOut(Money::ZERO)
                    | SessionEvent::Drop(Money::ZERO)
            )
        })
        .collect()
    }

    /// The reason lines in print order: in before out, then by sort and name.
    #[must_use]
    pub fn reason_lines(&self) -> Vec<&ReasonLine> {
        let mut lines: Vec<_> = self.reasons.values().collect();
        lines.sort_by(|a, b| (a.direction, a.sort, &a.name).cmp(&(b.direction, b.sort, &b.name)));
        lines
    }

    /// Adds `other` (a Z sums its sessions).
    ///
    /// # Errors
    /// [`CashError::AmountOverflow`].
    pub fn merge(&mut self, other: &Self) -> Result<(), CashError> {
        merge_count(&mut self.cash_in, other.cash_in)?;
        merge_count(&mut self.cash_out, other.cash_out)?;
        merge_count(&mut self.drops, other.drops)?;
        merge_count(&mut self.reversals, other.reversals)?;
        for (id, line) in &other.reasons {
            match self.reasons.get_mut(id) {
                Some(r) => {
                    r.count = r.count.saturating_add(line.count);
                    r.reversals = r.reversals.saturating_add(line.reversals);
                    r.amount = add(r.amount, line.amount)?;
                }
                None => {
                    self.reasons.insert(*id, line.clone());
                }
            }
        }
        Ok(())
    }
}

/// What a session's figures are computed from (§4).
#[derive(Debug, Clone, Copy)]
pub struct SessionInput<'a> {
    /// Cash in the drawer at open.
    pub opening_cash: Money,
    /// The sales events of spec 2.6 §10 and the balance payouts; **not** the cash movements,
    /// which come from `movements`.
    pub events: &'a [SessionEvent],
    /// The session's cash movements, reversals included.
    pub movements: &'a [CashMovement],
    /// The cash reasons (archived ones too), for the names.
    pub reasons: &'a [CashReason],
    /// The number of nota photos per movement; a missing entry is 0.
    pub photos: &'a BTreeMap<CashMovementId, usize>,
    /// The company's `nota_flag_above` (§2a).
    pub nota_flag_above: Money,
    /// No-sale drawer opens (`drawer.open` audit entries, K22).
    pub drawer_opens: u32,
    /// X reads (`session.x_report` audit entries, K12).
    pub x_reports: u32,
}

/// A session's figures: everything the close report and the X show except the count (§4, §5).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFigures {
    /// The `sales` block (spec 2.6 §10) with cash in / out / drops netted; its `expected_cash`
    /// is the session's.
    pub sales: SalesSessionReport,
    /// The cash movements per kind and reason.
    pub cash: CashFigures,
    /// No-sale drawer opens.
    pub drawer_opens: u32,
    /// X reads.
    pub x_reports: u32,
    /// Cash outs flagged "missing nota" (§2a).
    pub missing_nota: u32,
}

impl SessionFigures {
    /// `opening_cash + cash received − change given − cash refunds + cash_in − cash_out − drops −
    /// payouts` (§4).
    #[must_use]
    pub const fn expected_cash(&self) -> Money {
        self.sales.expected_cash
    }

    /// Adds `other` field by field (a Z sums its sessions).
    ///
    /// # Errors
    /// [`CashError::AmountOverflow`].
    pub fn merge(&mut self, other: &Self) -> Result<(), CashError> {
        merge_sales(&mut self.sales, &other.sales)?;
        self.cash.merge(&other.cash)?;
        self.drawer_opens = self.drawer_opens.saturating_add(other.drawer_opens);
        self.x_reports = self.x_reports.saturating_add(other.x_reports);
        self.missing_nota = self.missing_nota.saturating_add(other.missing_nota);
        Ok(())
    }
}

fn merge_sales(a: &mut SalesSessionReport, b: &SalesSessionReport) -> Result<(), CashError> {
    for (x, y) in [
        (&mut a.opening_cash, b.opening_cash),
        (&mut a.cash_received, b.cash_received),
        (&mut a.change_given, b.change_given),
        (&mut a.cash_refunds, b.cash_refunds),
        (&mut a.cash_rounding, b.cash_rounding),
        (&mut a.cash_in, b.cash_in),
        (&mut a.cash_out, b.cash_out),
        (&mut a.drops, b.drops),
        (&mut a.payouts, b.payouts),
        (&mut a.expected_cash, b.expected_cash),
        (&mut a.saved_change, b.saved_change),
        (&mut a.balance_paid, b.balance_paid),
    ] {
        *x = add(*x, y)?;
    }
    for (x, y) in [
        (&mut a.line_voids, b.line_voids),
        (&mut a.cart_voids, b.cart_voids),
        (&mut a.post_voids, b.post_voids),
        (&mut a.returns, b.returns),
        (&mut a.line_discounts, b.line_discounts),
        (&mut a.cart_discounts, b.cart_discounts),
        (&mut a.price_overrides, b.price_overrides),
    ] {
        merge_count(x, y)?;
    }
    for (key, t) in &b.tenders {
        let into: &mut TenderReport = a.tenders.entry(*key).or_default();
        merge_count(&mut into.sales, t.sales)?;
        merge_count(&mut into.refunds, t.refunds)?;
    }
    Ok(())
}

/// Computes a session's figures (§4): the cash movements per kind and reason, the sales block
/// with the netted cash movements, and the missing-nota count as of now.
///
/// # Errors
/// [`CashError::AmountOverflow`].
pub fn session_figures(input: &SessionInput<'_>) -> Result<SessionFigures, CashError> {
    let cash = CashFigures::from_movements(input.movements, input.reasons)?;
    let mut events = input.events.to_vec();
    events.extend(cash.events());
    let sales =
        session_report(input.opening_cash, &events).map_err(|_| CashError::AmountOverflow)?;
    let reversed: BTreeSet<CashMovementId> =
        input.movements.iter().filter_map(|m| m.reverses).collect();
    let missing = input
        .movements
        .iter()
        .filter(|m| {
            let photos = input.photos.get(&m.id).copied().unwrap_or(0);
            is_missing_nota(m, photos, reversed.contains(&m.id), input.nota_flag_above)
        })
        .count();
    Ok(SessionFigures {
        sales,
        cash,
        drawer_opens: input.drawer_opens,
        x_reports: input.x_reports,
        missing_nota: u32::try_from(missing).unwrap_or(u32::MAX),
    })
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

/// The session events as movements of the 2.1 close-report block
/// ([`crate::cashier_session::close_report`]). A cash tender counts the cash that stays in the
/// drawer (`amount + change_saved`); card, QRIS and transfer their amount; other tenders move no
/// cash and are left out. `CashOut`, `Drop` and `Payout` all become [`Movement::CashOut`], so the
/// block's `cash_out` = cash out + drops + payouts (§4) and its expected cash equals the `sales`
/// block's.
#[must_use]
pub fn close_movements(events: &[SessionEvent]) -> Vec<Movement> {
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
            SessionEvent::CashOut(a) | SessionEvent::Drop(a) | SessionEvent::Payout(a) => {
                out.push(Movement::CashOut(a));
            }
            _ => {}
        }
    }
    out
}

/// The float of a closed session (§3, K20).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct FloatFigures {
    /// Cash kept in the drawer for the next session.
    pub float_kept: Money,
    /// `counted − float_kept`, taken to the safe.
    pub to_safe: Money,
    /// The proposed opening cash, when the cashier typed another amount at open.
    pub opening_proposed: Option<Money>,
    /// `opening_cash − opening_proposed` (0 without a proposal).
    pub float_difference: Money,
}

/// The cash to keep as float (§3): `requested`, or by default the opening cash (at most the
/// counted cash, and not below 0).
///
/// # Errors
/// [`CashError::InvalidField`] (`float_kept`) below 0 or above the counted cash.
pub fn float_kept(
    opening_cash: Money,
    counted: Money,
    requested: Option<Money>,
) -> Result<Money, CashError> {
    let kept = requested.unwrap_or_else(|| opening_cash.min(counted).max(Money::ZERO));
    if kept.is_negative() {
        Err(invalid("float_kept", "must not be negative"))
    } else if kept > counted {
        Err(invalid("float_kept", "at most the counted cash"))
    } else {
        Ok(kept)
    }
}

/// The opening cash the open screen proposes (§3): the `float_kept` of the register's last
/// closed session, 0 if none.
#[must_use]
pub fn proposed_float(last_float_kept: Option<Money>) -> Money {
    last_float_kept.unwrap_or(Money::ZERO)
}

/// What a session stores as `opening_proposed` (§3): the proposal when the typed opening cash
/// differs from it, else nothing.
#[must_use]
pub fn opening_proposed(proposal: Money, typed: Money) -> Option<Money> {
    (typed != proposal).then_some(proposal)
}

/// The float difference (§3): `opening_cash − opening_proposed`, 0 without a proposal.
///
/// # Errors
/// [`CashError::AmountOverflow`].
pub fn float_difference(
    opening_cash: Money,
    opening_proposed: Option<Money>,
) -> Result<Money, CashError> {
    opening_proposed.map_or(Ok(Money::ZERO), |p| sub(opening_cash, p))
}

/// The float figures of a close (§3).
///
/// # Errors
/// [`CashError::InvalidField`] (`float_kept`) or [`CashError::AmountOverflow`].
pub fn float_figures(
    opening_cash: Money,
    counted: Money,
    requested: Option<Money>,
    opening_proposed: Option<Money>,
) -> Result<FloatFigures, CashError> {
    let float_kept = float_kept(opening_cash, counted, requested)?;
    Ok(FloatFigures {
        float_kept,
        to_safe: sub(counted, float_kept)?,
        opening_proposed,
        float_difference: float_difference(opening_cash, opening_proposed)?,
    })
}

/// The close report's figures (§4): the session's figures, the count, over/short and the float.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CloseFigures {
    /// Everything also on the X.
    pub session: SessionFigures,
    /// What the cashier counted.
    pub counted_cash: Money,
    /// `counted_cash − expected_cash`: positive = over, negative = short.
    pub over_short: Money,
    /// Float kept, to safe, float difference.
    pub float: FloatFigures,
}

/// Closes the figures of a session with the count (§3, §4). `float_kept` is the typed float
/// (`None` = the default, see [`float_kept`]).
///
/// # Errors
/// [`CashError::InvalidField`] (`float_kept`) or [`CashError::AmountOverflow`].
pub fn close_figures(
    session: SessionFigures,
    counted_cash: Money,
    float_kept: Option<Money>,
    opening_proposed: Option<Money>,
) -> Result<CloseFigures, CashError> {
    let float = float_figures(
        session.sales.opening_cash,
        counted_cash,
        float_kept,
        opening_proposed,
    )?;
    let over_short = sub(counted_cash, session.expected_cash())?;
    Ok(CloseFigures {
        session,
        counted_cash,
        over_short,
        float,
    })
}

/// An X report (§5): the session's figures as of `read_at`, with the expected cash. It has no
/// counted cash and no over/short: the type has no field for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XReport {
    /// The figures as of now.
    pub session: SessionFigures,
    /// The proposed opening cash, when another amount was typed (§3).
    pub opening_proposed: Option<Money>,
    /// `opening_cash − opening_proposed`.
    pub float_difference: Money,
    /// When it was read.
    pub read_at: Timestamp,
}

/// The X report of an open session (§5).
///
/// # Errors
/// [`CashError::AmountOverflow`].
pub fn x_report(
    session: SessionFigures,
    opening_proposed: Option<Money>,
    read_at: Timestamp,
) -> Result<XReport, CashError> {
    let float_difference = float_difference(session.sales.opening_cash, opening_proposed)?;
    Ok(XReport {
        session,
        opening_proposed,
        float_difference,
        read_at,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::cashier_session::close_report;
    use crate::ids::{CashierSessionId, TenderTypeId};
    use crate::sale::TenderKey;

    pub(crate) fn m(v: i64) -> Money {
        Money::from_minor(v)
    }

    pub(crate) fn key(kind: TenderKind) -> TenderKey {
        TenderKey {
            kind,
            tender_type: None,
        }
    }

    pub(crate) fn cash_tender(amount: i64, received: i64, change: i64) -> SessionEvent {
        SessionEvent::Tender {
            key: key(TenderKind::Cash),
            amount: m(amount),
            cash_received: Some(m(received)),
            change_given: m(change),
            change_saved: m(0),
        }
    }

    pub(crate) fn mv(
        session: CashierSessionId,
        kind: CashKind,
        amount: i64,
        reason: Option<&CashReason>,
    ) -> CashMovement {
        CashMovement {
            id: CashMovementId::generate(),
            session,
            kind,
            amount: m(amount),
            reason: reason.map(|r| r.id),
            note: None,
            reverses: None,
        }
    }

    pub(crate) fn rev(of: &CashMovement) -> CashMovement {
        CashMovement::reversal(
            CashMovementId::generate(),
            of.session,
            of,
            false,
            Some("salah"),
        )
        .unwrap()
    }

    pub(crate) fn reason(name: &str, direction: Direction, sort: i32) -> CashReason {
        CashReason {
            id: CashReasonId::generate(),
            name: name.into(),
            direction,
            active: true,
            sort,
            note_required: false,
        }
    }

    /// The scripted session of spec §12 case 1.
    pub(crate) struct Case1 {
        pub(crate) events: Vec<SessionEvent>,
        pub(crate) movements: Vec<CashMovement>,
        pub(crate) reasons: Vec<CashReason>,
    }

    pub(crate) fn case1() -> Case1 {
        let session = CashierSessionId::generate();
        let top_up = reason("Tambahan modal", Direction::In, 10);
        let shop = reason("Belanja", Direction::Out, 10);
        // Cash received 500 000, change 36 500 (sales of 463 500), a cash refund of 20 000, a
        // payout of 10 000.
        let events = vec![
            cash_tender(263_500, 300_000, 36_500),
            cash_tender(200_000, 200_000, 0),
            SessionEvent::Refund {
                key: key(TenderKind::Cash),
                amount: m(20_000),
            },
            SessionEvent::Payout(m(10_000)),
        ];
        let movements = vec![
            mv(session, CashKind::In, 50_000, Some(&top_up)),
            mv(session, CashKind::Out, 30_000, Some(&shop)),
            mv(session, CashKind::Drop, 300_000, None),
        ];
        Case1 {
            events,
            movements,
            reasons: vec![top_up, shop],
        }
    }

    fn input<'a>(
        events: &'a [SessionEvent],
        movements: &'a [CashMovement],
        reasons: &'a [CashReason],
        photos: &'a BTreeMap<CashMovementId, usize>,
    ) -> SessionInput<'a> {
        SessionInput {
            opening_cash: m(200_000),
            events,
            movements,
            reasons,
            photos,
            nota_flag_above: m(100_000),
            drawer_opens: 2,
            x_reports: 1,
        }
    }

    /// Spec §12 cases 1 and 2 (and case 8's expected cash): every case gives the same expected
    /// cash in the `sales` block and in the 2.1 block.
    #[test]
    fn expected_cash_in_both_blocks() {
        let c = case1();
        let session = c.movements[0].session;
        let big_out = mv(session, CashKind::Out, 25_000, Some(&c.reasons[1]));
        let mut with_reversal = c.movements.clone();
        with_reversal.push(big_out.clone());
        with_reversal.push(rev(&big_out));
        let card = SessionEvent::Tender {
            key: TenderKey {
                kind: TenderKind::Card,
                tender_type: Some(TenderTypeId::generate()),
            },
            amount: m(75_000),
            cash_received: None,
            change_given: m(0),
            change_saved: m(0),
        };
        let mut with_card = c.events.clone();
        with_card.push(card);
        let only_out = [big_out.clone()];
        let out_and_reversal = [big_out.clone(), rev(&big_out)];
        #[rustfmt::skip]
        let cases: [(&str, &[SessionEvent], &[CashMovement], i64); 7] = [
            ("case 1", &c.events, &c.movements, 353_500),
            ("case 1 + a reversed cash out", &c.events, &with_reversal, 353_500),
            ("case 1 + a card sale", &with_card, &c.movements, 353_500),
            ("no cash movements", &c.events, &[], 633_500),
            ("nothing", &[], &[], 200_000),
            ("a cash out", &[], &only_out, 175_000),
            ("its reversal: back to before", &[], &out_and_reversal, 200_000),
        ];
        let photos = BTreeMap::new();
        for (name, events, movements, expected) in cases {
            let f = session_figures(&input(events, movements, &c.reasons, &photos)).unwrap();
            assert_eq!(f.expected_cash(), m(expected), "{name}");
            let mut all = events.to_vec();
            all.extend(f.cash.events());
            let block = close_report(m(200_000), m(0), &close_movements(&all)).unwrap();
            assert_eq!(block.expected_cash, m(expected), "{name}: 2.1 block");
            // The 2.1 block's cash_out = cash out + drops + payouts.
            assert_eq!(
                block.cash_out,
                add(
                    add(f.sales.cash_out, f.sales.drops).unwrap(),
                    f.sales.payouts
                )
                .unwrap(),
                "{name}"
            );
        }
    }

    #[test]
    fn cash_figures_per_kind_and_reason() {
        let c = case1();
        let session = c.movements[0].session;
        let shop = &c.reasons[1];
        let other = reason("Lainnya", Direction::Out, 90);
        let unknown = reason("Gone", Direction::In, 5);
        let out2 = mv(session, CashKind::Out, 25_000, Some(shop));
        let drop2 = mv(session, CashKind::Drop, 100_000, None);
        let mut movements = c.movements.clone();
        movements.extend([
            out2.clone(),
            rev(&out2),
            mv(session, CashKind::Out, 150_000, Some(&other)),
            drop2.clone(),
            rev(&drop2),
            mv(session, CashKind::In, 1_000, Some(&unknown)),
        ]);
        let mut reasons = c.reasons.clone();
        reasons.push(other.clone());
        let f = CashFigures::from_movements(&movements, &reasons).unwrap();
        let ca = |count, amount| CountAmount {
            count,
            amount: m(amount),
        };
        assert_eq!(
            (f.cash_in, f.cash_out, f.drops, f.reversals),
            (
                ca(2, 51_000),
                ca(3, 180_000),
                ca(2, 300_000),
                ca(2, 125_000)
            )
        );
        let lines: Vec<_> = f
            .reason_lines()
            .iter()
            .map(|l| {
                (
                    l.name.clone(),
                    l.direction,
                    l.count,
                    l.amount.minor(),
                    l.reversals,
                )
            })
            .collect();
        assert_eq!(
            lines,
            [
                ("Tambahan modal".to_owned(), Direction::In, 1, 50_000, 0),
                (unknown.id.to_string(), Direction::In, 1, 1_000, 0),
                ("Belanja".to_owned(), Direction::Out, 2, 30_000, 1),
                ("Lainnya".to_owned(), Direction::Out, 1, 150_000, 0),
            ]
        );
        assert_eq!(
            f.events(),
            [
                SessionEvent::CashIn(m(51_000)),
                SessionEvent::CashOut(m(180_000)),
                SessionEvent::Drop(m(300_000)),
            ]
        );
        // An unknown out reason defaults to out.
        let lost = mv(
            session,
            CashKind::Out,
            1,
            Some(&reason("x", Direction::Out, 1)),
        );
        let f = CashFigures::from_movements(&[lost], &[]).unwrap();
        assert_eq!(f.reason_lines()[0].direction, Direction::Out);
        assert!(CashFigures::default().events().is_empty());

        // Merging adds everything, reasons by ID.
        let mut twice = CashFigures::from_movements(&movements, &reasons).unwrap();
        let once = twice.clone();
        twice.merge(&once).unwrap();
        assert_eq!(twice.cash_out, ca(6, 360_000));
        assert_eq!(twice.reversals, ca(4, 250_000));
        assert_eq!(twice.reasons[&shop.id].count, 4);
        assert_eq!(twice.reasons[&shop.id].reversals, 2);
        let mut empty = CashFigures::default();
        empty.merge(&once).unwrap();
        assert_eq!(empty, once);
    }

    #[test]
    fn session_counters_and_missing_nota() {
        let c = case1();
        let session = c.movements[0].session;
        let shop = &c.reasons[1];
        let flagged = mv(session, CashKind::Out, 100_000, Some(shop));
        let with_photo = mv(session, CashKind::Out, 150_000, Some(shop));
        let reversed = mv(session, CashKind::Out, 150_000, Some(shop));
        let mut movements = c.movements.clone();
        movements.extend([
            flagged,
            with_photo.clone(),
            reversed.clone(),
            rev(&reversed),
            mv(session, CashKind::Out, 99_999, Some(shop)),
        ]);
        let photos = BTreeMap::from([(with_photo.id, 1)]);
        let f = session_figures(&input(&c.events, &movements, &c.reasons, &photos)).unwrap();
        assert_eq!((f.drawer_opens, f.x_reports, f.missing_nota), (2, 1, 1));
        let mut off = input(&c.events, &movements, &c.reasons, &photos);
        off.nota_flag_above = m(0);
        assert_eq!(session_figures(&off).unwrap().missing_nota, 0);

        // Merging two sessions sums every field.
        let mut sum = f.clone();
        sum.merge(&f).unwrap();
        assert_eq!(sum.expected_cash(), m(f.expected_cash().minor() * 2));
        assert_eq!(sum.sales.opening_cash, m(400_000));
        assert_eq!(sum.sales.drops, m(600_000));
        assert_eq!(sum.sales.tenders[&key(TenderKind::Cash)].sales.count, 4);
        assert_eq!(
            (sum.drawer_opens, sum.x_reports, sum.missing_nota),
            (4, 2, 2)
        );
        assert_eq!(sum.cash.cash_out.count, 2 * f.cash.cash_out.count);
    }

    #[test]
    fn overflow_is_reported() {
        let session = CashierSessionId::generate();
        let huge = |kind| mv(session, kind, i64::MAX, None);
        let photos = BTreeMap::new();
        let twice = [huge(CashKind::Drop), huge(CashKind::Drop)];
        assert_eq!(
            CashFigures::from_movements(&twice, &[]),
            Err(CashError::AmountOverflow)
        );
        // The figures fit, the expected cash does not.
        let one = [huge(CashKind::Drop)];
        let mut i = input(&[], &one, &[], &photos);
        i.opening_cash = m(-2);
        assert_eq!(session_figures(&i), Err(CashError::AmountOverflow));
        let mut f = SessionFigures::default();
        f.sales.cash_in = m(i64::MAX);
        let g = f.clone();
        assert_eq!(f.merge(&g), Err(CashError::AmountOverflow));
        let mut f = SessionFigures::default();
        f.sales.line_voids.amount = m(i64::MAX);
        let g = f.clone();
        assert_eq!(f.merge(&g), Err(CashError::AmountOverflow));
        let mut f = SessionFigures::default();
        f.sales
            .tenders
            .entry(key(TenderKind::Cash))
            .or_default()
            .sales
            .amount = m(i64::MAX);
        let g = f.clone();
        assert_eq!(f.merge(&g), Err(CashError::AmountOverflow));
    }

    #[test]
    fn close_movements_map_every_event() {
        use SessionEvent as E;
        let tender = |kind, amount, saved| E::Tender {
            key: key(kind),
            amount: m(amount),
            cash_received: None,
            change_given: m(0),
            change_saved: m(saved),
        };
        let events = [
            tender(TenderKind::Cash, 100, 5),
            tender(TenderKind::Card, 200, 0),
            tender(TenderKind::Qris, 300, 0),
            tender(TenderKind::Transfer, 400, 0),
            tender(TenderKind::CustomerBalance, 500, 0),
            E::Refund {
                key: key(TenderKind::Cash),
                amount: m(7),
            },
            E::Refund {
                key: key(TenderKind::CustomerBalance),
                amount: m(8),
            },
            E::CashIn(m(10)),
            E::CashOut(m(20)),
            E::Drop(m(25)),
            E::Payout(m(30)),
            E::LineVoid(m(99)),
        ];
        assert_eq!(
            close_movements(&events),
            [
                Movement::Sale {
                    tender: Tender::Cash,
                    amount: m(105)
                },
                Movement::Sale {
                    tender: Tender::Card,
                    amount: m(200)
                },
                Movement::Sale {
                    tender: Tender::Qris,
                    amount: m(300)
                },
                Movement::Sale {
                    tender: Tender::Transfer,
                    amount: m(400)
                },
                Movement::Refund {
                    tender: Tender::Cash,
                    amount: m(7)
                },
                Movement::CashIn(m(10)),
                Movement::CashOut(m(20)),
                Movement::CashOut(m(25)),
                Movement::CashOut(m(30)),
            ]
        );
    }

    /// Name, opening, counted, typed float → the float kept, or the error reason.
    type FloatCase = (
        &'static str,
        i64,
        i64,
        Option<i64>,
        Result<i64, &'static str>,
    );

    /// Spec §12 cases 9 and 10.
    #[test]
    fn float_rules() {
        let ok = |v: i64| -> Result<i64, &'static str> { Ok(v) };
        #[rustfmt::skip]
        let cases: [FloatCase; 7] = [
            ("default = opening cash", 200_000, 1_250_000, None, ok(200_000)),
            ("typed", 200_000, 1_250_000, Some(500_000), ok(500_000)),
            ("everything", 200_000, 1_250_000, Some(1_250_000), ok(1_250_000)),
            ("nothing", 200_000, 1_250_000, Some(0), ok(0)),
            ("default capped at the count", 200_000, 150_000, None, ok(150_000)),
            ("above the count", 200_000, 1_250_000, Some(1_300_000), Err("at most the counted cash")),
            ("negative", 200_000, 1_250_000, Some(-1), Err("must not be negative")),
        ];
        for (name, opening, counted, requested, expected) in cases {
            let got = float_figures(m(opening), m(counted), requested.map(m), None);
            match expected {
                Ok(kept) => {
                    let f = got.unwrap();
                    assert_eq!(f.float_kept, m(kept), "{name}");
                    assert_eq!(f.to_safe, m(counted - kept), "{name}");
                    assert_eq!(f.float_difference, m(0), "{name}");
                }
                Err(reason) => assert_eq!(got, Err(invalid("float_kept", reason)), "{name}"),
            }
        }
        assert_eq!(
            float_figures(m(200_000), m(1_250_000), None, None)
                .unwrap()
                .to_safe,
            m(1_050_000)
        );

        // Case 10: the last close kept 200 000, the next open types 150 000.
        let proposal = proposed_float(Some(m(200_000)));
        assert_eq!(proposal, m(200_000));
        assert_eq!(proposed_float(None), m(0));
        let stored = opening_proposed(proposal, m(150_000));
        assert_eq!(stored, Some(m(200_000)));
        assert_eq!(opening_proposed(proposal, m(200_000)), None);
        assert_eq!(float_difference(m(150_000), stored), Ok(m(-50_000)));
        assert_eq!(float_difference(m(150_000), None), Ok(m(0)));
        assert_eq!(
            float_difference(m(i64::MIN), Some(m(1))),
            Err(CashError::AmountOverflow)
        );
    }

    #[test]
    fn close_and_x() {
        let c = case1();
        let photos = BTreeMap::new();
        let f = session_figures(&input(&c.events, &c.movements, &c.reasons, &photos)).unwrap();
        let close = close_figures(f.clone(), m(350_000), None, Some(m(250_000))).unwrap();
        assert_eq!(close.over_short, m(-3_500));
        assert_eq!(close.counted_cash, m(350_000));
        assert_eq!(
            close.float,
            FloatFigures {
                float_kept: m(200_000),
                to_safe: m(150_000),
                opening_proposed: Some(m(250_000)),
                float_difference: m(-50_000),
            }
        );
        assert_eq!(
            close_figures(f.clone(), m(350_000), Some(m(400_000)), None),
            Err(invalid("float_kept", "at most the counted cash"))
        );
        assert_eq!(
            close_figures(f.clone(), m(i64::MIN), Some(m(i64::MIN)), None),
            Err(invalid("float_kept", "must not be negative"))
        );
        let mut odd = f.clone();
        odd.sales.expected_cash = m(-1);
        assert_eq!(
            close_figures(odd, m(i64::MAX), None, None),
            Err(CashError::AmountOverflow)
        );

        let read_at = Timestamp::from_unix_millis(42);
        let x = x_report(f.clone(), Some(m(250_000)), read_at).unwrap();
        assert_eq!(x.session.expected_cash(), m(353_500));
        assert_eq!((x.float_difference, x.read_at), (m(-50_000), read_at));
        let mut odd = f;
        odd.sales.opening_cash = m(i64::MIN);
        assert_eq!(
            x_report(odd, Some(m(1)), read_at),
            Err(CashError::AmountOverflow)
        );
    }
}
