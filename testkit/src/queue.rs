//! In-memory `WorkQueue` with leases, dedupe, back-off and dead-lettering.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use vc_core::queue::{MAX_ATTEMPTS, WorkItem, WorkKind, WorkQueue};
use vc_core::{Clock, Error, Result, WorkId};

/// State of an item (mirror of `work_items.state`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemState {
    /// Ready.
    Ready,
    /// Leased.
    Leased,
    /// Done.
    Done,
    /// Dead-lettered.
    Dead,
}

#[derive(Debug, Clone)]
struct Row {
    item: WorkItem,
    state: ItemState,
    available_at: DateTime<Utc>,
    leased_until: Option<DateTime<Utc>>,
}

/// An in-memory work queue on a clock.
#[derive(Debug)]
pub struct MemQueue {
    clock: Arc<dyn Clock>,
    rows: Mutex<Vec<Row>>,
}

impl MemQueue {
    /// An empty queue.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        MemQueue {
            clock,
            rows: Mutex::new(Vec::new()),
        }
    }

    /// The state of an item.
    pub fn state(&self, id: WorkId) -> Option<ItemState> {
        self.rows
            .lock()
            .expect("queue lock")
            .iter()
            .find(|r| r.item.id == id)
            .map(|r| r.state)
    }

    /// Number of items in a state.
    pub fn count(&self, state: ItemState) -> usize {
        self.rows
            .lock()
            .expect("queue lock")
            .iter()
            .filter(|r| r.state == state)
            .count()
    }
}

#[async_trait]
impl WorkQueue for MemQueue {
    async fn enqueue(
        &self,
        kind: WorkKind,
        payload: serde_json::Value,
        dedupe_key: Option<&str>,
    ) -> Result<Option<WorkId>> {
        let now = self.clock.now();
        let mut rows = self.rows.lock().expect("queue lock");
        if let Some(d) = dedupe_key {
            let pending = rows.iter().any(|r| {
                r.item.kind == kind
                    && r.item.dedupe_key.as_deref() == Some(d)
                    && matches!(r.state, ItemState::Ready | ItemState::Leased)
            });
            if pending {
                return Ok(None);
            }
        }
        let id = WorkId(i64::try_from(rows.len()).unwrap_or(i64::MAX) + 1);
        rows.push(Row {
            item: WorkItem {
                id,
                kind,
                payload,
                attempts: 0,
                dedupe_key: dedupe_key.map(str::to_string),
                enqueued_at: now,
            },
            state: ItemState::Ready,
            available_at: now,
            leased_until: None,
        });
        Ok(Some(id))
    }

    async fn take(&self, kind: WorkKind, n: usize, lease: Duration) -> Result<Vec<WorkItem>> {
        let now = self.clock.now();
        let lease = chrono::Duration::from_std(lease).map_err(|e| Error::Invalid(e.to_string()))?;
        let mut rows = self.rows.lock().expect("queue lock");
        let mut out = Vec::new();
        for r in rows.iter_mut().filter(|r| r.item.kind == kind) {
            if out.len() >= n {
                break;
            }
            let ready = (r.state == ItemState::Ready && r.available_at <= now)
                || (r.state == ItemState::Leased && r.leased_until.is_some_and(|t| t <= now));
            if ready {
                r.state = ItemState::Leased;
                r.leased_until = Some(now + lease);
                r.item.attempts += 1;
                out.push(r.item.clone());
            }
        }
        Ok(out)
    }

    async fn ack(&self, id: WorkId) -> Result<()> {
        let mut rows = self.rows.lock().expect("queue lock");
        let r = rows
            .iter_mut()
            .find(|r| r.item.id == id)
            .ok_or(Error::NotFound(id.to_string()))?;
        r.state = ItemState::Done;
        Ok(())
    }

    async fn nack(&self, id: WorkId, retry_in: Duration) -> Result<()> {
        let now = self.clock.now();
        let mut rows = self.rows.lock().expect("queue lock");
        let r = rows
            .iter_mut()
            .find(|r| r.item.id == id)
            .ok_or(Error::NotFound(id.to_string()))?;
        if r.item.attempts >= MAX_ATTEMPTS {
            r.state = ItemState::Dead;
        } else {
            r.state = ItemState::Ready;
            r.leased_until = None;
            r.available_at = now
                + chrono::Duration::from_std(retry_in)
                    .map_err(|e| Error::Invalid(e.to_string()))?;
        }
        Ok(())
    }
}
