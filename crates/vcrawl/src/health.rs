//! `vcrawl health`: the container health check (deploy contract, ledger TD).
//!
//! Exits 0 when this container's dependencies are reachable and its role is healthy:
//!
//! 1. Postgres answers `SELECT 1` (every role needs it);
//! 2. the role's own `/healthz` on `obs.metrics_addr` answers 200. That endpoint is
//!    progress-based ([`vc_obs::ProgressTracker`]): e.g. the fetcher fails it after
//!    `obs.progress_window_s` without a completion while due work exists. A role that is not
//!    serving (crashed, still starting) fails the check too. Skipped when `metrics_addr` is
//!    empty.
//!
//! Each check has `vcrawl.health_timeout_s`. Nothing is written.

use std::time::Duration;

use crate::{Ctx, wiring};

/// One failed check (no secrets, no page content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unhealthy(pub String);

impl std::fmt::Display for Unhealthy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Unhealthy {}

/// Runs every check; the first failure is returned.
pub async fn check(ctx: &Ctx) -> Result<(), Unhealthy> {
    let t = Duration::from_secs(ctx.cfg.vcrawl.health_timeout_s.max(1));
    let db = async {
        let pool = ctx.pool().await.map_err(|e| e.to_string())?;
        sqlx::query_scalar::<_, i32>("SELECT 1")
            .fetch_one(&pool)
            .await
            .map_err(|e| e.to_string())
    };
    match tokio::time::timeout(t, db).await {
        Ok(Ok(1)) => {}
        Ok(Ok(_)) => return Err(Unhealthy("postgres: unexpected answer".into())),
        Ok(Err(e)) => return Err(Unhealthy(format!("postgres unreachable: {e}"))),
        Err(_) => return Err(Unhealthy("postgres: timeout".into())),
    }
    if let Some(addr) = wiring::obs::local_metrics_addr(&ctx.cfg.obs.metrics_addr) {
        match tokio::time::timeout(t, vc_obs::server::get(addr, "/healthz")).await {
            Ok(Ok((200, _))) => {}
            Ok(Ok((status, body))) => {
                return Err(Unhealthy(format!(
                    "role unhealthy: /healthz {status} {}",
                    body.chars().take(200).collect::<String>()
                )));
            }
            Ok(Err(e)) => return Err(Unhealthy(format!("role not serving on {addr}: {e}"))),
            Err(_) => return Err(Unhealthy(format!("role health timeout on {addr}"))),
        }
    }
    Ok(())
}

/// Exits 0 when [`check`] passes (the binary's `main` turns an error into exit code 1).
pub async fn run(ctx: Ctx) -> anyhow::Result<()> {
    check(&ctx).await?;
    println!("ok");
    Ok(())
}
