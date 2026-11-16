//! Raw storage: blobs, the spool and segment commits (spec §5.5, §17.2, §17.7). Implemented by
//! `vc-store`.
//!
//! Crash-consistent order: (1) append a [`SpoolRecord`] to the local spool and fsync
//! ([`CaptureSink::append`]); (2) roll a segment (128 MB / 10 min), upload it with
//! `ifGenerationMatch=0` ([`BlobStore::put_if_absent`]) plus its CDXJ; (3) after the crc32c
//! matches, **one** call to `vc_commit_segment(jsonb)` ([`SegmentCommitter::commit`]) inserts the
//! segment and captures, updates `url_state` validators (monotonic guard on `captured_at`, not
//! epoch-gated) and enqueues `capture_ready`; (4) delete the spool file. Steps 2–3 are
//! idempotent by segment id.

use std::ops::Range;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fetch::Exchange;
use crate::frontier::Validators;
use crate::ids::{
    CaptureId, CaptureRef, Epoch, PayloadHash, PolitenessKey, SegmentId, SourceId, UrlKey,
};
use crate::media::{FetchMode, MediaType};
use crate::url::CanonicalUrl;
use crate::usepolicy::{UsePolicy, UseSignals};

/// A capture time as every store keeps it: whole microseconds. `Utc::now()` has nanoseconds on
/// Linux, Postgres rounds them away, and the WARC header, CDXJ, the `capture_ready` payload and
/// `captures.captured_at` must all hold the same instant (the partition key is matched exactly).
pub fn capture_time(t: DateTime<Utc>) -> DateTime<Utc> {
    use chrono::SubsecRound;
    t.trunc_subsecs(6)
}

/// GCS prefix for archived WARC segments (`segments/YYYY/MM/DD/<ulid>.warc.zst`).
pub const PREFIX_SEGMENTS: &str = "segments/";
/// GCS prefix for `noarchive` / TDM-reserved captures (deleted after 30 days, never Coldline).
pub const PREFIX_EPHEMERAL: &str = "ephemeral/";
/// GCS prefix for the extracted-text tier (`text/YYYY/MM/<ulid>.jsonl.zst`, Standard).
pub const PREFIX_TEXT: &str = "text/";
/// GCS prefix for CDXJ indexes (excluded from the Coldline rule).
pub const PREFIX_CDXJ: &str = "cdxj/";

/// The outcome of [`BlobStore::put_if_absent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PutOutcome {
    /// The object was created.
    Created,
    /// An object with identical bytes already existed (idempotent retry).
    AlreadyIdentical,
}

/// Object storage (GCS, local FS, in-memory fake).
#[async_trait]
pub trait BlobStore: Send + Sync {
    /// Creates `path` only if absent (`ifGenerationMatch=0`), verifying `crc32c` of `bytes`.
    /// An existing object with the same bytes is [`PutOutcome::AlreadyIdentical`]; with other
    /// bytes it is [`crate::Error::Conflict`] (alert-worthy).
    async fn put_if_absent(&self, path: &str, bytes: Bytes, crc32c: u32) -> Result<PutOutcome>;

    /// Reads an object, or a byte range of it. Missing → [`crate::Error::NotFound`].
    async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes>;

    /// Deletes an object (purge). Missing is not an error.
    async fn delete(&self, path: &str) -> Result<()>;

    /// Lists object paths under a prefix, sorted (rebuild-captures, purge, maintenance).
    async fn list(&self, prefix: &str) -> Result<Vec<String>>;
}

/// What a capture is, relative to the crawl.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureRole {
    /// The leased URL itself (page, feed, sitemap, API response, rendered DOM). Only primary
    /// captures update `url_state` and enqueue `capture_ready`.
    Primary,
    /// A render sub-request.
    Subresource,
    /// robots.txt or tdmrep.json.
    Robots,
    /// A dump manifest or adapter metadata.
    Manifest,
}

/// Metadata of one captured record, carried in the spool, the segment manifest and the
/// `captures` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureMeta {
    /// Capture id (ULID).
    pub capture_id: CaptureId,
    /// Capture time (partition key).
    pub captured_at: DateTime<Utc>,
    /// The claim epoch it was fetched under.
    pub epoch: Epoch,
    /// The epoch's completion was rejected; replayed as a normal capture (§5.2).
    #[serde(default)]
    pub orphan_epoch: bool,
    /// Url key of `url`.
    pub url_key: UrlKey,
    /// The URL.
    pub url: CanonicalUrl,
    /// Politeness key.
    pub key: PolitenessKey,
    /// Source.
    pub source_id: Option<SourceId>,
    /// Role.
    pub role: CaptureRole,
    /// Fetch mode.
    pub fetch_mode: FetchMode,
    /// HTTP status (None for resource/metadata records).
    pub status: Option<u16>,
    /// Media type fixed at fetch time.
    pub media_type: MediaType,
    /// SHA-256 of the decoded payload (for revisits: the referenced payload's hash).
    pub payload_hash: Option<PayloadHash>,
    /// Decoded payload length.
    pub payload_len: u64,
    /// Validators returned by the server.
    #[serde(default)]
    pub validators: Validators,
    /// For revisit records: the capture whose payload this repeats.
    pub revisit_of: Option<CaptureRef>,
    /// Signals seen at fetch time (robots + headers).
    #[serde(default)]
    pub use_signals: UseSignals,
    /// Use policy at fetch time (`captures.use_policy_fetch`).
    pub use_policy_fetch: UsePolicy,
    /// For ephemeral (noarchive/TDM) captures: when the raw bytes must be purged.
    pub raw_expires_at: Option<DateTime<Utc>>,
    /// The next fetch time decided by the fetcher (also set by `mark_fetched`).
    pub next_fetch_at: Option<DateTime<Utc>>,
}

impl CaptureMeta {
    /// The `(capture_id, captured_at)` reference.
    pub fn capture_ref(&self) -> CaptureRef {
        CaptureRef {
            capture_id: self.capture_id,
            captured_at: self.captured_at,
        }
    }
}

/// The payload of a spool record (one WARC record, serialised by `vc-store::warc` only).
#[derive(Debug, Clone)]
pub enum SpoolPayload {
    /// A WARC `request` + `response` pair.
    Response(Exchange),
    /// A WARC `revisit` (profile `server-not-modified` for a 304, `identical-payload-digest`
    /// otherwise); the exchange carries headers, the body is not stored again.
    Revisit(Exchange),
    /// A WARC `resource` record (rendered DOM, filtered dump output).
    Resource {
        /// Content type of the body.
        content_type: String,
        /// The body.
        body: Bytes,
    },
    /// A WARC `metadata` record (e.g. a `dump_manifest`).
    Metadata {
        /// The fields (JSON).
        fields: serde_json::Value,
    },
}

/// One record appended to the spool.
#[derive(Debug, Clone)]
pub struct SpoolRecord {
    /// Metadata.
    pub meta: CaptureMeta,
    /// Payload.
    pub payload: SpoolPayload,
}

/// Durable, append-only capture sink: each role instance writes only its own spool
/// (`spool/<role>-<instance>/`) and runs its own segment roller.
#[async_trait]
pub trait CaptureSink: Send {
    /// Appends a record and **fsyncs** before returning its capture id.
    async fn append(&mut self, r: SpoolRecord) -> Result<CaptureId>;
}

/// One capture's location in a committed segment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManifestCapture {
    /// Metadata (flattened into the JSON object).
    #[serde(flatten)]
    pub meta: CaptureMeta,
    /// Byte offset of the record in the (compressed) segment.
    pub offset: u64,
    /// Length in bytes.
    pub length: u64,
    /// `WARC-Record-ID` (`<urn:uuid:…>`).
    pub warc_record_id: String,
}

/// The segment manifest. Its JSON (`serde_json::to_value`) is the argument of the SQL function
/// `vc_commit_segment(jsonb)` — this struct **is** that contract.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SegmentManifest {
    /// Segment id (ULID).
    pub segment_id: SegmentId,
    /// Object path of the `.warc.zst`.
    pub path: String,
    /// Object path of the `.cdxj`.
    pub cdxj_path: String,
    /// Size in bytes.
    pub bytes: u64,
    /// crc32c of the object.
    pub crc32c: u32,
    /// Number of WARC records.
    pub record_count: u32,
    /// Stored under `ephemeral/`.
    pub ephemeral: bool,
    /// When the segment was closed.
    pub created_at: DateTime<Utc>,
    /// The captures in it.
    pub captures: Vec<ManifestCapture>,
}

/// Commits uploaded segments (one `vc_commit_segment` call, one transaction).
#[async_trait]
pub trait SegmentCommitter: Send + Sync {
    /// Commits the manifest. Idempotent by `segment_id`: a second call is a no-op `Ok`.
    async fn commit(&self, m: &SegmentManifest) -> Result<()>;
}
