//! # vc-obs (Task 13, stub)
//!
//! JSON tracing with a field allowlist (never page content or tokens), the Prometheus registry,
//! progress-based health and the heartbeat writer.

use vc_config::ObsConfig;

/// Installs the global tracing subscriber.
pub fn init_tracing(_config: &ObsConfig, _role: &str) -> vc_core::Result<()> {
    todo!("Task 13")
}
