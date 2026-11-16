//! `[vcrawl]`: role wiring and maintenance (owner: vcrawl, P2a).

use serde::{Deserialize, Serialize};

/// Binary-level settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct VcrawlConfig {
    /// Instance name (`None` = hostname).
    pub instance: Option<String>,
    /// Data directory (on the data disk).
    pub data_dir: String,
    /// Fetcher concurrency (keys in flight per process).
    pub fetcher_concurrency: usize,
    /// Processor concurrency.
    pub processor_concurrency: usize,
    /// Partitions to keep created ahead, months (2).
    pub partitions_ahead_months: u32,
    /// `migrate --contract` requires the current image to have run this many days.
    pub contract_min_days: u32,
    /// Scheduler: seconds between passes (sources → seeds, source limits, robots refresh).
    pub scheduler_interval_s: u64,
    /// Scheduler: at most this many origins get a robots refresh per pass.
    pub robots_refresh_batch: u32,
    /// Fetcher: wait this long when no key is ready (milliseconds).
    pub fetcher_idle_ms: u64,
    /// Fetcher: seconds between segment-roller ticks (seal due files, upload, commit).
    pub roller_tick_s: u64,
    /// Fetcher: lower bound of the recrawl interval, seconds (the source cadence is the upper).
    pub recrawl_min_s: u64,
    /// Fetcher: recrawl interval for URLs without a source or with an `on_demand` cadence,
    /// seconds.
    pub recrawl_default_s: u64,
    /// `vcrawl health`: timeout of each dependency check, seconds.
    pub health_timeout_s: u64,
}

impl Default for VcrawlConfig {
    fn default() -> Self {
        VcrawlConfig {
            instance: None,
            data_dir: "/var/lib/voyager-crawler".into(),
            fetcher_concurrency: 16,
            processor_concurrency: 4,
            partitions_ahead_months: 2,
            contract_min_days: 7,
            scheduler_interval_s: 300,
            robots_refresh_batch: 100,
            fetcher_idle_ms: 1000,
            roller_tick_s: 5,
            recrawl_min_s: 3600,
            recrawl_default_s: 7 * 24 * 3600,
            health_timeout_s: 5,
        }
    }
}
