//! Money in minor currency units (`docs/plan.md` §1: `i64`, Rupiah minor unit = 1).

use std::fmt;

/// An amount of money in minor units. Can be negative (refunds, shortages).
///
/// Arithmetic is explicit: use the `checked_*` methods where an overflow must be reported and the
/// `saturating_*` methods where clamping is acceptable. There is deliberately no `Add`/`Sub`
/// operator, because `i64` overflow wraps silently in release builds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Money(i64);

impl Money {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// Creates an amount from minor units.
    #[must_use]
    pub const fn from_minor(minor: i64) -> Self {
        Self(minor)
    }

    /// Returns the amount in minor units.
    #[must_use]
    pub const fn minor(self) -> i64 {
        self.0
    }

    /// `self + other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_add(self, other: Self) -> Option<Self> {
        match self.0.checked_add(other.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// `self - other`, or `None` on overflow.
    #[must_use]
    pub const fn checked_sub(self, other: Self) -> Option<Self> {
        match self.0.checked_sub(other.0) {
            Some(v) => Some(Self(v)),
            None => None,
        }
    }

    /// `self + other`, clamped to the `i64` range.
    #[must_use]
    pub const fn saturating_add(self, other: Self) -> Self {
        Self(self.0.saturating_add(other.0))
    }

    /// `self - other`, clamped to the `i64` range.
    #[must_use]
    pub const fn saturating_sub(self, other: Self) -> Self {
        Self(self.0.saturating_sub(other.0))
    }

    /// Whether the amount is below zero.
    #[must_use]
    pub const fn is_negative(self) -> bool {
        self.0 < 0
    }
}

/// Formats the raw minor-unit amount. Currency formatting is a UI concern.
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: Money = Money::from_minor(i64::MAX);
    const MIN: Money = Money::from_minor(i64::MIN);

    #[test]
    fn checked_arithmetic() {
        let a = Money::from_minor(10_000);
        let b = Money::from_minor(2_500);
        assert_eq!(a.checked_add(b), Some(Money::from_minor(12_500)));
        assert_eq!(b.checked_sub(a), Some(Money::from_minor(-7_500)));
        assert_eq!(MAX.checked_add(Money::from_minor(1)), None);
        assert_eq!(MIN.checked_sub(Money::from_minor(1)), None);
    }

    #[test]
    fn saturating_arithmetic() {
        assert_eq!(MAX.saturating_add(Money::from_minor(1)), MAX);
        assert_eq!(MIN.saturating_sub(Money::from_minor(1)), MIN);
        assert_eq!(
            Money::from_minor(5).saturating_sub(Money::from_minor(7)),
            Money::from_minor(-2)
        );
    }

    #[test]
    fn accessors_and_display() {
        assert_eq!(Money::ZERO, Money::default());
        assert_eq!(Money::from_minor(-3).minor(), -3);
        assert!(Money::from_minor(-3).is_negative());
        assert!(!Money::ZERO.is_negative());
        assert_eq!(Money::from_minor(15_000).to_string(), "15000");
    }
}
