//! Records, entities and the read model (spec §5.6, §5.11, §17.9). Implemented by `vc-index`
//! (`PgRecordSink`, the reader) and consumed by the processor (`RecordSink`) and `vc-api`
//! (`IndexReader`).

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::extract::GeoPoint;
use crate::ids::{EntityId, KindId, RecordId, SourceId, UrlKey};
use crate::licence::{Licence, LicenceClass};
use crate::records::{ExtractedRecord, ExtractorInfo, Provenance};
use crate::usepolicy::UsePolicy;

/// What an upsert did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpsertReport {
    /// New records.
    pub inserted: u32,
    /// New versions of existing records (`data_hash` changed).
    pub new_versions: u32,
    /// Records seen again unchanged (only `last_seen` touched).
    pub unchanged: u32,
    /// Observation points appended.
    pub observations: u32,
    /// `changes` rows written.
    pub changes: u32,
}

/// What to tombstone / purge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
pub enum PurgeSelector {
    /// One URL.
    Url(UrlKey),
    /// Every URL starting with this canonical prefix.
    UrlPrefix(String),
    /// Every URL on a host.
    Host(String),
    /// Every record attributed to an author (forum dumps, GDPR erasure).
    Author(String),
    /// Every record of a source.
    Source(SourceId),
    /// One record.
    Record(RecordId),
}

/// Why something is tombstoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TombstoneReason {
    /// Site or person opted out.
    OptOut,
    /// robots.txt newly disallows the path.
    Robots,
    /// Host added to the denylist.
    Denylist,
    /// Operator purge.
    Purge,
    /// 404/410 twice.
    Gone,
}

/// Writes extracted records. One transaction per call: records, versions (only when
/// `data_hash` changes), observations, entities + claims + redirects (licence-compatible merges
/// only) and `changes` rows. Idempotent on (capture, extraction run, record key).
#[async_trait]
pub trait RecordSink: Send + Sync {
    /// Upserts the records of one capture.
    async fn upsert(&self, recs: Vec<ExtractedRecord>, prov: &Provenance) -> Result<UpsertReport>;

    /// Tombstones matching records (removed from the API and index; raw purge is separate).
    /// Returns the number of records tombstoned.
    async fn tombstone(&self, sel: &PurgeSelector, reason: TombstoneReason) -> Result<u64>;
}

/// Provenance of one served item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProvenanceView {
    /// Source.
    pub source_id: SourceId,
    /// Page URL.
    pub url: String,
    /// Capture time.
    pub captured_at: DateTime<Utc>,
    /// Extractor.
    pub extractor: ExtractorInfo,
}

/// One served item (entity or record). Every item carries `schema`, `provenance`, `licence` and
/// `conflict` (spec §5.11).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// Entity id (ULID) or record id (decimal).
    pub id: String,
    /// Kind.
    pub kind: KindId,
    /// `kind@version`.
    pub schema: String,
    /// Title.
    pub title: Option<String>,
    /// ≤ 300-char excerpt (the only text served for `facts_only`).
    pub excerpt: Option<String>,
    /// Data (facts).
    pub data: serde_json::Value,
    /// GeoNames ids.
    pub place_ids: Vec<i64>,
    /// Licence.
    pub licence: Licence,
    /// Provenance (one per contributing record).
    pub provenance: Vec<ProvenanceView>,
    /// Cross-source conflict on a high-stakes field.
    pub conflict: bool,
    /// Effective use policy.
    pub use_policy: UsePolicy,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Ranking score (search only).
    pub score: Option<f32>,
}

/// A hybrid search (RRF of `ts_rank` and vector similarity, × quality × freshness).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SearchQuery {
    /// Free text.
    pub q: Option<String>,
    /// Exact place.
    pub place_id: Option<i64>,
    /// Else a radius around a point (km).
    pub near: Option<(GeoPoint, f64)>,
    /// Kinds (empty = all).
    pub kinds: Vec<KindId>,
    /// Licence classes the client may receive (empty = none).
    pub licence_classes: Vec<LicenceClass>,
    /// Only items updated since.
    pub updated_since: Option<DateTime<Utc>>,
    /// Page size (≤ 100).
    pub limit: u32,
    /// Opaque cursor from a previous page.
    pub cursor: Option<String>,
}

/// A page of results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Page<T> {
    /// Items.
    pub items: Vec<T>,
    /// Cursor for the next page.
    pub next_cursor: Option<String>,
}

/// One per-attribute claim of an entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClaimView {
    /// Attribute name.
    pub attribute: String,
    /// Value.
    pub value: serde_json::Value,
    /// Contributing record.
    pub record_id: RecordId,
    /// Record version.
    pub version: i32,
    /// Confidence in `[0, 1]`.
    pub confidence: f32,
}

/// An entity with its claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityView {
    /// The entity as an item.
    pub item: Item,
    /// Its claims.
    pub claims: Vec<ClaimView>,
}

/// The result of an entity lookup.
#[derive(Debug, Clone, PartialEq)]
pub enum EntityLookup {
    /// Found.
    Found(Box<EntityView>),
    /// Merged into another entity (`entity_redirects`; the API answers 301).
    Redirect(EntityId),
    /// Unknown or tombstoned.
    NotFound,
}

/// One version of a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordVersionView {
    /// Record.
    pub record_id: RecordId,
    /// Version (1-based).
    pub version: i32,
    /// `kind@version`.
    pub schema: String,
    /// Data.
    pub data: serde_json::Value,
    /// blake3 hex of the canonical JSON.
    pub data_hash: String,
    /// The capture it came from.
    pub captured_at: DateTime<Utc>,
    /// Extractor.
    pub extractor: ExtractorInfo,
    /// Licence class.
    pub licence_class: LicenceClass,
    /// Use policy.
    pub use_policy: UsePolicy,
    /// Valid from.
    pub valid_from: DateTime<Utc>,
    /// Superseded at (None = current).
    pub superseded_at: Option<DateTime<Utc>>,
}

/// A `/v1/changes` cursor `(xid, seq)`; only rows with `xid < pg_snapshot_xmin(...)` are served,
/// so no commit is skipped. Text form: `<xid>-<seq>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ChangeCursor {
    /// Transaction id (`xid8`).
    pub xid: u64,
    /// Sequence.
    pub seq: i64,
}

impl fmt::Display for ChangeCursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.xid, self.seq)
    }
}

impl FromStr for ChangeCursor {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let (x, q) = s
            .split_once('-')
            .ok_or_else(|| Error::Invalid("cursor".into()))?;
        Ok(ChangeCursor {
            xid: x.parse().map_err(|_| Error::Invalid("cursor xid".into()))?,
            seq: q.parse().map_err(|_| Error::Invalid("cursor seq".into()))?,
        })
    }
}

impl Serialize for ChangeCursor {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ChangeCursor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

/// `changes.op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    /// Created or updated.
    Upsert,
    /// Tombstoned.
    Tombstone,
    /// Entity merged (redirect).
    Redirect,
}

/// `changes.owner_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerType {
    /// A record.
    Record,
    /// An entity.
    Entity,
    /// An observation series.
    Observation,
    /// A source.
    Source,
}

/// One change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeView {
    /// Cursor position of this change.
    pub cursor: ChangeCursor,
    /// Owner type.
    pub owner_type: OwnerType,
    /// Owner id.
    pub owner_id: String,
    /// Operation.
    pub op: ChangeOp,
    /// Kind, when applicable.
    pub kind: Option<KindId>,
    /// When.
    pub at: DateTime<Utc>,
}

/// A page of changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangesPage {
    /// Changes, in cursor order.
    pub changes: Vec<ChangeView>,
    /// Cursor to pass next time (the last returned, or the input when empty).
    pub next: Option<ChangeCursor>,
}

/// One observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationView {
    /// Series.
    pub series_key: String,
    /// Time.
    pub observed_at: DateTime<Utc>,
    /// Value.
    pub value: serde_json::Value,
    /// Source.
    pub source_id: SourceId,
    /// Licence.
    pub licence: Licence,
}

/// The read model behind `/v1`. Tombstoned data is never returned.
#[async_trait]
pub trait IndexReader: Send + Sync {
    /// Hybrid search (`/v1/search`, `/v1/entities?q=`).
    async fn search(&self, q: &SearchQuery) -> Result<Page<Item>>;

    /// One entity (`/v1/entities/{id}`), following redirects by returning them.
    async fn entity(&self, id: &EntityId) -> Result<EntityLookup>;

    /// Versions of a record, newest first (`/v1/records/{id}/versions`).
    async fn record_versions(
        &self,
        id: RecordId,
        limit: u32,
        cursor: Option<String>,
    ) -> Result<Page<RecordVersionView>>;

    /// The change feed after `since` (`/v1/changes`).
    async fn changes(
        &self,
        since: Option<ChangeCursor>,
        kinds: &[KindId],
        limit: u32,
    ) -> Result<ChangesPage>;

    /// Observations of a series in `[from, to)` (`/v1/observations`).
    async fn observations(
        &self,
        series: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ObservationView>>;
}
