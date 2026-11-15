//! Pure per-key scheduling arithmetic (spec §5.2 "Delay and budgets", "Back-off"): the delay
//! formula, the EWMA, the 429/503 back-off and the park ladder. The SQL in `lib.rs` only reads
//! the key row, calls [`next_key_state`] and writes the result back.

use std::time::Duration;

use vc_config::FrontierConfig;
use vc_core::frontier::{KeyOutcome, KeySignal, ParkReason};

/// The scheduling columns of a `politeness_keys` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyRow {
    /// Source minimum delay (`min_delay_ms`).
    pub min_delay_ms: i64,
    /// robots `Crawl-delay` (`crawl_delay_ms`).
    pub crawl_delay_ms: Option<i64>,
    /// EWMA response time (`ewma_latency_ms`).
    pub ewma_latency_ms: Option<i64>,
    /// Back-off penalty (`penalty_ms`).
    pub penalty_ms: i64,
    /// Consecutive 5xx (`consecutive_5xx`).
    pub consecutive_5xx: i64,
    /// Park ladder level (`park_level`).
    pub park_level: i64,
    /// Whether the key is parked right now.
    pub parked: bool,
}

/// A park to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Park {
    /// How long.
    pub duration: Duration,
    /// The new ladder level.
    pub level: i64,
}

/// The new scheduling state of a key after a completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextKeyState {
    /// New `crawl_delay_ms`.
    pub crawl_delay_ms: Option<i64>,
    /// New `ewma_latency_ms`.
    pub ewma_latency_ms: Option<i64>,
    /// New `penalty_ms`.
    pub penalty_ms: i64,
    /// New `consecutive_5xx`.
    pub consecutive_5xx: i64,
    /// New `delay_ms` (the per-request delay, ≤ the cap).
    pub delay_ms: i64,
    /// Time from now until the key may be claimed again: `max(delay, penalty)`.
    pub wait_ms: i64,
    /// A park to apply, if any.
    pub park: Option<(Park, ParkReason)>,
}

fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn hours_ms(h: u64) -> i64 {
    ms(Duration::from_secs(h.saturating_mul(3600)))
}

/// The per-request delay: `max(Crawl-delay, min delay, factor × EWMA, penalty)`, capped.
pub fn delay_ms(cfg: &FrontierConfig, row: &KeyRow) -> i64 {
    let floor = row.min_delay_ms.max(ms(Duration::from_secs(cfg.min_delay_s)));
    let latency = row
        .ewma_latency_ms
        .map(|e| (e as f64 * cfg.latency_factor).round() as i64)
        .unwrap_or(0);
    let d = floor
        .max(row.crawl_delay_ms.unwrap_or(0))
        .max(latency)
        .max(row.penalty_ms);
    d.clamp(0, ms(Duration::from_secs(cfg.max_delay_s)))
}

/// The EWMA of the response time.
pub fn ewma(prev: Option<i64>, sample: Duration, alpha: f64) -> i64 {
    let s = sample.as_secs_f64() * 1000.0;
    let a = alpha.clamp(f64::MIN_POSITIVE, 1.0);
    let v = match prev {
        None => s,
        Some(p) => a * s + (1.0 - a) * p as f64,
    };
    v.round().clamp(0.0, i32::MAX as f64) as i64
}

/// The park for `reason` at ladder `level`: 5xx parks are fixed (1 h by default) and leave the
/// ladder alone; everything else is `initial × 2^level`, capped at the maximum, and climbs.
pub fn park_for(cfg: &FrontierConfig, level: i64, reason: &ParkReason) -> Park {
    if *reason == ParkReason::ServerErrors {
        return Park {
            duration: Duration::from_secs(cfg.server_error_park_h.saturating_mul(3600)),
            level,
        };
    }
    let max = Duration::from_secs(cfg.park_max_days.saturating_mul(86_400));
    let shift = u32::try_from(level.clamp(0, 32)).unwrap_or(32);
    let hours = cfg.park_initial_h.saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX));
    let duration = Duration::from_secs(hours.saturating_mul(3600)).min(max);
    Park {
        duration,
        level: (level + 1).min(32),
    }
}

/// Applies a completion outcome to a key row.
pub fn next_key_state(cfg: &FrontierConfig, row: &KeyRow, o: &KeyOutcome) -> NextKeyState {
    let mut r = *row;
    if let Some(cd) = o.crawl_delay {
        r.crawl_delay_ms = Some(ms(cd));
    }
    if let Some(lat) = o.mean_latency {
        r.ewma_latency_ms = Some(ewma(row.ewma_latency_ms, lat, cfg.ewma_alpha));
    }
    let max_backoff = hours_ms(cfg.rate_limit_backoff_max_h);
    let mut park = None;
    match &o.signal {
        KeySignal::Normal => {
            r.penalty_ms /= 2;
            if r.penalty_ms < 1000 {
                r.penalty_ms = 0;
            }
            r.consecutive_5xx = 0;
        }
        KeySignal::RateLimited { retry_after } => {
            let base = delay_ms(cfg, &r).max(1000);
            let p = match retry_after {
                Some(ra) => ms(*ra),
                None => r.penalty_ms.saturating_mul(2).max(base.saturating_mul(2)),
            };
            r.penalty_ms = p.clamp(0, max_backoff);
        }
        KeySignal::ServerErrors { consecutive } => {
            r.consecutive_5xx = r.consecutive_5xx.saturating_add(i64::from(*consecutive));
            if r.consecutive_5xx >= i64::from(cfg.server_error_threshold) {
                r.consecutive_5xx = 0;
                if !r.parked {
                    let reason = ParkReason::ServerErrors;
                    park = Some((park_for(cfg, r.park_level, &reason), reason));
                }
            }
        }
        KeySignal::Park(reason) => {
            if !r.parked {
                park = Some((park_for(cfg, r.park_level, reason), reason.clone()));
            }
        }
    }
    let d = delay_ms(cfg, &r);
    NextKeyState {
        crawl_delay_ms: r.crawl_delay_ms,
        ewma_latency_ms: r.ewma_latency_ms,
        penalty_ms: r.penalty_ms,
        consecutive_5xx: r.consecutive_5xx,
        delay_ms: d,
        wait_ms: d.max(r.penalty_ms),
        park,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> FrontierConfig {
        FrontierConfig::default()
    }

    #[test]
    fn delay_is_max_of_inputs_and_capped() {
        let c = cfg();
        let mut r = KeyRow {
            min_delay_ms: 5000,
            ..KeyRow::default()
        };
        assert_eq!(delay_ms(&c, &r), 5000);
        r.crawl_delay_ms = Some(12_000);
        assert_eq!(delay_ms(&c, &r), 12_000);
        r.ewma_latency_ms = Some(2000);
        assert_eq!(delay_ms(&c, &r), 20_000);
        r.penalty_ms = 50_000;
        assert_eq!(delay_ms(&c, &r), 50_000);
        r.penalty_ms = 86_400_000;
        assert_eq!(delay_ms(&c, &r), 3_600_000);
        // The config floor applies even if the row says less.
        r = KeyRow::default();
        assert_eq!(delay_ms(&c, &r), 5000);
    }

    #[test]
    fn ewma_smooths() {
        assert_eq!(ewma(None, Duration::from_millis(100), 0.3), 100);
        assert_eq!(ewma(Some(100), Duration::from_millis(200), 0.5), 150);
        assert_eq!(ewma(Some(100), Duration::from_millis(200), 0.0), 100);
    }

    #[test]
    fn ladder() {
        let c = cfg();
        let hours: Vec<u64> = (0..8)
            .map(|l| park_for(&c, l, &ParkReason::Challenge).duration.as_secs() / 3600)
            .collect();
        assert_eq!(hours, vec![24, 48, 96, 192, 384, 720, 720, 720]);
        assert_eq!(park_for(&c, 100, &ParkReason::Challenge).level, 32);
        assert_eq!(
            park_for(&c, 100, &ParkReason::Challenge).duration,
            Duration::from_secs(30 * 86_400)
        );
        let p = park_for(&c, 3, &ParkReason::ServerErrors);
        assert_eq!((p.duration.as_secs(), p.level), (3600, 3));
    }

    #[test]
    fn signals() {
        let c = cfg();
        let row = KeyRow {
            min_delay_ms: 5000,
            ..KeyRow::default()
        };
        let mut o = KeyOutcome::normal();
        o.signal = KeySignal::Park(ParkReason::Challenge);
        let s = next_key_state(&c, &row, &o);
        assert_eq!(s.park.as_ref().map(|p| p.0.level), Some(1));
        // Already parked: no second climb.
        let parked = KeyRow {
            parked: true,
            ..row
        };
        assert!(next_key_state(&c, &parked, &o).park.is_none());
        o.signal = KeySignal::ServerErrors { consecutive: 9 };
        assert!(next_key_state(&c, &parked, &o).park.is_none());
        let s = next_key_state(&c, &row, &o);
        assert_eq!(s.consecutive_5xx, 0);
        assert_eq!(s.park.unwrap().1, ParkReason::ServerErrors);
        // Rate limit without Retry-After: twice the current delay first.
        o.signal = KeySignal::RateLimited { retry_after: None };
        let s = next_key_state(&c, &row, &o);
        assert_eq!((s.penalty_ms, s.wait_ms), (10_000, 10_000));
        // Normal decays a small penalty to zero.
        let small = KeyRow {
            penalty_ms: 1500,
            ..row
        };
        let s = next_key_state(&c, &small, &KeyOutcome::normal());
        assert_eq!(s.penalty_ms, 0);
    }
}
