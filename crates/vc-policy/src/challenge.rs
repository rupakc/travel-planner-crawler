//! Bot-challenge / interstitial detection. A challenge means **detect → park the key**; the
//! caller never retries with a browser or another identity.
//!
//! Signals, strongest first:
//! 1. headers: `cf-mitigated: challenge` (Cloudflare), `x-amzn-waf-action: captcha|challenge`
//!    (AWS WAF), `x-datadome` on a 4xx (DataDome);
//! 2. provider interstitial markers in the body (Cloudflare challenge options and block page,
//!    Akamai Bot Manager / edgesuite error, DataDome captcha-delivery, Imperva Incapsula,
//!    PerimeterX/HUMAN captcha), whatever the status;
//! 3. a captcha widget (reCAPTCHA, hCaptcha, Turnstile) on a 401/403/429/503, or on a short page
//!    whose title asks to verify the visitor. A captcha on an ordinary long page (a newsletter
//!    form) is not a challenge;
//! 4. Cloudflare's [`CF_PLATFORM_MARKER`] only together with a challenge status (403/503) or a
//!    verification title: ordinary Cloudflare pages load
//!    `/cdn-cgi/challenge-platform/scripts/jsd/main.js` (JavaScript detections) on a 200.
//!
//! Only the first [`SCAN_BYTES`] of the body are looked at, lower-cased.

use http::StatusCode;
use vc_core::fetch::Exchange;

/// How much of the body is scanned.
pub const SCAN_BYTES: usize = 256 * 1024;

/// A short page (bytes) whose title asks for verification may be an interstitial.
pub const SHORT_PAGE_BYTES: usize = 16 * 1024;

/// Provider interstitial markers (lower-case substrings), with the provider name.
pub const PROVIDER_MARKERS: &[(&str, &str)] = &[
    ("cloudflare", "cf_chl_opt"),
    ("cloudflare", "cf-browser-verification"),
    ("cloudflare", "<title>just a moment...</title>"),
    (
        "cloudflare",
        "<title>attention required! | cloudflare</title>",
    ),
    ("akamai", "errors.edgesuite.net"),
    ("akamai", "errors&#46;edgesuite&#46;net"),
    ("akamai", "/_sec/cp_challenge/"),
    ("akamai", "sec-if-cpt-container"),
    ("datadome", "captcha-delivery.com"),
    ("imperva", "_incapsula_resource"),
    ("imperva", "incapsula incident id"),
    ("perimeterx", "id=\"px-captcha\""),
    ("perimeterx", "captcha.px-cdn.net"),
    ("perimeterx", "_pxcaptcha"),
];

/// Cloudflare's challenge-platform path. Also referenced by the JavaScript-detections loader on
/// ordinary pages, so on its own it proves nothing (see [`detect`]).
pub const CF_PLATFORM_MARKER: &str = "/cdn-cgi/challenge-platform/";

/// Captcha widget markers (lower-case substrings).
pub const CAPTCHA_MARKERS: &[&str] = &[
    "g-recaptcha",
    "google.com/recaptcha/",
    "recaptcha.net/recaptcha/",
    "h-captcha",
    "hcaptcha.com/1/api.js",
    "cf-turnstile",
    "challenges.cloudflare.com/turnstile",
];

/// Title words of a verification page.
const VERIFY_TITLE_WORDS: &[&str] = &[
    "verify",
    "verification",
    "human",
    "robot",
    "captcha",
    "security check",
    "checking your browser",
    "one more step",
    "access denied",
    "please wait",
    "just a moment",
    "attention required",
];

fn header_is(ex: &Exchange, name: &str, pred: impl Fn(&str) -> bool) -> bool {
    ex.response_headers
        .get_all(name)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .any(|v| pred(&v.trim().to_ascii_lowercase()))
}

fn title(body: &str) -> Option<&str> {
    let start = body.find("<title")?;
    let rest = &body[start..];
    let open = rest.find('>')? + 1;
    let end = rest[open..].find("</title>")?;
    Some(rest[open..open + end].trim())
}

/// Which provider (or `captcha`/`header`) flagged the exchange, if any.
pub fn detect(ex: &Exchange) -> Option<&'static str> {
    let status = ex.status;
    if header_is(ex, "cf-mitigated", |v| v == "challenge") {
        return Some("cloudflare");
    }
    if header_is(ex, "x-amzn-waf-action", |v| {
        v == "captcha" || v == "challenge"
    }) {
        return Some("aws_waf");
    }
    if status.is_client_error() && ex.response_headers.contains_key("x-datadome") {
        return Some("datadome");
    }
    let n = ex.body.len().min(SCAN_BYTES);
    let body = String::from_utf8_lossy(&ex.body[..n]).to_ascii_lowercase();
    if let Some((provider, _)) = PROVIDER_MARKERS.iter().find(|(_, m)| body.contains(m)) {
        return Some(provider);
    }
    let verify_title =
        || title(&body).is_some_and(|t| VERIFY_TITLE_WORDS.iter().any(|w| t.contains(w)));
    if body.contains(CF_PLATFORM_MARKER)
        && (matches!(
            status,
            StatusCode::FORBIDDEN | StatusCode::SERVICE_UNAVAILABLE
        ) || verify_title())
    {
        return Some("cloudflare");
    }
    if CAPTCHA_MARKERS.iter().any(|m| body.contains(m)) {
        let blocking = matches!(
            status,
            StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::TOO_MANY_REQUESTS
                | StatusCode::SERVICE_UNAVAILABLE
        );
        let short_verify = ex.body.len() <= SHORT_PAGE_BYTES && verify_title();
        if blocking || short_verify {
            return Some("captcha");
        }
    }
    None
}
