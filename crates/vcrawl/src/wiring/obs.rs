//! Observability for a role: tracing, `/metrics` + `/healthz`, heartbeats.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::task::JoinHandle;
use vc_obs::{HeartbeatWriter, Metrics, ProgressTracker};

use crate::Ctx;

/// Installs the JSON/pretty tracing subscriber for `role` (a second call is a no-op).
pub fn init_tracing(ctx: &Ctx, role: &str) {
    if let Err(e) = vc_obs::init_tracing(&ctx.cfg.obs, role) {
        // Already installed (tests) or a bad filter: fall back to whatever is set.
        eprintln!("vcrawl: tracing not (re)initialised: {e}");
    }
}

/// The metrics registry for a process.
pub fn metrics(ctx_cfg: &vc_config::Config, clock: Arc<dyn vc_core::Clock>) -> Arc<Metrics> {
    Arc::new(Metrics::new(
        clock,
        vc_obs::heartbeat_freshness(&ctx_cfg.obs),
    ))
}

/// The progress tracker (health window `obs.progress_window_s`).
pub fn tracker(
    ctx_cfg: &vc_config::Config,
    clock: Arc<dyn vc_core::Clock>,
) -> Arc<ProgressTracker> {
    Arc::new(ProgressTracker::new(
        clock,
        Duration::from_secs(ctx_cfg.obs.progress_window_s.max(1)),
    ))
}

/// Background tasks serving `/metrics` + `/healthz` and writing heartbeats; aborted on drop.
#[derive(Debug, Default)]
pub struct ObsTasks {
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for ObsTasks {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

/// Starts the metrics/health endpoint (unless `obs.metrics_addr` is empty) and the heartbeat
/// writer for `role`.
pub async fn start(ctx: &Ctx, role: &str, pool: PgPool) -> vc_core::Result<ObsTasks> {
    let mut out = ObsTasks::default();
    if let Some(listener) = vc_obs::bind_metrics(&ctx.cfg.obs).await? {
        let (m, t) = (ctx.metrics.clone(), ctx.tracker.clone());
        out.tasks.push(tokio::spawn(async move {
            if let Err(e) = vc_obs::server::serve(listener, m, t).await {
                tracing::warn!(error = %e, "metrics server stopped");
            }
        }));
    }
    let hb = HeartbeatWriter::new(
        pool,
        role,
        &ctx.instance,
        ctx.tracker.clone(),
        ctx.metrics.clone(),
    );
    let every = Duration::from_secs(ctx.cfg.obs.heartbeat_s.max(1));
    out.tasks.push(tokio::spawn(hb.run(every)));
    Ok(out)
}

/// The local address `vcrawl health` probes for `obs.metrics_addr` (`0.0.0.0` → loopback).
pub fn local_metrics_addr(metrics_addr: &str) -> Option<SocketAddr> {
    let a = metrics_addr.trim();
    if a.is_empty() {
        return None;
    }
    let mut addr: SocketAddr = a.parse().ok()?;
    if addr.ip().is_unspecified() {
        addr.set_ip(if addr.is_ipv4() {
            std::net::Ipv4Addr::LOCALHOST.into()
        } else {
            std::net::Ipv6Addr::LOCALHOST.into()
        });
    }
    Some(addr)
}
