//! Recrawl scheduling: the Cho & Garcia-Molina change-rate estimator (spec §5.3).
//!
//! A page fetched `n` times at a regular interval `I`, whose content changed between `x` of
//! those fetches, has a Poisson change rate estimated by
//!
//! ```text
//! r̂ = −ln((n − x + 0.5) / (n + 0.5)) / I
//! ```
//!
//! (Cho & Garcia-Molina, "Estimating frequency of change", 2003). Unlike the naive `x / (n·I)`
//! it is not biased low when a page changes more than once between visits. The recrawl interval
//! is the expected time between changes, `1 / r̂`, bounded by the source cadence.

use std::time::Duration;

/// The estimated change rate in changes per second, or `None` without history (`n = 0` or a
/// zero interval). `x` is clamped to `n`.
pub fn change_rate(n: u32, x: u32, interval: Duration) -> Option<f64> {
    let i = interval.as_secs_f64();
    if n == 0 || i <= 0.0 {
        return None;
    }
    let n = f64::from(n);
    let x = f64::from(x).min(n);
    Some(-((n - x + 0.5) / (n + 0.5)).ln() / i)
}

/// The next recrawl interval: `1 / r̂`, clamped to `[min, max]`. Without history it is `min`;
/// a page that never changed gets `max`.
pub fn next_interval(n: u32, x: u32, interval: Duration, min: Duration, max: Duration) -> Duration {
    let max = max.max(min);
    match change_rate(n, x, interval) {
        None => min,
        Some(r) if r <= 0.0 || !r.is_finite() => max,
        Some(r) => {
            let secs = (1.0 / r).clamp(min.as_secs_f64(), max.as_secs_f64());
            Duration::from_secs_f64(secs)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// splitmix64: a tiny deterministic generator for the simulation.
    fn next_u64(s: &mut u64) -> u64 {
        *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = *s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn unit(s: &mut u64) -> f64 {
        (next_u64(s) >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Simulates `n` visits at interval `i` to a page changing as a Poisson process of rate
    /// `lambda`, returning the number of visits that saw a change.
    fn simulate(lambda: f64, i: f64, n: u32, seed: u64) -> u32 {
        let p_change = 1.0 - (-lambda * i).exp();
        let mut s = seed;
        (0..n).filter(|_| unit(&mut s) < p_change).count() as u32
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        /// With enough visits the estimate converges to the true rate (within 15 %), even when
        /// the page changes up to twice per visit interval.
        #[test]
        fn estimator_converges(lambda_i in 0.05f64..2.0, i_secs in 60u64..86_400, seed in any::<u64>()) {
            let i = i_secs as f64;
            let lambda = lambda_i / i;
            let n = 20_000;
            let x = simulate(lambda, i, n, seed);
            let est = change_rate(n, x, Duration::from_secs(i_secs)).unwrap();
            let rel = (est - lambda).abs() / lambda;
            prop_assert!(rel < 0.15, "lambda {lambda} est {est} rel {rel}");
        }

        /// The interval is always within the bounds.
        #[test]
        fn next_interval_is_bounded(n in 0u32..1000, x in 0u32..1000, i in 0u64..1_000_000,
                                    lo in 1u64..10_000, span in 0u64..1_000_000) {
            let min = Duration::from_secs(lo);
            let max = Duration::from_secs(lo + span);
            let d = next_interval(n, x, Duration::from_secs(i), min, max);
            prop_assert!(d >= min && d <= max);
        }
    }

    #[test]
    fn less_biased_than_naive_for_fast_changing_pages() {
        // λ·I = 1.5: the naive estimator x/(n·I) is strongly biased low.
        let (lambda, i, n) = (1.5, 1.0, 20_000);
        let x = simulate(lambda, i, n, 7);
        let est = change_rate(n, x, Duration::from_secs(1)).unwrap();
        let naive = f64::from(x) / f64::from(n);
        assert!((est - lambda).abs() < (naive - lambda).abs());
    }

    #[test]
    fn edge_cases() {
        let h = Duration::from_secs(3600);
        let d = Duration::from_secs(86_400);
        assert_eq!(change_rate(0, 0, h), None);
        assert_eq!(change_rate(5, 1, Duration::ZERO), None);
        assert_eq!(next_interval(0, 0, h, h, d), h);
        assert_eq!(next_interval(10, 0, h, h, d), d);
        // x > n is clamped.
        assert!(change_rate(3, 9, h).unwrap() > 0.0);
        // max < min collapses to min.
        assert_eq!(next_interval(10, 0, h, d, h), d);
    }
}
