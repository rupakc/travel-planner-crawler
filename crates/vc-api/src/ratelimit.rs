//! Per-client rate limit: a token bucket of `rate_per_min` capacity refilled at
//! `rate_per_min / 60` tokens per second (in-process; the API runs as one process).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Token buckets keyed by client id.
#[derive(Default)]
pub(crate) struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    /// Takes one token for `client_id`, or returns the seconds to wait (1..=60).
    pub(crate) fn check(&self, client_id: &str, rate_per_min: u32) -> Result<(), u64> {
        self.check_at(client_id, rate_per_min, Instant::now())
    }

    pub(crate) fn check_at(
        &self,
        client_id: &str,
        rate_per_min: u32,
        now: Instant,
    ) -> Result<(), u64> {
        if rate_per_min == 0 {
            return Err(60);
        }
        let cap = f64::from(rate_per_min);
        let per_s = cap / 60.0;
        let mut map = self
            .buckets
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let b = map.entry(client_id.to_string()).or_insert(Bucket {
            tokens: cap,
            last: now,
        });
        let dt = now.saturating_duration_since(b.last).as_secs_f64();
        b.tokens = (b.tokens + dt * per_s).min(cap);
        b.last = now;
        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            Ok(())
        } else {
            let wait = ((1.0 - b.tokens) / per_s).ceil();
            // Bounded to 1..=60 by construction (per_s ≥ 1/60).
            Err((wait as u64).clamp(1, 60))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn bucket_refills() {
        let rl = RateLimiter::default();
        let t = Instant::now();
        assert!(rl.check_at("a", 2, t).is_ok());
        assert!(rl.check_at("a", 2, t).is_ok());
        assert_eq!(rl.check_at("a", 2, t), Err(30));
        assert!(rl.check_at("b", 2, t).is_ok(), "per client");
        assert!(rl.check_at("a", 2, t + Duration::from_secs(30)).is_ok());
        assert!(rl.check_at("a", 0, t).is_err());
    }
}
