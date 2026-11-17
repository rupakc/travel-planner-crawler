//! Process context shared by every role: configuration now; pools, stores, clients and
//! metrics are added here as roles are wired.

use std::sync::Arc;

use sqlx::PgPool;
use tokio::sync::{OnceCell, watch};
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
    /// The process-wide shutdown flag set by `vcrawl run` (`None`: a role listens for
    /// SIGTERM / Ctrl-C itself, [`Ctx::stop`]).
    shutdown: Option<watch::Receiver<bool>>,
    /// `true` under `vcrawl run`: the supervisor serves `/metrics` + `/healthz` for every role
    /// of the process, so a role starts only its heartbeat ([`wiring::obs::start`]).
    pub(crate) shared_obs: bool,
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
            shutdown: None,
            shared_obs: false,
        }
    }

    /// The shutdown flag for a role: the process-wide one under `vcrawl run`, else a fresh
    /// channel that flips on SIGTERM / Ctrl-C.
    pub fn stop(&self) -> watch::Receiver<bool> {
        match &self.shutdown {
            Some(rx) => rx.clone(),
            None => crate::data::shutdown::channel(),
        }
    }

    /// Uses `rx` as the shutdown flag instead of the process signals (`vcrawl run`, tests).
    pub fn with_shutdown(mut self, rx: watch::Receiver<bool>) -> Self {
        self.shutdown = Some(rx);
        self
    }

    /// Uses `tracker` for this role's progress health (`vcrawl run` gives each role its own).
    pub fn with_tracker(mut self, tracker: Arc<ProgressTracker>) -> Self {
        self.tracker = tracker;
        self
    }

    /// The pool if one is already set or connected (no connection is made).
    pub fn pool_if_set(&self) -> Option<PgPool> {
        self.db.get().cloned()
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
