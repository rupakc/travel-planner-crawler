//! Challenge detection: every fixture in `testkit/fixtures/challenges/` is a challenge, no
//! normal page is.

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{exchange, policy};
use serde::Deserialize;
use testkit::vc_policy_fixtures as fx;
use vc_core::policy::Policy;
use vc_policy::challenge;

#[derive(Deserialize)]
struct Manifest {
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    file: String,
    status: u16,
    headers: BTreeMap<String, String>,
    #[serde(default)]
    provider: Option<String>,
}

fn load(dir: &Path) -> Vec<(Case, Vec<u8>)> {
    let m: Manifest =
        serde_json::from_slice(&std::fs::read(dir.join("manifest.json")).unwrap()).unwrap();
    m.cases
        .into_iter()
        .map(|c| {
            let body = std::fs::read(dir.join(&c.file)).unwrap();
            (c, body)
        })
        .collect()
}

fn ex(c: &Case, body: &[u8]) -> vc_core::fetch::Exchange {
    let h: Vec<(&str, &str)> = c
        .headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    exchange(c.status, &h, body)
}

#[test]
fn challenge_fixtures_detected() {
    let p = policy();
    let cases = load(&fx::challenges_dir());
    assert!(cases.len() >= 15);
    for (c, body) in &cases {
        let e = ex(c, body);
        assert!(p.is_challenge(&e), "{} ({}) not detected", c.file, c.status);
        assert_eq!(challenge::detect(&e), c.provider.as_deref(), "{}", c.file);
    }
}

#[test]
fn normal_pages_not_challenge() {
    let p = policy();
    let cases = load(&fx::policy_fixtures_dir().join("normal_pages"));
    assert!(cases.len() >= 8);
    for (c, body) in &cases {
        assert!(
            !p.is_challenge(&ex(c, body)),
            "{} flagged as a challenge",
            c.file
        );
    }
}

#[test]
fn plain_403_is_not_a_challenge_but_captcha_on_403_is() {
    let p = policy();
    assert!(!p.is_challenge(&exchange(403, &[], b"Forbidden")));
    assert!(!p.is_challenge(&exchange(200, &[("cf-mitigated", "none")], b"ok")));
    let long_page_with_captcha = format!(
        "<html><title>Hotels</title>{}<div class=\"g-recaptcha\"></div></html>",
        "x".repeat(20_000)
    );
    assert!(!p.is_challenge(&exchange(200, &[], long_page_with_captcha.as_bytes())));
    assert!(p.is_challenge(&exchange(403, &[], long_page_with_captcha.as_bytes())));
    assert!(p.is_challenge(&exchange(429, &[], long_page_with_captcha.as_bytes())));
}

#[test]
fn markers_beyond_scan_window_are_ignored() {
    let p = policy();
    let mut body = vec![b' '; challenge::SCAN_BYTES];
    body.extend_from_slice(b"/cdn-cgi/challenge-platform/");
    assert!(!p.is_challenge(&exchange(200, &[], &body)));
}
