//! The assertions as pure functions over what the stack recorded (request logs, rows, API
//! answers), so each one is unit-tested against hand-made evidence, including the vacuous
//! cases (nothing crawled must never pass).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use testkit::fake_web::sites;
use testkit::oracle::{PolicySnapshot, check_polite};
use testkit::request_log::{RequestLog, RequestLogEntry};
use url::Url;

use crate::Outcome;

/// Politeness keys every E2E source crawls: each must see at least one page request.
pub const CRAWLED_KEYS: [&str; 6] = [
    "blog.test",
    "attractions.test",
    "events.test",
    "hostile.test",
    "slow.test",
    "immigration.test",
];

fn path_of(url: &str) -> String {
    Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_default()
}

fn host_of(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        .unwrap_or_default()
}

fn is_robots(e: &RequestLogEntry) -> bool {
    path_of(&e.url) == "/robots.txt"
}

fn requested(log: &RequestLog, host: &str, path: &str) -> Vec<RequestLogEntry> {
    log.entries()
        .into_iter()
        .filter(|e| host_of(&e.url) == host && path_of(&e.url) == path)
        .collect()
}

/// `polite`: the testkit politeness oracle finds nothing, and the crawl reached every key
/// (an empty log would otherwise pass).
pub fn polite(log: &RequestLog, snap: &PolicySnapshot) -> Outcome {
    let mut problems: Vec<String> = check_polite(log, snap)
        .iter()
        .map(ToString::to_string)
        .collect();
    let by_key = log.by_key();
    for k in CRAWLED_KEYS {
        let pages = by_key
            .get(k)
            .map(|v| v.iter().filter(|e| !is_robots(e)).count())
            .unwrap_or(0);
        if pages == 0 {
            problems.push(format!(
                "no page request to {k}: the crawl never reached it"
            ));
        }
    }
    Outcome::from_problems(
        format!(
            "{} requests over {} keys, no politeness violation",
            log.len(),
            by_key.len()
        ),
        problems,
    )
}

/// The politeness key's park state, as read from `politeness_keys`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkRow {
    /// `parked_until`.
    pub parked_until: Option<DateTime<Utc>>,
    /// `park_reason` (serialised `ParkReason`).
    pub park_reason: Option<String>,
}

/// `challenge_parked`: the hostile host answered a challenge, the key is parked for a
/// challenge, and nothing at all was requested from it after the first challenge ended.
pub fn challenge_parked(log: &RequestLog, park: Option<&ParkRow>, now: DateTime<Utc>) -> Outcome {
    let key = sites::HOSTILE;
    let entries = log.by_key().remove(key).unwrap_or_default();
    let mut problems = Vec::new();
    let first = entries.iter().find(|e| !is_robots(e) && e.status == 403);
    match first {
        None => problems.push(format!(
            "{key} never answered a challenge to the crawler (no page was requested)"
        )),
        Some(c) => {
            for e in entries.iter().filter(|e| e.started_at >= c.ended_at) {
                problems.push(format!(
                    "request after the challenge at {}: {} {} at {}",
                    c.ended_at, e.status, e.url, e.started_at
                ));
            }
        }
    }
    match park {
        None => problems.push(format!("{key} has no politeness_keys row")),
        Some(p) => {
            if !p.parked_until.is_some_and(|t| t > now) {
                problems.push(format!(
                    "{key} is not parked (parked_until {:?}, now {now})",
                    p.parked_until
                ));
            }
            // The challenge answers 403: vcrawl parks a detected challenge as `Challenge`.
            // Anything else (unauthorized, server errors, manual, robots) means the page was
            // not recognised as the cause.
            let reason = p
                .park_reason
                .clone()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if !reason.contains("challenge") {
                problems.push(format!(
                    "{key} park reason is {reason:?}, expected the challenge"
                ));
            }
        }
    }
    Outcome::from_problems(
        format!(
            "{key} parked ({}), {} request(s) in all, none after the challenge",
            park.and_then(|p| p.park_reason.clone()).unwrap_or_default(),
            entries.len()
        ),
        problems,
    )
}

/// The marker only the script-built DOM contains: the shell HTML has no attraction names,
/// the JSON has no markup, and `app.js` builds the `<h2>` from data.
pub const RENDERED_MARKER: &str = "<h2>Fushimi Inari Taisha</h2>";

/// `spa_rendered`: a primary `render` capture of the SPA holds the script-built DOM; the
/// fetcher served the page's script and its XHR; images were never fetched.
///
/// `render_bodies`: `(url, stored WARC bytes of that capture)` for every primary capture of
/// the SPA with `fetch_mode = 'render'`.
pub fn spa_rendered(log: &RequestLog, render_bodies: &[(String, Vec<u8>)]) -> Outcome {
    let mut problems = Vec::new();
    if render_bodies.is_empty() {
        problems.push(format!("no primary render capture of {}", sites::SPA));
    } else if !render_bodies
        .iter()
        .any(|(_, b)| contains(b, RENDERED_MARKER.as_bytes()))
    {
        problems.push(format!(
            "{} render capture(s) of {} but none holds the script-built DOM ({RENDERED_MARKER})",
            render_bodies.len(),
            sites::SPA
        ));
    }
    if !requested(log, sites::CDN, "/js/app.js")
        .iter()
        .any(|e| e.status == 200)
    {
        problems.push("the SPA's script http://cdn.test/js/app.js was never served".into());
    }
    if !requested(log, sites::SPA, "/api/attractions.json")
        .iter()
        .any(|e| e.status == 200)
    {
        problems.push("the SPA's XHR /api/attractions.json was never served".into());
    }
    for e in requested(log, sites::CDN, "/img/logo.png") {
        problems.push(format!(
            "an image was fetched for a render ({} at {}); images are always blocked",
            e.url, e.started_at
        ));
    }
    Outcome::from_problems(
        format!(
            "{} render capture(s) of {}, script-built DOM stored",
            render_bodies.len(),
            sites::SPA
        ),
        problems,
    )
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

/// `private_ip_blocked`: the traps on [`sites::PRIVATE_IP`] and [`sites::METADATA_IP`]
/// (both reachable from the fetcher's networks) saw no request at all, while the crawl did
/// reach what tries them: the SPA script's `fetch`es of both and the blog's redirect to the
/// private IP.
///
/// `traps`: `(ip, that trap's request log)`.
pub fn private_ip_blocked(log: &RequestLog, traps: &[(&str, RequestLog)]) -> Outcome {
    let mut problems = Vec::new();
    for (ip, trap) in traps {
        for e in trap.entries() {
            problems.push(format!(
                "{ip} was requested: {} (UA {:?}) at {}",
                e.url, e.ua, e.started_at
            ));
        }
    }
    for ip in [sites::PRIVATE_IP, sites::METADATA_IP] {
        if !traps.iter().any(|(t, _)| *t == ip) {
            problems.push(format!(
                "no trap log for {ip}: the check would prove nothing"
            ));
        }
    }
    let script = requested(log, sites::CDN, "/js/app.js")
        .iter()
        .any(|e| e.status == 200);
    let redirect = !requested(log, sites::BLOG, "/go/internal").is_empty();
    if !script && !redirect {
        problems.push(
            "neither the SPA script nor the blog's /go/internal redirect was fetched: nothing tried the traps"
                .into(),
        );
    }
    let tried: Vec<&str> = [(script, "SPA script fetches"), (redirect, "redirect")]
        .iter()
        .filter(|(b, _)| *b)
        .map(|(_, s)| *s)
        .collect();
    let ips: Vec<&str> = traps.iter().map(|(ip, _)| *ip).collect();
    Outcome::from_problems(
        format!(
            "traps at {} saw nothing; exercised by: {}",
            ips.join(", "),
            tried.join(", ")
        ),
        problems,
    )
}

/// One `record_versions` row joined to its capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionRow {
    /// `records.record_id`.
    pub record_id: i64,
    /// The capture's URL.
    pub url: String,
    /// `record_versions.version`.
    pub version: i32,
    /// `record_versions.data_hash`.
    pub data_hash: Vec<u8>,
    /// `record_versions.capture_id`.
    pub capture_id: String,
}

/// Pages whose content changes in cycle 1 (`SiteOptions::content_change_cycle`).
pub const CHANGING_PAGES: [&str; 2] = [
    "http://blog.test/posts/hello",
    "http://events.test/events/jazz-night",
];

/// Whether every page of [`CHANGING_PAGES`] was served (200) at or after `since`: the end of
/// a crawl cycle.
pub fn changing_pages_fetched(log: &RequestLog, since: DateTime<Utc>) -> bool {
    CHANGING_PAGES.iter().all(|page| {
        log.entries()
            .iter()
            .any(|e| e.url == *page && e.status == 200 && e.started_at >= since)
    })
}

/// `version_bump`: a page that changed in cycle 1 got a new record version: ≥ 2 versions of
/// one record with different data hashes, each from its own capture.
pub fn version_bump(rows: &[VersionRow]) -> Outcome {
    let mut by_record: BTreeMap<i64, Vec<&VersionRow>> = BTreeMap::new();
    for r in rows {
        by_record.entry(r.record_id).or_default().push(r);
    }
    let mut bumped = Vec::new();
    let mut problems = Vec::new();
    for (id, vs) in &by_record {
        let hashes: BTreeSet<&[u8]> = vs.iter().map(|v| v.data_hash.as_slice()).collect();
        let caps: BTreeSet<&str> = vs.iter().map(|v| v.capture_id.as_str()).collect();
        let versions: BTreeSet<i32> = vs.iter().map(|v| v.version).collect();
        if versions.len() >= 2 {
            if hashes.len() < versions.len() {
                problems.push(format!(
                    "record {id} ({}) has {} versions but only {} distinct data hashes",
                    vs[0].url,
                    versions.len(),
                    hashes.len()
                ));
            } else if caps.len() < versions.len() {
                problems.push(format!(
                    "record {id} ({}) has versions sharing one capture",
                    vs[0].url
                ));
            } else {
                bumped.push(format!("{} v{:?}", vs[0].url, versions));
            }
        }
    }
    if bumped.is_empty() && problems.is_empty() {
        problems.push(format!(
            "no record of {} has a second version ({} version row(s) in all)",
            CHANGING_PAGES.join(" or "),
            rows.len()
        ));
    }
    Outcome::from_problems(format!("new version: {}", bumped.join("; ")), problems)
}

/// What the API run observed.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiEvidence {
    /// The `/v1/search` body (authenticated, over TLS verified against the E2E CA).
    pub search: serde_json::Value,
    /// The status of the same request without a token.
    pub unauthenticated_status: u16,
    /// Whether a client that trusts only the system roots refused the handshake.
    pub untrusted_handshake_refused: bool,
}

/// `api_provenance`: the API answers over a real TLS handshake, refuses anonymous callers,
/// and every item carries provenance pointing at an E2E source on the fake web.
pub fn api_provenance(ev: &ApiEvidence) -> Outcome {
    let mut problems = Vec::new();
    if !ev.untrusted_handshake_refused {
        problems.push(
            "a client without the E2E CA completed the handshake: the API is not serving our TLS certificate".into(),
        );
    }
    if ev.unauthenticated_status != 401 {
        problems.push(format!(
            "a request without a token got {} (expected 401)",
            ev.unauthenticated_status
        ));
    }
    let items = ev
        .search
        .get("items")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    if items.is_empty() {
        problems.push(format!(
            "/v1/search returned no items: {}",
            trunc(&ev.search)
        ));
    }
    let mut provs = 0usize;
    for it in &items {
        let id = it.get("id").and_then(|v| v.as_str()).unwrap_or("?");
        let pv = it
            .get("provenance")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if pv.is_empty() {
            problems.push(format!("item {id} has no provenance"));
        }
        for p in pv {
            provs += 1;
            let url = p.get("url").and_then(|v| v.as_str()).unwrap_or_default();
            let src = p
                .get("source_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let at = p.get("captured_at").and_then(|v| v.as_str());
            if !host_of(url).ends_with(".test") {
                problems.push(format!(
                    "item {id}: provenance url {url:?} is not on the fake web"
                ));
            }
            if !src.starts_with("e2e-") {
                problems.push(format!(
                    "item {id}: provenance source {src:?} is not an E2E source"
                ));
            }
            if at
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .is_none()
            {
                problems.push(format!("item {id}: provenance without a valid captured_at"));
            }
        }
    }
    Outcome::from_problems(
        format!(
            "{} item(s), {provs} provenance entr(ies), TLS verified, anonymous refused",
            items.len()
        ),
        problems,
    )
}

fn trunc(v: &serde_json::Value) -> String {
    let s = v.to_string();
    if s.len() > 200 {
        format!("{}…", &s[..s.floor_char_boundary(200)])
    } else {
        s
    }
}

/// `raw_first`: the testkit raw-first oracle finds nothing, over a store that holds captures,
/// segments and record versions (an empty database would otherwise pass).
pub fn raw_first(
    violations: &[String],
    captures: i64,
    segments: i64,
    record_versions: i64,
) -> Outcome {
    let mut problems = violations.to_vec();
    for (n, what) in [
        (captures, "captures"),
        (segments, "segments"),
        (record_versions, "record versions"),
    ] {
        if n == 0 {
            problems.push(format!(
                "no {what}: nothing was committed, the oracle proved nothing"
            ));
        }
    }
    Outcome::from_problems(
        format!(
            "{captures} captures in {segments} segments, {record_versions} versions, all committed and stored"
        ),
        problems,
    )
}

/// `two_fetchers`: two distinct fetcher instances reported progress.
pub fn two_fetchers(instances: &[String]) -> Outcome {
    let set: BTreeSet<&String> = instances.iter().collect();
    if set.len() >= 2 {
        Outcome::Pass(format!(
            "fetcher instances: {}",
            set.into_iter().cloned().collect::<Vec<_>>().join(", ")
        ))
    } else {
        Outcome::Fail(format!(
            "expected 2 fetcher instances in heartbeats, found {:?}",
            set
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use vc_core::USER_AGENT;

    fn t(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + s, 0).unwrap()
    }

    fn e(key: &str, url: &str, start: i64, status: u16) -> RequestLogEntry {
        RequestLogEntry {
            key: key.into(),
            url: url.into(),
            started_at: t(start),
            ended_at: t(start) + Duration::milliseconds(100),
            status,
            ua: USER_AGENT.into(),
        }
    }

    fn log(es: Vec<RequestLogEntry>) -> RequestLog {
        let l = RequestLog::new();
        for x in es {
            l.record(x);
        }
        l
    }

    /// One robots fetch and one page per crawled key, 10 s apart, robots served as allow-all.
    fn polite_world() -> (RequestLog, PolicySnapshot) {
        let mut es = Vec::new();
        let mut snap = PolicySnapshot::new();
        for (i, k) in CRAWLED_KEYS.iter().enumerate() {
            let host = if *k == "immigration.test" {
                "visa.immigration.test"
            } else {
                k
            };
            let base = i as i64 * 100;
            es.push(e(k, &format!("http://{host}/robots.txt"), base, 200));
            snap = snap.robots_served(
                &format!("http://{host}"),
                t(base) + Duration::milliseconds(100),
                200,
                "User-agent: *\nAllow: /\n",
            );
            es.push(e(k, &format!("http://{host}/"), base + 10, 200));
        }
        (log(es), snap)
    }

    #[test]
    fn polite_passes_a_clean_full_crawl() {
        let (l, s) = polite_world();
        assert!(
            matches!(polite(&l, &s), Outcome::Pass(_)),
            "{}",
            polite(&l, &s)
        );
    }

    #[test]
    fn polite_fails_an_empty_log() {
        let o = polite(&RequestLog::new(), &PolicySnapshot::new());
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("never reached it")),
            "{o}"
        );
    }

    #[test]
    fn polite_reports_oracle_violations() {
        let (l, s) = polite_world();
        // A second blog page 1 s after the first: short gap.
        l.record(e("blog.test", "http://blog.test/posts/hello", 11, 200));
        let o = polite(&l, &s);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("posts/hello")),
            "{o}"
        );
    }

    #[test]
    fn polite_reports_a_short_subresource_gap() {
        let (l, mut s) = polite_world();
        s.keys.insert(
            "attractions.test".into(),
            testkit::oracle::KeyRules {
                min_delay_ms: None,
                subresource_paths: vec!["/api/".into()],
            },
        );
        // Page at 110 s ends at 110.1 s; the XHR starts 100 ms later (< 250 ms).
        let mut x = e(
            "attractions.test",
            "http://attractions.test/api/attractions.json",
            110,
            200,
        );
        x.started_at = t(110) + Duration::milliseconds(200);
        x.ended_at = x.started_at + Duration::milliseconds(10);
        l.record(x);
        let o = polite(&l, &s);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("attractions.json")),
            "{o}"
        );
    }

    fn parked(reason: &str) -> ParkRow {
        ParkRow {
            parked_until: Some(t(1000) + Duration::hours(24)),
            park_reason: Some(reason.into()),
        }
    }

    #[test]
    fn challenge_parked_passes_one_challenge_then_silence() {
        let l = log(vec![
            e("hostile.test", "http://hostile.test/robots.txt", 0, 200),
            e("hostile.test", "http://hostile.test/", 10, 403),
        ]);
        let o = challenge_parked(&l, Some(&parked("\"challenge\"")), t(1000));
        assert!(matches!(o, Outcome::Pass(_)), "{o}");
    }

    #[test]
    fn challenge_parked_fails_a_request_after_the_challenge() {
        let l = log(vec![
            e("hostile.test", "http://hostile.test/robots.txt", 0, 200),
            e("hostile.test", "http://hostile.test/", 10, 403),
            e("hostile.test", "http://hostile.test/guide", 20, 403),
        ]);
        let o = challenge_parked(&l, Some(&parked("\"challenge\"")), t(1000));
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("/guide")),
            "{o}"
        );
    }

    #[test]
    fn challenge_parked_fails_without_park_or_with_wrong_reason() {
        let l = log(vec![e("hostile.test", "http://hostile.test/", 10, 403)]);
        let o = challenge_parked(&l, None, t(1000));
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("no politeness_keys row")),
            "{o}"
        );
        let o = challenge_parked(
            &l,
            Some(&parked("{\"reason\":\"unauthorized\",\"status\":403}")),
            t(1000),
        );
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("expected the challenge")),
            "{o}"
        );
        let o = challenge_parked(&l, Some(&parked("{\"reason\":\"server_errors\"}")), t(1000));
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("expected the challenge")),
            "{o}"
        );
        let expired = ParkRow {
            parked_until: Some(t(500)),
            park_reason: Some("challenge".into()),
        };
        let o = challenge_parked(&l, Some(&expired), t(1000));
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("not parked")),
            "{o}"
        );
    }

    #[test]
    fn challenge_parked_fails_when_never_requested() {
        let l = log(vec![e(
            "hostile.test",
            "http://hostile.test/robots.txt",
            0,
            200,
        )]);
        let o = challenge_parked(&l, Some(&parked("challenge")), t(1000));
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("never answered a challenge")),
            "{o}"
        );
    }

    fn spa_log() -> RequestLog {
        log(vec![
            e("attractions.test", "http://attractions.test/", 0, 200),
            e("cdn.test", "http://cdn.test/js/app.js", 1, 200),
            e(
                "attractions.test",
                "http://attractions.test/api/attractions.json",
                2,
                200,
            ),
        ])
    }

    #[test]
    fn spa_rendered_needs_the_script_built_dom() {
        let dom = format!("<ul><li class=\"attraction\">{RENDERED_MARKER}</li></ul>").into_bytes();
        let o = spa_rendered(&spa_log(), &[("http://attractions.test/".into(), dom)]);
        assert!(matches!(o, Outcome::Pass(_)), "{o}");
        // The shell alone (what a plain fetch stores) is not a render.
        let shell = b"<div id=\"root\"></div><script src=\"http://cdn.test/js/app.js\">".to_vec();
        let o = spa_rendered(&spa_log(), &[("http://attractions.test/".into(), shell)]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("none holds")),
            "{o}"
        );
        let o = spa_rendered(&spa_log(), &[]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("no primary render capture")),
            "{o}"
        );
    }

    #[test]
    fn spa_rendered_fails_on_a_fetched_image_or_missing_xhr() {
        let dom = RENDERED_MARKER.as_bytes().to_vec();
        let l = spa_log();
        l.record(e("cdn.test", "http://cdn.test/img/logo.png", 3, 200));
        let o = spa_rendered(&l, &[("u".into(), dom.clone())]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("logo.png")),
            "{o}"
        );
        let l = log(vec![e("cdn.test", "http://cdn.test/js/app.js", 1, 200)]);
        let o = spa_rendered(&l, &[("u".into(), dom)]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("attractions.json")),
            "{o}"
        );
    }

    fn empty_traps() -> Vec<(&'static str, RequestLog)> {
        vec![
            (sites::PRIVATE_IP, RequestLog::new()),
            (sites::METADATA_IP, RequestLog::new()),
        ]
    }

    #[test]
    fn private_ip_blocked_passes_empty_traps_when_exercised() {
        let o = private_ip_blocked(&spa_log(), &empty_traps());
        assert!(
            matches!(&o, Outcome::Pass(m) if m.contains("SPA script")),
            "{o}"
        );
    }

    #[test]
    fn private_ip_blocked_fails_any_trap_request() {
        let mut traps = empty_traps();
        traps[0].1 = log(vec![e("10.0.0.5", "http://10.0.0.5/internal", 5, 404)]);
        let o = private_ip_blocked(&spa_log(), &traps);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("10.0.0.5/internal")),
            "{o}"
        );
        let mut traps = empty_traps();
        traps[1].1 = log(vec![e(
            "169.254.169.254",
            "http://169.254.169.254/computeMetadata/v1/",
            5,
            404,
        )]);
        let o = private_ip_blocked(&spa_log(), &traps);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("computeMetadata")),
            "{o}"
        );
    }

    #[test]
    fn private_ip_blocked_is_not_vacuous() {
        let o = private_ip_blocked(&RequestLog::new(), &empty_traps());
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("nothing tried")),
            "{o}"
        );
        let l = log(vec![e("blog.test", "http://blog.test/go/internal", 0, 302)]);
        assert!(matches!(
            private_ip_blocked(&l, &empty_traps()),
            Outcome::Pass(_)
        ));
        let only_one = vec![(sites::PRIVATE_IP, RequestLog::new())];
        let o = private_ip_blocked(&spa_log(), &only_one);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("no trap log for 169.254.169.254")),
            "{o}"
        );
    }

    #[test]
    fn a_cycle_ends_when_both_changing_pages_were_fetched_in_it() {
        let l = log(vec![
            e("blog.test", CHANGING_PAGES[0], 10, 200),
            e("events.test", CHANGING_PAGES[1], 20, 200),
        ]);
        assert!(changing_pages_fetched(&l, t(5)));
        assert!(
            !changing_pages_fetched(&l, t(15)),
            "the blog post was fetched before the cycle"
        );
        l.record(e("blog.test", CHANGING_PAGES[0], 30, 304));
        assert!(
            !changing_pages_fetched(&l, t(25)),
            "a 304 or missing fetch does not count"
        );
        l.record(e("blog.test", CHANGING_PAGES[0], 40, 200));
        l.record(e("events.test", CHANGING_PAGES[1], 41, 200));
        assert!(changing_pages_fetched(&l, t(25)));
    }

    fn vr(id: i64, v: i32, hash: u8, cap: &str) -> VersionRow {
        VersionRow {
            record_id: id,
            url: CHANGING_PAGES[1].into(),
            version: v,
            data_hash: vec![hash],
            capture_id: cap.into(),
        }
    }

    #[test]
    fn version_bump_needs_two_distinct_versions() {
        assert!(matches!(
            version_bump(&[vr(1, 1, 1, "a"), vr(1, 2, 2, "b")]),
            Outcome::Pass(_)
        ));
        let o = version_bump(&[vr(1, 1, 1, "a")]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("no record")),
            "{o}"
        );
        let o = version_bump(&[]);
        assert!(matches!(o, Outcome::Fail(_)));
        let o = version_bump(&[vr(1, 1, 1, "a"), vr(1, 2, 1, "b")]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("distinct data hashes")),
            "{o}"
        );
        let o = version_bump(&[vr(1, 1, 1, "a"), vr(1, 2, 2, "a")]);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("sharing one capture")),
            "{o}"
        );
    }

    fn api_ok() -> ApiEvidence {
        ApiEvidence {
            search: serde_json::json!({"items": [{
                "id": "1", "provenance": [{
                    "source_id": "e2e-events", "url": "http://events.test/events/jazz-night",
                    "captured_at": "2026-11-16T10:00:00Z", "extractor": {}
                }]
            }]}),
            unauthenticated_status: 401,
            untrusted_handshake_refused: true,
        }
    }

    #[test]
    fn api_provenance_passes_items_with_fake_web_provenance() {
        assert!(matches!(api_provenance(&api_ok()), Outcome::Pass(_)));
    }

    #[test]
    fn api_provenance_fails_bad_answers() {
        let mut ev = api_ok();
        ev.search = serde_json::json!({"items": []});
        assert!(matches!(api_provenance(&ev), Outcome::Fail(m) if m.contains("no items")));
        let mut ev = api_ok();
        ev.search["items"][0]["provenance"] = serde_json::json!([]);
        assert!(matches!(api_provenance(&ev), Outcome::Fail(m) if m.contains("no provenance")));
        let mut ev = api_ok();
        ev.search["items"][0]["provenance"][0]["url"] = "https://example.com/x".into();
        assert!(
            matches!(api_provenance(&ev), Outcome::Fail(m) if m.contains("not on the fake web"))
        );
        let mut ev = api_ok();
        ev.unauthenticated_status = 200;
        assert!(matches!(api_provenance(&ev), Outcome::Fail(m) if m.contains("without a token")));
        let mut ev = api_ok();
        ev.untrusted_handshake_refused = false;
        assert!(matches!(api_provenance(&ev), Outcome::Fail(m) if m.contains("handshake")));
    }

    #[test]
    fn raw_first_is_not_vacuous() {
        assert!(matches!(raw_first(&[], 3, 1, 2), Outcome::Pass(_)));
        assert!(matches!(raw_first(&[], 0, 0, 0), Outcome::Fail(m) if m.contains("no captures")));
        let o = raw_first(&["segment_blob_missing: s1".into()], 3, 1, 2);
        assert!(
            matches!(&o, Outcome::Fail(m) if m.contains("segment_blob_missing")),
            "{o}"
        );
    }

    #[test]
    fn two_fetchers_counts_distinct_instances() {
        assert!(matches!(
            two_fetchers(&["a".into(), "b".into()]),
            Outcome::Pass(_)
        ));
        assert!(matches!(
            two_fetchers(&["a".into(), "a".into()]),
            Outcome::Fail(_)
        ));
        assert!(matches!(two_fetchers(&[]), Outcome::Fail(_)));
    }
}
