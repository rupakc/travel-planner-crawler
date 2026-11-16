//! Peers for the high-stakes agreement and conflict checks (spec §5.7 step 4, §17.8).
//!
//! * [`index_peers`]: current, active records of the given kinds from `official` and
//!   `reference` sources (the only classes that count towards agreement).
//! * [`PendingPeers`]: `reference` records that `vc-extract` held back (`Gate::NeedsAgreement`)
//!   because no second independent source agreed yet. They are fed back as peers when later
//!   captures of the same kind are extracted; once another source agrees, the held record is
//!   promoted (upserted with its own provenance). Held records are never served.
//!
//! The pending set lives in process memory, bounded and expiring: a restart forgets it, and the
//! next recrawl of either source rebuilds it. (A durable table needs a migration block.)

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sqlx::PgPool;
use vc_core::licence::SourceClass;
use vc_core::records::{ExtractedRecord, Provenance};
use vc_core::{Error, KindId, RecordKey, Result, SourceId};
use vc_extract::PeerRecord;

/// `(source_id, source_class, kind, place_ids, data)`.
type PeerRow = (String, Option<String>, String, Vec<i64>, serde_json::Value);

/// Max peers read from the index per capture.
pub const MAX_INDEX_PEERS: i64 = 500;

/// Current records of `kinds` from official/reference sources other than `exclude`.
pub async fn index_peers(
    pool: &PgPool,
    kinds: &[KindId],
    exclude: &SourceId,
) -> Result<Vec<PeerRecord>> {
    if kinds.is_empty() {
        return Ok(Vec::new());
    }
    let kinds: Vec<String> = kinds.iter().map(|k| k.0.to_string()).collect();
    let rows: Vec<PeerRow> = sqlx::query_as(
        "SELECT r.source_id, r.source_class, r.kind, r.place_ids, v.data
             FROM records r
             JOIN record_versions v ON v.record_id = r.record_id AND v.version = r.current_version
             WHERE r.kind = ANY($1) AND r.state = 'active' AND r.source_id <> $2
               AND r.source_class IN ('official', 'reference')
             ORDER BY r.updated_at DESC
             LIMIT $3",
    )
    .bind(&kinds)
    .bind(&exclude.0)
    .bind(MAX_INDEX_PEERS)
    .fetch_all(pool)
    .await
    .map_err(|e| Error::Db(e.to_string()))?;
    Ok(rows
        .into_iter()
        .filter_map(|(sid, class, kind, place_ids, data)| {
            let class: SourceClass =
                serde_json::from_value(serde_json::Value::String(class?)).ok()?;
            Some(PeerRecord {
                source_id: SourceId(sid),
                source_class: class,
                kind: KindId::new(&kind),
                place_ids,
                data,
            })
        })
        .collect())
}

/// One held record.
#[derive(Debug, Clone)]
pub struct Held {
    /// The record (as extracted).
    pub record: ExtractedRecord,
    /// Its capture's provenance.
    pub provenance: Provenance,
    held_at: Instant,
}

impl Held {
    /// The record as a peer.
    pub fn as_peer(&self) -> PeerRecord {
        PeerRecord {
            source_id: self.provenance.source_id.clone(),
            source_class: self.provenance.source_class,
            kind: self.record.kind.clone(),
            place_ids: self.record.place_ids.clone(),
            data: self.record.data.clone(),
        }
    }
}

type HeldKey = (KindId, SourceId, RecordKey);

/// Bounded, expiring in-memory set of held records, keyed by (kind, source, record key): a
/// newer capture of the same record replaces the older one.
#[derive(Debug)]
pub struct PendingPeers {
    ttl: Duration,
    max: usize,
    inner: Mutex<HashMap<HeldKey, Held>>,
}

impl Default for PendingPeers {
    fn default() -> Self {
        PendingPeers::new(Duration::from_secs(7 * 24 * 3600), 10_000)
    }
}

impl PendingPeers {
    /// A set keeping records for `ttl`, at most `max` of them.
    pub fn new(ttl: Duration, max: usize) -> Self {
        PendingPeers {
            ttl,
            max: max.max(1),
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, HashMap<HeldKey, Held>>> {
        self.inner
            .lock()
            .map_err(|_| Error::Invalid("pending peers poisoned".into()))
    }

    /// Holds a record (replacing the same record from an older capture).
    pub fn hold(&self, record: ExtractedRecord, provenance: Provenance) -> Result<()> {
        let mut g = self.lock()?;
        let ttl = self.ttl;
        g.retain(|_, h| h.held_at.elapsed() < ttl);
        if g.len() >= self.max {
            // Drop the oldest entry.
            if let Some(k) = g
                .iter()
                .min_by_key(|(_, h)| h.held_at)
                .map(|(k, _)| k.clone())
            {
                g.remove(&k);
            }
        }
        let key = (
            record.kind.clone(),
            provenance.source_id.clone(),
            record.record_key.clone(),
        );
        g.insert(
            key,
            Held {
                record,
                provenance,
                held_at: Instant::now(),
            },
        );
        Ok(())
    }

    /// Held records of `kinds`.
    pub fn of_kinds(&self, kinds: &[KindId]) -> Result<Vec<Held>> {
        let g = self.lock()?;
        Ok(g.values()
            .filter(|h| h.held_at.elapsed() < self.ttl && kinds.contains(&h.record.kind))
            .cloned()
            .collect())
    }

    /// Forgets a held record (promoted, or superseded by an accepted one).
    pub fn release(&self, kind: &KindId, source: &SourceId, key: &RecordKey) -> Result<()> {
        self.lock()?
            .remove(&(kind.clone(), source.clone(), key.clone()));
        Ok(())
    }

    /// Number of held records.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
