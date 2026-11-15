//! Segments: build, upload, commit (spec §5.5 steps 2–4) and segment-at-a-time reads.
//!
//! A segment is the concatenation of one zstd frame per capture (each frame holds the capture's
//! uncompressed WARC record group), so a capture can be read with one range request at its
//! `(offset, length)`. Building is deterministic (same sealed spool → same bytes), which makes
//! the `ifGenerationMatch=0` upload idempotent across crashes and replays.

use std::io::Read;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use vc_core::store::{BlobStore, ManifestCapture, PutOutcome, SegmentCommitter, SegmentManifest};
use vc_core::{Clock, Error, Result, SegmentId};

use crate::cdxj::{CdxjEntry, parse_cdxj, write_cdxj};
use crate::paths::segment_paths;
use crate::spool::{SealedSpool, Spool, SpoolEntry};
use crate::warc::{WarcRecord, read_records};

/// A built (not yet uploaded) segment.
#[derive(Debug, Clone)]
pub struct BuiltSegment {
    /// The `.warc.zst` bytes.
    pub bytes: Bytes,
    /// The `.cdxj` bytes.
    pub cdxj: Bytes,
    /// The manifest (the `vc_commit_segment` argument).
    pub manifest: SegmentManifest,
    /// Per capture, the number of WARC records (same order as `manifest.captures`).
    pub records: Vec<u32>,
}

/// Compresses one record group as an independent zstd frame.
pub fn compress_frame(warc: &[u8], level: i32) -> Result<Vec<u8>> {
    zstd::bulk::compress(warc, level).map_err(|e| Error::Storage(format!("zstd: {e}")))
}

/// Builds a segment from spool entries. Pure and deterministic.
pub fn build_segment(
    segment_id: SegmentId,
    ephemeral: bool,
    entries: &[SpoolEntry],
    zstd_level: i32,
) -> Result<BuiltSegment> {
    let (path, cdxj_path) = segment_paths(segment_id, ephemeral);
    let mut bytes = Vec::new();
    let mut captures = Vec::with_capacity(entries.len());
    let mut records = Vec::with_capacity(entries.len());
    let mut record_count = 0u32;
    for e in entries {
        let frame = compress_frame(&e.warc, zstd_level)?;
        captures.push(ManifestCapture {
            meta: e.meta.clone(),
            offset: bytes.len() as u64,
            length: frame.len() as u64,
            warc_record_id: e.warc_record_id.clone(),
        });
        records.push(e.record_count);
        record_count += e.record_count;
        bytes.extend_from_slice(&frame);
    }
    let cdxj = cdxj_for(&path, &captures, &records)?;
    let manifest = SegmentManifest {
        segment_id,
        path,
        cdxj_path,
        bytes: bytes.len() as u64,
        crc32c: crc32c::crc32c(&bytes),
        record_count,
        ephemeral,
        created_at: segment_id.datetime(),
        captures,
    };
    Ok(BuiltSegment {
        bytes: Bytes::from(bytes),
        cdxj: Bytes::from(cdxj),
        manifest,
        records,
    })
}

pub(crate) fn cdxj_for(path: &str, caps: &[ManifestCapture], records: &[u32]) -> Result<String> {
    let entries: Vec<CdxjEntry> = caps
        .iter()
        .zip(records)
        .map(|(c, r)| CdxjEntry {
            capture: c.clone(),
            filename: path.to_string(),
            records: *r,
        })
        .collect();
    write_cdxj(&entries)
}

/// Uploads one object, treating an identical existing object as success.
pub async fn upload(store: &dyn BlobStore, path: &str, bytes: Bytes) -> Result<PutOutcome> {
    let crc = crc32c::crc32c(&bytes);
    let n = bytes.len();
    match store.put_if_absent(path, bytes, crc).await {
        Ok(o) => {
            tracing::debug!(path, bytes = n, outcome = ?o, "store: uploaded");
            Ok(o)
        }
        Err(e @ Error::Conflict(_)) => {
            tracing::error!(path, "store: object exists with different bytes (alert)");
            Err(e)
        }
        Err(e) => Err(e),
    }
}

/// Decompresses a byte string of concatenated zstd frames.
pub fn decompress(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    zstd::stream::read::Decoder::new(bytes)
        .and_then(|mut d| d.read_to_end(&mut out))
        .map_err(|e| Error::Invalid(format!("zstd: {e}")))?;
    Ok(out)
}

/// Reads a whole segment (re-processing reads one segment at a time).
pub async fn read_segment(store: &dyn BlobStore, path: &str) -> Result<Vec<WarcRecord>> {
    let b = store.get(path, None).await?;
    read_records(&decompress(&b)?)
}

/// Reads one capture's record group with a single range request.
pub async fn read_capture(
    store: &dyn BlobStore,
    path: &str,
    offset: u64,
    length: u64,
) -> Result<Vec<WarcRecord>> {
    let end = offset
        .checked_add(length)
        .ok_or_else(|| Error::Invalid("capture range overflows".into()))?;
    let b = store.get(path, Some(offset..end)).await?;
    read_records(&decompress(&b)?)
}

/// Reads a segment's CDXJ.
pub async fn read_cdxj(store: &dyn BlobStore, cdxj_path: &str) -> Result<Vec<CdxjEntry>> {
    let b = store.get(cdxj_path, None).await?;
    let text = std::str::from_utf8(&b).map_err(|_| Error::Invalid("cdxj not utf-8".into()))?;
    parse_cdxj(text)
}

/// Rolls sealed spool files into committed segments.
///
/// Order per segment: build → upload `.warc.zst` (`ifGenerationMatch=0`) → upload `.cdxj` →
/// one `vc_commit_segment` → delete the sealed spool file. Every step is idempotent by segment
/// id, so a crash anywhere is repaired by publishing the same sealed file again ([`Self::tick`]
/// on start replays everything on disk).
pub struct SegmentRoller {
    spool: Spool,
    store: Arc<dyn BlobStore>,
    committer: Arc<dyn SegmentCommitter>,
    clock: Arc<dyn Clock>,
    zstd_level: i32,
    upload_pause: chrono::Duration,
    failing_since: Mutex<Option<DateTime<Utc>>>,
}

impl std::fmt::Debug for SegmentRoller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SegmentRoller")
            .field("spool", &self.spool.dir())
            .field("zstd_level", &self.zstd_level)
            .finish_non_exhaustive()
    }
}

impl SegmentRoller {
    /// A roller for `spool`.
    pub fn new(
        spool: Spool,
        store: Arc<dyn BlobStore>,
        committer: Arc<dyn SegmentCommitter>,
        clock: Arc<dyn Clock>,
        cfg: &vc_config::StoreConfig,
    ) -> Self {
        SegmentRoller {
            spool,
            store,
            committer,
            clock,
            zstd_level: cfg.zstd_level,
            upload_pause: chrono::Duration::seconds(
                i64::try_from(cfg.upload_failure_pause_s).unwrap_or(900),
            ),
            failing_since: Mutex::new(None),
        }
    }

    /// Publishes one sealed spool file: upload, commit, delete.
    pub async fn publish(&self, sealed: &SealedSpool) -> Result<SegmentManifest> {
        let s = sealed.clone();
        let level = self.zstd_level;
        let built = tokio::task::spawn_blocking(move || {
            let entries = s.read_entries()?;
            build_segment(s.segment_id, s.ephemeral, &entries, level)
        })
        .await
        .map_err(|e| Error::Storage(format!("segment build task: {e}")))??;
        let m = built.manifest;
        if m.captures.is_empty() {
            sealed.remove()?;
            return Ok(m);
        }
        fail::fail_point!("segment::before_upload", |_| Err(Error::Storage(
            "failpoint segment::before_upload".into()
        )));
        upload(self.store.as_ref(), &m.path, built.bytes).await?;
        fail::fail_point!("segment::after_upload", |_| Err(Error::Storage(
            "failpoint segment::after_upload".into()
        )));
        upload(self.store.as_ref(), &m.cdxj_path, built.cdxj).await?;
        fail::fail_point!("segment::before_commit", |_| Err(Error::Storage(
            "failpoint segment::before_commit".into()
        )));
        self.committer.commit(&m).await?;
        fail::fail_point!("segment::after_commit", |_| Err(Error::Storage(
            "failpoint segment::after_commit".into()
        )));
        let s = sealed.clone();
        tokio::task::spawn_blocking(move || s.remove())
            .await
            .map_err(|e| Error::Storage(format!("spool remove task: {e}")))??;
        tracing::info!(
            segment = %m.segment_id,
            captures = m.captures.len(),
            bytes = m.bytes,
            ephemeral = m.ephemeral,
            "store: segment committed"
        );
        Ok(m)
    }

    /// Seals due files and publishes every sealed file, oldest first. Stops at the first
    /// failure (the rest stay on disk for the next tick). Call on start (replay) and
    /// periodically.
    pub async fn tick(&self) -> Result<Vec<SegmentManifest>> {
        let spool = self.spool.clone();
        tokio::task::spawn_blocking(move || spool.seal_due())
            .await
            .map_err(|e| Error::Storage(format!("seal task: {e}")))??;
        self.publish_sealed().await
    }

    /// Seals everything and publishes it (graceful shutdown).
    pub async fn flush(&self) -> Result<Vec<SegmentManifest>> {
        let spool = self.spool.clone();
        tokio::task::spawn_blocking(move || spool.seal_all())
            .await
            .map_err(|e| Error::Storage(format!("seal task: {e}")))??;
        self.publish_sealed().await
    }

    async fn publish_sealed(&self) -> Result<Vec<SegmentManifest>> {
        let mut out = Vec::new();
        for s in self.spool.sealed()? {
            match self.publish(&s).await {
                Ok(m) => out.push(m),
                Err(e) => {
                    let now = self.clock.now();
                    let mut f = self.failing_since.lock().unwrap_or_else(|p| p.into_inner());
                    f.get_or_insert(now);
                    tracing::warn!(segment = %s.segment_id, error = %e, "store: publish failed");
                    return Err(e);
                }
            }
        }
        *self.failing_since.lock().unwrap_or_else(|p| p.into_inner()) = None;
        Ok(out)
    }

    /// Since when publishing has been failing, if it is.
    pub fn failing_since(&self) -> Option<DateTime<Utc>> {
        *self.failing_since.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Backpressure (spec §5.5): the spool is over its threshold, or uploads have been failing
    /// for longer than `upload_failure_pause_s` → fetchers pause and an alert fires.
    pub fn should_pause_fetchers(&self) -> bool {
        self.spool.over_backpressure()
            || self
                .failing_since()
                .is_some_and(|t| self.clock.now() - t > self.upload_pause)
    }
}
