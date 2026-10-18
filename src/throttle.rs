//! Independent bandwidth ceilings.
//!
//! IDM exposes "Maximum download speed" (global) and "Maximum download speed for one
//! file" (per job) as separate controls. Riptide models each as its own leaky/token
//! bucket so a queue of jobs shares the global ceiling while each job may additionally
//! be capped on its own.

use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Bucket {
    last: Instant,
    tokens: f64,
}

/// Token bucket limiting `rate` bytes per second. `None` means unlimited.
pub struct RateLimiter {
    rate: Option<u64>,
    state: Mutex<Bucket>,
}

impl RateLimiter {
    pub fn new(rate: Option<u64>) -> Self {
        RateLimiter {
            rate: rate.filter(|r| *r > 0),
            state: Mutex::new(Bucket {
                last: Instant::now(),
                tokens: rate.unwrap_or(0) as f64,
            }),
        }
    }

    pub fn unlimited() -> Self {
        RateLimiter::new(None)
    }

    pub fn is_unlimited(&self) -> bool {
        self.rate.is_none()
    }

    pub fn rate(&self) -> Option<u64> {
        self.rate
    }

    /// Account for `bytes` transferred, sleeping if the bucket is empty.
    ///
    /// The sleep happens *outside* the lock so many workers can share one bucket
    /// without serialising on it.
    pub async fn acquire(&self, bytes: u64) {
        let Some(rate) = self.rate else {
            return;
        };
        if bytes == 0 {
            return;
        }
        // Cap the debit at one second's worth so an oversized chunk can never
        // starve the bucket permanently.
        let debit = bytes.min(rate) as f64;

        let wait = {
            let mut b = match self.state.lock() {
                Ok(b) => b,
                Err(p) => p.into_inner(),
            };
            let now = Instant::now();
            let elapsed = now.saturating_duration_since(b.last).as_secs_f64();
            b.last = now;
            let cap = rate as f64;
            b.tokens = (b.tokens + elapsed * cap).min(cap);
            if b.tokens >= debit {
                b.tokens -= debit;
                Duration::ZERO
            } else {
                let deficit = debit - b.tokens;
                b.tokens = 0.0;
                Duration::from_secs_f64(deficit / cap)
            }
        };

        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unlimited_never_waits() {
        let l = RateLimiter::unlimited();
        let t = Instant::now();
        for _ in 0..1000 {
            l.acquire(1 << 20).await;
        }
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[tokio::test]
    async fn bursting_then_throttling() {
        // 100 KB/s. The bucket starts full, so the first 100 KB is free and the
        // next 100 KB must wait roughly a second.
        let l = RateLimiter::new(Some(100 * 1024));
        l.acquire(100 * 1024).await;

        let t = Instant::now();
        l.acquire(100 * 1024).await;
        let waited = t.elapsed();
        assert!(waited >= Duration::from_millis(700), "waited {waited:?}");
        assert!(waited <= Duration::from_millis(1400), "waited {waited:?}");
    }

    #[test]
    fn zero_rate_is_treated_as_unlimited() {
        assert!(RateLimiter::new(Some(0)).is_unlimited());
    }
}
