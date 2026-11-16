//! Committed captures: the `captures` row joined to its segment, and the stored body read back
//! from the raw store with one range request. Revisits (304s and identical payloads) resolve,
//! through their revisit chain, to the body of the capture they repeat.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use vc_core::frontier::Validators;
use vc_core::media::{FetchMode, MediaType};
use vc_core::store::{BlobStore, CaptureMeta, CaptureRole};
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_core::{
    CanonicalUrl, CaptureId, CaptureRef, Epoch, Error, PayloadHash, PolitenessKey, Result,
    SourceId, UrlKey,
};

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// The `capture_ready` payload written by `vc_commit_segment`.
#[derive(Debug, Clone, Deserialize)]
pub struct CaptureReady {
    /// Capture id.
    pub capture_id: String,
    /// Partition key.
    pub captured_at: DateTime<Utc>,
}

impl CaptureReady {
    /// Parses a work-item payload.
    pub fn parse(v: &serde_json::Value) -> Result<CaptureRef> {
        let p: CaptureReady = serde_json::from_value(v.clone())?;
        Ok(CaptureRef {
            capture_id: p
                .capture_id
                .parse()
                .map_err(|_| Error::Invalid("capture_ready: capture_id".into()))?,
            captured_at: p.captured_at,
        })
    }
}

/// One committed capture with its location in the raw store.
#[derive(Debug, Clone)]
pub struct StoredCapture {
    /// The metadata (as it was committed).
    pub meta: CaptureMeta,
    /// Segment object path.
    pub segment_path: String,
    /// Byte offset of the record group in the segment.
    pub offset: u64,
    /// Length of the record group.
    pub length: u64,
}

fn enum_from_str<T: serde::de::DeserializeOwned>(s: &str, what: &str) -> Result<T> {
    serde_json::from_value(serde_json::Value::String(s.to_string()))
        .map_err(|_| Error::Invalid(format!("capture {what} {s:?}")))
}

fn to_capture(r: &PgRow) -> Result<StoredCapture> {
    let g = |e: sqlx::Error| Error::Db(e.to_string());
    let id: String = r.try_get("capture_id").map_err(g)?;
    let capture_id: CaptureId = id
        .parse()
        .map_err(|_| Error::Invalid("capture id".into()))?;
    let url: String = r.try_get("url").map_err(g)?;
    let url = CanonicalUrl::parse(&url).map_err(|e| Error::Invalid(format!("capture url: {e}")))?;
    let payload_hash = match r.try_get::<Option<Vec<u8>>, _>("payload_hash").map_err(g)? {
        Some(b) => Some(PayloadHash(
            b.as_slice()
                .try_into()
                .map_err(|_| Error::Invalid("payload hash length".into()))?,
        )),
        None => None,
    };
    let revisit_of = match (
        r.try_get::<Option<String>, _>("revisit_of_id").map_err(g)?,
        r.try_get::<Option<DateTime<Utc>>, _>("revisit_of_at")
            .map_err(g)?,
    ) {
        (Some(id), Some(at)) => Some(CaptureRef {
            capture_id: id
                .parse()
                .map_err(|_| Error::Invalid("revisit_of id".into()))?,
            captured_at: at,
        }),
        _ => None,
    };
    let fm: String = r.try_get("fetch_mode").map_err(g)?;
    let fetch_mode: FetchMode = fm
        .parse()
        .map_err(|_| Error::Invalid(format!("capture fetch_mode {fm:?}")))?;
    let role: String = r.try_get("role").map_err(g)?;
    let media: String = r.try_get("media_type").map_err(g)?;
    let url_key: Vec<u8> = r.try_get("url_key").map_err(g)?;
    let meta = CaptureMeta {
        capture_id,
        captured_at: r.try_get("captured_at").map_err(g)?,
        epoch: Epoch(r.try_get("epoch").map_err(g)?),
        orphan_epoch: r.try_get("orphan_epoch").map_err(g)?,
        url_key: UrlKey::from_slice(&url_key)?,
        url,
        key: PolitenessKey(r.try_get("key").map_err(g)?),
        source_id: r
            .try_get::<Option<String>, _>("source_id")
            .map_err(g)?
            .map(SourceId),
        role: enum_from_str::<CaptureRole>(&role, "role")?,
        fetch_mode,
        status: r
            .try_get::<Option<i16>, _>("status")
            .map_err(g)?
            .and_then(|s| u16::try_from(s).ok()),
        media_type: enum_from_str::<MediaType>(&media, "media_type")?,
        payload_hash,
        payload_len: u64::try_from(r.try_get::<i64, _>("payload_len").map_err(g)?).unwrap_or(0),
        validators: Validators {
            etag: r.try_get("etag").map_err(g)?,
            last_modified: r.try_get("last_modified").map_err(g)?,
        },
        revisit_of,
        use_signals: serde_json::from_value::<UseSignals>(
            r.try_get::<serde_json::Value, _>("use_signals")
                .map_err(g)?,
        )
        .unwrap_or_default(),
        use_policy_fetch: serde_json::from_value::<UsePolicy>(
            r.try_get::<serde_json::Value, _>("use_policy_fetch")
                .map_err(g)?,
        )?,
        raw_expires_at: r.try_get("raw_expires_at").map_err(g)?,
        next_fetch_at: None,
    };
    let off: i64 = r.try_get("warc_offset").map_err(g)?;
    let len: i64 = r.try_get("warc_length").map_err(g)?;
    Ok(StoredCapture {
        meta,
        offset: u64::try_from(off).map_err(|_| Error::Invalid("warc offset".into()))?,
        length: u64::try_from(len).map_err(|_| Error::Invalid("warc length".into()))?,
        segment_path: r.try_get("path").map_err(g)?,
    })
}

/// Loads a committed capture; `None` when it is unknown (e.g. purged).
pub async fn load(pool: &PgPool, c: &CaptureRef) -> Result<Option<StoredCapture>> {
    let row = sqlx::query(
        "SELECT c.capture_id, c.captured_at, c.epoch, c.orphan_epoch, c.url_key, c.url, c.key,
                c.source_id, c.role, c.fetch_mode, c.status, c.media_type, c.payload_hash,
                c.payload_len, c.etag, c.last_modified, c.revisit_of_id, c.revisit_of_at,
                c.use_signals, c.use_policy_fetch, c.raw_expires_at, c.warc_offset,
                c.warc_length, s.path
         FROM captures c JOIN segments s ON s.segment_id = c.segment_id
         WHERE c.capture_id = $1 AND c.captured_at = $2 AND s.purged_at IS NULL",
    )
    .bind(c.capture_id.to_string())
    .bind(c.captured_at)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    row.as_ref().map(to_capture).transpose()
}

/// The committed captures of one segment, ordered by capture id.
pub async fn of_segment(pool: &PgPool, segment_id: &str) -> Result<Vec<StoredCapture>> {
    let rows = sqlx::query(
        "SELECT c.capture_id, c.captured_at, c.epoch, c.orphan_epoch, c.url_key, c.url, c.key,
                c.source_id, c.role, c.fetch_mode, c.status, c.media_type, c.payload_hash,
                c.payload_len, c.etag, c.last_modified, c.revisit_of_id, c.revisit_of_at,
                c.use_signals, c.use_policy_fetch, c.raw_expires_at, c.warc_offset,
                c.warc_length, s.path
         FROM captures c JOIN segments s ON s.segment_id = c.segment_id
         WHERE c.segment_id = $1 AND s.purged_at IS NULL
         ORDER BY c.capture_id",
    )
    .bind(segment_id)
    .fetch_all(pool)
    .await
    .map_err(db)?;
    rows.iter().map(to_capture).collect()
}

/// Whether this capture's payload differs from the URL's previous primary capture with a
/// stored payload (no previous capture → changed). A revisit is never a change.
pub async fn payload_changed(pool: &PgPool, c: &StoredCapture) -> Result<bool> {
    if c.meta.revisit_of.is_some() || c.meta.status == Some(304) {
        return Ok(false);
    }
    let Some(ph) = &c.meta.payload_hash else {
        return Ok(true);
    };
    let prev: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        "SELECT payload_hash FROM captures
         WHERE url_key = $1 AND role = 'primary' AND captured_at < $2
           AND payload_hash IS NOT NULL
           AND (status IS NULL OR status BETWEEN 200 AND 299)
         ORDER BY captured_at DESC LIMIT 1",
    )
    .bind(c.meta.url_key.as_bytes().as_slice())
    .bind(c.meta.captured_at)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    Ok(match prev {
        Some((Some(p),)) => p.as_slice() != ph.as_bytes().as_slice(),
        _ => true,
    })
}

/// The payload of a capture's record group: a `response` body or a `resource` block.
/// `None` for revisits and metadata.
pub(crate) fn payload_of(records: &[vc_store::warc::WarcRecord]) -> Result<Option<bytes::Bytes>> {
    for r in records {
        match r.warc_type() {
            Some("response") => return Ok(Some(vc_store::warc::parse_http(&r.block)?.body)),
            Some("resource") => return Ok(Some(r.block.clone())),
            _ => {}
        }
    }
    Ok(None)
}

/// Max revisit hops [`body`] follows (a 304 chain grows by one per unchanged fetch while the
/// frontier points `last_capture_id` at revisits).
pub const MAX_REVISIT_HOPS: usize = 64;

/// Reads the stored body of `c`, following its revisit chain (R2 → R1 → C0) to the capture
/// that stored the body: bounded ([`MAX_REVISIT_HOPS`]) and cycle-safe. `None` when there is no
/// body (metadata record, the chain is broken or loops, or the repeated capture is gone).
pub async fn body(
    pool: &PgPool,
    blobs: &Arc<dyn BlobStore>,
    c: &StoredCapture,
) -> Result<Option<bytes::Bytes>> {
    let mut seen = std::collections::HashSet::new();
    let mut cur = c.clone();
    for _ in 0..=MAX_REVISIT_HOPS {
        if !seen.insert(cur.meta.capture_id) {
            tracing::warn!(capture = %c.meta.capture_id, "revisit chain loops; no body");
            return Ok(None);
        }
        let recs =
            vc_store::read_capture(blobs.as_ref(), &cur.segment_path, cur.offset, cur.length)
                .await?;
        if let Some(b) = payload_of(&recs)? {
            return Ok(Some(b));
        }
        let Some(prev) = &cur.meta.revisit_of else {
            return Ok(None);
        };
        let Some(p) = load(pool, prev).await? else {
            return Ok(None);
        };
        cur = p;
    }
    tracing::warn!(capture = %c.meta.capture_id, hops = MAX_REVISIT_HOPS,
        "revisit chain too long; no body");
    Ok(None)
}

/// Records the final use policy of a capture (the meet of fetch-time and body signals).
pub async fn set_final_policy(pool: &PgPool, c: &CaptureRef, p: &UsePolicy) -> Result<()> {
    sqlx::query(
        "UPDATE captures SET use_policy_final = $3
         WHERE capture_id = $1 AND captured_at = $2",
    )
    .bind(c.capture_id.to_string())
    .bind(c.captured_at)
    .bind(serde_json::to_value(p)?)
    .execute(pool)
    .await
    .map_err(db)?;
    Ok(())
}
