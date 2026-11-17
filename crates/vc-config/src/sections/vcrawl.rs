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
    /// Go-live ramp: a file of source ids (one per line, `#` comments; a relative path is
    /// resolved against `sources.dir`). When set, the scheduler seeds and enqueues only these
    /// sources and the fetcher skips (and releases) claimed URLs of any other source. Unset or
    /// empty = every enabled source. A set but unreadable file fails closed.
    pub source_allowlist: Option<String>,
    /// `vcrawl run`: seconds a SIGTERM leaves the roles to finish (in-flight fetches end, the
    /// spool is flushed, leases are released) before the rest is aborted (20: the process is
    /// gone well inside the host's `docker compose stop -t 25` and compose's 30 s
    /// `stop_grace_period`).
    pub shutdown_grace_s: u64,
    /// `vcrawl run`: size of the one shared Postgres pool; 0 = the sum of the chosen roles'
    /// needs (`vcrawl::run::pool_size`).
    pub run_pool_max: u32,
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
            source_allowlist: None,
            shutdown_grace_s: 20,
            run_pool_max: 0,
        }
    }
}
