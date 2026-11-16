//! Shared helpers for the vc-parse fuzz targets.

use std::time::Duration;

use vc_core::frontier::Validators;
use vc_core::ids::{CaptureId, Epoch};
use vc_core::media::{FetchMode, MediaType};
use vc_core::parse::{Limits, Parsed};
use vc_core::store::{CaptureMeta, CaptureRole};
use vc_core::url::CanonicalUrl;
use vc_core::usepolicy::{UsePolicy, UseSignals};

/// A primary capture of a fixed URL with media type `media`.
pub fn meta(media: MediaType) -> CaptureMeta {
    let url = CanonicalUrl::parse("https://fuzz.example.com/dir/page").expect("static url");
    let at = chrono_epoch();
    CaptureMeta {
        capture_id: CaptureId::at(at),
        captured_at: at,
        epoch: Epoch(1),
        orphan_epoch: false,
        url_key: url.key(),
        key: url.politeness_key(),
        url,
        source_id: None,
        role: CaptureRole::Primary,
        fetch_mode: FetchMode::Http,
        status: Some(200),
        media_type: media,
        payload_hash: None,
        payload_len: 0,
        validators: Validators::default(),
        revisit_of: None,
        use_signals: UseSignals::default(),
        use_policy_fetch: UsePolicy::ALLOW_ALL,
        raw_expires_at: None,
        next_fetch_at: None,
    }
}

fn chrono_epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_794_000_000, 0).expect("static time")
}

/// Limits for fuzzing: defaults, with a CPU budget that turns pathological inputs into errors
/// instead of libFuzzer timeouts only when the parser is genuinely slow.
pub fn limits() -> Limits {
    Limits {
        cpu_time: Duration::from_secs(2),
        ..Limits::default()
    }
}

/// Invariants every successful parse must hold.
pub fn check(p: &Parsed, lim: &Limits) {
    assert!(p.links.len() <= lim.max_links.max(lim.max_sitemap_entries));
    assert!(p.main_text.as_ref().is_none_or(|t| t.len() <= lim.max_text_bytes));
    for l in &p.links {
        assert!(l.anchor.chars().count() <= 200);
        let s = l.url.as_str();
        assert!(s.starts_with("http://") || s.starts_with("https://"));
    }
}
