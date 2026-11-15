//! `[obs]`: logs, metrics, health (owner: vc-obs).

use serde::{Deserialize, Serialize};

/// Observability settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ObsConfig {
    /// `json` (Cloud Logging) or `pretty`.
    pub log_format: String,
    /// `RUST_LOG`-style filter.
    pub log_level: String,
    /// Heartbeat interval, seconds.
    pub heartbeat_s: u64,
    /// Health fails without progress for this long while work is due, seconds (600).
    pub progress_window_s: u64,
    /// Listen address of the role's `/metrics` + `/healthz` endpoint (Ops Agent scrape,
    /// `vcrawl health`); empty disables it.
    pub metrics_addr: String,
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            log_format: "json".into(),
            log_level: "info".into(),
            heartbeat_s: 60,
            progress_window_s: 600,
            metrics_addr: "0.0.0.0:9464".into(),
        }
    }
}
