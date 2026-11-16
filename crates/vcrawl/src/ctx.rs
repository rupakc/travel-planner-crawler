//! Process context shared by every role: configuration now; pools, stores, clients and
//! metrics are added here as roles are wired.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::OnceCell;
use vc_core::{Clock, SystemClock};
use vc_obs::{Metrics, ProgressTracker};

use crate::wiring;

/// Shared process context (cheap to clone).
#[derive(Clone)]
pub struct Ctx {
    /// Loaded configuration.
    pub cfg: Arc<vc_config::Config>,
    /// The clock (system time in production).
    pub clock: Arc<dyn Clock>,
    /// The instance name ([`wiring::instance_name`]).
    pub instance: String,
    /// The process's metrics (served on `obs.metrics_addr`).
    pub metrics: Arc<Metrics>,
    /// Progress-based health of the running role (served as `/healthz`).
    pub tracker: Arc<ProgressTracker>,
    db: Arc<OnceCell<PgPool>>,
}

impl Ctx {
    /// Builds the context from configuration.
    pub fn new(cfg: vc_config::Config) -> Self {
        Self::with_clock(cfg, Arc::new(SystemClock))
    }

    /// Builds the context with a given clock (tests).
    pub fn with_clock(cfg: vc_config::Config, clock: Arc<dyn Clock>) -> Self {
        let metrics = wiring::obs::metrics(&cfg, clock.clone());
        let tracker = wiring::obs::tracker(&cfg, clock.clone());
        let instance = wiring::instance_name(&cfg);
        Self {
            cfg: Arc::new(cfg),
            clock,
            instance,
            metrics,
            tracker,
            db: Arc::new(OnceCell::new()),
        }
    }

    /// Uses an existing pool (tests) instead of connecting from `[database]`.
    pub fn with_pool(self, pool: PgPool) -> Self {
        let _ = self.db.set(pool);
        self
    }

    /// The shared Postgres pool, connected on first use ([`wiring::db::connect`]).
    pub async fn pool(&self) -> vc_core::Result<PgPool> {
        self.db
            .get_or_try_init(|| async {
                let secrets = wiring::sources::secrets(&self.cfg);
                wiring::db::connect(&self.cfg.database, Some(secrets.as_ref())).await
            })
            .await
            .cloned()
    }
}
