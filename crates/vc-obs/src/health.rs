//! Progress-based health (spec §6, §17.5): a role is unhealthy when it has made no progress for
//! `progress_window_s` **while work is due**. An idle role is healthy however long it idles,
//! and the window starts when work becomes due (not at the last completion), so a role that
//! idled for hours isn't failed the moment work arrives.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::json;
use vc_core::Clock;

/// The health of a role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// No work is due.
    Idle,
    /// Work is due and progress is recent (or the window has not elapsed yet).
    Progressing,
    /// Work is due and nothing completed for longer than the window.
    Stalled {
        /// Seconds since the last progress (or since work became due, if later).
        since_s: u64,
    },
}

impl Health {
    /// `true` unless [`Health::Stalled`].
    pub fn is_ok(&self) -> bool {
        !matches!(self, Health::Stalled { .. })
    }

    /// `idle`, `progressing` or `stalled`.
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Idle => "idle",
            Health::Progressing => "progressing",
            Health::Stalled { .. } => "stalled",
        }
    }
}

#[derive(Debug)]
struct State {
    started_at: DateTime<Utc>,
    last_progress: Option<DateTime<Utc>>,
    due_since: Option<DateTime<Utc>>,
    completed: u64,
}

/// Tracks completions and due work for one role.
#[derive(Debug)]
pub struct ProgressTracker {
    clock: Arc<dyn Clock>,
    window: chrono::Duration,
    state: Mutex<State>,
}

impl ProgressTracker {
    /// A tracker failing after `window` without progress while work is due.
    pub fn new(clock: Arc<dyn Clock>, window: Duration) -> Self {
        let now = clock.now();
        ProgressTracker {
            clock,
            window: chrono::Duration::from_std(window).unwrap_or(chrono::Duration::MAX),
            state: Mutex::new(State {
                started_at: now,
                last_progress: None,
                due_since: None,
                completed: 0,
            }),
        }
    }

    /// Records `n` completed units of work (a commit, an ack, …).
    pub fn record_progress(&self, n: u64) {
        let now = self.clock.now();
        let mut s = self.state.lock().expect("progress lock");
        s.completed = s.completed.saturating_add(n);
        s.last_progress = Some(now);
    }

    /// Tells the tracker whether work is due for this role (e.g. due URLs, ready queue items).
    pub fn set_work_due(&self, due: bool) {
        let now = self.clock.now();
        let mut s = self.state.lock().expect("progress lock");
        match (due, s.due_since) {
            (true, None) => s.due_since = Some(now),
            (false, _) => s.due_since = None,
            (true, Some(_)) => {}
        }
    }

    /// The current health.
    pub fn health(&self) -> Health {
        let now = self.clock.now();
        let s = self.state.lock().expect("progress lock");
        let Some(due_since) = s.due_since else {
            return Health::Idle;
        };
        let reference = s.last_progress.unwrap_or(s.started_at).max(due_since);
        let elapsed = now - reference;
        if elapsed > self.window {
            Health::Stalled {
                since_s: u64::try_from(elapsed.num_seconds()).unwrap_or(0),
            }
        } else {
            Health::Progressing
        }
    }

    /// A small JSON summary (ids and counters only), written to `heartbeats.progress`.
    pub fn snapshot(&self) -> serde_json::Value {
        let health = self.health();
        let s = self.state.lock().expect("progress lock");
        json!({
            "completed": s.completed,
            "last_progress_at": s.last_progress.map(|t| t.to_rfc3339()),
            "work_due": s.due_since.is_some(),
            "health": health.as_str(),
            "healthy": health.is_ok(),
        })
    }
}
