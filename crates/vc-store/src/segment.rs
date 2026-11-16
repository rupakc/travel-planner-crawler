//! Segments: build, upload, commit (spec §5.5 steps 2–4) and segment-at-a-time reads.
//!
//! A segment is the concatenation of one zstd frame per capture (each frame holds the capture's
//! uncompressed WARC record group), so a capture can be read with one range request at its
//! `(offset, length)`. Building is deterministic (same sealed spool → same bytes), which makes
//! the `ifGenerationMatch=0` upload idempotent across crashes and replays.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

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

/// Whether a segment is already committed (a replayed sealed file must not upload its original
/// bytes again: a purge may have rewritten or dropped the segment since).
#[async_trait]
pub trait CommitProbe: Send + Sync {
    /// `true` when the segment has a `segments` row.
    async fn is_committed(&self, segment: SegmentId) -> Result<bool>;
}

/// A sealed file whose segment cannot be built (damaged, unreadable) or whose object exists
/// with different bytes fails the same way every time; after this many consecutive failures it
/// is renamed to `<file>.quarantined` (kept for the operator, never published) and an alert
/// counter ([`SegmentRoller::quarantined_total`]) goes up.
pub const QUARANTINE_AFTER: u32 = 3;

/// Why a publish failed: the file itself (skip it, newer files still go) or the environment
/// (store/database down: stop, the next tick retries everything).
enum PublishError {
    File(Error),
    Env(Error),
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
    probe: Option<Arc<dyn CommitProbe>>,
    file_failures: Mutex<HashMap<PathBuf, u32>>,
    quarantined: AtomicU64,
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
            probe: None,
            file_failures: Mutex::new(HashMap::new()),
            quarantined: AtomicU64::new(0),
        }
    }

    /// Checks `probe` before every upload: a sealed file whose segment is already committed
    /// (a crash after the commit, replayed) is only deleted, never uploaded again.
    #[must_use]
    pub fn with_commit_probe(mut self, probe: Arc<dyn CommitProbe>) -> Self {
        self.probe = Some(probe);
        self
    }

    /// Sealed files quarantined since start (alert when > 0).
    pub fn quarantined_total(&self) -> u64 {
        self.quarantined.load(Ordering::Relaxed)
    }

    /// Publishes one sealed spool file: upload, commit, delete. A file whose segment is
    /// already committed (see [`Self::with_commit_probe`]) is deleted and returned with no
    /// captures.
    pub async fn publish(&self, sealed: &SealedSpool) -> Result<SegmentManifest> {
        self.publish_classified(sealed).await.map_err(|e| match e {
            PublishError::File(e) | PublishError::Env(e) => e,
        })
    }

    async fn remove_sealed(sealed: &SealedSpool) -> Result<()> {
        let s = sealed.clone();
        tokio::task::spawn_blocking(move || s.remove())
            .await
            .map_err(|e| Error::Storage(format!("spool remove task: {e}")))?
    }

    async fn publish_classified(
        &self,
        sealed: &SealedSpool,
    ) -> std::result::Result<SegmentManifest, PublishError> {
        use PublishError::{Env, File};
        let s = sealed.clone();
        let level = self.zstd_level;
        let built = tokio::task::spawn_blocking(move || {
            let entries = s.read_entries()?;
            build_segment(s.segment_id, s.ephemeral, &entries, level)
        })
        .await
        .map_err(|e| Env(Error::Storage(format!("segment build task: {e}"))))?
        .map_err(File)?;
        let mut m = built.manifest;
        if m.captures.is_empty() {
            sealed.remove().map_err(Env)?;
            return Ok(m);
        }
        if let Some(p) = &self.probe
            && p.is_committed(m.segment_id).await.map_err(Env)?
        {
            Self::remove_sealed(sealed).await.map_err(Env)?;
            tracing::info!(
                segment = %m.segment_id,
                "store: sealed file already committed; dropped without re-upload"
            );
            m.captures.clear();
            return Ok(m);
        }
        let up = |e: Error| match e {
            e @ Error::Conflict(_) => File(e),
            e => Env(e),
        };
        fail::fail_point!("segment::before_upload", |_| Err(Env(Error::Storage(
            "failpoint segment::before_upload".into()
        ))));
        upload(self.store.as_ref(), &m.path, built.bytes)
            .await
            .map_err(up)?;
        fail::fail_point!("segment::after_upload", |_| Err(Env(Error::Storage(
            "failpoint segment::after_upload".into()
        ))));
        upload(self.store.as_ref(), &m.cdxj_path, built.cdxj)
            .await
            .map_err(up)?;
        fail::fail_point!("segment::before_commit", |_| Err(Env(Error::Storage(
            "failpoint segment::before_commit".into()
        ))));
        self.committer.commit(&m).await.map_err(Env)?;
        fail::fail_point!("segment::after_commit", |_| Err(Env(Error::Storage(
            "failpoint segment::after_commit".into()
        ))));
        Self::remove_sealed(sealed).await.map_err(Env)?;
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
            match self.publish_classified(&s).await {
                Ok(m) => {
                    self.file_failures().remove(&s.path);
                    out.push(m);
                }
                Err(PublishError::File(e)) => self.file_failed(&s, &e)?,
                Err(PublishError::Env(e)) => {
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

    fn file_failures(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, u32>> {
        self.file_failures.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Counts a file-local failure; quarantines the file after [`QUARANTINE_AFTER`].
    fn file_failed(&self, s: &SealedSpool, e: &Error) -> Result<()> {
        let n = {
            let mut f = self.file_failures();
            let n = f.entry(s.path.clone()).or_insert(0);
            *n += 1;
            *n
        };
        if n < QUARANTINE_AFTER {
            tracing::warn!(
                segment = %s.segment_id,
                failures = n,
                error = %e,
                "store: sealed file failed; skipped, newer files continue"
            );
            return Ok(());
        }
        let mut q = s.path.clone().into_os_string();
        q.push(".quarantined");
        std::fs::rename(&s.path, &q)?;
        if let Some(dir) = s.path.parent() {
            File::open(dir)?.sync_all()?;
        }
        self.file_failures().remove(&s.path);
        self.quarantined.fetch_add(1, Ordering::Relaxed);
        tracing::error!(
            segment = %s.segment_id,
            file = %PathBuf::from(&q).display(),
            error = %e,
            "store: sealed file quarantined (alert); its captures are not published"
        );
        Ok(())
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
