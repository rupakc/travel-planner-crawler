//! # vc-store (Task 3, stub)
//!
//! WARC/1.1 writer/reader + CDXJ, the fsynced `Spool`, `SegmentRoller`, `GcsBlobStore` /
//! `FsBlobStore`, and `PgSegmentCommitter` (one `vc_commit_segment` call). Block 12xx.

use std::ops::Range;

use async_trait::async_trait;
use bytes::Bytes;
use sqlx::PgPool;
use vc_core::store::{
    BlobStore, CaptureSink, PutOutcome, SegmentCommitter, SegmentManifest, SpoolRecord,
};
use vc_core::{CaptureId, Result};

/// Local-filesystem blob store.
#[derive(Debug, Clone)]
pub struct FsBlobStore {
    /// Root directory.
    pub root: std::path::PathBuf,
}

#[async_trait]
impl BlobStore for FsBlobStore {
    async fn put_if_absent(&self, _path: &str, _bytes: Bytes, _crc32c: u32) -> Result<PutOutcome> {
        todo!("Task 3")
    }
    async fn get(&self, _path: &str, _range: Option<Range<u64>>) -> Result<Bytes> {
        todo!("Task 3")
    }
    async fn delete(&self, _path: &str) -> Result<()> {
        todo!("Task 3")
    }
    async fn list(&self, _prefix: &str) -> Result<Vec<String>> {
        todo!("Task 3")
    }
}

/// The fsynced local spool of one role instance.
#[derive(Debug)]
pub struct Spool {
    /// Spool directory (`spool/<role>-<instance>/`).
    pub dir: std::path::PathBuf,
}

#[async_trait]
impl CaptureSink for Spool {
    async fn append(&mut self, _r: SpoolRecord) -> Result<CaptureId> {
        todo!("Task 3")
    }
}

/// Commits segments through `vc_commit_segment(jsonb)`.
#[derive(Debug, Clone)]
pub struct PgSegmentCommitter {
    /// Pool.
    pub pool: PgPool,
}

#[async_trait]
impl SegmentCommitter for PgSegmentCommitter {
    async fn commit(&self, _m: &SegmentManifest) -> Result<()> {
        todo!("Task 3")
    }
}
