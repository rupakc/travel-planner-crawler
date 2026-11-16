//! Shared builders for the operations tests (admin, maintenance, migrate).
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::PathBuf;

use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use sqlx::PgPool;
use testkit::MemBlobStore;
use vc_core::media::{FetchMode, MediaType};
use vc_core::store::{CaptureMeta, CaptureRole, SegmentCommitter, SegmentManifest};
use vc_core::usepolicy::UsePolicy;
use vc_core::{CanonicalUrl, CaptureId, Epoch, SegmentId, SourceId};
use vc_store::{PgSegmentCommitter, SpoolEntry, build_segment};

pub fn at(day: u32, hour: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, day, hour, 0, 0)
        .single()
        .expect("time")
}

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testkit/fixtures/vcrawl/ops")
}

pub fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A primary 200 capture of `url` at `t`.
pub fn meta(url: &str, t: DateTime<Utc>, source: Option<&str>) -> CaptureMeta {
    let u = CanonicalUrl::parse(url).expect("url");
    CaptureMeta {
        capture_id: CaptureId::at(t),
        captured_at: t,
        epoch: Epoch(7),
        orphan_epoch: false,
        url_key: u.key(),
        key: u.politeness_key(),
        url: u,
        source_id: source.map(|s| SourceId(s.into())),
        role: CaptureRole::Primary,
        fetch_mode: FetchMode::Http,
        status: Some(200),
        media_type: MediaType::Html,
        payload_hash: None,
        payload_len: 3,
        validators: Default::default(),
        revisit_of: None,
        use_signals: Default::default(),
        use_policy_fetch: UsePolicy::default(),
        raw_expires_at: None,
        next_fetch_at: None,
    }
}

pub fn entry(m: CaptureMeta, body: &str) -> SpoolEntry {
    let warc = format!(
        "WARC/1.1\r\nWARC-Type: resource\r\nWARC-Target-URI: {}\r\nContent-Length: {}\r\n\r\n{}\r\n\r\n",
        m.url,
        body.len(),
        body
    );
    SpoolEntry {
        warc_record_id: format!("<urn:uuid:{}>", m.capture_id),
        record_count: 1,
        warc: Bytes::from(warc),
        meta: m,
    }
}

/// Builds, uploads (segment + CDXJ) and commits one segment.
pub async fn commit_segment(
    pool: &PgPool,
    store: &MemBlobStore,
    id: SegmentId,
    ephemeral: bool,
    entries: &[SpoolEntry],
) -> SegmentManifest {
    let b = build_segment(id, ephemeral, entries, 3).expect("build");
    vc_store::segment::upload(store, &b.manifest.path, b.bytes.clone())
        .await
        .expect("upload segment");
    vc_store::segment::upload(store, &b.manifest.cdxj_path, b.cdxj.clone())
        .await
        .expect("upload cdxj");
    PgSegmentCommitter::new(pool.clone())
        .commit(&b.manifest)
        .await
        .expect("commit");
    b.manifest
}

/// Rewrites a committed segment without `remove` (commit only; old objects kept unless
/// `finish`).
pub async fn purge_from_segment(
    pool: &PgPool,
    store: &MemBlobStore,
    m: &SegmentManifest,
    remove: &[CaptureId],
    finish: bool,
) -> vc_store::SegmentRewrite {
    let set: HashSet<CaptureId> = remove.iter().copied().collect();
    let rw = vc_store::rewrite_segment_without(store, &m.path, &m.cdxj_path, &set)
        .await
        .expect("rewrite");
    assert!(
        PgSegmentCommitter::new(pool.clone())
            .commit_rewrite(&rw)
            .await
            .expect("commit rewrite")
    );
    if finish {
        vc_store::finish_rewrite(store, &rw).await.expect("finish");
    }
    rw
}

/// Every `segments` row (without the commit/rewrite timestamps) and every `captures` row, as
/// JSON, in a stable order.
pub async fn raw_rows(pool: &PgPool) -> (serde_json::Value, serde_json::Value) {
    let segs: serde_json::Value = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(to_jsonb(s) - 'committed_at' - 'rewritten_at'
                    || jsonb_build_object('rewritten', s.rewritten_at IS NOT NULL)
                    ORDER BY s.segment_id), '[]') FROM segments s",
    )
    .fetch_one(pool)
    .await
    .expect("segments");
    let caps: serde_json::Value = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(to_jsonb(c) ORDER BY c.capture_id), '[]') FROM captures c",
    )
    .fetch_one(pool)
    .await
    .expect("captures");
    (segs, caps)
}

pub async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .expect(sql)
}
