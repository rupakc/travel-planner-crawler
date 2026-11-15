//! The heartbeat writer: every `obs.heartbeat_s` it upserts `heartbeats(role, instance)` with
//! the progress snapshot and marks the role's `crawler_heartbeat` series fresh. The metric is
//! refreshed only after the database write succeeds, so a role that lost its database stops
//! beating and the crawler-down alert fires.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use vc_core::{Error, Result};

use crate::health::ProgressTracker;
use crate::metrics::Metrics;

/// Writes heartbeats for one role instance.
#[derive(Debug, Clone)]
pub struct HeartbeatWriter {
    pool: PgPool,
    role: String,
    instance: String,
    tracker: Arc<ProgressTracker>,
    metrics: Arc<Metrics>,
}

impl HeartbeatWriter {
    /// A writer for `role` / `instance` (e.g. the container hostname).
    pub fn new(
        pool: PgPool,
        role: &str,
        instance: &str,
        tracker: Arc<ProgressTracker>,
        metrics: Arc<Metrics>,
    ) -> Self {
        HeartbeatWriter {
            pool,
            role: role.to_string(),
            instance: instance.to_string(),
            tracker,
            metrics,
        }
    }

    /// One heartbeat.
    pub async fn beat_once(&self) -> Result<()> {
        sqlx::query(
            "INSERT INTO heartbeats (role, instance, last_beat, progress)
             VALUES ($1, $2, now(), $3)
             ON CONFLICT (role, instance)
             DO UPDATE SET last_beat = now(), progress = EXCLUDED.progress",
        )
        .bind(&self.role)
        .bind(&self.instance)
        .bind(self.tracker.snapshot())
        .execute(&self.pool)
        .await
        .map_err(|e| Error::Db(e.to_string()))?;
        self.metrics.heartbeat(&self.role);
        Ok(())
    }

    /// Beats every `interval` forever (abort the task to stop). Failures are logged by class.
    pub async fn run(self, interval: Duration) {
        let mut tick = tokio::time::interval(interval.max(Duration::from_secs(1)));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Err(e) = self.beat_once().await {
                tracing::warn!(
                    role = %self.role,
                    error_class = ?e.class(),
                    reason = "heartbeat write failed",
                    "heartbeat failed"
                );
            }
        }
    }
}
