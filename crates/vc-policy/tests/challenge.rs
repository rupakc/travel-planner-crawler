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

/// An ordinary Cloudflare-fronted 200 page: Cloudflare injects its "JavaScript detections"
/// loader, which references `/cdn-cgi/challenge-platform/scripts/jsd/main.js`. Not a challenge.
fn cloudflare_normal_page() -> String {
    let filler = "<p>The Louvre is open from 9:00 to 18:00 except Tuesdays.</p>".repeat(400);
    format!(
        "<!DOCTYPE html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Paris museums: opening hours and tickets | City Guide</title>\
         <link rel=\"stylesheet\" href=\"/static/site.css\"></head><body>\
         <h1>Paris museums</h1>{filler}\
         <script>(function(){{function c(){{var b=a.contentDocument||a.contentWindow.document;\
         if(b){{var d=b.createElement('script');d.innerHTML=\"window.__CF$cv$params={{r:'8f1c2a3b4d5e6f70',\
         t:'MTczMTc2MDAwMC4wMDAwMDA='}};var a=document.createElement('script');a.nonce='';\
         a.src='/cdn-cgi/challenge-platform/scripts/jsd/main.js';\
         document.getElementsByTagName('head')[0].appendChild(a);\";\
         b.getElementsByTagName('head')[0].appendChild(d)}}}}if(document.body){{var a=document.createElement('iframe');\
         a.height=1;a.width=1;a.style.position='absolute';a.style.top=0;a.style.left=0;a.style.border='none';\
         a.style.visibility='hidden';document.body.appendChild(a);c()}}}})();</script>\
         </body></html>"
    )
}

#[test]
fn cloudflare_jsd_script_on_normal_page_is_not_a_challenge() {
    let p = policy();
    let page = cloudflare_normal_page();
    assert!(page.contains("/cdn-cgi/challenge-platform/scripts/jsd/main.js"));
    assert!(!p.is_challenge(&exchange(200, &[], page.as_bytes())));
    assert_eq!(
        challenge::detect(&exchange(200, &[], page.as_bytes())),
        None
    );
    // A small page with the loader is still an ordinary page.
    let small = "<html><head><title>Opening hours</title></head><body>Open daily.\
                 <script>a.src='/cdn-cgi/challenge-platform/scripts/jsd/main.js';</script>\
                 </body></html>";
    assert!(!p.is_challenge(&exchange(200, &[], small.as_bytes())));
    // A 404 or 500 with the loader is an error page, not a challenge.
    assert!(!p.is_challenge(&exchange(404, &[], page.as_bytes())));
    assert!(!p.is_challenge(&exchange(500, &[], page.as_bytes())));
}

#[test]
fn cloudflare_challenge_platform_marker_needs_corroboration() {
    let p = policy();
    let page = cloudflare_normal_page();
    // With a challenge status it is a challenge.
    assert_eq!(
        challenge::detect(&exchange(403, &[], page.as_bytes())),
        Some("cloudflare")
    );
    assert_eq!(
        challenge::detect(&exchange(503, &[], page.as_bytes())),
        Some("cloudflare")
    );
    // With a challenge title on a 200 it is a challenge.
    let titled = page.replace(
        "<title>Paris museums: opening hours and tickets | City Guide</title>",
        "<title>Just a moment...</title>",
    );
    assert_eq!(
        challenge::detect(&exchange(200, &[], titled.as_bytes())),
        Some("cloudflare")
    );
    let verify = page.replace(
        "<title>Paris museums: opening hours and tickets | City Guide</title>",
        "<title>Checking your browser before accessing</title>",
    );
    assert!(p.is_challenge(&exchange(200, &[], verify.as_bytes())));
    // With the challenge options object it is a challenge.
    let opt = format!("{page}<script>window._cf_chl_opt={{cvId:'3'}};</script>");
    assert!(p.is_challenge(&exchange(200, &[], opt.as_bytes())));
    // The real challenge pages still are.
    for f in [
        "cloudflare_js_flyertalk.html",
        "cloudflare_js_wikitravel.html",
    ] {
        let body = std::fs::read(fx::challenges_dir().join(f)).unwrap();
        assert_eq!(
            challenge::detect(&exchange(200, &[], &body)),
            Some("cloudflare"),
            "{f}"
        );
    }
}
