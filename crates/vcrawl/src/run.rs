//! `vcrawl run`: several roles as tasks of one process (the 4 GB Spot VM runs them all in one
//! container).
//!
//! - **One runtime, one pool.** Every role shares the process's tokio runtime and one Postgres
//!   pool of [`pool_size`] connections (the sum of the chosen roles' needs, or
//!   `vcrawl.run_pool_max`).
//! - **One `/metrics` + `/healthz` listener** on `obs.metrics_addr`: the metrics registry is
//!   shared, every role writes its own heartbeat (`crawler_heartbeat{role="…"}` and a
//!   `heartbeats(role, instance)` row), and `/healthz` is 200 only while every role is up and
//!   not stalled ([`wiring::obs::roles_health`]).
//! - **Failure isolation.** A role that fails (error or panic) is restarted inside the process
//!   after a backoff (1 s doubling to 60 s); while it is down, `/healthz` answers 503 and its
//!   heartbeat stops (the crawler-down alert names it). The other roles keep running.
//! - **Graceful shutdown.** SIGTERM / Ctrl-C flips one shutdown flag: fetch workers stop
//!   before their next request (requests in flight finish and are spooled with fsync), claims
//!   are completed so their leases are released, the spool is flushed (sealed, uploaded,
//!   committed); the other roles finish their current step. Whatever still runs after
//!   `vcrawl.shutdown_grace_s` (25 s) is aborted, which is crash-safe: fsynced spool frames
//!   are replayed at the next start and unfinished leases expire.
//!
//! The per-role subcommands (`vcrawl fetcher`, …) are unchanged.

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::wiring::obs::RoleHealth;
use crate::{Ctx, roles, wiring};

/// A long-running role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RoleName {
    /// Sources → frontier.
    Scheduler,
    /// The claim loop.
    Fetcher,
    /// Captures → records.
    Processor,
    /// Embeddings.
    Embedder,
    /// The `/v1` API.
    Api,
    /// Partitions, purge, drills, digest.
    Maintenance,
}

impl RoleName {
    /// Every role, in start order.
    pub const ALL: [RoleName; 6] = [
        RoleName::Maintenance,
        RoleName::Scheduler,
        RoleName::Processor,
        RoleName::Embedder,
        RoleName::Fetcher,
        RoleName::Api,
    ];

    /// The role's name (heartbeats, metrics, logs).
    pub fn as_str(self) -> &'static str {
        match self {
            RoleName::Scheduler => "scheduler",
            RoleName::Fetcher => "fetcher",
            RoleName::Processor => "processor",
            RoleName::Embedder => "embedder",
            RoleName::Api => "api",
            RoleName::Maintenance => "maintenance",
        }
    }

    /// Postgres connections the role needs at most at once (small: most work is I/O on the
    /// web, the spool or GCS, and a connection is held only per statement or transaction).
    pub fn pool_need(self, cfg: &vc_config::Config) -> u32 {
        let n = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
        match self {
            RoleName::Scheduler => 2,
            // Workers mostly wait out key delays; the roller commits one segment at a time.
            RoleName::Fetcher => 2 + n(cfg.vcrawl.fetcher_concurrency.max(1)).div_ceil(2),
            RoleName::Processor => 1 + n(cfg.vcrawl.processor_concurrency.max(1)),
            RoleName::Embedder => 1,
            RoleName::Api => 4,
            // Heartbeat, alert gauges, the job tick.
            RoleName::Maintenance => 3,
        }
    }
}

impl std::fmt::Display for RoleName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RoleName {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        RoleName::ALL
            .into_iter()
            .find(|r| r.as_str() == s.trim())
            .ok_or_else(|| {
                format!(
                    "unknown role {s:?} (expected one of: {})",
                    RoleName::ALL.map(RoleName::as_str).join(", ")
                )
            })
    }
}

/// Parses `--roles a,b,c` (deduplicated, start order); empty → every role.
pub fn parse_roles(s: &str) -> Result<Vec<RoleName>, String> {
    let set: BTreeSet<RoleName> = s
        .split(',')
        .filter(|p| !p.trim().is_empty())
        .map(RoleName::from_str)
        .collect::<Result<_, _>>()?;
    if set.is_empty() {
        return Ok(RoleName::ALL.to_vec());
    }
    Ok(RoleName::ALL
        .into_iter()
        .filter(|r| set.contains(r))
        .collect())
}

/// The shared pool's size: `vcrawl.run_pool_max`, else the sum of the roles' needs.
pub fn pool_size(cfg: &vc_config::Config, roles: &[RoleName]) -> u32 {
    if cfg.vcrawl.run_pool_max > 0 {
        return cfg.vcrawl.run_pool_max;
    }
    roles.iter().map(|r| r.pool_need(cfg)).sum::<u32>().max(1)
}

/// Options of [`run_until`].
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Where the `api` role looks for the installed TLS pair (`/run/secrets`).
    pub tls_dir: std::path::PathBuf,
    /// First restart delay of a failed role (doubles up to [`RunOptions::max_backoff`]).
    pub backoff: Duration,
    /// Longest restart delay.
    pub max_backoff: Duration,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            tls_dir: vc_api::tls::SECRETS_DIR.into(),
            backoff: Duration::from_secs(1),
            max_backoff: Duration::from_secs(60),
        }
    }
}

/// `vcrawl run --roles …`: runs until SIGTERM / Ctrl-C.
pub async fn run(ctx: Ctx, roles: Vec<RoleName>) -> anyhow::Result<()> {
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        crate::data::shutdown::signal().await;
        tracing::info!("vcrawl run: shutdown requested");
        let _ = tx.send(true);
    });
    run_until(ctx, roles, RunOptions::default(), rx).await
}

/// Runs `roles` until `stop` turns true, then gives them `vcrawl.shutdown_grace_s` to finish.
pub async fn run_until(
    ctx: Ctx,
    roles: Vec<RoleName>,
    opts: RunOptions,
    stop: watch::Receiver<bool>,
) -> anyhow::Result<()> {
    wiring::obs::init_tracing(&ctx, "vcrawl");
    let roles = if roles.is_empty() {
        RoleName::ALL.to_vec()
    } else {
        roles
    };
    let ctx = match ctx.pool_if_set() {
        Some(_) => ctx,
        None => {
            let size = pool_size(&ctx.cfg, &roles);
            let secrets = wiring::sources::secrets(&ctx.cfg);
            let pool =
                wiring::db::connect_max(&ctx.cfg.database, Some(secrets.as_ref()), size).await?;
            let names: Vec<&str> = roles.iter().map(|r| r.as_str()).collect();
            tracing::info!(
                count = size,
                "vcrawl run: starting {} (pool of {size})",
                names.join(",")
            );
            ctx.with_pool(pool)
        }
    };
    let mut base = ctx.with_shutdown(stop.clone());
    base.shared_obs = true;

    let health: Vec<RoleHealth> = roles
        .iter()
        .map(|r| RoleHealth {
            role: r.as_str().into(),
            tracker: wiring::obs::tracker(&base.cfg, base.clock.clone()),
            up: Arc::new(AtomicBool::new(false)),
        })
        .collect();
    let health = Arc::new(health);
    let server = match vc_obs::bind_metrics(&base.cfg.obs).await? {
        Some(listener) => {
            let (m, h) = (base.metrics.clone(), health.clone());
            Some(tokio::spawn(async move {
                if let Err(e) = wiring::obs::serve_roles(listener, m, h).await {
                    tracing::warn!(reason = %e, "vcrawl run: metrics server stopped");
                }
            }))
        }
        None => None,
    };

    let mut set = JoinSet::new();
    for (role, h) in roles.iter().copied().zip(health.iter().cloned()) {
        let rctx = base.clone().with_tracker(h.tracker.clone());
        set.spawn(supervise(rctx, role, h.up, opts.clone(), stop.clone()));
    }

    let mut stop_wait = stop.clone();
    let _ = stop_wait.wait_for(|s| *s).await;
    let grace = Duration::from_secs(base.cfg.vcrawl.shutdown_grace_s.max(1));
    let finished = tokio::time::timeout(grace, async {
        while let Some(r) = set.join_next().await {
            if let Err(e) = r {
                tracing::warn!(reason = %e, "vcrawl run: role task ended abnormally");
            }
        }
    })
    .await;
    if finished.is_err() {
        tracing::warn!(
            "vcrawl run: roles still busy after the {}s grace period, aborting them",
            grace.as_secs()
        );
        set.abort_all();
        while set.join_next().await.is_some() {}
    }
    if let Some(s) = server {
        s.abort();
    }
    tracing::info!("vcrawl run: stopped");
    Ok(())
}

/// Runs one role, restarting it after a failure until shutdown.
async fn supervise(
    ctx: Ctx,
    role: RoleName,
    up: Arc<AtomicBool>,
    opts: RunOptions,
    mut stop: watch::Receiver<bool>,
) {
    let mut backoff = opts.backoff;
    loop {
        up.store(true, Ordering::Relaxed);
        let started = std::time::Instant::now();
        let task = tokio::spawn(start_role(ctx.clone(), role, opts.tls_dir.clone()));
        let res = task.await;
        up.store(false, Ordering::Relaxed);
        if *stop.borrow() {
            if let Ok(Err(e)) = &res {
                tracing::warn!(reason = %format!("{e:#}"), "vcrawl run: role {role} failed while stopping");
            }
            return;
        }
        match res {
            Ok(Ok(())) => tracing::error!("vcrawl run: role {role} exited before shutdown"),
            Ok(Err(e)) => {
                tracing::error!(reason = %format!("{e:#}"), "vcrawl run: role {role} failed");
            }
            Err(e) => tracing::error!(reason = %e, "vcrawl run: role {role} panicked"),
        }
        // A role that ran for a while starts over from the first delay.
        if started.elapsed() > opts.max_backoff * 2 {
            backoff = opts.backoff;
        }
        tracing::info!(
            duration_ms = u64::try_from(backoff.as_millis()).unwrap_or(u64::MAX),
            "vcrawl run: restarting role {role}"
        );
        if crate::data::shutdown::sleep_or_stop(&mut stop, backoff).await {
            return;
        }
        backoff = (backoff * 2).min(opts.max_backoff);
    }
}

async fn start_role(ctx: Ctx, role: RoleName, tls_dir: std::path::PathBuf) -> anyhow::Result<()> {
    match role {
        RoleName::Scheduler => roles::scheduler::run(ctx).await,
        RoleName::Fetcher => roles::fetcher::run(ctx).await,
        RoleName::Processor => roles::processor::run(ctx).await,
        RoleName::Embedder => roles::embedder::run(ctx).await,
        RoleName::Api => roles::api::run_with_tls_dir(ctx, &tls_dir).await,
        RoleName::Maintenance => roles::maintenance::run(ctx).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_parse_in_start_order_and_default_to_all() {
        assert_eq!(parse_roles("").unwrap(), RoleName::ALL.to_vec());
        assert_eq!(
            parse_roles("api,fetcher,api").unwrap(),
            vec![RoleName::Fetcher, RoleName::Api]
        );
        assert_eq!(
            parse_roles(" scheduler , maintenance").unwrap(),
            vec![RoleName::Maintenance, RoleName::Scheduler]
        );
        let e = parse_roles("fetcher,crawler").unwrap_err();
        assert!(e.contains("crawler") && e.contains("scheduler"), "{e}");
    }

    #[test]
    fn the_pool_is_the_sum_of_the_roles_needs_unless_configured() {
        let mut c = vc_config::Config::default();
        c.vcrawl.fetcher_concurrency = 8;
        c.vcrawl.processor_concurrency = 2;
        // scheduler 2 + fetcher 6 + processor 3 + embedder 1 + api 4 + maintenance 3
        assert_eq!(pool_size(&c, &RoleName::ALL), 19);
        assert_eq!(pool_size(&c, &[RoleName::Fetcher]), 6);
        c.vcrawl.run_pool_max = 12;
        assert_eq!(pool_size(&c, &RoleName::ALL), 12);
    }
}
