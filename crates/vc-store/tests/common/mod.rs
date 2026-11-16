//! Shared builders for vc-store tests.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use http::{HeaderMap, HeaderValue, Method, StatusCode};
use vc_core::fetch::{Exchange, RequestPurpose};
use vc_core::media::{FetchMode, MediaType};
use vc_core::store::{
    CaptureMeta, CaptureRole, SegmentCommitter, SegmentManifest, SpoolPayload, SpoolRecord,
};
use vc_core::usepolicy::UsePolicy;
use vc_core::{CanonicalUrl, CaptureId, Epoch, Result, SegmentId};

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0)
        .single()
        .expect("time")
}

pub fn url(s: &str) -> CanonicalUrl {
    CanonicalUrl::parse(s).expect("url")
}

pub fn exchange(u: &CanonicalUrl, status: u16, body: &[u8]) -> Exchange {
    let mut req = HeaderMap::new();
    req.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static(vc_core::USER_AGENT),
    );
    let mut resp = HeaderMap::new();
    resp.insert(
        http::header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    resp.insert(http::header::ETAG, HeaderValue::from_static("\"v1\""));
    resp.insert(
        http::header::CONTENT_ENCODING,
        HeaderValue::from_static("gzip"),
    );
    resp.insert(http::header::CONTENT_LENGTH, HeaderValue::from_static("12"));
    Exchange {
        url: u.clone(),
        redirects: vec![],
        method: Method::GET,
        request_headers: req,
        status: StatusCode::from_u16(status).expect("status"),
        response_headers: resp,
        body: Bytes::copy_from_slice(body),
        original_encoding: Some("gzip".into()),
        truncated: false,
        peer_ip: Some("93.184.216.34".parse().expect("ip")),
        started_at: t0(),
        ended_at: t0(),
        purpose: RequestPurpose::Page,
    }
}

pub fn meta(u: &CanonicalUrl, at: DateTime<Utc>, epoch: i64) -> CaptureMeta {
    CaptureMeta {
        capture_id: CaptureId::at(at),
        captured_at: at,
        epoch: Epoch(epoch),
        orphan_epoch: false,
        url_key: u.key(),
        url: u.clone(),
        key: u.politeness_key(),
        source_id: None,
        role: CaptureRole::Primary,
        fetch_mode: FetchMode::Http,
        status: Some(200),
        media_type: MediaType::Html,
        payload_hash: None,
        payload_len: 0,
        validators: Default::default(),
        revisit_of: None,
        use_signals: Default::default(),
        use_policy_fetch: UsePolicy::default(),
        raw_expires_at: None,
        next_fetch_at: None,
    }
}

/// A primary 200 response capture.
pub fn response(u: &str, at: DateTime<Utc>, body: &[u8]) -> SpoolRecord {
    let u = url(u);
    let ex = exchange(&u, 200, body);
    let mut m = meta(&u, at, 1);
    m.validators = ex.validators();
    SpoolRecord {
        meta: m,
        payload: SpoolPayload::Response(ex),
    }
}

/// A committer that records manifests, idempotent by segment id (like `vc_commit_segment`).
#[derive(Debug, Default, Clone)]
pub struct RecordingCommitter {
    pub committed: Arc<Mutex<BTreeMap<SegmentId, SegmentManifest>>>,
    pub calls: Arc<Mutex<u32>>,
}

impl RecordingCommitter {
    pub fn manifests(&self) -> Vec<SegmentManifest> {
        self.committed
            .lock()
            .expect("lock")
            .values()
            .cloned()
            .collect()
    }
    pub fn capture_ids(&self) -> Vec<CaptureId> {
        let mut v: Vec<_> = self
            .manifests()
            .iter()
            .flat_map(|m| m.captures.iter().map(|c| c.meta.capture_id))
            .collect();
        v.sort();
        v
    }
}

#[async_trait]
impl SegmentCommitter for RecordingCommitter {
    async fn commit(&self, m: &SegmentManifest) -> Result<()> {
        *self.calls.lock().expect("lock") += 1;
        self.committed
            .lock()
            .expect("lock")
            .entry(m.segment_id)
            .or_insert_with(|| m.clone());
        Ok(())
    }
}
