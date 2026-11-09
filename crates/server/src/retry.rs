//! Rerunning a write transaction that lost a race (a unique key or counter taken by a concurrent
//! transaction, a `MySQL` deadlock or lock wait timeout, a busy `SQLite` database:
//! `sultan_db::DbError::is_retryable`).
//!
//! [`run`] tries the whole operation at most [`ATTEMPTS`] (5) times, so there are at most 4
//! waits. Before retry `n` (after attempt `n` lost) it sleeps a jittered [`backoff`] in
//! `[ceiling / 2, ceiling]`, with ceilings of 5, 10, 20 and 40 ms for the 4 retries (75 ms of
//! sleep at most in total), so racing writers spread out instead of colliding again at once.
//!
//! Worst case: on `SQLite` each attempt can also wait up to the 5 s busy timeout for the write
//! lock (`BEGIN IMMEDIATE`, see `sultan_db::Db::begin`), so a request gives up after about
//! 5 × 5 s + 75 ms ≈ 25 s. On `MySQL` an attempt is bounded by `innodb_lock_wait_timeout`.
//!
//! When every attempt lost a race the caller gets [`Failure::Exhausted`] and answers `409`
//! ([`conflict`] or a more specific conflict), never `500`: the request itself was fine and may
//! simply be sent again. A non-retryable error ends the loop after that attempt
//! ([`Failure::Final`]).

use std::fmt;
use std::future::Future;
use std::time::Duration;

use axum::http::StatusCode;
use uuid::Uuid;

use crate::error::ApiError;

/// How often a write transaction runs when it keeps losing races (spec 2.2 §12: at most 5).
pub const ATTEMPTS: u32 = 5;
/// Upper bound of the delay after the first lost race.
const BASE_DELAY: Duration = Duration::from_millis(5);
/// Largest delay between two attempts (only reached if [`ATTEMPTS`] grows beyond 5).
const MAX_DELAY: Duration = Duration::from_millis(50);

/// Why [`run`] gave up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure<E> {
    /// The operation failed with an error that is not worth retrying.
    Final(E),
    /// Every one of the [`ATTEMPTS`] lost a race; this is the last error.
    Exhausted(E),
}

/// The delay after attempt `attempt` (1-based) lost a race: a value in `[ceiling / 2, ceiling]`
/// picked by `entropy`, where the ceiling doubles from [`BASE_DELAY`] up to [`MAX_DELAY`].
#[must_use]
pub fn backoff(attempt: u32, entropy: u64) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    let ceiling = BASE_DELAY.saturating_mul(1 << doublings).min(MAX_DELAY);
    let ceiling = u64::try_from(ceiling.as_micros()).unwrap_or(u64::MAX);
    let floor = ceiling / 2;
    Duration::from_micros(floor + entropy % (ceiling - floor + 1))
}

/// Cheap randomness for the jitter: the random bits of a fresh UUID v7.
fn entropy() -> u64 {
    // The low 64 bits of a v7 UUID hold 62 random bits (and the 2 variant bits).
    u64::try_from(Uuid::now_v7().as_u128() & u128::from(u64::MAX)).unwrap_or_default()
}

/// Runs `op` until it succeeds, fails with an error `retryable` rejects, or [`ATTEMPTS`] attempts
/// all failed with retryable errors; sleeps [`backoff`] between attempts. `what` names the
/// operation in the debug log.
///
/// # Errors
/// [`Failure::Final`] with the first non-retryable error, or [`Failure::Exhausted`] with the
/// last retryable one.
pub async fn run<T, E, F, Fut>(
    what: &'static str,
    mut op: F,
    retryable: impl Fn(&E) -> bool,
) -> Result<T, Failure<E>>
where
    E: fmt::Debug,
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt = 1;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(error) if !retryable(&error) => return Err(Failure::Final(error)),
            Err(error) if attempt >= ATTEMPTS => {
                tracing::warn!(?error, attempt, what, "write kept losing races; giving up");
                return Err(Failure::Exhausted(error));
            }
            Err(error) => {
                let delay = backoff(attempt, entropy());
                tracing::debug!(?error, attempt, ?delay, what, "write lost a race; retrying");
                tokio::time::sleep(delay).await;
                attempt += 1;
            }
        }
    }
}

/// `409 conflict`: the write kept colliding with concurrent writes (answer of an exhausted
/// [`run`] whose last error says nothing more specific).
#[must_use]
pub fn conflict() -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "conflict",
        "the change collided with concurrent changes; try again",
    )
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    #[test]
    fn backoff_grows_with_jitter_and_is_capped() {
        let ms =
            |attempt, entropy: u64| u64::try_from(backoff(attempt, entropy).as_micros()).unwrap();
        // Ceilings 5, 10, 20, 40, 50, 50 ms (in µs here); the delay is in [ceiling / 2, ceiling].
        for (attempt, ceiling) in [
            (1, 5_000_u64),
            (2, 10_000),
            (3, 20_000),
            (4, 40_000),
            (5, 50_000),
        ] {
            assert_eq!(ms(attempt, 0), ceiling / 2, "attempt {attempt}");
            assert_eq!(ms(attempt, ceiling / 2), ceiling, "attempt {attempt}");
            for entropy in [1, 7, 12_345, u64::MAX] {
                let delay = ms(attempt, entropy);
                assert!(
                    (ceiling / 2..=ceiling).contains(&delay),
                    "{attempt} {delay}"
                );
            }
        }
        assert_eq!(ms(40, u64::MAX), ms(5, u64::MAX));
        assert_eq!(ms(u32::MAX, 0), 25_000);
        // The jitter really varies.
        let delays: std::collections::HashSet<_> = (0..32).map(|_| backoff(3, entropy())).collect();
        assert!(delays.len() > 1);
    }

    /// The module docs promise 4 waits of at most 5, 10, 20 and 40 ms (75 ms in total).
    #[test]
    fn retry_waits_match_the_documented_sequence() {
        assert_eq!(ATTEMPTS, 5);
        let mut total = Duration::ZERO;
        for (attempt, ceiling_ms) in (1..ATTEMPTS).zip([5_u64, 10, 20, 40]) {
            // `backoff` lies in `[ceiling / 2, ceiling]`; this entropy picks the ceiling itself.
            let longest = backoff(attempt, ceiling_ms * 500);
            assert_eq!(longest, Duration::from_millis(ceiling_ms), "{attempt}");
            assert!(backoff(attempt, u64::MAX) <= longest);
            total += longest;
        }
        assert_eq!(total, Duration::from_millis(75));
    }

    #[tokio::test]
    async fn lost_races_are_retried_until_one_wins() {
        let calls = Cell::new(0);
        let calls = &calls;
        let started = std::time::Instant::now();
        let value = run(
            "test",
            || async move {
                calls.set(calls.get() + 1);
                if calls.get() < 4 {
                    Err("lost")
                } else {
                    Ok(calls.get())
                }
            },
            |_| true,
        )
        .await
        .unwrap();
        assert_eq!(value, 4);
        // Three backoffs of at least 2.5, 5 and 10 ms.
        assert!(started.elapsed() >= Duration::from_micros(17_500));
    }

    #[tokio::test]
    async fn other_errors_are_final_at_once() {
        let calls = Cell::new(0);
        let calls = &calls;
        let err = run(
            "test",
            || async move {
                calls.set(calls.get() + 1);
                Result::<(), _>::Err("broken")
            },
            |e| *e == "lost",
        )
        .await
        .unwrap_err();
        assert_eq!((calls.get(), err), (1, Failure::Final("broken")));
    }

    #[tokio::test]
    async fn every_race_lost_gives_up_after_all_attempts() {
        let calls = Cell::new(0);
        let calls = &calls;
        let err = run(
            "test",
            || async move {
                calls.set(calls.get() + 1);
                Result::<(), _>::Err(calls.get())
            },
            |_| true,
        )
        .await
        .unwrap_err();
        assert_eq!(calls.get(), ATTEMPTS);
        assert_eq!(err, Failure::Exhausted(ATTEMPTS));
        let api = conflict();
        assert_eq!((api.status().as_u16(), api.code()), (409, "conflict"));
    }
}
