//! `ClaimPermit::deadline_handle` shares the permit's deadline and pacing state.
#![allow(clippy::disallowed_methods)] // the test plays the frontier: it mints and extends

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{TimeZone, Utc};
use vc_core::frontier::{ClaimPermit, PermitScope};
use vc_core::{DbTime, Epoch, PolitenessKey};

fn permit(deadline_s: i64) -> ClaimPermit {
    let key = PolitenessKey("example.com".into());
    ClaimPermit::mint(
        key.clone(),
        Epoch(7),
        DbTime(Utc.timestamp_opt(deadline_s, 0).unwrap()),
        Arc::new(PermitScope::new(key)),
        Duration::from_secs(5),
    )
}

#[test]
fn deadline_handle_sees_later_extension() {
    let p = permit(1_000);
    let h = p.deadline_handle();
    let h2 = h.clone();
    assert_eq!(h.deadline(), p.deadline());
    p.extend_to(DbTime(Utc.timestamp_opt(1_600, 0).unwrap()));
    assert_eq!(h.deadline().0.timestamp(), 1_600);
    assert_eq!(h2.deadline().0.timestamp(), 1_600);
    let now = Utc.timestamp_opt(1_500, 0).unwrap();
    assert_eq!(h.remaining(now), Duration::from_secs(100));
    assert_eq!(
        h.clamp_timeout(Duration::from_secs(30), now),
        Duration::from_secs(30)
    );
    assert_eq!(
        h.remaining(Utc.timestamp_opt(2_000, 0).unwrap()),
        Duration::ZERO
    );
}

#[test]
fn deadline_handle_records_body_end_without_counting_a_request() {
    let p = permit(1_000);
    let h = p.deadline_handle();
    let t0 = Instant::now();
    p.record_request_end(t0);
    assert_eq!(p.requests_made(), 1);
    let t1 = t0 + Duration::from_secs(3);
    h.record_request_end(t1);
    assert_eq!(p.last_request_end(), Some(t1));
    assert_eq!(h.last_request_end(), Some(t1));
    assert_eq!(p.requests_made(), 1, "a body end is not a new request");
    // Never moves backwards.
    h.record_request_end(t0);
    assert_eq!(p.last_request_end(), Some(t1));
}

/// `raise_delay` only ever raises the per-request delay of a live permit.
#[test]
fn raise_delay_only_increases() {
    let p = permit(1_000);
    assert_eq!(p.delay(), Duration::from_secs(5));
    p.raise_delay(Duration::from_secs(2));
    assert_eq!(p.delay(), Duration::from_secs(5), "never lowered");
    p.raise_delay(Duration::from_millis(12_500));
    assert_eq!(p.delay(), Duration::from_millis(12_500));
}
