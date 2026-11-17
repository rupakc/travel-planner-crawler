//! Self-tests of the politeness oracle (plan Task 14): it must catch every class of violation
//! and pass a clean log.

use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use testkit::oracle::{
    Backoff, KeyRules, PolicySnapshot, ViolationKind, assert_polite, check_polite,
};
use testkit::{RequestLog, RequestLogEntry};
use vc_core::USER_AGENT;

fn t(ms: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).single().unwrap() + ChronoDuration::milliseconds(ms)
}

fn entry(url: &str, start_ms: i64, end_ms: i64, status: u16) -> RequestLogEntry {
    let host = url::Url::parse(url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    RequestLogEntry {
        key: vc_core::PolitenessKey::for_host(&host).0,
        url: url.to_string(),
        started_at: t(start_ms),
        ended_at: t(end_ms),
        status,
        ua: USER_AGENT.to_string(),
    }
}

fn log(entries: Vec<RequestLogEntry>) -> RequestLog {
    let l = RequestLog::new();
    entries.into_iter().for_each(|e| l.record(e));
    l
}

/// A snapshot where `http://blog.test` served an allow-all robots.txt at t=0..100 ms.
fn snap_allow_all() -> PolicySnapshot {
    PolicySnapshot::new().robots_served(
        "http://blog.test",
        t(100),
        200,
        "User-agent: *\nAllow: /\n",
    )
}

fn kinds(l: &RequestLog, s: &PolicySnapshot) -> Vec<ViolationKind> {
    check_polite(l, s).into_iter().map(|v| v.kind).collect()
}

#[test]
fn clean_log_passes() {
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/", 5_200, 5_300, 200),
        entry("http://blog.test/posts/hello", 10_400, 10_500, 200),
    ]);
    let s = snap_allow_all();
    assert_eq!(check_polite(&l, &s), vec![]);
    assert_polite(&l, &s);
}

#[test]
fn oracle_detects_overlap() {
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 6_000, 8_000, 200),
        entry("http://blog.test/b", 7_000, 9_000, 200),
    ]);
    let k = kinds(&l, &snap_allow_all());
    assert!(k.contains(&ViolationKind::Overlap), "{k:?}");
}

#[test]
fn oracle_detects_overlap_across_hosts_of_one_key() {
    // www.blog.test and blog.test share the key blog.test.
    let s = snap_allow_all().robots_served("http://www.blog.test", t(5_100), 200, "");
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://www.blog.test/robots.txt", 5_000, 5_100, 200),
        entry("http://blog.test/a", 10_200, 12_000, 200),
        entry("http://www.blog.test/b", 11_000, 11_500, 200),
    ]);
    assert!(kinds(&l, &s).contains(&ViolationKind::Overlap));
}

#[test]
fn oracle_detects_short_gap() {
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 5_100, 5_200, 200),
        entry("http://blog.test/b", 6_200, 6_300, 200), // 1 s after /a, min delay is 5 s
    ]);
    let v = check_polite(&l, &snap_allow_all());
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].kind, ViolationKind::ShortGap);
    assert_eq!(v[0].url, "http://blog.test/b");
}

#[test]
fn oracle_short_gap_honours_crawl_delay() {
    let s = PolicySnapshot::new().robots_served(
        "http://slow.test",
        t(100),
        200,
        "User-agent: *\nCrawl-delay: 10\nAllow: /\n",
    );
    let ok = log(vec![
        entry("http://slow.test/robots.txt", 0, 100, 200),
        entry("http://slow.test/a", 10_100, 10_200, 200),
    ]);
    assert_eq!(check_polite(&ok, &s), vec![]);
    let bad = log(vec![
        entry("http://slow.test/robots.txt", 0, 100, 200),
        entry("http://slow.test/a", 6_100, 6_200, 200), // ≥ 5 s but < 10 s
    ]);
    assert_eq!(kinds(&bad, &s), vec![ViolationKind::ShortGap]);
}

#[test]
fn oracle_short_gap_honours_key_min_delay() {
    let s = snap_allow_all().key_rules(
        "blog.test",
        KeyRules {
            min_delay_ms: Some(8_000),
            ..KeyRules::default()
        },
    );
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 6_000, 6_100, 200),
    ]);
    assert_eq!(kinds(&l, &s), vec![ViolationKind::ShortGap]);
}

#[test]
fn subresources_use_the_short_gap_but_pages_do_not() {
    let s = PolicySnapshot::new()
        .robots_served("http://attractions.test", t(100), 200, "")
        .key_rules(
            "attractions.test",
            KeyRules {
                subresource_paths: vec!["/api/".into()],
                ..KeyRules::default()
            },
        );
    let ok = log(vec![
        entry("http://attractions.test/robots.txt", 0, 100, 200),
        entry("http://attractions.test/", 5_100, 5_200, 200),
        entry("http://attractions.test/api/a.json", 5_450, 5_500, 200), // 250 ms
        entry("http://attractions.test/api/b.json", 5_750, 5_800, 200),
        entry("http://attractions.test/next", 10_200, 10_300, 200), // 5 s after the last page
    ]);
    assert_eq!(check_polite(&ok, &s), vec![]);
    let bad = log(vec![
        entry("http://attractions.test/robots.txt", 0, 100, 200),
        entry("http://attractions.test/", 5_100, 5_200, 200),
        entry("http://attractions.test/api/a.json", 5_300, 5_400, 200), // 100 ms
    ]);
    assert_eq!(kinds(&bad, &s), vec![ViolationKind::ShortGap]);
}

#[test]
fn oracle_detects_disallowed() {
    let s = PolicySnapshot::new().robots_served(
        "http://blog.test",
        t(100),
        200,
        "User-agent: *\nDisallow: /private/\n",
    );
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/public", 5_100, 5_200, 200),
        entry("http://blog.test/private/draft", 10_200, 10_300, 200),
    ]);
    let v = check_polite(&l, &s);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].kind, ViolationKind::Disallowed);
    assert_eq!(v[0].url, "http://blog.test/private/draft");
}

#[test]
fn oracle_detects_disallowed_after_robots_refresh() {
    // Review Focus 1: robots changes mid-crawl to disallow a path.
    let s = PolicySnapshot::new()
        .robots_served("http://blog.test", t(100), 200, "User-agent: *\nAllow: /\n")
        .robots_served(
            "http://blog.test",
            t(20_100),
            200,
            "User-agent: *\nDisallow: /private/\n",
        );
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/private/draft", 5_100, 5_200, 200), // allowed then
        entry("http://blog.test/robots.txt", 20_000, 20_100, 200),
        entry("http://blog.test/private/draft", 25_100, 25_200, 200), // not any more
    ]);
    let v = check_polite(&l, &s);
    assert_eq!(v.len(), 1, "{v:?}");
    assert_eq!(v[0].kind, ViolationKind::Disallowed);
    assert_eq!(v[0].at, t(25_100));
}

#[test]
fn voyagerbot_group_wins_over_star() {
    let s = PolicySnapshot::new().robots_served(
        "http://blog.test",
        t(100),
        200,
        "User-agent: *\nAllow: /\n\nUser-agent: VoyagerBot\nDisallow: /no-bots/\n",
    );
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/no-bots/x", 5_100, 5_200, 200),
    ]);
    assert_eq!(kinds(&l, &s), vec![ViolationKind::Disallowed]);
}

#[test]
fn request_before_any_robots_is_flagged() {
    let l = log(vec![entry("http://blog.test/", 0, 100, 200)]);
    assert_eq!(
        kinds(&l, &PolicySnapshot::new()),
        vec![ViolationKind::NoRobots]
    );
}

#[test]
fn robots_404_allows_all_and_5xx_disallows_all() {
    let s404 = PolicySnapshot::new().robots_served("http://blog.test", t(100), 404, "");
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 404),
        entry("http://blog.test/x", 5_100, 5_200, 200),
    ]);
    assert_eq!(check_polite(&l, &s404), vec![]);

    let s503 = PolicySnapshot::new().robots_served("http://blog.test", t(100), 503, "");
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 503),
        entry("http://blog.test/x", 5_100, 5_200, 200),
    ]);
    assert_eq!(kinds(&l, &s503), vec![ViolationKind::Disallowed]);
}

/// robots.txt 401/403 blocks its own origin for good (robots.txt included); another host of the
/// key with its own robots.txt is still crawlable, and the refusal does not park the key.
#[test]
fn robots_401_403_blocks_its_origin_only() {
    for status in [401, 403] {
        let s = PolicySnapshot::new()
            .robots_served("http://blog.test", t(100), status, "")
            .robots_served("http://www.blog.test", t(5_200), 404, "");
        // The other host of the key: polite.
        let l = log(vec![
            entry("http://blog.test/robots.txt", 0, 100, status),
            entry("http://www.blog.test/robots.txt", 5_100, 5_200, 404),
            entry("http://www.blog.test/x", 10_200, 10_300, 200),
        ]);
        assert_eq!(check_polite(&l, &s), vec![], "{status}");
        // The refusing origin: any later request, a page or robots.txt again, is a violation.
        for later in ["http://blog.test/x", "http://blog.test/robots.txt"] {
            let l = log(vec![
                entry("http://blog.test/robots.txt", 0, 100, status),
                entry(later, 5_100, 5_200, 200),
            ]);
            let k = kinds(&l, &s);
            assert!(
                k.contains(&ViolationKind::OriginBlocked),
                "{status} {later}: {k:?}"
            );
        }
    }
}

/// A challenge on robots.txt blocks every host of the key.
#[test]
fn robots_challenge_blocks_the_whole_key() {
    let s = PolicySnapshot::new().robots_challenge("http://blog.test", t(100));
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/x", 5_100, 5_200, 200),
    ]);
    assert!(kinds(&l, &s).contains(&ViolationKind::KeyBlocked));
}

#[test]
fn oracle_detects_requests_after_a_park() {
    // A challenge page (served with 200) parks the key.
    let s = snap_allow_all().backoff(Backoff::park("blog.test", t(5_200)));
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 5_100, 5_200, 200),
        entry("http://blog.test/b", 10_300, 10_400, 200),
    ]);
    assert_eq!(kinds(&l, &s), vec![ViolationKind::AfterPark]);

    // A 403 parks it too, without any snapshot entry.
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 5_100, 5_200, 403),
        entry("http://blog.test/b", 10_300, 10_400, 200),
    ]);
    assert_eq!(kinds(&l, &snap_allow_all()), vec![ViolationKind::AfterPark]);
}

#[test]
fn retry_after_is_honoured() {
    let s = snap_allow_all().backoff(Backoff {
        key: "blog.test".into(),
        from: t(5_200),
        until: t(65_200),
        reason: "retry_after".into(),
    });
    let early = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 5_100, 5_200, 429),
        entry("http://blog.test/a", 30_000, 30_100, 200),
    ]);
    assert_eq!(kinds(&early, &s), vec![ViolationKind::AfterPark]);
    let late = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 5_100, 5_200, 429),
        entry("http://blog.test/a", 65_300, 65_400, 200),
    ]);
    assert_eq!(check_polite(&late, &s), vec![]);
}

#[test]
fn oracle_detects_denylisted_hosts() {
    let s = PolicySnapshot::new()
        .robots_served("http://www.expedia.com", t(100), 200, "")
        .robots_served("http://denied.test", t(100), 200, "")
        .deny("expedia.*")
        .deny("denied.test");
    let l = log(vec![
        entry("http://www.expedia.com/robots.txt", 0, 100, 200),
        entry("http://sub.denied.test/x", 0, 100, 200),
    ]);
    let k = kinds(&l, &s);
    assert_eq!(
        k.iter()
            .filter(|k| **k == ViolationKind::Denylisted)
            .count(),
        2,
        "{k:?}"
    );
}

#[test]
fn oracle_detects_a_foreign_user_agent() {
    let mut e = entry("http://blog.test/robots.txt", 0, 100, 200);
    e.ua = "Mozilla/5.0 (Windows NT 10.0; Win64; x64)".into();
    let l = log(vec![e]);
    assert_eq!(
        kinds(&l, &snap_allow_all()),
        vec![ViolationKind::WrongUserAgent]
    );
}

#[test]
#[should_panic(expected = "politeness oracle")]
fn assert_polite_panics_with_a_report() {
    let l = log(vec![
        entry("http://blog.test/robots.txt", 0, 100, 200),
        entry("http://blog.test/a", 200, 300, 200),
    ]);
    assert_polite(&l, &snap_allow_all());
}

#[test]
fn snapshot_round_trips_as_json() {
    let s = snap_allow_all()
        .deny("denied.test")
        .backoff(Backoff::park("hostile.test", t(1_000)));
    let j = serde_json::to_string(&s).unwrap();
    let back: PolicySnapshot = serde_json::from_str(&j).unwrap();
    assert_eq!(back, s);
}
