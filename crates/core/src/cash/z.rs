//! The Z report: end of day per branch (spec 2.7 §6, K13–K18).

use std::collections::BTreeMap;

use super::figures::{CloseFigures, SessionFigures};
use super::{CashError, add, invalid, sub};
use crate::doc_number::{BranchTimeZone, LocalDay};
use crate::ids::CashierSessionId;
use crate::money::Money;
use crate::sale::TaxLine;
use crate::tax::{TaxSettings, line_tax};
use crate::time::Timestamp;

/// The business day of a session (K14): the branch-local day of its **open** time, the date in
/// its number. A session opened at 21:00 and closed at 00:30 belongs to the day it started.
#[must_use]
pub fn business_day(opened_at: Timestamp, tz: BranchTimeZone) -> LocalDay {
    LocalDay::of(opened_at, tz)
}

/// A session of the branch as the Z run check sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZCandidate<'a> {
    /// The session.
    pub id: CashierSessionId,
    /// Its number, for `sessions_open`.
    pub number: &'a str,
    /// Its business day ([`business_day`]).
    pub business_day: LocalDay,
    /// Still open.
    pub open: bool,
    /// Already in a Z.
    pub in_z: bool,
}

/// Checks whether a Z of `day` can run (§6) and returns the sessions it covers: every closed
/// session of `day` not yet in a Z. Sessions of other days never block (K15).
///
/// # Errors
/// - `day` after `today` (the branch-local date now) → [`CashError::InvalidField`] (`day`);
/// - a session of `day` still open → [`CashError::SessionsOpen`] with their numbers;
/// - nothing left → [`CashError::NothingToClose`].
pub fn check_z_run(
    day: LocalDay,
    today: LocalDay,
    sessions: &[ZCandidate<'_>],
) -> Result<Vec<CashierSessionId>, CashError> {
    if day > today {
        return Err(invalid("day", "after today"));
    }
    let of_day = || sessions.iter().filter(|s| s.business_day == day);
    let open: Vec<String> = of_day()
        .filter(|s| s.open)
        .map(|s| s.number.to_owned())
        .collect();
    if !open.is_empty() {
        return Err(CashError::SessionsOpen { numbers: open });
    }
    let covered: Vec<_> = of_day().filter(|s| !s.in_z).map(|s| s.id).collect();
    if covered.is_empty() {
        Err(CashError::NothingToClose)
    } else {
        Ok(covered)
    }
}

/// A sale number and when it was posted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SaleRef {
    /// The sale number.
    pub number: String,
    /// When it was posted.
    pub posted_at: Timestamp,
}

/// The sales documents of a session that the Z needs beyond its close figures.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionSales {
    /// Sales posted in the session.
    pub count: u32,
    /// Σ gross of their lines (before discounts).
    pub gross: Money,
    /// The first sale.
    pub first: Option<SaleRef>,
    /// The last sale.
    pub last: Option<SaleRef>,
    /// Their tax per rate (rate 0 included), as stored on the documents.
    pub tax: Vec<TaxLine>,
    /// The tax per rate of the returns and post-voids **paid in** the session (spec 2.6 D10),
    /// subtracted.
    pub returns_tax: Vec<TaxLine>,
}

/// The tax summary of document lines (§6, spec 2.3 §7): per rate (0 included, lowest first), the
/// base (DPP) and the tax of `(amount, rate_bp)` lines by [`line_tax`].
#[must_use]
pub fn tax_summary(lines: &[(Money, u16)], settings: TaxSettings) -> Vec<TaxLine> {
    let mut by_rate: BTreeMap<u16, TaxLine> = BTreeMap::new();
    for &(amount, rate_bp) in lines {
        let t = line_tax(amount, rate_bp, settings);
        let e = by_rate.entry(rate_bp).or_insert(TaxLine {
            rate_bp,
            base: Money::ZERO,
            tax: Money::ZERO,
        });
        e.base = e.base.saturating_add(t.net);
        e.tax = e.tax.saturating_add(t.tax);
    }
    by_rate.into_values().collect()
}

/// Adds (or with `subtract`, removes) `lines` into the per-rate `into`, kept lowest rate first.
fn merge_tax(into: &mut Vec<TaxLine>, lines: &[TaxLine], subtract: bool) -> Result<(), CashError> {
    for l in lines {
        let at = match into.binary_search_by_key(&l.rate_bp, |t| t.rate_bp) {
            Ok(at) => at,
            Err(at) => {
                into.insert(
                    at,
                    TaxLine {
                        rate_bp: l.rate_bp,
                        base: Money::ZERO,
                        tax: Money::ZERO,
                    },
                );
                at
            }
        };
        let t = &mut into[at];
        if subtract {
            t.base = sub(t.base, l.base)?;
            t.tax = sub(t.tax, l.tax)?;
        } else {
            t.base = add(t.base, l.base)?;
            t.tax = add(t.tax, l.tax)?;
        }
    }
    Ok(())
}

/// A closed session going into a Z.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZSession<'a> {
    /// The session number.
    pub number: &'a str,
    /// The register code.
    pub register: &'a str,
    /// The cashier's name.
    pub cashier: &'a str,
    /// Opened at.
    pub opened_at: Timestamp,
    /// Closed at.
    pub closed_at: Timestamp,
    /// Force-closed (spec 2.1 §5).
    pub forced: bool,
    /// Its stored close figures.
    pub close: &'a CloseFigures,
    /// Its sales documents.
    pub sales: &'a SessionSales,
}

/// One session in the Z's session list (§6).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ZSessionLine {
    /// The session number.
    pub number: String,
    /// The register code.
    pub register: String,
    /// The cashier's name.
    pub cashier: String,
    /// Opened at.
    pub opened_at: Timestamp,
    /// Closed at.
    pub closed_at: Timestamp,
    /// Force-closed.
    pub forced: bool,
    /// Opening cash.
    pub opening_cash: Money,
    /// Expected cash.
    pub expected_cash: Money,
    /// Counted cash.
    pub counted_cash: Money,
    /// Over (+) / short (−).
    pub over_short: Money,
    /// Float kept.
    pub float_kept: Money,
    /// To the safe.
    pub to_safe: Money,
    /// Float difference at open.
    pub float_difference: Money,
}

/// The totals of a register or of the branch in a Z (§6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ZTotals {
    /// The summed session figures: tenders, cash received, refunds, rounding, voids, returns,
    /// discounts, overrides, saved change, balance paid, payouts, cash in / out per reason,
    /// drops, reversals, drawer opens, X reads, missing notas.
    pub figures: SessionFigures,
    /// Sessions.
    pub sessions: u32,
    /// Sales posted.
    pub sales_count: u32,
    /// Σ gross of the sales.
    pub gross_sales: Money,
    /// Gross sales − line discounts − cart discounts.
    pub net_sales: Money,
    /// The first sale (by posting time).
    pub first_sale: Option<SaleRef>,
    /// The last sale (by posting time).
    pub last_sale: Option<SaleRef>,
    /// The tax per rate, sales minus returns.
    pub tax: Vec<TaxLine>,
    /// Σ counted cash.
    pub counted_cash: Money,
    /// Σ over/short.
    pub over_short: Money,
    /// Σ float kept.
    pub float_kept: Money,
    /// Σ to safe.
    pub to_safe: Money,
    /// Σ float differences.
    pub float_difference: Money,
    /// Σ drops + Σ to safe.
    pub cash_to_deposit: Money,
}

fn earlier(a: Option<SaleRef>, b: Option<&SaleRef>, first: bool) -> Option<SaleRef> {
    match (a, b) {
        (None, b) => b.cloned(),
        (a, None) => a,
        (Some(a), Some(b)) => {
            let b_wins = if first {
                (&b.posted_at, &b.number) < (&a.posted_at, &a.number)
            } else {
                (&b.posted_at, &b.number) > (&a.posted_at, &a.number)
            };
            Some(if b_wins { b.clone() } else { a })
        }
    }
}

impl ZTotals {
    fn add_session(&mut self, s: &ZSession<'_>) -> Result<(), CashError> {
        let c = s.close;
        let f = &c.session.sales;
        self.figures.merge(&c.session)?;
        self.sessions = self.sessions.saturating_add(1);
        self.sales_count = self.sales_count.saturating_add(s.sales.count);
        self.gross_sales = add(self.gross_sales, s.sales.gross)?;
        let net = sub(
            sub(s.sales.gross, f.line_discounts.amount)?,
            f.cart_discounts.amount,
        )?;
        self.net_sales = add(self.net_sales, net)?;
        self.first_sale = earlier(self.first_sale.take(), s.sales.first.as_ref(), true);
        self.last_sale = earlier(self.last_sale.take(), s.sales.last.as_ref(), false);
        merge_tax(&mut self.tax, &s.sales.tax, false)?;
        merge_tax(&mut self.tax, &s.sales.returns_tax, true)?;
        self.counted_cash = add(self.counted_cash, c.counted_cash)?;
        self.over_short = add(self.over_short, c.over_short)?;
        self.float_kept = add(self.float_kept, c.float.float_kept)?;
        self.to_safe = add(self.to_safe, c.float.to_safe)?;
        self.float_difference = add(self.float_difference, c.float.float_difference)?;
        self.cash_to_deposit = add(
            self.cash_to_deposit,
            add(c.session.cash.drops.amount, c.float.to_safe)?,
        )?;
        Ok(())
    }
}

/// A Z report (§6): one snapshot, never recomputed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZReport {
    /// `JKT01-Z-261119-01` ([`crate::doc_number::z_number`]).
    pub number: String,
    /// The business day.
    pub day: LocalDay,
    /// The Z's sequence within the day, from 1.
    pub seq: u32,
    /// A later Z of the same day (K16): `seq > 1`.
    pub supplementary: bool,
    /// When it was run.
    pub run_at: Timestamp,
    /// The sessions, by open time.
    pub sessions: Vec<ZSessionLine>,
    /// Subtotals per register code.
    pub registers: BTreeMap<String, ZTotals>,
    /// Branch totals.
    pub branch: ZTotals,
}

/// Builds the Z of `day` from the closed sessions it covers (§6): the session list by open time,
/// per-register subtotals and branch totals.
///
/// # Errors
/// [`CashError::NothingToClose`] without sessions, or [`CashError::AmountOverflow`].
pub fn build_z(
    number: &str,
    day: LocalDay,
    seq: u32,
    run_at: Timestamp,
    sessions: &[ZSession<'_>],
) -> Result<ZReport, CashError> {
    if sessions.is_empty() {
        return Err(CashError::NothingToClose);
    }
    let mut sorted: Vec<&ZSession<'_>> = sessions.iter().collect();
    sorted.sort_by(|a, b| (a.opened_at, a.number).cmp(&(b.opened_at, b.number)));
    let mut registers: BTreeMap<String, ZTotals> = BTreeMap::new();
    let mut branch = ZTotals::default();
    let mut lines = Vec::with_capacity(sorted.len());
    for s in sorted {
        registers
            .entry(s.register.to_owned())
            .or_default()
            .add_session(s)?;
        branch.add_session(s)?;
        let c = s.close;
        lines.push(ZSessionLine {
            number: s.number.to_owned(),
            register: s.register.to_owned(),
            cashier: s.cashier.to_owned(),
            opened_at: s.opened_at,
            closed_at: s.closed_at,
            forced: s.forced,
            opening_cash: c.session.sales.opening_cash,
            expected_cash: c.session.expected_cash(),
            counted_cash: c.counted_cash,
            over_short: c.over_short,
            float_kept: c.float.float_kept,
            to_safe: c.float.to_safe,
            float_difference: c.float.float_difference,
        });
    }
    Ok(ZReport {
        number: number.to_owned(),
        day,
        seq,
        supplementary: seq > 1,
        run_at,
        sessions: lines,
        registers,
        branch,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::cash::figures::tests::{case1, cash_tender, m, mv, rev};
    use crate::cash::{CashKind, SessionInput, close_figures, session_figures};
    use crate::doc_number::z_number;
    use crate::sale::SessionEvent;

    const HOUR: i64 = 3_600_000;
    /// 2026-11-19T00:00:00Z.
    pub(crate) const NOV19_UTC: i64 = 1_795_046_400_000;

    fn jkt() -> BranchTimeZone {
        BranchTimeZone::default()
    }

    fn day_of(at: i64) -> LocalDay {
        business_day(Timestamp::from_unix_millis(at), jkt())
    }

    /// Spec §12 case 11.
    #[test]
    fn business_day_is_the_open_day() {
        // 21:00 WIB on 19/11 = 14:00Z; closed 00:30 WIB on 20/11 = 17:30Z.
        let opened = NOV19_UTC + 14 * HOUR;
        let closed = NOV19_UTC + 17 * HOUR + HOUR / 2;
        assert_eq!(day_of(opened).ymd(), (2026, 11, 19));
        assert_eq!(day_of(closed).ymd(), (2026, 11, 20));
        assert_eq!(z_number("JKT01", day_of(opened), 1), "JKT01-Z-261119-01");
    }

    /// Name, day, today, sessions → the covered sessions or the error.
    type RunCase<'a> = (
        &'a str,
        LocalDay,
        LocalDay,
        Vec<ZCandidate<'a>>,
        Result<Vec<CashierSessionId>, CashError>,
    );

    /// A figure of the totals.
    type Figure = fn(&ZTotals) -> Money;

    /// Spec §12 cases 22–24 and 27 (the pure part of the run check).
    #[test]
    fn z_run_check() {
        let d19 = day_of(NOV19_UTC + 3 * HOUR);
        let d20 = day_of(NOV19_UTC + 20 * HOUR);
        let ids: Vec<_> = (0..4).map(|_| CashierSessionId::generate()).collect();
        let s = |i: usize, number, day, open, in_z| ZCandidate {
            id: ids[i],
            number,
            business_day: day,
            open,
            in_z,
        };
        let closed = s(0, "R1-S-261119-01", d19, false, false);
        let open19 = s(1, "R2-S-261119-01", d19, true, false);
        let open20 = s(2, "R1-S-261120-01", d20, true, false);
        let done = s(3, "R2-S-261119-02", d19, false, true);
        #[rustfmt::skip]
        let cases: Vec<RunCase<'_>> = vec![
            ("one closed", d19, d19, vec![closed, done], Ok(vec![ids[0]])),
            ("open of the day", d19, d20, vec![closed, open19, open20],
                Err(CashError::SessionsOpen { numbers: vec!["R2-S-261119-01".into()] })),
            ("a session of 20/11 does not block 19/11", d19, d20, vec![closed, open20], Ok(vec![ids[0]])),
            ("all in a Z", d19, d20, vec![done, open20], Err(CashError::NothingToClose)),
            ("no sessions", d19, d19, vec![], Err(CashError::NothingToClose)),
            ("future day", d20, d19, vec![open20], Err(invalid("day", "after today"))),
        ];
        for (name, day, today, sessions, expected) in cases {
            assert_eq!(check_z_run(day, today, &sessions), expected, "{name}");
        }
    }

    /// Spec §12 case 13.
    #[test]
    fn tax_summary_sales_minus_returns() {
        let tax = TaxSettings::default();
        let sale = tax_summary(
            &[
                (m(11_100), 1_100),
                (m(5_000), 0),
                (m(22_200), 1_100),
                (m(2_000), 0),
            ],
            tax,
        );
        let ret = tax_summary(&[(m(11_100), 1_100)], tax);
        let tl = |rate_bp, base, tax| TaxLine {
            rate_bp,
            base: m(base),
            tax: m(tax),
        };
        assert_eq!(sale, [tl(0, 7_000, 0), tl(1_100, 30_000, 3_300)]);
        assert_eq!(ret, [tl(1_100, 10_000, 1_100)]);
        let mut z = Vec::new();
        merge_tax(&mut z, &sale, false).unwrap();
        merge_tax(&mut z, &ret, true).unwrap();
        assert_eq!(z, [tl(0, 7_000, 0), tl(1_100, 20_000, 2_200)]);
        // A return at a rate the sales do not have shows as a negative line.
        merge_tax(&mut z, &[tl(1_200, 100, 12)], true).unwrap();
        assert_eq!(z[2], tl(1_200, -100, -12));
        assert_eq!(
            merge_tax(&mut z, &[tl(0, i64::MAX, 0), tl(0, i64::MAX, 0)], false),
            Err(CashError::AmountOverflow)
        );
        assert_eq!(
            merge_tax(&mut vec![tl(0, 0, i64::MIN)], &[tl(0, 0, 1)], true),
            Err(CashError::AmountOverflow)
        );
    }

    pub(crate) struct Scripted {
        pub(crate) closes: Vec<CloseFigures>,
        pub(crate) sales: Vec<SessionSales>,
    }

    /// Three closed sessions: two on R1, one on R2.
    pub(crate) fn scripted() -> Scripted {
        let c = case1();
        let photos = BTreeMap::new();
        let figures = |events: &[SessionEvent], movements: &[crate::cash::CashMovement]| {
            session_figures(&SessionInput {
                opening_cash: m(200_000),
                events,
                movements,
                reasons: &c.reasons,
                photos: &photos,
                nota_flag_above: m(100_000),
                drawer_opens: 1,
                x_reports: 1,
            })
            .unwrap()
        };
        let session = c.movements[0].session;
        let drop = mv(session, CashKind::Drop, 50_000, None);
        let second = [
            drop.clone(),
            rev(&drop),
            mv(session, CashKind::Drop, 80_000, None),
        ];
        let f1 = figures(&c.events, &c.movements); // expected 353 500
        let f2 = figures(&[cash_tender(100_000, 100_000, 0)], &second); // 220 000
        let f3 = figures(&[], &[]); // 200 000
        let closes = vec![
            close_figures(f1, m(353_000), None, None).unwrap(),
            close_figures(f2, m(220_000), Some(m(100_000)), Some(m(150_000))).unwrap(),
            close_figures(f3, m(201_000), Some(m(201_000)), None).unwrap(),
        ];
        let at = |h| Timestamp::from_unix_millis(NOV19_UTC + h * HOUR);
        let sale = |n: &str, h| {
            Some(SaleRef {
                number: n.into(),
                posted_at: at(h),
            })
        };
        let tl = |rate_bp, base, tax| TaxLine {
            rate_bp,
            base: m(base),
            tax: m(tax),
        };
        let sales = vec![
            SessionSales {
                count: 2,
                gross: m(470_000),
                first: sale("JKT01-R1-261119-0001", 2),
                last: sale("JKT01-R1-261119-0002", 3),
                tax: vec![tl(1_100, 417_568, 45_932)],
                returns_tax: vec![tl(1_100, 18_018, 1_982)],
            },
            SessionSales {
                count: 1,
                gross: m(100_000),
                first: sale("JKT01-R1-261119-0003", 9),
                last: sale("JKT01-R1-261119-0003", 9),
                tax: vec![tl(0, 100_000, 0)],
                returns_tax: vec![],
            },
            SessionSales::default(),
        ];
        Scripted { closes, sales }
    }

    pub(crate) fn sessions(s: &Scripted) -> Vec<ZSession<'_>> {
        let at = |h| Timestamp::from_unix_millis(NOV19_UTC + h * HOUR);
        vec![
            ZSession {
                number: "JKT01-R1-S-261119-02",
                register: "R1",
                cashier: "Sari",
                opened_at: at(8),
                closed_at: at(12),
                forced: false,
                close: &s.closes[1],
                sales: &s.sales[1],
            },
            ZSession {
                number: "JKT01-R1-S-261119-01",
                register: "R1",
                cashier: "Budi",
                opened_at: at(1),
                closed_at: at(7),
                forced: false,
                close: &s.closes[0],
                sales: &s.sales[0],
            },
            ZSession {
                number: "JKT01-R2-S-261119-01",
                register: "R2",
                cashier: "Andi",
                opened_at: at(2),
                closed_at: at(14),
                forced: true,
                close: &s.closes[2],
                sales: &s.sales[2],
            },
        ]
    }

    /// Spec §12 case 12.
    #[test]
    fn z_totals_add_up() {
        let s = scripted();
        let day = day_of(NOV19_UTC + 3 * HOUR);
        let run_at = Timestamp::from_unix_millis(NOV19_UTC + 15 * HOUR);
        let z = build_z("JKT01-Z-261119-01", day, 1, run_at, &sessions(&s)).unwrap();
        assert!(!z.supplementary);
        let order: Vec<_> = z.sessions.iter().map(|l| l.number.as_str()).collect();
        assert_eq!(
            order,
            [
                "JKT01-R1-S-261119-01",
                "JKT01-R2-S-261119-01",
                "JKT01-R1-S-261119-02"
            ]
        );
        let first = &z.sessions[0];
        assert_eq!(
            (
                first.expected_cash,
                first.counted_cash,
                first.over_short,
                first.to_safe
            ),
            (m(353_500), m(353_000), m(-500), m(153_000))
        );
        assert!(z.sessions[1].forced);
        assert_eq!(z.sessions[2].float_difference, m(50_000));

        let r1 = &z.registers["R1"];
        let r2 = &z.registers["R2"];
        let b = &z.branch;
        assert_eq!((r1.sessions, r2.sessions, b.sessions), (2, 1, 3));
        // Per-register subtotals add up to the branch totals.
        let checks: [(&str, Figure); 8] = [
            ("expected", |t: &ZTotals| t.figures.expected_cash()),
            ("counted", |t: &ZTotals| t.counted_cash),
            ("over/short", |t: &ZTotals| t.over_short),
            ("to safe", |t: &ZTotals| t.to_safe),
            ("drops", |t: &ZTotals| t.figures.cash.drops.amount),
            ("deposit", |t: &ZTotals| t.cash_to_deposit),
            ("gross", |t: &ZTotals| t.gross_sales),
            ("net", |t: &ZTotals| t.net_sales),
        ];
        for (name, f) in checks {
            assert_eq!(add(f(r1), f(r2)).unwrap(), f(b), "{name}: R1 + R2 = branch");
        }
        // Drops: 300 000 + (50 000 − 50 000 + 80 000); to safe: 153 000 + 120 000 + 0.
        assert_eq!(b.figures.cash.drops.amount, m(380_000));
        assert_eq!(b.figures.cash.reversals.count, 1);
        assert_eq!(b.to_safe, m(273_000));
        assert_eq!(b.cash_to_deposit, m(653_000));
        assert_eq!(b.over_short, m(-500 + 1_000));
        assert_eq!(b.float_difference, m(50_000));
        assert_eq!((b.sales_count, b.gross_sales), (3, m(570_000)));
        assert_eq!(b.net_sales, m(570_000));
        assert_eq!(
            b.first_sale.as_ref().map(|s| s.number.as_str()),
            Some("JKT01-R1-261119-0001")
        );
        assert_eq!(
            b.last_sale.as_ref().map(|s| s.number.as_str()),
            Some("JKT01-R1-261119-0003")
        );
        assert_eq!(r2.first_sale, None);
        let rates: Vec<_> = b
            .tax
            .iter()
            .map(|t| (t.rate_bp, t.base.minor(), t.tax.minor()))
            .collect();
        assert_eq!(rates, [(0, 100_000, 0), (1_100, 399_550, 43_950)]);
        assert_eq!((b.figures.drawer_opens, b.figures.x_reports), (3, 3));

        // A second Z of the day is supplementary; none without sessions.
        let z2 = build_z("JKT01-Z-261119-02", day, 2, run_at, &sessions(&s)[..1]).unwrap();
        assert!(z2.supplementary);
        assert_eq!(
            build_z("X", day, 1, run_at, &[]),
            Err(CashError::NothingToClose)
        );
    }

    #[test]
    fn first_and_last_sale_and_overflow() {
        let r = |n: &str, t| SaleRef {
            number: n.into(),
            posted_at: Timestamp::from_unix_millis(t),
        };
        assert_eq!(earlier(None, None, true), None);
        assert_eq!(earlier(Some(r("a", 1)), None, true), Some(r("a", 1)));
        assert_eq!(
            earlier(Some(r("a", 2)), Some(&r("b", 1)), true),
            Some(r("b", 1))
        );
        assert_eq!(
            earlier(Some(r("a", 1)), Some(&r("b", 2)), true),
            Some(r("a", 1))
        );
        assert_eq!(
            earlier(Some(r("a", 1)), Some(&r("b", 2)), false),
            Some(r("b", 2))
        );
        assert_eq!(
            earlier(Some(r("b", 1)), Some(&r("a", 1)), false),
            Some(r("b", 1))
        );

        let s = scripted();
        let mut big = s.sales[0].clone();
        big.gross = m(i64::MAX);
        let mut list = sessions(&s);
        list[0].sales = &big;
        list[1].sales = &big;
        let day = day_of(NOV19_UTC);
        assert_eq!(
            build_z("X", day, 1, Timestamp::from_unix_millis(0), &list),
            Err(CashError::AmountOverflow)
        );
    }
}
