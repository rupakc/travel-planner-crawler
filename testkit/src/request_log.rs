//! The **frozen** request-log schema (Task 0A). The fake web (P1 testkit) appends one entry per
//! request it serves; the politeness oracle (`assert_polite`) reads them. One JSON object per
//! line when persisted (JSONL).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One request as seen by a (fake) server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestLogEntry {
    /// The politeness key the request belongs to (registrable domain or source group).
    pub key: String,
    /// The full requested URL.
    pub url: String,
    /// When the server started handling the request.
    pub started_at: DateTime<Utc>,
    /// When the server finished sending the response.
    pub ended_at: DateTime<Utc>,
    /// HTTP status sent.
    pub status: u16,
    /// The `User-Agent` received.
    pub ua: String,
}

/// A shared, append-only request log. Clones share the same entries.
#[derive(Debug, Clone, Default)]
pub struct RequestLog(Arc<Mutex<Vec<RequestLogEntry>>>);

impl RequestLog {
    /// An empty log.
    pub fn new() -> Self {
        RequestLog::default()
    }

    /// Appends an entry.
    pub fn record(&self, e: RequestLogEntry) {
        self.0.lock().expect("request log lock").push(e);
    }

    /// All entries, ordered by `started_at` (stable).
    pub fn entries(&self) -> Vec<RequestLogEntry> {
        let mut v = self.0.lock().expect("request log lock").clone();
        v.sort_by_key(|e| e.started_at);
        v
    }

    /// Entries grouped by key, each group ordered by `started_at`.
    pub fn by_key(&self) -> BTreeMap<String, Vec<RequestLogEntry>> {
        let mut m: BTreeMap<String, Vec<RequestLogEntry>> = BTreeMap::new();
        for e in self.entries() {
            m.entry(e.key.clone()).or_default().push(e);
        }
        m
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.0.lock().expect("request log lock").len()
    }

    /// Whether the log is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// JSONL serialisation (ordered by `started_at`).
    pub fn to_jsonl(&self) -> String {
        self.entries()
            .iter()
            .map(|e| serde_json::to_string(e).expect("entry serialises"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Parses JSONL (blank lines ignored).
    pub fn from_jsonl(s: &str) -> Result<RequestLog, serde_json::Error> {
        let log = RequestLog::new();
        for line in s.lines().filter(|l| !l.trim().is_empty()) {
            log.record(serde_json::from_str(line)?);
        }
        Ok(log)
    }
}
