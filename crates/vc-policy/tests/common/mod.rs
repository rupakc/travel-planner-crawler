//! Shared helpers for the vc-policy integration tests.
#![allow(dead_code)]

use bytes::Bytes;
use chrono::{DateTime, Duration, TimeZone, Utc};
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use testkit::vc_policy_fixtures as fx;
use vc_config::PolicyConfig;
use vc_core::CanonicalUrl;
use vc_core::fetch::{Exchange, RequestPurpose};
use vc_core::policy::{RobotsEntry, RobotsStatus};
use vc_policy::VoyagerPolicy;

/// The policy built from the repository's real policy files.
pub fn policy() -> VoyagerPolicy {
    let config = PolicyConfig {
        denylist_path: fx::denylist_path().display().to_string(),
        subresource_allowlist_path: fx::subresource_allowlist_path().display().to_string(),
        ..PolicyConfig::default()
    };
    VoyagerPolicy::new(config).expect("policy files load")
}

pub fn url(s: &str) -> CanonicalUrl {
    CanonicalUrl::parse(s).expect("valid url")
}

pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
}

/// A robots entry for `https://example.com`.
pub fn entry(status: RobotsStatus, http_status: Option<u16>, body: Option<&str>) -> RobotsEntry {
    RobotsEntry {
        origin: url("https://example.com/").origin(),
        status,
        http_status,
        body: body.map(|b| Bytes::copy_from_slice(b.as_bytes())),
        last_good: None,
        fetched_at: t0(),
        expires_at: t0() + Duration::hours(24),
    }
}

pub fn ok(body: &str) -> RobotsEntry {
    entry(RobotsStatus::Ok, Some(200), Some(body))
}

/// An exchange with the given status, headers and body.
pub fn exchange(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Exchange {
    let mut h = HeaderMap::new();
    for (k, v) in headers {
        h.append(
            HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    Exchange {
        url: url("https://example.com/page"),
        redirects: vec![],
        method: Method::GET,
        request_headers: HeaderMap::new(),
        status: StatusCode::from_u16(status).unwrap(),
        response_headers: h,
        body: Bytes::copy_from_slice(body),
        original_encoding: None,
        truncated: false,
        peer_ip: None,
        started_at: t0(),
        ended_at: t0(),
        purpose: RequestPurpose::Page,
    }
}
