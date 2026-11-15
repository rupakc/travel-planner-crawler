//! `[queue]`: work queue (owner: vc-queue).

use serde::{Deserialize, Serialize};

/// Work-queue settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    /// Default lease, seconds.
    pub lease_s: u64,
    /// Attempts before dead-letter (8).
    pub max_attempts: u32,
    /// First retry back-off, seconds.
    pub backoff_base_s: u64,
    /// Max retry back-off, seconds.
    pub backoff_max_s: u64,
    /// Delete `done` items after this many days.
    pub retain_done_days: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        QueueConfig {
            lease_s: 300,
            max_attempts: 8,
            backoff_base_s: 30,
            backoff_max_s: 6 * 3600,
            retain_done_days: 7,
        }
    }
}
