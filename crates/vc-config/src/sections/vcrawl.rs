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
        }
    }
}
