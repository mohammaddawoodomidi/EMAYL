//! Retry with exponential backoff and jitter.
//!
//! Used for transient failures only (timeouts, dropped connections, 4xx-style
//! "try later" answers). Permanent failures (bad password, untrusted certificate)
//! must never be retried, so the caller passes in an `is_transient` test.

use std::future::Future;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Delay before the first retry.
    pub base: Duration,
    /// No delay is ever longer than this.
    pub cap: Duration,
    /// Total attempts including the first one. `u32::MAX` means "keep trying".
    pub max_attempts: u32,
}

impl RetryPolicy {
    /// Background sync: patient, because the user is not waiting on it.
    pub const SYNC: Self = Self {
        base: Duration::from_secs(5),
        cap: Duration::from_secs(15 * 60),
        max_attempts: u32::MAX,
    };

    /// Something the user is waiting on (adding an account, opening a folder).
    pub const INTERACTIVE: Self = Self {
        base: Duration::from_millis(500),
        cap: Duration::from_secs(4),
        max_attempts: 3,
    };

    /// Delay before retry number `attempt` (0 = first retry). `jitter` is in
    /// `[0, 1]`: the delay is half fixed and half random ("equal jitter"), so
    /// many clients recovering together do not stampede the server, yet there
    /// is never a near-zero delay.
    pub fn delay_for(&self, attempt: u32, jitter: f64) -> Duration {
        let jitter = if jitter.is_finite() {
            jitter.clamp(0.0, 1.0)
        } else {
            0.5
        };
        let factor = 1u32.checked_shl(attempt.min(30)).unwrap_or(u32::MAX);
        let raw = self.base.saturating_mul(factor).min(self.cap);
        let half = raw / 2;
        half + half.mul_f64(jitter)
    }
}

/// A random number in `[0, 1]` from the OS; 0.5 if the OS refuses.
pub fn random_unit() -> f64 {
    match getrandom::u32() {
        Ok(v) => f64::from(v) / f64::from(u32::MAX),
        Err(_) => 0.5,
    }
}

/// Runs `op` until it succeeds, fails permanently, or attempts run out.
pub async fn retry<T, E, F, Fut>(
    policy: RetryPolicy,
    is_transient: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt: u32 = 0;
    loop {
        match op(attempt).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                let next = attempt.saturating_add(1);
                if !is_transient(&e) || next >= policy.max_attempts {
                    return Err(e);
                }
                tokio::time::sleep(policy.delay_for(attempt, random_unit())).await;
                attempt = next;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    const P: RetryPolicy = RetryPolicy {
        base: Duration::from_millis(1),
        cap: Duration::from_millis(4),
        max_attempts: 4,
    };

    #[test]
    fn delays_grow_then_cap() {
        let p = RetryPolicy {
            base: Duration::from_secs(1),
            cap: Duration::from_secs(60),
            max_attempts: 10,
        };
        assert_eq!(p.delay_for(0, 0.0), Duration::from_millis(500));
        assert_eq!(p.delay_for(0, 1.0), Duration::from_secs(1));
        assert_eq!(p.delay_for(2, 1.0), Duration::from_secs(4));
        assert_eq!(p.delay_for(30, 1.0), Duration::from_secs(60));
        assert_eq!(p.delay_for(u32::MAX, 1.0), Duration::from_secs(60));
    }

    #[test]
    fn jitter_is_clamped_and_never_below_half() {
        let p = RetryPolicy::SYNC;
        assert_eq!(p.delay_for(3, -5.0), p.delay_for(3, 0.0));
        assert_eq!(p.delay_for(3, 99.0), p.delay_for(3, 1.0));
        assert_eq!(p.delay_for(3, f64::NAN), p.delay_for(3, 0.5));
        assert!(p.delay_for(3, 0.0) >= p.delay_for(3, 1.0) / 2);
    }

    #[tokio::test]
    async fn succeeds_after_transient_failures() {
        let calls = AtomicU32::new(0);
        let r: Result<&str, &str> = retry(
            P,
            |e| *e == "again",
            |_| async {
                if calls.fetch_add(1, Ordering::SeqCst) < 2 {
                    Err("again")
                } else {
                    Ok("done")
                }
            },
        )
        .await;
        assert_eq!(r, Ok("done"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn permanent_error_is_not_retried() {
        let calls = AtomicU32::new(0);
        let r: Result<(), &str> = retry(
            P,
            |e| *e == "again",
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("bad password")
            },
        )
        .await;
        assert_eq!(r, Err("bad password"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        let calls = AtomicU32::new(0);
        let r: Result<(), &str> = retry(
            P,
            |_| true,
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err("again")
            },
        )
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
