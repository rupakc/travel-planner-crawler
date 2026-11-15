//! The work queue (at-least-once). Implemented by `vc-queue` (`PgWorkQueue`, `SKIP LOCKED`).
//! `vc_enqueue(kind, payload, dedupe_key)` is the SQL form used inside other transactions.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::WorkId;

/// Attempts before an item is dead-lettered.
pub const MAX_ATTEMPTS: u32 = 8;

/// Kinds of work (`work_items.kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// A committed capture to process (`{capture_id, captured_at}`), enqueued by
    /// `vc_commit_segment`.
    CaptureReady,
    /// An LLM extraction.
    Extract,
    /// An embedding.
    Embed,
    /// Link triage.
    Triage,
    /// Quality audit.
    Audit,
    /// A purge step.
    Purge,
}

impl WorkKind {
    /// The stable snake_case name stored in Postgres.
    pub fn as_str(&self) -> &'static str {
        match self {
            WorkKind::CaptureReady => "capture_ready",
            WorkKind::Extract => "extract",
            WorkKind::Embed => "embed",
            WorkKind::Triage => "triage",
            WorkKind::Audit => "audit",
            WorkKind::Purge => "purge",
        }
    }
}

/// A leased work item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    /// Id.
    pub id: WorkId,
    /// Kind.
    pub kind: WorkKind,
    /// Payload (ids only; never page content).
    pub payload: serde_json::Value,
    /// Attempts so far, including this one.
    pub attempts: u32,
    /// Dedupe key (unique among pending items of a kind).
    pub dedupe_key: Option<String>,
    /// When it was enqueued.
    pub enqueued_at: DateTime<Utc>,
}

/// The work queue.
#[async_trait]
pub trait WorkQueue: Send + Sync {
    /// Enqueues an item; `Ok(None)` when an identical `dedupe_key` is already pending.
    async fn enqueue(
        &self,
        kind: WorkKind,
        payload: serde_json::Value,
        dedupe_key: Option<&str>,
    ) -> Result<Option<WorkId>>;

    /// Leases up to `n` ready items of `kind` for `lease` (expired leases are redelivered).
    async fn take(&self, kind: WorkKind, n: usize, lease: Duration) -> Result<Vec<WorkItem>>;

    /// Marks an item done.
    async fn ack(&self, id: WorkId) -> Result<()>;

    /// Returns an item for a retry after `retry_in`; dead-letters it after [`MAX_ATTEMPTS`].
    async fn nack(&self, id: WorkId, retry_in: Duration) -> Result<()>;
}
