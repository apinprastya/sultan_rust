//! Membership tiers: when a tier applies (spec 2.5 §4) and the automatic tier selection
//! (spec 2.5 §5, built after HQ sync).

use chrono::NaiveDate;
use uuid::Uuid;

use crate::doc_number::{BranchTimeZone, LocalDay};
use crate::money::Money;
use crate::time::Timestamp;

/// Whether a customer's tier applies on `date`, the sale's local date in the selling branch's
/// time zone: the tier is not archived and `valid_until` is empty or on or after `date`.
/// A tier valid until `2026-12-31` still applies on `2026-12-31`, not on `2027-01-01`.
///
/// The caller checks that the customer has a tier at all.
#[must_use]
pub fn tier_applies(archived: bool, valid_until: Option<NaiveDate>, date: NaiveDate) -> bool {
    !archived && valid_until.is_none_or(|until| until >= date)
}

/// [`tier_applies`] at the instant `at`, taking the local date in the branch's time zone
/// (23:30 WIB on the 31st is still the 31st).
#[must_use]
pub fn tier_applies_at(
    archived: bool,
    valid_until: Option<NaiveDate>,
    at: Timestamp,
    tz: BranchTimeZone,
) -> bool {
    let (y, m, d) = LocalDay::of(at, tz).ymd();
    // `LocalDay` always holds a valid chrono date, so this never falls back.
    NaiveDate::from_ymd_opt(y, m, d).is_some_and(|date| tier_applies(archived, valid_until, date))
}

/// A tier as the automatic selection sees it (spec 2.5 §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TierRule {
    /// The tier.
    pub id: Uuid,
    /// Its rank, higher is better (unique).
    pub rank: i32,
    /// The minimum net sales over the last 12 months, or `None` when the tier is never reached
    /// automatically.
    pub qualifying_spend: Option<Money>,
    /// An archived tier is never selected.
    pub archived: bool,
}

/// A customer's tier state as the automatic selection sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TierState {
    /// The current tier, if any.
    pub tier_id: Option<Uuid>,
    /// The current tier's last valid date, if any.
    pub valid_until: Option<NaiveDate>,
    /// "Manual tier": the job never changes this customer.
    pub manual: bool,
}

/// The outcome of [`auto_tier`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoTier {
    /// Keep the current tier.
    Unchanged,
    /// Move to this tier (`None` removes the tier).
    Change(Option<Uuid>),
}

/// The daily automatic tier selection at HQ (spec 2.5 §5).
///
/// - A manual-tier customer is never changed.
/// - The target is the highest-ranked, non-archived tier whose qualifying spend is at most
///   `spend_12m` (the net sales after returns over the last 12 months, all branches).
/// - A target ranked above the current tier (or any target when there is none) is an upgrade
///   and happens at once.
/// - A target ranked below the current tier (or no target) is a downgrade and happens only
///   when `valid_until` has passed (`valid_until < today`). A tier without a `valid_until` is
///   never downgraded, so a paid or manual tier is never cut short.
/// - A current tier missing from `tiers` ranks below every tier.
///
/// Limitation: the decision is only the tier; the new `valid_until` (if any) is the caller's.
#[must_use]
pub fn auto_tier(
    tiers: &[TierRule],
    state: TierState,
    spend_12m: Money,
    today: NaiveDate,
) -> AutoTier {
    if state.manual {
        return AutoTier::Unchanged;
    }
    let target = tiers
        .iter()
        .filter(|t| !t.archived && t.qualifying_spend.is_some_and(|min| spend_12m >= min))
        .max_by_key(|t| t.rank);
    let current_rank = state
        .tier_id
        .and_then(|id| tiers.iter().find(|t| t.id == id))
        .map(|t| t.rank);
    if target.map(|t| t.id) == state.tier_id {
        return AutoTier::Unchanged;
    }
    let upgrade = match (target, state.tier_id) {
        (Some(_), None) => true,
        (Some(t), Some(_)) => current_rank.is_none_or(|rank| t.rank > rank),
        (None, _) => false,
    };
    let expired = state.valid_until.is_some_and(|until| until < today);
    if upgrade || expired {
        AutoTier::Change(target.map(|t| t.id))
    } else {
        AutoTier::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }

    /// Spec 2.5 §15 case 4 (date part).
    #[test]
    fn applies_on_a_date() {
        let cases: &[(bool, Option<&str>, &str, bool)] = &[
            (false, None, "2026-12-31", true),
            (false, Some("2026-12-31"), "2026-12-31", true),
            (false, Some("2026-12-31"), "2026-12-30", true),
            (false, Some("2026-12-31"), "2027-01-01", false),
            (true, None, "2026-12-31", false),
            (true, Some("2027-12-31"), "2026-12-31", false),
        ];
        for (archived, until, on, want) in cases {
            assert_eq!(
                tier_applies(*archived, until.map(d), d(on)),
                *want,
                "{archived} {until:?} {on}"
            );
        }
    }

    /// Spec 2.5 §15 case 4 (time zone part): 23:30 WIB on the 31st is still the 31st.
    #[test]
    fn applies_in_the_branch_time_zone() {
        let jakarta = BranchTimeZone::default();
        let jayapura: BranchTimeZone = "Asia/Jayapura".parse().unwrap();
        // 2026-12-31 16:30 UTC = 23:30 WIB (UTC+7) = 01:30 WIT (UTC+9) on 1 January.
        let at = Timestamp::from_unix_millis(1_798_734_600_000);
        let until = Some(d("2026-12-31"));
        assert!(tier_applies_at(false, until, at, jakarta));
        assert!(!tier_applies_at(false, until, at, jayapura));
        assert!(!tier_applies_at(true, None, at, jakarta));
    }

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    const SILVER: u128 = 1;
    const GOLD: u128 = 2;
    const PLATINUM: u128 = 3;
    const VIP: u128 = 4;
    const OLD: u128 = 5;

    fn tiers() -> Vec<TierRule> {
        let rule = |n, rank, min: Option<i64>, archived| TierRule {
            id: id(n),
            rank,
            qualifying_spend: min.map(Money::from_minor),
            archived,
        };
        vec![
            rule(GOLD, 20, Some(5_000_000), false),
            rule(SILVER, 10, Some(1_000_000), false),
            rule(PLATINUM, 30, Some(20_000_000), false),
            // Never reached automatically.
            rule(VIP, 40, None, false),
            // Archived, would qualify.
            rule(OLD, 25, Some(0), true),
        ]
    }

    /// Spec 2.5 §15 case 9 and the other branches of the rule.
    #[test]
    fn auto_tier_cases() {
        let today = d("2026-11-11");
        let state = |tier: Option<u128>, until: Option<&str>, manual| TierState {
            tier_id: tier.map(id),
            valid_until: until.map(d),
            manual,
        };
        let change = |tier: Option<u128>| AutoTier::Change(tier.map(id));
        let cases: &[(&str, TierState, i64, AutoTier)] = &[
            (
                "5 M → Gold",
                state(None, None, false),
                5_000_000,
                change(Some(GOLD)),
            ),
            (
                "upgrade from Silver",
                state(Some(SILVER), None, false),
                6_000_000,
                change(Some(GOLD)),
            ),
            (
                "below every threshold",
                state(None, None, false),
                999_999,
                AutoTier::Unchanged,
            ),
            (
                "same tier",
                state(Some(GOLD), Some("2026-01-01"), false),
                5_000_000,
                AutoTier::Unchanged,
            ),
            (
                "spend drops, valid-until not passed",
                state(Some(GOLD), Some("2026-11-11"), false),
                1_000_000,
                AutoTier::Unchanged,
            ),
            (
                "spend drops, no valid-until",
                state(Some(GOLD), None, false),
                0,
                AutoTier::Unchanged,
            ),
            (
                "spend drops, valid-until passed",
                state(Some(GOLD), Some("2026-11-10"), false),
                1_000_000,
                change(Some(SILVER)),
            ),
            (
                "nothing qualifies, valid-until passed",
                state(Some(SILVER), Some("2026-11-10"), false),
                0,
                change(None),
            ),
            (
                "manual flag",
                state(Some(SILVER), Some("2026-01-01"), true),
                50_000_000,
                AutoTier::Unchanged,
            ),
            (
                "VIP above every threshold is kept",
                state(Some(VIP), None, false),
                50_000_000,
                AutoTier::Unchanged,
            ),
            (
                "unknown current tier is upgraded",
                state(Some(99), None, false),
                1_000_000,
                change(Some(SILVER)),
            ),
            (
                "archived current tier ranked by its rank",
                state(Some(OLD), None, false),
                25_000_000,
                change(Some(PLATINUM)),
            ),
        ];
        for (name, state, spend, want) in cases {
            assert_eq!(
                auto_tier(&tiers(), *state, Money::from_minor(*spend), today),
                *want,
                "{name}"
            );
        }
    }
}
