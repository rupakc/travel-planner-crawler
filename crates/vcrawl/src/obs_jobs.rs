//! Operational jobs the maintenance role runs beside its own schedule (go-live review 1, 10,
//! 14): every minute the alert gauges no role measures itself ([`vc_obs::ops::export_ops`]:
//! WAL archive / base backup / light restore drill ages, failed host units, deploy drift, TLS
//! expiry, LLM month spend, an LLM forced off by its config), and every hour
//! [`vc_llm::PgLedger::settle_stale`], so a crashed caller's reservation is settled as spent
//! within about `llm.stale_settle_h` + 1 h instead of holding budget forever.

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use sqlx::PgPool;
use vc_core::Clock;
use vc_obs::Metrics;
use vc_obs::ops::{OpsInputs, export_ops};

/// How often the gauges are refreshed.
pub const EXPORT_EVERY: StdDuration = StdDuration::from_secs(60);
/// How often stale LLM reservations are settled.
pub const SETTLE_EVERY: Duration = Duration::hours(1);

/// The operational jobs over one database.
pub struct OpsJobs {
    pool: PgPool,
    metrics: Arc<Metrics>,
    inputs: OpsInputs,
    ledger: vc_llm::PgLedger,
    clock: Arc<dyn Clock>,
    last_settle: Option<DateTime<Utc>>,
}

impl OpsJobs {
    /// Jobs for `cfg` (the LLM section decides the stale-settle age and `forced_off`).
    pub fn new(
        pool: PgPool,
        metrics: Arc<Metrics>,
        cfg: &vc_config::Config,
        clock: Arc<dyn Clock>,
    ) -> Self {
        OpsJobs {
            ledger: vc_llm::PgLedger::new(pool.clone(), cfg.llm.clone(), clock.clone()),
            pool,
            metrics,
            inputs: OpsInputs {
                llm_forced_off: cfg.llm.forced_off.clone(),
            },
            clock,
            last_settle: None,
        }
    }

    /// One pass at the clock's now: the gauges, then the hourly settle when due. Returns how
    /// many stale reservations were settled (`None` when the settle was not due). Errors are
    /// logged, never returned: the next pass retries.
    pub async fn tick(&mut self) -> Option<u64> {
        let now = self.clock.now();
        if let Err(e) = export_ops(&self.pool, &self.metrics, &self.inputs, now).await {
            tracing::warn!(error = %e, "ops gauges: export failed");
        }
        if self
            .last_settle
            .is_some_and(|t| now - t < SETTLE_EVERY && now >= t)
        {
            return None;
        }
        match self.ledger.settle_stale().await {
            Ok(n) => {
                self.last_settle = Some(now);
                Some(n)
            }
            Err(e) => {
                tracing::warn!(error = %e, "llm ledger: stale settle failed");
                None
            }
        }
    }

    /// Runs [`OpsJobs::tick`] every [`EXPORT_EVERY`] until the task is aborted.
    pub async fn run(mut self) {
        if let Some(why) = &self.inputs.llm_forced_off {
            tracing::error!(reason = %why, "llm forced OFF by an invalid [llm] config");
        }
        let mut every = tokio::time::interval(EXPORT_EVERY);
        every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            every.tick().await;
            self.tick().await;
        }
    }
}
