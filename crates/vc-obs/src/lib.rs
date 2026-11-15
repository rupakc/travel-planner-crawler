//! # vc-obs
//!
//! Observability for every role (spec §9, §6, §17.5):
//! - [`logging`]: JSON tracing for Cloud Logging with a **field allowlist** (never page
//!   content, tokens or secrets);
//! - [`metrics`]: the Prometheus registry, with the alert metrics under the names the policies
//!   in `infrastructure/monitoring/` read ([`ALERT_METRICS`]);
//! - [`health`]: progress-based health ([`ProgressTracker`]);
//! - [`heartbeat`]: the heartbeat writer ([`HeartbeatWriter`]);
//! - [`server`]: `GET /metrics` and `GET /healthz` on `obs.metrics_addr`.

pub mod health;
pub mod heartbeat;
pub mod logging;
pub mod metrics;
pub mod server;

pub use health::{Health, ProgressTracker};
pub use heartbeat::HeartbeatWriter;
pub use metrics::{ALERT_METRICS, AlertMetric, Metrics};

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use vc_config::ObsConfig;
use vc_core::{Error, Result};

/// How long a role's heartbeat series stays exported after a beat: 3 × `obs.heartbeat_s`.
pub fn heartbeat_freshness(config: &ObsConfig) -> std::time::Duration {
    std::time::Duration::from_secs(config.heartbeat_s.max(1).saturating_mul(3))
}

/// Binds `obs.metrics_addr` for [`server::serve`]; `None` when it is empty (disabled).
pub async fn bind_metrics(config: &ObsConfig) -> Result<Option<tokio::net::TcpListener>> {
    if config.metrics_addr.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(
        tokio::net::TcpListener::bind(config.metrics_addr.trim()).await?,
    ))
}

/// Installs the global tracing subscriber: the allowlisting layer (`obs.log_format` `json` or
/// `pretty`) on stdout, filtered by `obs.log_level` (`RUST_LOG` syntax). Fails if a global
/// subscriber is already set or the filter is invalid.
pub fn init_tracing(config: &ObsConfig, role: &str) -> Result<()> {
    let filter = EnvFilter::try_new(&config.log_level)
        .map_err(|e| Error::Invalid(format!("obs.log_level: {e}")))?;
    let set = match config.log_format.as_str() {
        "json" => tracing::subscriber::set_global_default(
            tracing_subscriber::registry()
                .with(filter)
                .with(logging::json_layer(role, std::io::stdout)),
        ),
        "pretty" => tracing::subscriber::set_global_default(
            tracing_subscriber::registry()
                .with(filter)
                .with(logging::pretty_layer(role, std::io::stdout)),
        ),
        other => {
            return Err(Error::Invalid(format!(
                "obs.log_format must be json or pretty, not {other:?}"
            )));
        }
    };
    set.map_err(|e| Error::Invalid(format!("tracing: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn metrics_bind_can_be_disabled() {
        let mut c = ObsConfig {
            metrics_addr: String::new(),
            ..ObsConfig::default()
        };
        assert!(bind_metrics(&c).await.unwrap().is_none());
        c.metrics_addr = "127.0.0.1:0".into();
        assert!(bind_metrics(&c).await.unwrap().is_some());
        assert_eq!(heartbeat_freshness(&c).as_secs(), 180);
    }

    #[test]
    fn bad_settings_are_rejected() {
        let mut c = ObsConfig {
            log_format: "xml".into(),
            ..ObsConfig::default()
        };
        assert!(matches!(init_tracing(&c, "api"), Err(Error::Invalid(_))));
        c.log_format = "json".into();
        c.log_level = "info,=[".into();
        assert!(matches!(init_tracing(&c, "api"), Err(Error::Invalid(_))));
    }
}
