//! Human document numbers and the branch-local calendar they use (`docs/plan.md` §1: "human
//! document numbers per branch/register"; spec 2.1 §1, §5).
//!
//! - Every branch has an IANA [`BranchTimeZone`] (default [`DEFAULT_TIME_ZONE`],
//!   `Asia/Jakarta`). A number's date is the **branch-local** calendar day of the document's
//!   time ([`LocalDay::of`]), so a session opened at 00:30 WIB on 8 November is dated `261108`
//!   even though it is still 7 November in UTC (decided 2026-11-08, spec 2.1 §5).
//! - Cashier session numbers are `<BRANCH>-<REGISTER>-S-<YYMMDD>-<NN>` ([`session_number`]),
//!   e.g. `JKT01-R2-S-261107-01`. `NN` is a per-register, per-local-day sequence starting at 1,
//!   printed with at least two digits; the 100th session of a day is `-100`, never a failure.
//! - Sale numbers are `<BRANCH>-<REGISTER>-<YYMMDD>-<NNNN>` ([`sale_number`]) and return numbers
//!   `<BRANCH>-<REGISTER>-RT-<YYMMDD>-<NNNN>` ([`return_number`]), per register and local day,
//!   at least four digits (spec 2.6 §1).
//! - Cash movement numbers are `<BRANCH>-<REGISTER>-K-<YYMMDD>-<NN>` ([`cash_movement_number`]),
//!   one sequence for cash in, cash out and safe drops per register and local day, and Z report
//!   numbers `<BRANCH>-Z-<YYMMDD>-<NN>` ([`z_number`]) per branch and business day, both with at
//!   least two digits (spec 2.7 §1, K9, K17).
//!
//! Pure: the time zone and the instant are passed in. The tz database is compiled in
//! (`chrono-tz`), so the result does not depend on the machine's zone files.

use std::fmt;
use std::str::FromStr;

use chrono::{Datelike, TimeZone as _};
use chrono_tz::Tz;

use crate::time::Timestamp;

/// The time zone of a new branch, and of every branch that existed before time zones were
/// stored (spec 2.1 §1).
pub const DEFAULT_TIME_ZONE: &str = "Asia/Jakarta";

/// A name that is not an IANA time zone.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("`{0}` is not an IANA time zone, e.g. Asia/Jakarta")]
pub struct InvalidTimeZone(pub String);

/// A branch's IANA time zone, e.g. `Asia/Jakarta`, `Asia/Makassar`, `Asia/Jayapura`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BranchTimeZone(Tz);

impl BranchTimeZone {
    /// The canonical IANA name, e.g. `Asia/Jakarta`.
    #[must_use]
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// The local date and time of instant `at` in this zone, clamped like [`LocalDay::of`].
    #[must_use]
    pub fn local_datetime(self, at: Timestamp) -> chrono::NaiveDateTime {
        self.0
            .timestamp_millis_opt(at.unix_millis())
            .single()
            .map_or_else(
                || {
                    if at.unix_millis() < 0 {
                        chrono::NaiveDateTime::MIN
                    } else {
                        chrono::NaiveDateTime::MAX
                    }
                },
                |dt| dt.naive_local(),
            )
    }
}

impl Default for BranchTimeZone {
    fn default() -> Self {
        Self(Tz::Asia__Jakarta)
    }
}

impl FromStr for BranchTimeZone {
    type Err = InvalidTimeZone;

    /// Parses an exact IANA name (case-sensitive, surrounding spaces ignored). Fixed-offset or
    /// abbreviated names that are not IANA zones (`WIB`, `+07:00`) are rejected.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let name = s.trim();
        name.parse::<Tz>()
            .map(Self)
            .map_err(|_| InvalidTimeZone(name.to_owned()))
    }
}

impl fmt::Display for BranchTimeZone {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A calendar day in a branch's time zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalDay {
    year: i32,
    month: u32,
    day: u32,
}

impl LocalDay {
    /// The local calendar day of instant `at` in `tz`.
    ///
    /// Instants outside chrono's range (about ±262 000 years) clamp to its first or last day;
    /// they cannot come from a real clock.
    #[must_use]
    pub fn of(at: Timestamp, tz: BranchTimeZone) -> Self {
        let local =
            tz.0.timestamp_millis_opt(at.unix_millis())
                .single()
                .map_or_else(
                    || {
                        if at.unix_millis() < 0 {
                            chrono::NaiveDate::MIN
                        } else {
                            chrono::NaiveDate::MAX
                        }
                    },
                    |dt| dt.date_naive(),
                );
        Self {
            year: local.year(),
            month: local.month(),
            day: local.day(),
        }
    }

    /// Year, month (1–12) and day (1–31).
    #[must_use]
    pub const fn ymd(self) -> (i32, u32, u32) {
        (self.year, self.month, self.day)
    }

    /// The day as the integer `YYYYMMDD` (e.g. `20261107`): the stored, sortable form.
    #[must_use]
    pub fn key(self) -> i64 {
        i64::from(self.year) * 10_000 + i64::from(self.month) * 100 + i64::from(self.day)
    }

    /// The day of a stored [`key`](Self::key) (`YYYYMMDD`); `None` when it is not a calendar
    /// day (e.g. `20261131`) or outside years 1–9999.
    #[must_use]
    pub fn from_key(key: i64) -> Option<Self> {
        if !(10_101..=99_991_231).contains(&key) {
            return None;
        }
        let year = i32::try_from(key / 10_000).ok()?;
        let month = u32::try_from(key / 100 % 100).ok()?;
        let day = u32::try_from(key % 100).ok()?;
        chrono::NaiveDate::from_ymd_opt(year, month, day)?;
        Some(Self { year, month, day })
    }

    /// The day `days` later (earlier when negative), e.g. a due date or an expiry warning
    /// horizon; `None` outside years 1–9999.
    #[must_use]
    pub fn add_days(self, days: i64) -> Option<Self> {
        let date = self.date()?;
        let moved = date.checked_add_signed(chrono::TimeDelta::try_days(days)?)?;
        Self::from_key(
            i64::from(moved.year()) * 10_000
                + i64::from(moved.month()) * 100
                + i64::from(moved.day()),
        )
    }

    /// The number of days from `earlier` to `self` (negative when `earlier` is later).
    #[must_use]
    pub fn days_since(self, earlier: Self) -> i64 {
        match (self.date(), earlier.date()) {
            (Some(a), Some(b)) => (a - b).num_days(),
            _ => 0,
        }
    }

    fn date(self) -> Option<chrono::NaiveDate> {
        chrono::NaiveDate::from_ymd_opt(self.year, self.month, self.day)
    }

    /// The `YYMMDD` printed in document numbers (e.g. `261107`).
    #[must_use]
    pub fn yymmdd(self) -> String {
        format!(
            "{:02}{:02}{:02}",
            self.year.rem_euclid(100),
            self.month,
            self.day
        )
    }
}

/// The number of the `seq`-th cashier session of `day` on a register (spec 2.1 §5):
/// `JKT01-R2-S-261107-01`. `seq` starts at 1 and is printed with at least two digits.
#[must_use]
pub fn session_number(branch_code: &str, register_code: &str, day: LocalDay, seq: u32) -> String {
    format!("{branch_code}-{register_code}-S-{}-{seq:02}", day.yymmdd())
}

/// The number of the `seq`-th sale of `day` on a register (spec 2.6 §1):
/// `JKT01-R2-261116-0001`. `seq` starts at 1 and is printed with at least four digits; the
/// 10 000th sale of a day is `-10000`, never a failure.
#[must_use]
pub fn sale_number(branch_code: &str, register_code: &str, day: LocalDay, seq: u32) -> String {
    format!("{branch_code}-{register_code}-{}-{seq:04}", day.yymmdd())
}

/// The number of the `seq`-th return of `day` on a register (spec 2.6 §1):
/// `JKT01-R2-RT-261116-0001`, with the same sequence rules as [`sale_number`].
#[must_use]
pub fn return_number(branch_code: &str, register_code: &str, day: LocalDay, seq: u32) -> String {
    format!("{branch_code}-{register_code}-RT-{}-{seq:04}", day.yymmdd())
}

/// The number of the `seq`-th cash movement (in, out or drop) of `day` on a register (spec 2.7
/// §1, K9): `JKT01-R2-K-261119-01`. `seq` starts at 1 and is printed with at least two digits.
#[must_use]
pub fn cash_movement_number(
    branch_code: &str,
    register_code: &str,
    day: LocalDay,
    seq: u32,
) -> String {
    format!("{branch_code}-{register_code}-K-{}-{seq:02}", day.yymmdd())
}

/// The number of the `seq`-th Z report of the business `day` of a branch (spec 2.7 §1, K17):
/// `JKT01-Z-261119-01`; a supplementary Z of the same day is `-02` (K16).
#[must_use]
pub fn z_number(branch_code: &str, day: LocalDay, seq: u32) -> String {
    format!("{branch_code}-Z-{}-{seq:02}", day.yymmdd())
}

/// The number of the `seq`-th inventory document of `kind` of `day` at a branch (spec 2.8 I19):
/// `JKT01-PO-261126-01`, `-GR-` goods receipt, `-RS-` supplier return, `-TR-` transfer, `-IT-`
/// internal transfer, `-ADJ-` adjustment, `-OP-` opname, `-OS-` opening stock. One sequence per
/// branch, kind and branch-local day, at least two digits.
#[must_use]
pub fn stock_number(
    branch_code: &str,
    kind: crate::stock::DocKind,
    day: LocalDay,
    seq: u32,
) -> String {
    format!("{branch_code}-{}-{}-{seq:02}", kind.prefix(), day.yymmdd())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-11-07T00:00:00Z.
    const NOV7_UTC: i64 = 1_794_009_600_000;
    const HOUR: i64 = 3_600_000;

    fn tz(name: &str) -> BranchTimeZone {
        name.parse().unwrap()
    }

    #[test]
    fn time_zone_names() {
        #[rustfmt::skip]
        let cases = [
            ("Asia/Jakarta", Some("Asia/Jakarta")),
            (" Asia/Makassar ", Some("Asia/Makassar")),
            ("Asia/Jayapura", Some("Asia/Jayapura")),
            ("UTC", Some("UTC")),
            ("Europe/Amsterdam", Some("Europe/Amsterdam")),
            ("asia/jakarta", None),
            ("WIB", None),
            ("+07:00", None),
            ("", None),
            ("Mars/Olympus", None),
        ];
        for (input, expected) in cases {
            let got = input
                .parse::<BranchTimeZone>()
                .ok()
                .map(BranchTimeZone::name);
            assert_eq!(got, expected, "{input:?}");
        }
        let err = "WIB".parse::<BranchTimeZone>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "`WIB` is not an IANA time zone, e.g. Asia/Jakarta"
        );
        assert_eq!(BranchTimeZone::default().name(), DEFAULT_TIME_ZONE);
        assert_eq!(tz("Asia/Jakarta"), BranchTimeZone::default());
        assert_eq!(BranchTimeZone::default().to_string(), "Asia/Jakarta");
    }

    #[test]
    fn local_day_follows_the_branch_zone() {
        #[rustfmt::skip]
        let cases = [
            // (instant, zone, expected local day)
            (NOV7_UTC, "UTC", (2026, 11, 7)),
            // 16:59:59.999Z = 23:59:59.999 WIB, still the 7th.
            (NOV7_UTC + 17 * HOUR - 1, "Asia/Jakarta", (2026, 11, 7)),
            // 17:00Z = 00:00 WIB on the 8th, although UTC still says the 7th.
            (NOV7_UTC + 17 * HOUR, "Asia/Jakarta", (2026, 11, 8)),
            // WITA (+8) and WIT (+9) roll over earlier.
            (NOV7_UTC + 16 * HOUR, "Asia/Makassar", (2026, 11, 8)),
            (NOV7_UTC + 15 * HOUR, "Asia/Jayapura", (2026, 11, 8)),
            (NOV7_UTC + 15 * HOUR - 1, "Asia/Jayapura", (2026, 11, 7)),
            // West of UTC: early UTC morning is the previous local day.
            (NOV7_UTC + 4 * HOUR, "America/New_York", (2026, 11, 6)),
            // Year boundary.
            (1_798_736_400_000, "Asia/Jakarta", (2027, 1, 1)),
            (1_798_736_400_000 - 1, "Asia/Jakarta", (2026, 12, 31)),
            // Out of range clamps instead of panicking.
            (i64::MAX, "UTC", (262_142, 12, 31)),
            (i64::MIN, "UTC", (-262_143, 1, 1)),
        ];
        for (at, zone, ymd) in cases {
            let day = LocalDay::of(Timestamp::from_unix_millis(at), tz(zone));
            assert_eq!(day.ymd(), ymd, "{at} {zone}");
        }
    }

    #[test]
    fn day_forms() {
        let day = LocalDay::of(Timestamp::from_unix_millis(NOV7_UTC), tz("UTC"));
        assert_eq!(day.key(), 20_261_107);
        assert_eq!(day.yymmdd(), "261107");
        let y2k = LocalDay::of(Timestamp::from_unix_millis(946_684_800_000), tz("UTC"));
        assert_eq!((y2k.key(), y2k.yymmdd()), (20_000_101, "000101".to_owned()));
        assert!(y2k < day);
        assert_eq!(LocalDay::from_key(day.key()), Some(day));
        assert_eq!(LocalDay::from_key(y2k.key()), Some(y2k));
        #[rustfmt::skip]
        let cases = [
            (20_261_119, Some((2026, 11, 19))),
            (20_240_229, Some((2024, 2, 29))),
            (10_101, Some((1, 1, 1))),
            (99_991_231, Some((9999, 12, 31))),
            (20_250_229, None),
            (20_261_131, None),
            (20_261_300, None),
            (20_261_001 - 1, None),
            (0, None),
            (-20_261_119, None),
            (100_000_101, None),
        ];
        for (key, ymd) in cases {
            assert_eq!(LocalDay::from_key(key).map(LocalDay::ymd), ymd, "{key}");
        }
    }

    #[test]
    fn session_numbers() {
        let day = LocalDay::of(Timestamp::from_unix_millis(NOV7_UTC), tz("Asia/Jakarta"));
        #[rustfmt::skip]
        let cases = [
            ("JKT01", "R2", 1, "JKT01-R2-S-261107-01"),
            ("JKT01", "R2", 9, "JKT01-R2-S-261107-09"),
            ("JKT01", "R2", 42, "JKT01-R2-S-261107-42"),
            // Grows past 99 instead of failing.
            ("BDG01", "R1", 100, "BDG01-R1-S-261107-100"),
            ("BDG01", "R1", 1234, "BDG01-R1-S-261107-1234"),
        ];
        for (branch, register, seq, expected) in cases {
            assert_eq!(session_number(branch, register, day, seq), expected);
        }
    }

    /// Spec 2.6 §15 case 19: sale and return numbers across midnight in `Asia/Jakarta`.
    #[test]
    fn sale_and_return_numbers_across_midnight() {
        // 2026-11-16T00:00:00Z.
        const NOV16_UTC: i64 = NOV7_UTC + 9 * 24 * HOUR;
        let jkt = tz("Asia/Jakarta");
        let day = |at| LocalDay::of(Timestamp::from_unix_millis(at), jkt);
        #[rustfmt::skip]
        let cases = [
            // 16:59:59.999Z = 23:59:59.999 WIB on the 16th.
            (NOV16_UTC + 17 * HOUR - 1, 1, "JKT01-R2-261116-0001", "JKT01-R2-RT-261116-0001"),
            // 17:00Z = 00:00 WIB on the 17th: a new day, the sequence restarts.
            (NOV16_UTC + 17 * HOUR, 1, "JKT01-R2-261117-0001", "JKT01-R2-RT-261117-0001"),
            (NOV16_UTC + 18 * HOUR, 42, "JKT01-R2-261117-0042", "JKT01-R2-RT-261117-0042"),
            (NOV16_UTC + 18 * HOUR, 10_000, "JKT01-R2-261117-10000", "JKT01-R2-RT-261117-10000"),
        ];
        for (at, seq, sale, ret) in cases {
            assert_eq!(sale_number("JKT01", "R2", day(at), seq), sale);
            assert_eq!(return_number("JKT01", "R2", day(at), seq), ret);
        }
    }

    /// Spec 2.7 §12 case 14: cash movement and Z numbers across midnight in `Asia/Jakarta`
    /// (UTC+7) and `Asia/Jayapura` (UTC+9).
    #[test]
    fn cash_movement_and_z_numbers_across_midnight() {
        // 2026-11-19T00:00:00Z.
        const NOV19_UTC: i64 = NOV7_UTC + 12 * 24 * HOUR;
        #[rustfmt::skip]
        let cases = [
            // 16:59:59.999Z = 23:59:59.999 WIB on the 19th.
            (NOV19_UTC + 17 * HOUR - 1, "Asia/Jakarta", 1, "JKT01-R2-K-261119-01", "JKT01-Z-261119-01"),
            // 17:00Z = 00:00 WIB on the 20th.
            (NOV19_UTC + 17 * HOUR, "Asia/Jakarta", 1, "JKT01-R2-K-261120-01", "JKT01-Z-261120-01"),
            // 14:59:59.999Z = 23:59:59.999 WIT on the 19th; 15:00Z is already the 20th in WIT.
            (NOV19_UTC + 15 * HOUR - 1, "Asia/Jayapura", 7, "JKT01-R2-K-261119-07", "JKT01-Z-261119-07"),
            (NOV19_UTC + 15 * HOUR, "Asia/Jayapura", 2, "JKT01-R2-K-261120-02", "JKT01-Z-261120-02"),
            // The same instant is still the 19th in Jakarta.
            (NOV19_UTC + 15 * HOUR, "Asia/Jakarta", 2, "JKT01-R2-K-261119-02", "JKT01-Z-261119-02"),
            // Grows past 99 instead of failing.
            (NOV19_UTC, "Asia/Jakarta", 100, "JKT01-R2-K-261119-100", "JKT01-Z-261119-100"),
        ];
        for (at, zone, seq, movement, z) in cases {
            let day = LocalDay::of(Timestamp::from_unix_millis(at), tz(zone));
            assert_eq!(
                cash_movement_number("JKT01", "R2", day, seq),
                movement,
                "{zone} {at}"
            );
            assert_eq!(z_number("JKT01", day, seq), z, "{zone} {at}");
        }
    }

    /// Spec 2.8 §17 case 12: inventory numbers per branch and local day for every prefix.
    #[test]
    fn stock_numbers_per_branch_and_local_day() {
        use crate::stock::DocKind;
        // 2026-11-26T00:00:00Z.
        const NOV26_UTC: i64 = NOV7_UTC + 19 * 24 * HOUR;
        let jkt = tz("Asia/Jakarta");
        let before = LocalDay::of(Timestamp::from_unix_millis(NOV26_UTC + 17 * HOUR - 1), jkt);
        let after = LocalDay::of(Timestamp::from_unix_millis(NOV26_UTC + 17 * HOUR), jkt);
        #[rustfmt::skip]
        let cases = [
            (DocKind::PurchaseOrder, "JKT01", before, 1, "JKT01-PO-261126-01"),
            (DocKind::GoodsReceipt, "JKT01", before, 2, "JKT01-GR-261126-02"),
            (DocKind::SupplierReturn, "JKT01", after, 1, "JKT01-RS-261127-01"),
            (DocKind::Transfer, "BDG01", after, 12, "BDG01-TR-261127-12"),
            (DocKind::InternalTransfer, "BDG01", before, 3, "BDG01-IT-261126-03"),
            (DocKind::Adjustment, "JKT01", after, 100, "JKT01-ADJ-261127-100"),
            (DocKind::Opname, "JKT01", before, 1, "JKT01-OP-261126-01"),
            (DocKind::OpeningStock, "JKT01", after, 9, "JKT01-OS-261127-09"),
        ];
        for (kind, branch, day, seq, expected) in cases {
            assert_eq!(stock_number(branch, kind, day, seq), expected);
        }
    }

    #[test]
    fn day_arithmetic() {
        let day = LocalDay::from_key(20_261_126).unwrap();
        #[rustfmt::skip]
        let cases = [
            (0, Some(20_261_126)),
            (5, Some(20_261_201)),
            (30, Some(20_261_226)),
            (36, Some(20_270_101)),
            (-26, Some(20_261_031)),
            (i64::MAX, None),
            (3_000_000, None),
        ];
        for (days, key) in cases {
            assert_eq!(day.add_days(days).map(LocalDay::key), key, "{days}");
        }
        let later = LocalDay::from_key(20_270_101).unwrap();
        assert_eq!(later.days_since(day), 36);
        assert_eq!(day.days_since(later), -36);
        assert_eq!(day.days_since(day), 0);
        // A clamped day outside chrono's range has no date.
        let far = LocalDay::of(Timestamp::from_unix_millis(i64::MAX), tz("UTC"));
        assert_eq!(far.add_days(1), None);
    }

    #[test]
    fn local_datetime_follows_the_zone() {
        let jkt = tz("Asia/Jakarta");
        let at = jkt.local_datetime(Timestamp::from_unix_millis(
            NOV7_UTC + 17 * HOUR + 5 * 60_000,
        ));
        assert_eq!(at.to_string(), "2026-11-08 00:05:00");
        assert_eq!(
            jkt.local_datetime(Timestamp::from_unix_millis(i64::MAX)),
            chrono::NaiveDateTime::MAX
        );
        assert_eq!(
            jkt.local_datetime(Timestamp::from_unix_millis(i64::MIN)),
            chrono::NaiveDateTime::MIN
        );
    }
}
