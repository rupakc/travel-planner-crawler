//! robots.txt: RFC 9309 + our stricter status rules (spec §5.2), Google's test corpus,
//! AI reservation and the refresh → tombstone set (Review Focus 1).

mod common;

use std::collections::BTreeSet;
use std::time::Duration;

use bytes::Bytes;
use common::{entry, ok, policy, t0, url};
use proptest::prelude::*;
use serde::Deserialize;
use testkit::vc_policy_fixtures as fx;
use vc_core::policy::{Policy, RobotsStatus, RobotsSummary, RobotsVerdict};
use vc_policy::robots::{self, ParsedRobots};

#[derive(Deserialize)]
struct Corpus {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    test: String,
    robotstxt: String,
    agent: String,
    url: String,
    allowed: bool,
}

/// Cases of Google's corpus where VoyagerBot deliberately differs, as (test, url), with why.
const KNOWN_DEVIATIONS: &[(&str, &str, &str)] = &[
    (
        "ID_Encoding",
        "http://foo.bar/foo/bar/ツ",
        "URLs are canonical (percent-encoded per RFC 3986) before matching, so the raw-UTF-8 URL \
         Google passes as-is is the same URL as /foo/bar/%E3%83%84",
    ),
    (
        "ID_Encoding",
        "http://foo.bar/foo/bar/baz",
        "escapes of unreserved characters are decoded on both sides (as CanonicalUrl does), so \
         Allow: /foo/bar/%62%61%7A is /foo/bar/baz; Google itself says not to rely on this case",
    ),
    (
        "ID_Encoding",
        "http://foo.bar/foo/bar?qux=taz&baz=http://foo.bar?tar&par",
        "CanonicalUrl sorts query pairs and we request (so match) the canonical URL; the Allow \
         written for the original order no longer matches, so this is stricter (disallowed)",
    ),
    (
        "GoogleOnly_IndexHTMLisDirectory",
        "http://foo.com/allowed-slash/",
        "Google-only extension that widens an Allow; we do not widen allows",
    ),
];

/// Google cases we cannot express: an empty URL is not a crawlable URL.
fn expressible(c: &Case) -> Option<String> {
    vc_core::CanonicalUrl::parse(&c.url)
        .ok()
        .map(|u| u.path_and_query())
}

#[test]
fn robots_google_corpus() {
    let path = fx::policy_fixtures_dir().join("google_robots/cases.json");
    let corpus: Corpus = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert!(corpus.cases.len() >= 140, "corpus is vendored in full");
    let mut deviations = BTreeSet::new();
    let mut checked = 0;
    for c in &corpus.cases {
        let Some(pq) = expressible(c) else {
            assert!(c.url.is_empty(), "only the empty URL is skipped");
            continue;
        };
        checked += 1;
        let got = robots::is_allowed(c.robotstxt.as_bytes(), &c.agent, &pq);
        if got != c.allowed {
            deviations.insert((c.test.clone(), c.url.clone()));
        }
    }
    let known: BTreeSet<_> = KNOWN_DEVIATIONS
        .iter()
        .map(|(t, u, _)| (t.to_string(), u.to_string()))
        .collect();
    assert_eq!(deviations, known, "only the documented deviations");
    assert!(checked >= 136);
}

/// 401/403 on robots.txt: every path of that origin is disallowed (robots.txt included), but
/// the key is not blocked: robots.txt speaks for its own origin only (RFC 9309 §2.3).
#[test]
fn robots_401_403_disallows_the_origin_not_the_key() {
    let p = policy();
    for code in [401, 403] {
        let r = entry(RobotsStatus::Unauthorized, Some(code), None);
        for path in ["/", "/any", "/robots.txt", "/a/b?c=d"] {
            assert_eq!(
                p.robots_verdict(&r, &url(&format!("https://example.com{path}"))),
                RobotsVerdict::Disallowed,
                "{code} {path}"
            );
        }
        assert!(!p.robots_summary(&r).key_blocked, "{code}");
        assert_eq!(p.robots_summary(&r), RobotsSummary::default());
    }
}

/// A challenge page on robots.txt is a bot wall: the whole key is blocked.
#[test]
fn robots_challenge_blocks_key() {
    let p = policy();
    let r = entry(
        RobotsStatus::Challenge,
        Some(200),
        Some("<title>Just a moment...</title>"),
    );
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/robots.txt")),
        RobotsVerdict::KeyBlocked
    );
    assert!(p.robots_summary(&r).key_blocked);
}

#[test]
fn robots_5xx_disallows() {
    let p = policy();
    let r = entry(RobotsStatus::Unavailable, Some(503), None);
    let v = p.robots_verdict(&r, &url("https://example.com/page"));
    assert_eq!(v, RobotsVerdict::Unavailable);
    assert!(!v.allows());
    assert!(!p.robots_summary(&r).key_blocked);
}

#[test]
fn robots_5xx_uses_last_good_after_30_days_only() {
    let p = policy();
    let mut r = entry(RobotsStatus::Unavailable, Some(500), None);
    r.last_good = Some((
        Bytes::from_static(b"User-agent: *\nDisallow: /private\n"),
        t0() - chrono::Duration::days(29),
    ));
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/page")),
        RobotsVerdict::Unavailable
    );
    r.last_good.as_mut().unwrap().1 = t0() - chrono::Duration::days(30);
    assert!(
        p.robots_verdict(&r, &url("https://example.com/page"))
            .allows()
    );
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/private/x")),
        RobotsVerdict::Disallowed
    );
}

#[test]
fn robots_404_allows() {
    let p = policy();
    let r = entry(RobotsStatus::NotFound, Some(404), None);
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/anything?q=1")),
        RobotsVerdict::Allowed {
            crawl_delay: None,
            ai_reserved: false
        }
    );
    let s = p.robots_summary(&r);
    assert!(!s.key_blocked && !s.ai_reserved && s.sitemaps.is_empty());
}

#[test]
fn ai_bot_block_sets_ai_reservation() {
    let p = policy();
    let body = "User-agent: GPTBot\nDisallow: /\n\nUser-agent: CCBot\nDisallow: /\n\n\
                User-agent: *\nAllow: /\nCrawl-delay: 7\n";
    let r = ok(body);
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/guide")),
        RobotsVerdict::Allowed {
            crawl_delay: Some(Duration::from_secs(7)),
            ai_reserved: true
        }
    );
    assert!(p.robots_summary(&r).ai_reserved);

    // No AI block → no reservation.
    let plain = ok("User-agent: *\nDisallow: /admin\n");
    assert!(!p.robots_summary(&plain).ai_reserved);
    // A partial AI block (one path) is not a reservation.
    let partial = ok("User-agent: GPTBot\nDisallow: /private\n\nUser-agent: *\nAllow: /\n");
    assert!(!p.robots_summary(&partial).ai_reserved);
    // Everyone blocked → not a reservation, just disallowed.
    let all = ok("User-agent: *\nDisallow: /\n");
    assert!(!p.robots_summary(&all).ai_reserved);
    assert_eq!(
        p.robots_verdict(&all, &url("https://example.com/x")),
        RobotsVerdict::Disallowed
    );
    // The reservation feeds the UsePolicy (no LLM / embedding).
    let sig = vc_core::usepolicy::UseSignals {
        ai_bot_blocked: true,
        ..Default::default()
    };
    assert_eq!(
        p.use_policy(&sig),
        vc_core::usepolicy::UsePolicy::from_signals(&sig)
    );
}

#[test]
fn robots_refresh_tombstones() {
    let p = policy();
    let old = ok("User-agent: *\nDisallow: /admin\n");
    let new = ok("User-agent: *\nDisallow: /admin\nDisallow: /guides/\n\
                  User-agent: VoyagerBot\nDisallow: /admin\nDisallow: /guides/\nDisallow: /events$\n");
    let urls: Vec<_> = [
        "https://example.com/guides/lisbon",
        "https://example.com/guides/porto?lang=en",
        "https://example.com/events",
        "https://example.com/events/2026",
        "https://example.com/admin/x",
        "https://example.com/about",
    ]
    .into_iter()
    .map(url)
    .collect();
    let got: Vec<String> = p
        .newly_disallowed(&old, &new, &urls)
        .iter()
        .map(|u| u.to_string())
        .collect();
    assert_eq!(
        got,
        vec![
            "https://example.com/guides/lisbon",
            "https://example.com/guides/porto?lang=en",
            "https://example.com/events",
        ]
    );
    // A switch to 403 disallows the whole origin: everything previously allowed is tombstoned.
    let blocked = entry(RobotsStatus::Unauthorized, Some(403), None);
    assert_eq!(p.newly_disallowed(&old, &blocked, &urls).len(), 5);
    // A transient 5xx never tombstones.
    let down = entry(RobotsStatus::Unavailable, Some(503), None);
    assert!(p.newly_disallowed(&old, &down, &urls).is_empty());
    // Unchanged robots → nothing.
    assert!(p.newly_disallowed(&old, &old, &urls).is_empty());
}

#[test]
fn voyagerbot_group_beats_star_and_is_case_insensitive() {
    let p = policy();
    let r =
        ok("User-agent: *\nDisallow: /\n\nUser-agent: voyagerbot\nAllow: /\nCrawl-delay: 2.5\n");
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/a")),
        RobotsVerdict::Allowed {
            crawl_delay: Some(Duration::from_millis(2500)),
            ai_reserved: false
        }
    );
    let r = ok("User-agent: *\nAllow: /\n\nUser-agent: VoyagerBot\nDisallow: /\n");
    assert_eq!(
        p.robots_verdict(&r, &url("https://example.com/a")),
        RobotsVerdict::Disallowed
    );
}

#[test]
fn voyagerbot_group_with_version_matches_us() {
    // `User-agent: VoyagerBot/1.0` names our product token: the version is stripped and the
    // group applies (erring on the side of obeying), in any case.
    let p = policy();
    for ua in [
        "VoyagerBot/1.0",
        "voyagerbot/2",
        "VOYAGERBOT/0.1.0 (+https://x)",
    ] {
        let body = format!("User-agent: *\nAllow: /\n\nUser-agent: {ua}\nDisallow: /private\n");
        let r = ok(&body);
        assert_eq!(
            p.robots_verdict(&r, &url("https://example.com/private/x")),
            RobotsVerdict::Disallowed,
            "{ua}"
        );
        assert!(
            ParsedRobots::parse(body.as_bytes()).names_agent("VoyagerBot"),
            "{ua}"
        );
        assert!(
            !robots::is_allowed(body.as_bytes(), "VoyagerBot", "/private"),
            "{ua}"
        );
        assert!(
            robots::is_allowed(body.as_bytes(), "VoyagerBot", "/public"),
            "{ua}"
        );
    }
    // Our own full UA string as the agent argument resolves to the same group.
    let body = b"User-agent: VoyagerBot/1.0\nDisallow: /\n";
    assert!(!robots::is_allowed(
        body,
        "VoyagerBot/1.0 (+https://example.com/bot)",
        "/a"
    ));
    // A different product that merely starts with our name does not match.
    let body = b"User-agent: VoyagerBotX/1.0\nDisallow: /\n";
    assert!(robots::is_allowed(body, "VoyagerBot", "/a"));
}

#[test]
fn normalize_is_idempotent_with_bare_percent() {
    // Found by the nightly fuzzer: a bare `%` next to an escape that decodes to a hex digit.
    for s in [
        "%7%39",
        "/a%",
        "%%0071%737777",
        "/%zz/%4",
        "2\u{ff}\u{7}7%\u{0}71%737777%\u{0}71%7%391",
    ] {
        let n = robots::normalize(s);
        assert_eq!(robots::normalize(&n), n, "{s:?}");
    }
    assert_eq!(robots::normalize("%7%39"), "%2579");
    // A bare `%` in a rule and in a path still match each other.
    assert!(!robots::is_allowed(
        b"User-agent: *\nDisallow: /a%b\n",
        "VoyagerBot",
        "/a%b"
    ));
    assert!(!robots::is_allowed(
        b"User-agent: *\nDisallow: /a%25b\n",
        "VoyagerBot",
        "/a%b"
    ));
}

#[test]
fn robots_txt_itself_is_always_allowed_unless_key_blocked() {
    let p = policy();
    let r = ok("User-agent: *\nDisallow: /\n");
    assert!(
        p.robots_verdict(&r, &url("https://example.com/robots.txt"))
            .allows()
    );
}

#[test]
fn other_origin_fails_closed() {
    let p = policy();
    let r = ok("");
    assert_eq!(
        p.robots_verdict(&r, &url("https://other.example.com/")),
        RobotsVerdict::Disallowed
    );
    assert_eq!(
        p.robots_verdict(&r, &url("http://example.com/")),
        RobotsVerdict::Disallowed
    );
}

#[test]
fn escaped_patterns_match_canonical_urls() {
    let p = policy();
    let r = ok("User-agent: *\nDisallow: /%7Euser\nDisallow: /caf%c3%a9\nDisallow: /ünï\n");
    for u in [
        "https://example.com/~user/x",
        "https://example.com/%7euser",
        "https://example.com/café",
        "https://example.com/%C3%BCn%C3%AF",
    ] {
        assert_eq!(
            p.robots_verdict(&r, &url(u)),
            RobotsVerdict::Disallowed,
            "{u}"
        );
    }
}

#[test]
fn summary_sitemaps_and_crawl_delay() {
    let p = policy();
    let r = ok(
        "Sitemap: https://example.com/s1.xml\nUser-agent: *\nCrawl-delay: 99999\n\
                Disallow: /x\nsitemap: ftp://example.com/bad.xml\nSitemap: not a url\n\
                Sitemap: http://cdn.example.com/s2.xml\n",
    );
    let s = p.robots_summary(&r);
    assert_eq!(
        s.sitemaps.iter().map(|u| u.as_str()).collect::<Vec<_>>(),
        vec![
            "https://example.com/s1.xml",
            "http://cdn.example.com/s2.xml"
        ]
    );
    assert_eq!(
        s.crawl_delay,
        Some(Duration::from_secs(3600)),
        "capped at 1 h"
    );
    assert!(!s.key_blocked && !s.ai_reserved);
    assert_eq!(
        p.robots_summary(&entry(RobotsStatus::Unavailable, Some(502), None)),
        Default::default()
    );
}

#[test]
fn parser_line_endings_bom_and_missing_colon() {
    for body in [
        "User-agent: *\nDisallow: /x\n",
        "User-agent: *\r\nDisallow: /x\r\n",
        "User-agent: *\rDisallow: /x",
        "\u{feff}User-agent: *\nDisallow: /x",
        "useragent *\ndisallow /x",
        "USER-AGENT:*\nDISALLOW:/x # comment\n",
    ] {
        assert!(
            !robots::is_allowed(body.as_bytes(), "VoyagerBot", "/x/y"),
            "{body:?}"
        );
        assert!(
            robots::is_allowed(body.as_bytes(), "VoyagerBot", "/y"),
            "{body:?}"
        );
    }
    // A partial BOM is skipped too.
    let mut partial = vec![0xEF, 0xBB];
    partial.extend_from_slice(b"User-agent: *\nDisallow: /x\n");
    assert!(!robots::is_allowed(&partial, "VoyagerBot", "/x"));
    // Invalid UTF-8 never panics and does not hide later rules.
    let mut bad = b"User-agent: *\nDisallow: /\xff\xfe\n".to_vec();
    bad.extend_from_slice(b"Disallow: /x\n");
    assert!(!robots::is_allowed(&bad, "VoyagerBot", "/x"));
}

#[test]
fn empty_and_rules_outside_groups() {
    assert!(robots::is_allowed(b"", "VoyagerBot", "/"));
    assert!(robots::is_allowed(b"Disallow: /\n", "VoyagerBot", "/x"));
    let p = ParsedRobots::parse(b"User-agent: GPTBot\nDisallow: /\n");
    assert!(p.names_agent("gptbot"));
    assert!(!p.names_agent("VoyagerBot"));
    assert!(p.rules_for("VoyagerBot").rules.is_empty());
}

#[test]
fn wildcards_and_anchors() {
    let body = b"User-agent: *\nDisallow: /*.pdf$\nDisallow: /a*b*c\nAllow: /a*b*c/ok\n";
    assert!(!robots::is_allowed(body, "x", "/files/doc.pdf"));
    assert!(robots::is_allowed(body, "x", "/files/doc.pdf?x=1"));
    assert!(!robots::is_allowed(body, "x", "/aXbYc"));
    assert!(robots::is_allowed(body, "x", "/aXbYc/ok"));
    assert!(robots::is_allowed(body, "x", "/acb"));
    assert!(robots::matches(b"*", b""));
    assert!(robots::matches(b"$", b""));
    assert!(!robots::matches(b"$", b"/"));
    assert!(robots::matches(b"/**x$", b"/abx"));
}

#[test]
fn robots_body_truncated_to_max_bytes() {
    let mut p = policy();
    p.config.robots_max_bytes = 32;
    // The Disallow lies beyond the cap and is not read.
    let body = format!("User-agent: *\n#{}\nDisallow: /x\n", "a".repeat(64));
    assert!(
        p.robots_verdict(&ok(&body), &url("https://example.com/x"))
            .allows()
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// The fuzz target's property, run in CI: no panic on arbitrary bytes, and verdicts are
    /// deterministic.
    #[test]
    fn fuzz_robots_never_panics(body in proptest::collection::vec(any::<u8>(), 0..600),
                                path in "/[ -~]{0,40}") {
        let a = robots::is_allowed(&body, "VoyagerBot", &path);
        let b = robots::is_allowed(&body, "VoyagerBot", &path);
        prop_assert_eq!(a, b);
    }

    /// Structured robots files: the longest matching rule wins, ties go to allow.
    #[test]
    fn longest_match_wins(rules in proptest::collection::vec(("[a-c/]{1,6}", any::<bool>()), 1..8),
                          path in "/[a-c/]{0,10}") {
        let mut body = String::from("User-agent: *\n");
        for (pat, allow) in &rules {
            body.push_str(if *allow { "Allow: /" } else { "Disallow: /" });
            body.push_str(pat);
            body.push('\n');
        }
        let mut best: Option<(usize, bool)> = None;
        for (pat, allow) in &rules {
            let full = format!("/{pat}");
            if path.starts_with(&full) {
                best = match best {
                    Some((l, a)) if l > full.len() || (l == full.len() && a) => Some((l, a)),
                    _ => Some((full.len(), *allow)),
                };
            }
        }
        let expected = best.is_none_or(|(_, a)| a) || path == "/robots.txt";
        prop_assert_eq!(robots::is_allowed(body.as_bytes(), "VoyagerBot", &path), expected);
    }

    #[test]
    fn normalize_is_idempotent(s in "\\PC{0,30}") {
        let once = robots::normalize(&s);
        prop_assert_eq!(robots::normalize(&once), once);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Differential check against `texting_robots` (tested on Google's and Moz's corpora) on
    /// plain robots files where both must agree.
    #[test]
    fn agrees_with_texting_robots(rules in proptest::collection::vec(("[a-c/]{0,4}[*]?[a-c]{0,2}[$]?", any::<bool>()), 1..6),
                                  path in "/[a-c/]{0,8}") {
        let mut body = String::from("User-agent: *\n");
        for (pat, allow) in &rules {
            body.push_str(if *allow { "Allow: /" } else { "Disallow: /" });
            body.push_str(pat);
            body.push('\n');
        }
        let theirs = texting_robots::Robot::new("VoyagerBot", body.as_bytes()).unwrap();
        prop_assert_eq!(
            robots::is_allowed(body.as_bytes(), "VoyagerBot", &path),
            theirs.allowed(&path),
            "{}", body
        );
    }
}
