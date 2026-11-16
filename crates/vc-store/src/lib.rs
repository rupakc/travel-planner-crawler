//! # vc-store
//!
//! The raw store (spec §5.5, §17.2, §17.7): WARC/1.1 writer/reader + CDXJ, the fsynced
//! [`Spool`], the [`SegmentRoller`], [`GcsBlobStore`] / [`FsBlobStore`], [`PgSegmentCommitter`]
//! (one `vc_commit_segment` call) and the purge rewrite. Migration block 12xx.
//!
//! ## Raw first, crash-consistent
//!
//! 1. [`vc_core::store::CaptureSink::append`] writes a spool frame and **fsyncs** before it
//!    returns; an unacknowledged (torn) frame is truncated on the next open and never published.
//! 2. A sealed spool file fixes a segment's id and content. [`SegmentRoller::publish`] builds the
//!    segment deterministically, uploads it with `ifGenerationMatch=0` (an identical existing
//!    object is success, a different one is a conflict), then the CDXJ.
//! 3. One `vc_commit_segment(jsonb)` transaction makes the captures visible and moves the
//!    `url_state` validators (monotonic on `captured_at`).
//! 4. Only then is the sealed spool file deleted. On start, [`SegmentRoller::tick`] replays every
//!    sealed file; steps 2–3 are idempotent by segment id, and with a [`CommitProbe`] a file
//!    whose segment is already committed is deleted without uploading anything (a purge may
//!    have rewritten the segment since). A file that fails on its own (damaged, conflicting
//!    object) is skipped and quarantined after [`segment::QUARANTINE_AFTER`] tries.
//!
//! Until step 3 nothing derived from a capture is visible and the frontier keeps the **old**
//! validators, so a crash can never produce a 304 for a body that is not stored.
//!
//! ## Failpoints
//!
//! With the `failpoints` feature (on in this crate's tests): `spool::before_fsync`,
//! `spool::before_seal`, `spool::after_rename`, `segment::before_upload`, `segment::after_upload`,
//! `segment::before_commit`, `segment::after_commit`.

pub mod cdxj;
pub mod commit;
pub mod fs;
pub mod gcs;
pub mod paths;
pub mod purge;
pub mod segment;
pub mod spool;
pub mod text;
pub mod warc;

use std::sync::Arc;

use vc_core::Result;
use vc_core::store::BlobStore;

pub use crate::commit::PgSegmentCommitter;
pub use crate::fs::FsBlobStore;
pub use crate::gcs::{GcsBlobStore, GcsOptions};
pub use crate::purge::{SegmentRewrite, finish_rewrite, rewrite_segment_without};
pub use crate::segment::{
    BuiltSegment, CommitProbe, SegmentRoller, build_segment, read_capture, read_cdxj, read_segment,
};
pub use crate::spool::{SealedSpool, Spool, SpoolEntry, SpoolOptions};

/// Builds the configured raw-bucket blob store (`[store] backend`).
pub async fn blob_store_from_config(cfg: &vc_config::StoreConfig) -> Result<Arc<dyn BlobStore>> {
    Ok(match cfg.backend {
        vc_config::BlobBackend::Fs => Arc::new(FsBlobStore::new(&cfg.fs_root)),
        vc_config::BlobBackend::Gcs => {
            let opts = GcsOptions {
                endpoint: (!cfg.gcs_endpoint.is_empty()).then(|| cfg.gcs_endpoint.clone()),
                anonymous: cfg.gcs_anonymous,
            };
            Arc::new(GcsBlobStore::new(&cfg.raw_bucket, opts).await?)
        }
    })
}
