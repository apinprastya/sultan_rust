//! A minimal point in time for pure domain logic.
//!
//! `core` does not read the clock and does not depend on a date-time crate: callers pass the
//! current time in as a [`Timestamp`] (plan Phase 2: "time is passed in as an argument").

use std::fmt;

/// Milliseconds since the Unix epoch (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// Creates a timestamp from Unix milliseconds.
    #[must_use]
    pub const fn from_unix_millis(millis: i64) -> Self {
        Self(millis)
    }

    /// Returns the Unix milliseconds.
    #[must_use]
    pub const fn unix_millis(self) -> i64 {
        self.0
    }

    /// `self + millis`, clamped to the `i64` range.
    #[must_use]
    pub const fn saturating_add_millis(self, millis: i64) -> Self {
        Self(self.0.saturating_add(millis))
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}ms", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arithmetic_and_display() {
        let t = Timestamp::from_unix_millis(1_000);
        assert_eq!(t.saturating_add_millis(500).unix_millis(), 1_500);
        assert_eq!(
            Timestamp::from_unix_millis(i64::MAX).saturating_add_millis(1),
            Timestamp::from_unix_millis(i64::MAX)
        );
        assert_eq!(t.to_string(), "1000ms");
    }
}
