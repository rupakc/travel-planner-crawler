//! `[frontier]`: claims, leases, delays, park ladder, global rate (owner: vc-frontier).

use serde::{Deserialize, Serialize};

/// Frontier settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FrontierConfig {
    /// Shards this process claims from.
    pub shards: Vec<i16>,
    /// URLs leased per claim (≤ 5).
    pub max_urls_per_claim: usize,
    /// Per-URL budget in seconds; the claim deadline is `max_urls_per_claim ×` this (≈ 4 min).
    pub per_url_budget_s: u64,
    /// Global fetch rate across all processes, requests/second.
    pub global_rate_per_s: f64,
    /// Token bucket burst capacity.
    pub global_rate_burst: f64,
    /// Default minimum delay between requests to a key, seconds.
    pub min_delay_s: u64,
    /// Delay cap, seconds (1 h).
    pub max_delay_s: u64,
    /// Delay factor on the EWMA latency (10 ×).
    pub latency_factor: f64,
    /// EWMA smoothing factor in (0, 1].
    pub ewma_alpha: f64,
    /// Default daily request budget per key (sources can only lower it).
    pub daily_budget: u32,
    /// First park duration, hours (24).
    pub park_initial_h: u64,
    /// Max park duration, days (30).
    pub park_max_days: u64,
    /// Consecutive 5xx before a 1 h park.
    pub server_error_threshold: u32,
    /// Park after the 5xx threshold, hours.
    pub server_error_park_h: u64,
    /// Max back-off for 429/503 without Retry-After, hours.
    pub rate_limit_backoff_max_h: u64,
    /// Rows per enqueue batch.
    pub enqueue_batch: usize,
    /// Holder name written to `claims.holder` (`None` = hostname + pid).
    pub holder: Option<String>,
}

impl Default for FrontierConfig {
    fn default() -> Self {
        FrontierConfig {
            shards: vec![0],
            max_urls_per_claim: 5,
            per_url_budget_s: 240,
            global_rate_per_s: 4.0,
            global_rate_burst: 4.0,
            min_delay_s: 5,
            max_delay_s: 3600,
            latency_factor: 10.0,
            ewma_alpha: 0.3,
            daily_budget: 3000,
            park_initial_h: 24,
            park_max_days: 30,
            server_error_threshold: 5,
            server_error_park_h: 1,
            rate_limit_backoff_max_h: 24,
            enqueue_batch: 1000,
            holder: None,
        }
    }
}
