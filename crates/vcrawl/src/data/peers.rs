//! Peers for the high-stakes agreement and conflict checks (spec §5.7 step 4, §17.8).
//!
//! * [`index_peers`]: current, active records of the given kinds from `official` and
//!   `reference` sources (the only classes that count towards agreement).
//! * [`PendingPeers`]: `reference` records that `vc-extract` held back (`Gate::NeedsAgreement`)
//!   because no second independent source agreed yet. They are fed back as peers when later
//!   captures of the same kind are extracted; once another source agrees, the held record is
//!   promoted (upserted with its own provenance). Held records are never served.
//!
//! The held set is the `held_records` table (migration 1901): shared by every processor
//! instance, kept across restarts, bounded (oldest dropped first) and expiring (7 days by
//! default; expired rows are ignored and deleted).

use std::time::Duration;

use chrono::{DateTime, Utc};
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
    /// When it was (last) held.
    pub held_at: DateTime<Utc>,
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

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// The durable, bounded, expiring set of held records (`held_records`), keyed by
/// (kind, source, record key): a newer capture of the same record replaces the older one, an
/// older capture never replaces a newer one.
#[derive(Debug, Clone)]
pub struct PendingPeers {
    pool: PgPool,
    ttl: Duration,
    max: i64,
}

/// Default expiry of a held record.
pub const HELD_TTL: Duration = Duration::from_secs(7 * 24 * 3600);
/// Default bound on held records.
pub const HELD_MAX: usize = 10_000;

/// `(record, provenance, held_at)`.
type HeldRow = (serde_json::Value, serde_json::Value, DateTime<Utc>);

impl PendingPeers {
    /// The held set over `pool` with the default expiry and bound.
    pub fn new(pool: PgPool) -> Self {
        PendingPeers::with_limits(pool, HELD_TTL, HELD_MAX)
    }

    /// The held set keeping records for `ttl`, at most `max` of them.
    pub fn with_limits(pool: PgPool, ttl: Duration, max: usize) -> Self {
        PendingPeers {
            pool,
            ttl,
            max: i64::try_from(max.max(1)).unwrap_or(i64::MAX),
        }
    }

    fn ttl_ms(&self) -> i64 {
        i64::try_from(self.ttl.as_millis()).unwrap_or(i64::MAX)
    }

    /// Holds a record (replacing the same record from an older capture), then drops expired
    /// records and the oldest ones beyond the bound.
    pub async fn hold(&self, record: ExtractedRecord, provenance: Provenance) -> Result<()> {
        let rec = serde_json::to_value(&record)?;
        let prov = serde_json::to_value(&provenance)?;
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query(
            "INSERT INTO held_records
                (kind, source_id, record_key, record, provenance, captured_at, held_at)
             VALUES ($1, $2, $3, $4, $5, $6, now())
             ON CONFLICT (kind, source_id, record_key) DO UPDATE SET
                record = EXCLUDED.record, provenance = EXCLUDED.provenance,
                captured_at = EXCLUDED.captured_at, held_at = now()
             WHERE held_records.captured_at <= EXCLUDED.captured_at",
        )
        .bind(&*record.kind.0)
        .bind(&provenance.source_id.0)
        .bind(&record.record_key.0)
        .bind(rec)
        .bind(prov)
        .bind(provenance.capture.captured_at)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "DELETE FROM held_records
             WHERE held_at < now() - make_interval(secs => $1::double precision / 1000)",
        )
        .bind(self.ttl_ms())
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        sqlx::query(
            "DELETE FROM held_records WHERE (kind, source_id, record_key) IN (
                SELECT kind, source_id, record_key FROM held_records
                ORDER BY held_at DESC, kind, source_id, record_key OFFSET $1)",
        )
        .bind(self.max)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
        tx.commit().await.map_err(db)
    }

    /// Unexpired held records of `kinds`.
    pub async fn of_kinds(&self, kinds: &[KindId]) -> Result<Vec<Held>> {
        if kinds.is_empty() {
            return Ok(Vec::new());
        }
        let kinds: Vec<String> = kinds.iter().map(|k| k.0.to_string()).collect();
        let rows: Vec<HeldRow> = sqlx::query_as(
            "SELECT record, provenance, held_at FROM held_records
             WHERE kind = ANY($1)
               AND held_at >= now() - make_interval(secs => $2::double precision / 1000)
             ORDER BY held_at, source_id, record_key
             LIMIT $3",
        )
        .bind(&kinds)
        .bind(self.ttl_ms())
        .bind(self.max)
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        rows.into_iter()
            .map(|(r, p, held_at)| {
                Ok(Held {
                    record: serde_json::from_value(r)?,
                    provenance: serde_json::from_value(p)?,
                    held_at,
                })
            })
            .collect()
    }

    /// Forgets a held record (promoted, or superseded by an accepted one).
    pub async fn release(&self, kind: &KindId, source: &SourceId, key: &RecordKey) -> Result<()> {
        sqlx::query(
            "DELETE FROM held_records WHERE kind = $1 AND source_id = $2 AND record_key = $3",
        )
        .bind(&*kind.0)
        .bind(&source.0)
        .bind(&key.0)
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(())
    }

    /// Number of unexpired held records.
    pub async fn len(&self) -> Result<usize> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM held_records
             WHERE held_at >= now() - make_interval(secs => $1::double precision / 1000)",
        )
        .bind(self.ttl_ms())
        .fetch_one(&self.pool)
        .await
        .map_err(db)?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Whether nothing (unexpired) is held.
    pub async fn is_empty(&self) -> Result<bool> {
        Ok(self.len().await? == 0)
    }
}
