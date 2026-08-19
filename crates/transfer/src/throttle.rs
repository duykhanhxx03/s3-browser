//! Queue-level bandwidth cap.
//!
//! A token bucket shared by every in-flight part. Granularity is one part: a
//! task asks for its whole part's worth of tokens before the network call, and
//! the SDK then moves those bytes as fast as it can. So the *average* rate
//! honours the limit while individual parts still arrive in bursts — throttling
//! inside a part would mean wrapping the SDK's body stream, which is a bigger
//! change than the cap is worth.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Bytes per second. Zero means unlimited, which is the default.
pub struct Throttle {
    limit: AtomicU64,
    bucket: Mutex<Bucket>,
}

struct Bucket {
    /// Tokens available now. Allowed to go negative: a part larger than one
    /// second's worth of budget borrows against the future rather than being
    /// rejected or clamped, which keeps the long-run average exact.
    tokens: f64,
    last: Instant,
}

impl Throttle {
    pub fn unlimited() -> Self {
        Self::new(0)
    }

    pub fn new(bytes_per_second: u64) -> Self {
        Self {
            limit: AtomicU64::new(bytes_per_second),
            bucket: Mutex::new(Bucket {
                tokens: bytes_per_second as f64,
                last: Instant::now(),
            }),
        }
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Changing the limit refills the bucket to one second's worth, so raising a
    /// cap takes effect immediately instead of after the old debt drains.
    pub fn set_limit(&self, bytes_per_second: u64) {
        self.limit.store(bytes_per_second, Ordering::Relaxed);
        let mut bucket = self.bucket.lock().unwrap();
        bucket.tokens = bytes_per_second as f64;
        bucket.last = Instant::now();
    }

    /// How long a caller wanting `bytes` has to wait, charging the bucket for
    /// them. Separate from the sleeping so the arithmetic is testable without a
    /// runtime, and so the lock is never held across an await.
    pub fn charge(&self, bytes: u64, now: Instant) -> Duration {
        let limit = self.limit();
        if limit == 0 {
            return Duration::ZERO;
        }

        let mut bucket = self.bucket.lock().unwrap();
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.last = now;
        // Refill, capped at one second of burst so a long idle period cannot
        // bank unlimited credit.
        bucket.tokens = (bucket.tokens + elapsed * limit as f64).min(limit as f64);
        bucket.tokens -= bytes as f64;

        if bucket.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-bucket.tokens / limit as f64)
        }
    }

    /// Waits for this part's share of the budget.
    pub async fn acquire(&self, bytes: u64) {
        let wait = self.charge(bytes, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

impl Default for Throttle {
    fn default() -> Self {
        Self::unlimited()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let throttle = Throttle::unlimited();
        let now = Instant::now();
        assert_eq!(throttle.charge(u64::MAX, now), Duration::ZERO);
    }

    #[test]
    fn the_first_second_of_budget_is_free_then_callers_wait() {
        // 1 MB/s, so the bucket starts with exactly 1 MB of credit.
        let throttle = Throttle::new(1_000_000);
        let now = Instant::now();

        // Spending the initial burst costs nothing.
        assert_eq!(throttle.charge(1_000_000, now), Duration::ZERO);
        // The next megabyte, requested at the same instant, waits a full second.
        assert_eq!(throttle.charge(1_000_000, now), Duration::from_secs(1));
    }

    #[test]
    fn debt_accumulates_so_the_average_rate_holds() {
        let throttle = Throttle::new(1_000_000);
        let now = Instant::now();
        throttle.charge(1_000_000, now); // drains the burst

        // Three more megabytes at the same instant: each waits one second longer
        // than the last, rather than each waiting one second independently.
        assert_eq!(throttle.charge(1_000_000, now), Duration::from_secs(1));
        assert_eq!(throttle.charge(1_000_000, now), Duration::from_secs(2));
        assert_eq!(throttle.charge(1_000_000, now), Duration::from_secs(3));
    }

    #[test]
    fn waiting_repays_the_debt() {
        let throttle = Throttle::new(1_000_000);
        let start = Instant::now();
        throttle.charge(2_000_000, start);

        // One megabyte of debt at 1 MB/s clears after a second, and by then a
        // fresh megabyte is affordable again.
        let later = start + Duration::from_secs(2);
        assert_eq!(throttle.charge(1_000_000, later), Duration::ZERO);
    }

    #[test]
    fn idle_time_cannot_bank_more_than_one_second_of_burst() {
        let throttle = Throttle::new(1_000_000);
        let start = Instant::now();
        // An hour idle must not buy an hour's worth of credit.
        let much_later = start + Duration::from_secs(3600);
        assert_eq!(throttle.charge(1_000_000, much_later), Duration::ZERO);
        assert_eq!(throttle.charge(1_000_000, much_later), Duration::from_secs(1));
    }

    #[test]
    fn raising_the_limit_takes_effect_at_once() {
        let throttle = Throttle::new(1_000_000);
        let now = Instant::now();
        throttle.charge(5_000_000, now); // deep in debt
        assert!(throttle.charge(1, now) > Duration::ZERO);

        throttle.set_limit(10_000_000);
        // The old debt is gone rather than being repaid at the new rate.
        assert_eq!(throttle.charge(10_000_000, now), Duration::ZERO);
    }

    #[test]
    fn dropping_to_unlimited_stops_waiting() {
        let throttle = Throttle::new(1_000);
        let now = Instant::now();
        throttle.charge(100_000, now);
        assert!(throttle.charge(1_000, now) > Duration::ZERO);

        throttle.set_limit(0);
        assert_eq!(throttle.charge(u64::MAX, now), Duration::ZERO);
    }
}
