//! The fsynced local spool (spec §5.5 step 1).
//!
//! Layout of `spool/<role>-<instance>/`:
//!
//! * `archive.open`, `ephemeral.open` — the files being appended to (one per segment class:
//!   captures with `raw_expires_at` go to `ephemeral/`);
//! * `<class>-<SEGMENT_ULID>.sealed` — a closed file whose content and segment id are fixed.
//!   It is published by the [`crate::SegmentRoller`] and deleted only after the commit.
//!
//! Each entry is a frame: `VCSP` · version (1) · meta length (u32 LE) · WARC length (u32 LE) ·
//! crc32c of meta+WARC (u32 LE), then the meta JSON and the uncompressed WARC record group.
//! [`CaptureSink::append`] returns only after `fsync`. On open, a torn tail of an `.open` file
//! (a frame that was never acknowledged) is truncated away.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use vc_core::store::{CaptureMeta, CaptureSink, SpoolPayload, SpoolRecord};
use vc_core::{CaptureId, Clock, Error, Result, SegmentId};

use crate::warc;

const MAGIC: &[u8; 4] = b"VCSP";
const FRAME_VERSION: u8 = 1;
const FRAME_HEADER: usize = 4 + 1 + 4 + 4 + 4;

/// Spool limits.
#[derive(Debug, Clone, Copy)]
pub struct SpoolOptions {
    /// Seal a file once it holds this many bytes.
    pub max_bytes: u64,
    /// Seal a file once its first record is this old.
    pub max_age: chrono::Duration,
    /// Fetchers pause above this many bytes on disk.
    pub backpressure_bytes: u64,
}

impl Default for SpoolOptions {
    fn default() -> Self {
        SpoolOptions::from(&vc_config::StoreConfig::default())
    }
}

impl From<&vc_config::StoreConfig> for SpoolOptions {
    fn from(c: &vc_config::StoreConfig) -> Self {
        SpoolOptions {
            max_bytes: c.segment_max_bytes,
            max_age: chrono::Duration::seconds(i64::try_from(c.segment_max_age_s).unwrap_or(600)),
            backpressure_bytes: c.spool_backpressure_bytes,
        }
    }
}

/// The persisted part of a frame besides the WARC bytes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct FrameMeta {
    meta: CaptureMeta,
    warc_record_id: String,
    record_count: u32,
}

/// One spooled capture read back from disk.
#[derive(Debug, Clone, PartialEq)]
pub struct SpoolEntry {
    /// Metadata.
    pub meta: CaptureMeta,
    /// The main record's `WARC-Record-ID`.
    pub warc_record_id: String,
    /// Records in `warc`.
    pub record_count: u32,
    /// The uncompressed WARC record group.
    pub warc: Bytes,
}

/// A sealed spool file: the fixed content of one segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSpool {
    /// The file.
    pub path: PathBuf,
    /// The segment id it will be published as.
    pub segment_id: SegmentId,
    /// Whether it goes to `ephemeral/`.
    pub ephemeral: bool,
}

impl SealedSpool {
    /// Reads every entry. A sealed file was fsynced before sealing, so any damage is an error.
    pub fn read_entries(&self) -> Result<Vec<SpoolEntry>> {
        let bytes = fs::read(&self.path)?;
        let (entries, good) = parse_frames(&bytes);
        if good != bytes.len() {
            return Err(Error::Storage(format!(
                "sealed spool {} is damaged at byte {good}",
                self.path.display()
            )));
        }
        entries
    }

    /// Deletes the file (after the segment commit) and fsyncs the directory.
    pub fn remove(&self) -> Result<()> {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        if let Some(dir) = self.path.parent() {
            sync_dir(dir)?;
        }
        Ok(())
    }
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Parses frames; returns the entries and the length of the valid prefix.
fn parse_frames(bytes: &[u8]) -> (Result<Vec<SpoolEntry>>, usize) {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let Some(h) = bytes.get(pos..pos + FRAME_HEADER) else {
            break;
        };
        if &h[0..4] != MAGIC || h[4] != FRAME_VERSION {
            break;
        }
        let u32_at = |i: usize| u32::from_le_bytes([h[i], h[i + 1], h[i + 2], h[i + 3]]);
        let meta_len = u32_at(5) as usize;
        let warc_len = u32_at(9) as usize;
        let crc = u32_at(13);
        let start = pos + FRAME_HEADER;
        let Some(end) = start
            .checked_add(meta_len)
            .and_then(|x| x.checked_add(warc_len))
        else {
            break;
        };
        let Some(body) = bytes.get(start..end) else {
            break;
        };
        if crc32c::crc32c(body) != crc {
            break;
        }
        let fm: FrameMeta = match serde_json::from_slice(&body[..meta_len]) {
            Ok(m) => m,
            Err(e) => return (Err(Error::Storage(format!("spool frame meta: {e}"))), pos),
        };
        out.push(SpoolEntry {
            meta: fm.meta,
            warc_record_id: fm.warc_record_id,
            record_count: fm.record_count,
            warc: Bytes::copy_from_slice(&body[meta_len..]),
        });
        pos = end;
    }
    (Ok(out), pos)
}

fn encode_frame(meta: &FrameMeta, warc: &[u8]) -> Result<Vec<u8>> {
    let mj = serde_json::to_vec(meta)?;
    let ml = u32::try_from(mj.len()).map_err(|_| Error::Invalid("spool meta too large".into()))?;
    let wl =
        u32::try_from(warc.len()).map_err(|_| Error::Invalid("spool record too large".into()))?;
    let mut crc = crc32c::crc32c(&mj);
    crc = crc32c::crc32c_append(crc, warc);
    let mut out = Vec::with_capacity(FRAME_HEADER + mj.len() + warc.len());
    out.extend_from_slice(MAGIC);
    out.push(FRAME_VERSION);
    out.extend_from_slice(&ml.to_le_bytes());
    out.extend_from_slice(&wl.to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(&mj);
    out.extend_from_slice(warc);
    Ok(out)
}

#[derive(Debug)]
struct OpenFile {
    /// `None` after a seal renamed the file but could not open its successor: the next append
    /// opens it, so nothing is ever appended to a sealed file.
    file: Option<File>,
    bytes: u64,
    first_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct State {
    archive: OpenFile,
    ephemeral: OpenFile,
}

/// The spool of one role instance. Cloning gives another handle to the same spool.
#[derive(Debug, Clone)]
pub struct Spool {
    dir: PathBuf,
    opts: SpoolOptions,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<State>>,
}

fn class_name(ephemeral: bool) -> &'static str {
    if ephemeral { "ephemeral" } else { "archive" }
}

impl Spool {
    /// Opens (or creates) the spool in `dir`, truncating torn tails of the open files.
    pub fn open(
        dir: impl Into<PathBuf>,
        opts: SpoolOptions,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let archive = Self::recover_open(&dir, false)?;
        let ephemeral = Self::recover_open(&dir, true)?;
        sync_dir(&dir)?;
        Ok(Spool {
            dir,
            opts,
            clock,
            state: Arc::new(Mutex::new(State { archive, ephemeral })),
        })
    }

    fn open_path(dir: &Path, ephemeral: bool) -> PathBuf {
        dir.join(format!("{}.open", class_name(ephemeral)))
    }

    fn recover_open(dir: &Path, ephemeral: bool) -> Result<OpenFile> {
        let path = Self::open_path(dir, ephemeral);
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (entries, good) = parse_frames(&bytes);
        let entries = entries?;
        if good != bytes.len() {
            tracing::warn!(
                file = %path.display(),
                kept_bytes = good,
                dropped_bytes = bytes.len() - good,
                "spool: truncating torn tail"
            );
            file.set_len(good as u64)?;
            file.sync_all()?;
        }
        Ok(OpenFile {
            file: Some(file),
            bytes: good as u64,
            first_at: entries.first().map(|e| e.meta.captured_at),
        })
    }

    /// The spool directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Appends synchronously (encode → write → fsync). Prefer [`CaptureSink::append`].
    pub fn append_blocking(&self, mut r: SpoolRecord) -> Result<CaptureId> {
        normalise(&mut r);
        let enc = warc::encode(&r)?;
        let ephemeral = r.meta.raw_expires_at.is_some();
        let frame = encode_frame(
            &FrameMeta {
                meta: r.meta.clone(),
                warc_record_id: enc.warc_record_id,
                record_count: enc.record_count,
            },
            &enc.bytes,
        )?;
        let mut st = self.lock();
        let of = if ephemeral {
            &mut st.ephemeral
        } else {
            &mut st.archive
        };
        if of.file.is_none() {
            of.file = Some(Self::create_open(&self.dir, ephemeral)?);
        }
        let Some(file) = of.file.as_mut() else {
            return Err(Error::Storage("spool: no open file".into()));
        };
        let written = (|| -> Result<()> {
            file.write_all(&frame)?;
            fail::fail_point!("spool::before_fsync", |_| Err(Error::Storage(
                "failpoint spool::before_fsync".into()
            )));
            file.sync_data()?;
            Ok(())
        })();
        if let Err(e) = written {
            // Not acknowledged: cut the partial frame so it can never be published.
            let _ = file.set_len(of.bytes);
            let _ = file.sync_data();
            return Err(e);
        }
        of.bytes += frame.len() as u64;
        of.first_at.get_or_insert(r.meta.captured_at);
        // The frame is durable from here on: the append is acknowledged whatever happens to
        // the seal (a full file is sealed again by the next `seal_due`).
        let full = of.bytes >= self.opts.max_bytes;
        if full && let Err(e) = self.seal_locked(&mut st, ephemeral) {
            tracing::warn!(error = %e, "spool: seal after append failed; retried by seal_due");
        }
        Ok(r.meta.capture_id)
    }

    fn create_open(dir: &Path, ephemeral: bool) -> Result<File> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(Self::open_path(dir, ephemeral))?;
        sync_dir(dir)?;
        Ok(file)
    }

    fn seal_locked(&self, st: &mut State, ephemeral: bool) -> Result<Option<SealedSpool>> {
        let of = if ephemeral {
            &mut st.ephemeral
        } else {
            &mut st.archive
        };
        if of.bytes == 0 {
            return Ok(None);
        }
        fail::fail_point!("spool::before_seal", |_| Err(Error::Storage(
            "failpoint spool::before_seal".into()
        )));
        if let Some(f) = &of.file {
            f.sync_all()?;
        }
        let segment_id = SegmentId::at(self.clock.now());
        let sealed = self
            .dir
            .join(format!("{}-{segment_id}.sealed", class_name(ephemeral)));
        let open = Self::open_path(&self.dir, ephemeral);
        fs::rename(&open, &sealed)?;
        // The old handle now names the sealed file: drop it before anything else can fail.
        *of = OpenFile {
            file: None,
            bytes: 0,
            first_at: None,
        };
        fail::fail_point!("spool::after_rename", |_| Err(Error::Storage(
            "failpoint spool::after_rename".into()
        )));
        of.file = Some(Self::create_open(&self.dir, ephemeral)?);
        Ok(Some(SealedSpool {
            path: sealed,
            segment_id,
            ephemeral,
        }))
    }

    /// Seals the open files that are full or whose first record is older than `max_age`.
    pub fn seal_due(&self) -> Result<Vec<SealedSpool>> {
        let now = self.clock.now();
        let mut st = self.lock();
        let mut out = Vec::new();
        for eph in [false, true] {
            let of = if eph { &st.ephemeral } else { &st.archive };
            let due = of.bytes >= self.opts.max_bytes
                || of.first_at.is_some_and(|t| now - t >= self.opts.max_age);
            if due && let Some(s) = self.seal_locked(&mut st, eph)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Seals both open files if they hold anything (shutdown, flush).
    pub fn seal_all(&self) -> Result<Vec<SealedSpool>> {
        let mut st = self.lock();
        let mut out = Vec::new();
        for eph in [false, true] {
            if let Some(s) = self.seal_locked(&mut st, eph)? {
                out.push(s);
            }
        }
        Ok(out)
    }

    /// Every sealed file on disk, oldest segment first (the replay set on start).
    pub fn sealed(&self) -> Result<Vec<SealedSpool>> {
        let mut out = Vec::new();
        for e in fs::read_dir(&self.dir)? {
            let e = e?;
            let name = e.file_name();
            let Some(name) = name.to_str() else { continue };
            let Some(stem) = name.strip_suffix(".sealed") else {
                continue;
            };
            let Some((class, id)) = stem.split_once('-') else {
                continue;
            };
            let ephemeral = match class {
                "archive" => false,
                "ephemeral" => true,
                _ => continue,
            };
            let Ok(segment_id) = id.parse::<SegmentId>() else {
                continue;
            };
            out.push(SealedSpool {
                path: e.path(),
                segment_id,
                ephemeral,
            });
        }
        out.sort_by_key(|s| s.segment_id);
        Ok(out)
    }

    /// Entries in the open (unsealed) files (diagnostics, tests).
    pub fn open_entries(&self) -> Result<Vec<SpoolEntry>> {
        let mut out = Vec::new();
        for eph in [false, true] {
            let bytes = fs::read(Self::open_path(&self.dir, eph))?;
            out.extend(parse_frames(&bytes).0?);
        }
        Ok(out)
    }

    /// Bytes on disk (open + sealed files).
    pub fn disk_bytes(&self) -> u64 {
        fs::read_dir(&self.dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter_map(|e| e.metadata().ok())
                    .filter(|m| m.is_file())
                    .map(|m| m.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// Whether the spool is over its backpressure threshold (fetchers must pause).
    pub fn over_backpressure(&self) -> bool {
        self.disk_bytes() > self.opts.backpressure_bytes
    }
}

/// Fills the payload hash and length from the body (r5 Q2: the digest is of the decoded body).
fn normalise(r: &mut SpoolRecord) {
    match &r.payload {
        SpoolPayload::Response(ex) => {
            r.meta.payload_hash = Some(warc::payload_hash(&ex.body));
            r.meta.payload_len = ex.body.len() as u64;
        }
        SpoolPayload::Resource { body, .. } => {
            r.meta.payload_hash = Some(warc::payload_hash(body));
            r.meta.payload_len = body.len() as u64;
        }
        SpoolPayload::Revisit(_) | SpoolPayload::Metadata { .. } => {}
    }
}

#[async_trait]
impl CaptureSink for Spool {
    async fn append(&mut self, r: SpoolRecord) -> Result<CaptureId> {
        let me = self.clone();
        tokio::task::spawn_blocking(move || me.append_blocking(r))
            .await
            .map_err(|e| Error::Storage(format!("spool append task: {e}")))?
    }
}
