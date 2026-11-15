//! # vc-queue
//!
//! [`PgWorkQueue`]: the Postgres implementation of [`vc_core::queue::WorkQueue`] on the
//! `work_items` table (migration 0005).
//!
//! - **At-least-once.** [`WorkQueue::take`] leases ready items with `FOR UPDATE SKIP LOCKED`
//!   (concurrent takers never share an item) and counts the attempt. An item whose lease
//!   expires without an ack is delivered again.
//! - **Back-off.** [`WorkQueue::nack`] makes an item ready again after
//!   `max(retry_in, backoff(attempts))`, where `backoff` doubles from `queue.backoff_base_s` up
//!   to `queue.backoff_max_s`. A caller-supplied `retry_in` (e.g. `Retry-After`) is honoured.
//! - **Dead letters.** An item is dead-lettered (`state = 'dead'`) when it is nacked after
//!   `queue.max_attempts` (8, [`vc_core::queue::MAX_ATTEMPTS`]) attempts, **or** when its lease
//!   expires after the last attempt (a poison item that crashes its worker never gets a nack).
//! - **Dedupe.** Enqueue goes through the SQL function `vc_enqueue`, the same one other
//!   transactions use (e.g. `vc_commit_segment`): a `dedupe_key` is unique among the *pending*
//!   (ready or leased) items of a kind, so a key is free again once its item is done or dead.
//!
//! Acks and nacks are not fenced (the trait carries only the id): a worker that outlived its
//! lease may ack an item another worker re-leased. Handlers are idempotent (spec §5), so a
//! double processing is harmless. Payloads hold ids only, never content.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;
use vc_config::QueueConfig;
use vc_core::queue::{WorkItem, WorkKind, WorkQueue};
use vc_core::{Error, Result, WorkId};

/// The Postgres work queue.
#[derive(Debug, Clone)]
pub struct PgWorkQueue {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: QueueConfig,
}

/// Count of items per kind and state (for metrics and `vcrawl admin status`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStat {
    /// Kind.
    pub kind: WorkKind,
    /// `ready`, `leased`, `done` or `dead`.
    pub state: String,
    /// Number of items.
    pub count: i64,
}

const ALL_KINDS: [WorkKind; 6] = [
    WorkKind::CaptureReady,
    WorkKind::Extract,
    WorkKind::Embed,
    WorkKind::Triage,
    WorkKind::Audit,
    WorkKind::Purge,
];

/// Parses the stored snake_case kind.
pub fn parse_kind(s: &str) -> Option<WorkKind> {
    ALL_KINDS.into_iter().find(|k| k.as_str() == s)
}

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

type Row = (
    i64,
    String,
    serde_json::Value,
    i32,
    Option<String>,
    DateTime<Utc>,
);

fn to_item(r: Row) -> Result<WorkItem> {
    let (id, kind, payload, attempts, dedupe_key, enqueued_at) = r;
    Ok(WorkItem {
        id: WorkId(id),
        kind: parse_kind(&kind).ok_or_else(|| Error::Invalid(format!("unknown kind {kind}")))?,
        payload,
        attempts: u32::try_from(attempts).unwrap_or(0),
        dedupe_key,
        enqueued_at,
    })
}

impl PgWorkQueue {
    /// A queue on `pool`.
    pub fn new(pool: PgPool, config: QueueConfig) -> Self {
        PgWorkQueue { pool, config }
    }

    /// The back-off after the `attempts`-th failed attempt: `base × 2^(attempts − 1)`, capped
    /// at `backoff_max_s`.
    pub fn backoff(config: &QueueConfig, attempts: u32) -> Duration {
        let exp = attempts.saturating_sub(1).min(32);
        let s = config
            .backoff_base_s
            .saturating_mul(1u64 << exp)
            .min(config.backoff_max_s);
        Duration::from_secs(s)
    }

    fn max_attempts(&self) -> i32 {
        i32::try_from(self.config.max_attempts.max(1)).unwrap_or(i32::MAX)
    }

    /// Dead-letters items whose lease expired after their last allowed attempt.
    async fn reap_exhausted(&self, kind: WorkKind) -> Result<u64> {
        let r = sqlx::query(
            "UPDATE work_items SET state = 'dead', leased_until = NULL, updated_at = now(),
                    last_error = coalesce(last_error, 'lease expired on the last attempt')
             WHERE work_id IN (
                 SELECT work_id FROM work_items
                 WHERE kind = $1 AND state = 'leased' AND leased_until <= now()
                   AND attempts >= $2
                 FOR UPDATE SKIP LOCKED)",
        )
        .bind(kind.as_str())
        .bind(self.max_attempts())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.rows_affected())
    }

    /// [`WorkQueue::nack`] that also records a short error class (never content; cut to 200
    /// characters).
    pub async fn nack_with_error(
        &self,
        id: WorkId,
        retry_in: Duration,
        error: Option<&str>,
    ) -> Result<()> {
        let error = error.map(|e| e.chars().take(200).collect::<String>());
        let r = sqlx::query(
            "UPDATE work_items SET
                 state = CASE WHEN attempts >= $2 THEN 'dead' ELSE 'ready' END,
                 available_at = now() + make_interval(secs =>
                     greatest($3::float8, least($4::float8 * power(2, greatest(attempts - 1, 0)),
                                               $5::float8))),
                 leased_until = NULL,
                 last_error = coalesce($6, last_error),
                 updated_at = now()
             WHERE work_id = $1 AND state = 'leased'",
        )
        .bind(id.0)
        .bind(self.max_attempts())
        .bind(secs(retry_in))
        .bind(self.config.backoff_base_s as f64)
        .bind(self.config.backoff_max_s as f64)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        if r.rows_affected() == 0 {
            // Not leased: a stale nack (already done, dead or not yet re-leased) is a no-op.
            self.ensure_exists(id).await?;
        }
        Ok(())
    }

    async fn ensure_exists(&self, id: WorkId) -> Result<()> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM work_items WHERE work_id = $1)")
                .bind(id.0)
                .fetch_one(&self.pool)
                .await
                .map_err(db)?;
        if exists {
            Ok(())
        } else {
            Err(Error::NotFound(format!("work item {id}")))
        }
    }

    /// The most recently dead-lettered items of a kind.
    pub async fn dead_letters(&self, kind: WorkKind, limit: usize) -> Result<Vec<WorkItem>> {
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT work_id, kind, payload, attempts, dedupe_key, created_at FROM work_items
             WHERE kind = $1 AND state = 'dead' ORDER BY updated_at DESC, work_id DESC LIMIT $2",
        )
        .bind(kind.as_str())
        .bind(i64::try_from(limit).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.into_iter().map(to_item).collect()
    }

    /// Puts a dead item back in the queue with its attempts reset (operator action). `false`
    /// when the item is not dead, or when a pending item already holds its dedupe key.
    pub async fn requeue_dead(&self, id: WorkId) -> Result<bool> {
        let r = sqlx::query(
            "UPDATE work_items w SET state = 'ready', attempts = 0, available_at = now(),
                    leased_until = NULL, updated_at = now()
             WHERE w.work_id = $1 AND w.state = 'dead'
               AND (w.dedupe_key IS NULL OR NOT EXISTS (
                   SELECT 1 FROM work_items o
                   WHERE o.kind = w.kind AND o.dedupe_key = w.dedupe_key
                     AND o.state IN ('ready', 'leased')))",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.rows_affected() == 1)
    }

    /// Item counts per kind and state.
    pub async fn stats(&self) -> Result<Vec<QueueStat>> {
        let rows: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT kind, state, count(*) FROM work_items GROUP BY kind, state ORDER BY 1, 2",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        Ok(rows
            .into_iter()
            .filter_map(|(k, state, count)| {
                parse_kind(&k).map(|kind| QueueStat { kind, state, count })
            })
            .collect())
    }

    /// Deletes `done` items older than `queue.retain_done_days`. Returns the number deleted.
    pub async fn prune_done(&self) -> Result<u64> {
        let r = sqlx::query(
            "DELETE FROM work_items
             WHERE state = 'done' AND updated_at < now() - make_interval(days => $1)",
        )
        .bind(i32::try_from(self.config.retain_done_days).unwrap_or(i32::MAX))
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(r.rows_affected())
    }
}

#[async_trait]
impl WorkQueue for PgWorkQueue {
    async fn enqueue(
        &self,
        kind: WorkKind,
        payload: serde_json::Value,
        dedupe_key: Option<&str>,
    ) -> Result<Option<WorkId>> {
        let id: Option<i64> = sqlx::query_scalar("SELECT vc_enqueue($1, $2, $3)")
            .bind(kind.as_str())
            .bind(payload)
            .bind(dedupe_key)
            .fetch_one(&self.pool)
            .await
            .map_err(db)?;
        Ok(id.map(WorkId))
    }

    async fn take(&self, kind: WorkKind, n: usize, lease: Duration) -> Result<Vec<WorkItem>> {
        if n == 0 {
            return Ok(Vec::new());
        }
        self.reap_exhausted(kind).await?;
        let rows: Vec<Row> = sqlx::query_as(
            "WITH picked AS (
                 SELECT work_id FROM work_items
                 WHERE kind = $1
                   AND ((state = 'ready' AND available_at <= now())
                        OR (state = 'leased' AND leased_until <= now() AND attempts < $4))
                 ORDER BY available_at, work_id
                 LIMIT $2
                 FOR UPDATE SKIP LOCKED)
             UPDATE work_items w SET state = 'leased', attempts = w.attempts + 1,
                    leased_until = now() + make_interval(secs => $3), updated_at = now()
             FROM picked WHERE w.work_id = picked.work_id
             RETURNING w.work_id, w.kind, w.payload, w.attempts, w.dedupe_key, w.created_at",
        )
        .bind(kind.as_str())
        .bind(i64::try_from(n).unwrap_or(i64::MAX))
        .bind(secs(lease))
        .bind(self.max_attempts())
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let mut items = rows
            .into_iter()
            .map(to_item)
            .collect::<Result<Vec<_>>>()?;
        // RETURNING has no defined order: hand items out FIFO.
        items.sort_by_key(|i| i.id);
        Ok(items)
    }

    async fn ack(&self, id: WorkId) -> Result<()> {
        let r = sqlx::query(
            "UPDATE work_items SET state = 'done', leased_until = NULL, updated_at = now()
             WHERE work_id = $1 AND state <> 'done'",
        )
        .bind(id.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        if r.rows_affected() == 0 {
            self.ensure_exists(id).await?;
        }
        Ok(())
    }

    async fn nack(&self, id: WorkId, retry_in: Duration) -> Result<()> {
        self.nack_with_error(id, retry_in, None).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip() {
        for k in ALL_KINDS {
            assert_eq!(parse_kind(k.as_str()), Some(k));
        }
        assert_eq!(parse_kind("nope"), None);
    }
}
