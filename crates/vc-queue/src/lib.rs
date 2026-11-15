//! # vc-queue (Task 13, stub)
//!
//! `PgWorkQueue`: `SKIP LOCKED` leases, ack/nack with back-off, dedupe, dead-letter after 8.

use std::time::Duration;

use async_trait::async_trait;
use sqlx::PgPool;
use vc_config::QueueConfig;
use vc_core::queue::{WorkItem, WorkKind, WorkQueue};
use vc_core::{Result, WorkId};

/// The Postgres work queue.
#[derive(Debug, Clone)]
pub struct PgWorkQueue {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: QueueConfig,
}

#[async_trait]
impl WorkQueue for PgWorkQueue {
    async fn enqueue(
        &self,
        _kind: WorkKind,
        _payload: serde_json::Value,
        _dedupe_key: Option<&str>,
    ) -> Result<Option<WorkId>> {
        todo!("Task 13")
    }
    async fn take(&self, _kind: WorkKind, _n: usize, _lease: Duration) -> Result<Vec<WorkItem>> {
        todo!("Task 13")
    }
    async fn ack(&self, _id: WorkId) -> Result<()> {
        todo!("Task 13")
    }
    async fn nack(&self, _id: WorkId, _retry_in: Duration) -> Result<()> {
        todo!("Task 13")
    }
}
