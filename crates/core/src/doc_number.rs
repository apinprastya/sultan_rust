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
}
