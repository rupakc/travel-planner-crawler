//! Purge rewrite (spec §17.7): rewrite a committed segment without some captures.
//!
//! Crash-consistent order:
//! 1. [`rewrite_segment_without`] uploads the new segment and CDXJ under a new generation path
//!    (`<id>.r<N>.warc.zst`, create-if-absent, so a replay is idempotent);
//! 2. [`crate::PgSegmentCommitter::commit_rewrite`] switches the `segments` row, fixes the kept
//!    captures' offsets and deletes the purged capture rows in one transaction (fenced on the old
//!    path);
//! 3. [`finish_rewrite`] deletes the old objects (GCS then drops the non-current versions per the
//!    bucket's soft-delete window).
//!
//! The kept captures' compressed frames are copied byte for byte (no recompression).

use std::collections::HashSet;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use vc_core::store::{BlobStore, ManifestCapture};
use vc_core::{CaptureId, CaptureRef, Error, Result, SegmentId};

use crate::paths::{is_ephemeral, rewrite_generation, segment_paths_gen};
use crate::segment::{cdxj_for, read_cdxj, upload};

/// A kept capture's new location.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeptCapture {
    /// Capture id.
    pub capture_id: CaptureId,
    /// Partition key.
    pub captured_at: DateTime<Utc>,
    /// New offset.
    pub offset: u64,
    /// Length (unchanged).
    pub length: u64,
}

/// The result of a rewrite; its JSON is the argument of `vc_rewrite_segment(jsonb)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentRewrite {
    /// Segment id.
    pub segment_id: SegmentId,
    /// The current (old) segment path.
    pub old_path: String,
    /// The current (old) CDXJ path.
    pub old_cdxj_path: String,
    /// The new segment path (`None` when nothing is kept: the segment is purged entirely).
    pub new_path: Option<String>,
    /// The new CDXJ path.
    pub new_cdxj_path: Option<String>,
    /// New size.
    pub bytes: u64,
    /// New crc32c.
    pub crc32c: u32,
    /// New WARC record count.
    pub record_count: u32,
    /// Kept captures and their new offsets.
    pub kept: Vec<KeptCapture>,
    /// Removed captures.
    pub removed: Vec<CaptureRef>,
    /// Objects of uncommitted generations this rewrite stepped over (a rewrite that crashed
    /// before its commit, with another removal set): garbage once this rewrite commits.
    #[serde(default)]
    pub abandoned: Vec<String>,
}

/// How many occupied generations a rewrite steps over before giving up.
pub const MAX_GENERATION_TRIES: u32 = 16;

/// Rewrites the segment at `path` (CDXJ at `cdxj_path`) without the captures in `remove`, and
/// uploads the new objects. The database and the old objects are untouched (see the module
/// docs for the next steps).
pub async fn rewrite_segment_without(
    store: &dyn BlobStore,
    path: &str,
    cdxj_path: &str,
    remove: &HashSet<CaptureId>,
) -> Result<SegmentRewrite> {
    let entries = read_cdxj(store, cdxj_path).await?;
    let old = store.get(path, None).await?;
    let segment_id = segment_id_of(path)?;
    let mut bytes = Vec::new();
    let mut caps: Vec<ManifestCapture> = Vec::new();
    let mut records = Vec::new();
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    let mut ordered = entries;
    ordered.sort_by_key(|e| e.capture.offset);
    for e in ordered {
        let c = e.capture;
        if remove.contains(&c.meta.capture_id) {
            removed.push(c.meta.capture_ref());
            continue;
        }
        let start = usize::try_from(c.offset).map_err(|_| Error::Invalid("offset".into()))?;
        let end = start
            .checked_add(usize::try_from(c.length).map_err(|_| Error::Invalid("length".into()))?)
            .filter(|e| *e <= old.len())
            .ok_or_else(|| Error::Invalid(format!("capture outside segment {path}")))?;
        let offset = bytes.len() as u64;
        bytes.extend_from_slice(&old[start..end]);
        kept.push(KeptCapture {
            capture_id: c.meta.capture_id,
            captured_at: c.meta.captured_at,
            offset,
            length: c.length,
        });
        records.push(e.records);
        caps.push(ManifestCapture { offset, ..c });
    }
    let record_count = records.iter().sum();
    if caps.is_empty() {
        return Ok(SegmentRewrite {
            segment_id,
            old_path: path.into(),
            old_cdxj_path: cdxj_path.into(),
            new_path: None,
            new_cdxj_path: None,
            bytes: 0,
            crc32c: 0,
            record_count: 0,
            kept,
            removed,
            abandoned: vec![],
        });
    }
    let crc = crc32c::crc32c(&bytes);
    let n = bytes.len() as u64;
    let bytes = Bytes::from(bytes);
    let mut abandoned = Vec::new();
    let mut generation = rewrite_generation(path);
    let (new_path, new_cdxj_path) = loop {
        generation += 1;
        if generation > rewrite_generation(path) + MAX_GENERATION_TRIES {
            return Err(Error::Conflict(format!(
                "{path}: {MAX_GENERATION_TRIES} rewrite generations occupied"
            )));
        }
        let (p, c) = segment_paths_gen(segment_id, is_ephemeral(path), generation);
        // An object already at this generation with other bytes is an uncommitted rewrite
        // (the committed one is `path`): step over it and report it for deletion.
        match upload(store, &p, bytes.clone()).await {
            Ok(_) => {}
            Err(Error::Conflict(_)) => {
                abandoned.extend([p, c]);
                continue;
            }
            Err(e) => return Err(e),
        }
        let cdxj = cdxj_for(&p, &caps, &records)?;
        match upload(store, &c, Bytes::from(cdxj)).await {
            Ok(_) => break (p, c),
            Err(Error::Conflict(_)) => {
                abandoned.extend([p, c]);
            }
            Err(e) => return Err(e),
        }
    };
    tracing::info!(
        segment = %segment_id,
        kept = kept.len(),
        removed = removed.len(),
        "store: segment rewritten"
    );
    Ok(SegmentRewrite {
        segment_id,
        old_path: path.into(),
        old_cdxj_path: cdxj_path.into(),
        new_path: Some(new_path),
        new_cdxj_path: Some(new_cdxj_path),
        bytes: n,
        crc32c: crc,
        record_count,
        kept,
        removed,
        abandoned,
    })
}

/// Deletes the old objects of a committed rewrite.
pub async fn finish_rewrite(store: &dyn BlobStore, rw: &SegmentRewrite) -> Result<()> {
    store.delete(&rw.old_path).await?;
    store.delete(&rw.old_cdxj_path).await
}

fn segment_id_of(path: &str) -> Result<SegmentId> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let id = file.split('.').next().unwrap_or(file);
    id.parse()
}
