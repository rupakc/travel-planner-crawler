//! The raw-first oracle (spec §5.5, §17.2; CLAUDE.md rule 7): nothing derived from a capture
//! may be visible unless the capture is committed and its bytes are in the blob store.
//!
//! Checks, over the whole database:
//!
//! 1. every `record_versions` row's `(capture_id, captured_at)` is a committed capture;
//! 2. every `observations` row's capture is committed;
//! 3. every capture's segment has a `segments` row;
//! 4. every live (not purged) segment's `.warc.zst` and `.cdxj` exist in the blob store;
//! 5. every `url_state.last_capture_id` is a committed capture, and validators (etag,
//!    last_modified, payload hash) are never set without one (no 304 without a stored body).

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use vc_core::store::BlobStore;

/// One raw-first violation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RawFirstViolation {
    /// Which check failed (`version_without_capture`, `observation_without_capture`,
    /// `capture_without_segment`, `segment_blob_missing`, `cdxj_blob_missing`,
    /// `url_state_capture_missing`, `validators_without_capture`).
    pub check: String,
    /// What it is about (ids, paths).
    pub subject: String,
}

impl std::fmt::Display for RawFirstViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.check, self.subject)
    }
}

fn v(check: &str, subject: String) -> RawFirstViolation {
    RawFirstViolation {
        check: check.to_string(),
        subject,
    }
}

/// Every raw-first violation (empty = OK). Database errors are returned as `Err`.
pub async fn check_raw_first(
    pool: &PgPool,
    blobs: &dyn BlobStore,
) -> Result<Vec<RawFirstViolation>, sqlx::Error> {
    let mut out = Vec::new();

    let rows: Vec<(i64, i32, String)> = sqlx::query_as(
        "SELECT rv.record_id, rv.version, rv.capture_id FROM record_versions rv
         WHERE NOT EXISTS (SELECT 1 FROM captures c
                           WHERE c.capture_id = rv.capture_id AND c.captured_at = rv.captured_at)
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?;
    for (rid, ver, cap) in rows {
        out.push(v(
            "version_without_capture",
            format!("record {rid} v{ver} -> capture {cap}"),
        ));
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT o.series_key, o.capture_id FROM observations o
         WHERE NOT EXISTS (SELECT 1 FROM captures c
                           WHERE c.capture_id = o.capture_id AND c.captured_at = o.captured_at)
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?;
    for (series, cap) in rows {
        out.push(v(
            "observation_without_capture",
            format!("{series} -> capture {cap}"),
        ));
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT c.capture_id, c.segment_id FROM captures c
         WHERE NOT EXISTS (SELECT 1 FROM segments s WHERE s.segment_id = c.segment_id)
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    for (cap, seg) in rows {
        out.push(v(
            "capture_without_segment",
            format!("capture {cap} -> segment {seg}"),
        ));
    }

    let segs: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT segment_id, path, cdxj_path FROM segments WHERE purged_at IS NULL ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    if !segs.is_empty() {
        let present: BTreeSet<String> = blobs
            .list("")
            .await
            .map_err(|e| sqlx::Error::Protocol(format!("blob store list failed: {e}")))?
            .into_iter()
            .collect();
        for (id, path, cdxj) in segs {
            if !present.contains(&path) {
                out.push(v("segment_blob_missing", format!("segment {id}: {path}")));
            }
            if !present.contains(&cdxj) {
                out.push(v("cdxj_blob_missing", format!("segment {id}: {cdxj}")));
            }
        }
    }

    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT encode(us.url_key, 'hex'), us.last_capture_id FROM url_state us
         WHERE us.last_capture_id IS NOT NULL
           AND NOT EXISTS (SELECT 1 FROM captures c WHERE c.capture_id = us.last_capture_id)
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    for (k, cap) in rows {
        out.push(v(
            "url_state_capture_missing",
            format!("url_key {k} -> capture {cap}"),
        ));
    }

    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT encode(url_key, 'hex') FROM url_state
         WHERE last_capture_id IS NULL
           AND (etag IS NOT NULL OR last_modified IS NOT NULL OR payload_hash IS NOT NULL)
         ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    for (k,) in rows {
        out.push(v(
            "validators_without_capture",
            format!("url_key {k} has validators but no committed capture"),
        ));
    }

    Ok(out)
}

/// Panics with a readable report if anything is visible ahead of its raw capture.
pub async fn assert_raw_first(pool: &PgPool, blobs: &dyn BlobStore) {
    let v = check_raw_first(pool, blobs)
        .await
        .expect("raw-first oracle: database query failed");
    if !v.is_empty() {
        let mut msg = format!("raw-first oracle: {} violation(s)\n", v.len());
        for x in v.iter().take(50) {
            msg.push_str(&format!("  {x}\n"));
        }
        panic!("{msg}");
    }
}
