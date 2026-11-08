//! Login lockout policy (spec 2.1 §4).

use super::AccessError;
use crate::time::Timestamp;

/// Consecutive failures that lock a user (spec 2.1 §4).
pub const MAX_CONSECUTIVE_FAILURES: u8 = 5;

/// How long a lockout lasts: 5 minutes (spec 2.1 §4).
pub const LOCKOUT_DURATION_MS: i64 = 5 * 60 * 1_000;

/// Login failure state of one user on one branch (spec 2.1 §4). The caller stores it per
/// (user, branch) and passes the current time in.
///
/// Flow per login attempt: call [`ensure_not_locked`](Self::ensure_not_locked) before verifying
/// the PIN or password, then [`record_failure`](Self::record_failure) or
/// [`record_success`](Self::record_success). A wrong approver PIN in a manager override counts too
/// (spec 2.1 §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LoginAttempts {
    /// Consecutive failures since the last success, unlock or lockout.
    pub failures: u8,
    /// End of the current lockout, if one was started. It may lie in the past (expired).
    pub locked_until: Option<Timestamp>,
}

impl LoginAttempts {
    /// Whether the user is locked out at `now`. The lockout ends exactly at `locked_until`.
    #[must_use]
    pub fn is_locked(&self, now: Timestamp) -> bool {
        self.locked_until.is_some_and(|until| now < until)
    }

    /// Rejects the attempt while locked out.
    ///
    /// # Errors
    /// [`AccessError::LockedOut`] with the end of the lockout.
    pub fn ensure_not_locked(&self, now: Timestamp) -> Result<(), AccessError> {
        match self.locked_until {
            Some(until) if now < until => Err(AccessError::LockedOut { until }),
            _ => Ok(()),
        }
    }

    /// Records a wrong PIN or password at `now`.
    ///
    /// The [`MAX_CONSECUTIVE_FAILURES`]th consecutive failure starts a lockout of
    /// [`LOCKOUT_DURATION_MS`] and resets the counter, so after the lockout expires the user has a
    /// full set of attempts again. Failures during an active lockout are not counted and do not
    /// extend it.
    ///
    /// # Errors
    /// [`AccessError::LockedOut`] if the user is locked out after this failure.
    pub fn record_failure(&mut self, now: Timestamp) -> Result<(), AccessError> {
        self.ensure_not_locked(now)?;
        self.locked_until = None;
        self.failures = self.failures.saturating_add(1);
        if self.failures >= MAX_CONSECUTIVE_FAILURES {
            let until = now.saturating_add_millis(LOCKOUT_DURATION_MS);
            self.failures = 0;
            self.locked_until = Some(until);
            return Err(AccessError::LockedOut { until });
        }
        Ok(())
    }

    /// Records a successful login: resets the counter (spec 2.1 §4).
    pub fn record_success(&mut self) {
        *self = Self::default();
    }

    /// Explicit unlock by a user holding `user.unlock` (spec 2.1 §4). The permission check is the
    /// caller's job.
    pub fn unlock(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: Timestamp = Timestamp::from_unix_millis(1_000_000);

    fn at(offset_ms: i64) -> Timestamp {
        T0.saturating_add_millis(offset_ms)
    }

    /// Records `n` failures one millisecond apart and returns the result of the last one.
    fn fail_times(attempts: &mut LoginAttempts, n: u8) -> Result<(), AccessError> {
        let mut last = Ok(());
        for i in 0..n {
            last = attempts.record_failure(at(i64::from(i)));
        }
        last
    }

    /// Spec 2.1 §9 case 7.
    #[test]
    fn fifth_failure_locks_and_sixth_attempt_is_refused() {
        let mut a = LoginAttempts::default();
        fail_times(&mut a, 4).unwrap();
        assert_eq!(a.failures, 4);
        assert!(!a.is_locked(at(4)));

        let until = at(4 + LOCKOUT_DURATION_MS);
        assert_eq!(
            a.record_failure(at(4)),
            Err(AccessError::LockedOut { until })
        );
        assert!(a.is_locked(at(5)));
        // The 6th attempt is refused before the PIN is even checked.
        assert_eq!(
            a.ensure_not_locked(at(5)),
            Err(AccessError::LockedOut { until })
        );
        // A failure during the lockout does not extend it.
        assert_eq!(
            a.record_failure(at(6)),
            Err(AccessError::LockedOut { until })
        );
        assert_eq!(a.locked_until, Some(until));
    }

    #[test]
    fn lockout_expires_after_five_minutes() {
        let mut a = LoginAttempts::default();
        fail_times(&mut a, 5).unwrap_err();
        let until = a.locked_until.unwrap();
        assert_eq!(until, at(4 + LOCKOUT_DURATION_MS));

        let cases = [
            (until.saturating_add_millis(-1), true),
            (until, false),
            (until.saturating_add_millis(1), false),
        ];
        for (now, locked) in cases {
            assert_eq!(a.is_locked(now), locked, "at {now}");
            assert_eq!(a.ensure_not_locked(now).is_err(), locked, "at {now}");
        }

        // After expiry the user has a full set of attempts again.
        a.record_failure(until).unwrap();
        assert_eq!(a.failures, 1);
        assert_eq!(a.locked_until, None);
    }

    #[test]
    fn unlock_clears_lockout() {
        let mut a = LoginAttempts::default();
        fail_times(&mut a, 5).unwrap_err();
        assert!(a.is_locked(at(10)));
        a.unlock();
        assert!(!a.is_locked(at(10)));
        assert_eq!(a, LoginAttempts::default());
    }

    #[test]
    fn success_resets_counter() {
        let mut a = LoginAttempts::default();
        fail_times(&mut a, 4).unwrap();
        a.record_success();
        assert_eq!(a.failures, 0);
        // Four more failures do not lock: the counter started over.
        fail_times(&mut a, 4).unwrap();
        assert!(!a.is_locked(at(4)));
    }
}
