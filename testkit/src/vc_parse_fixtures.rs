//! Fixtures for `vc-parse` tests (owned by the vc-parse task; add items here, never edit lib.rs).

use chrono::{DateTime, TimeZone, Utc};
use vc_core::frontier::Validators;
use vc_core::ids::{CaptureId, Epoch};
use vc_core::media::{FetchMode, MediaType};
use vc_core::store::{CaptureMeta, CaptureRole};
use vc_core::url::CanonicalUrl;
use vc_core::usepolicy::{UsePolicy, UseSignals};

/// A fixed capture time for parser fixtures.
pub fn parse_fixture_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
}

/// A primary HTTP capture of `url` with media type `media` (everything else neutral).
pub fn capture_meta(url: &str, media: MediaType) -> CaptureMeta {
    let url = CanonicalUrl::parse(url).expect("fixture url");
    let at = parse_fixture_time();
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
