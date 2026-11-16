//! Denylist matching against the real `policy/denylist.yaml`: hosts + subdomains, `<brand>.*`
//! under any public suffix, `path_prefix`, `except_hosts`.

mod common;

use common::{policy, url};
use vc_config::PolicyConfig;
use vc_core::policy::{DenyEntry, Denylist, Policy};
use vc_policy::{SubresourceAllowlist, VoyagerPolicy};

fn denied(u: &str) -> Option<String> {
    policy().denied(&url(u)).map(|r| r.rule)
}

#[test]
fn host_entries_cover_subdomains_only() {
    assert_eq!(denied("https://booking.com/"), Some("booking.com".into()));
    assert_eq!(
        denied("https://www.booking.com/hotel/pt/x.html"),
        Some("booking.com".into())
    );
    assert_eq!(
        denied("https://M.Booking.COM./"),
        Some("booking.com".into())
    );
    assert_eq!(denied("https://notbooking.com/"), None);
    assert_eq!(denied("https://booking.com.evil.example/"), None);
    assert_eq!(
        denied("https://reddit.com/r/travel"),
        Some("reddit.com".into())
    );
    assert_eq!(denied("https://old.reddit.com/"), Some("reddit.com".into()));
}

#[test]
fn brand_wildcard_matches_any_public_suffix() {
    for u in [
        "https://expedia.com/",
        "https://www.expedia.co.uk/Hotels",
        "https://expedia.de/",
        "https://www.expedia.com.au/",
        "https://www.skyscanner.net/",
        "https://www.skyscanner.co.in/",
        "https://kayak.co.jp/",
        "https://www.tripadvisor.co.nz/",
        "https://www.trivago.it/",
    ] {
        assert!(denied(u).is_some(), "{u} must be denied");
    }
    assert_eq!(
        denied("https://expedia.co.uk/").as_deref(),
        Some("expedia.*")
    );
    // The brand must be the registrable label, not any label.
    for u in [
        "https://expedia.example.com/",
        "https://myexpedia.com/",
        "https://expedia-tips.blogspot.com/",
        "https://www.example.co.uk/expedia",
    ] {
        assert_eq!(denied(u), None, "{u} must not be denied");
    }
}

#[test]
fn path_prefix_only_denies_that_path() {
    assert_eq!(
        denied("https://www.google.com/travel/flights"),
        Some("google.com/travel".into())
    );
    assert_eq!(
        denied("https://google.com/flights?x=1"),
        Some("google.com/flights".into())
    );
    assert_eq!(denied("https://www.google.com/maps"), None);
    assert_eq!(
        denied("https://www.eventbrite.com/rss/x"),
        Some("eventbrite.com/rss/".into())
    );
    assert_eq!(
        denied("https://www.eventbrite.com/events/rss/feed"),
        Some("eventbrite.com/events/rss/".into())
    );
    assert_eq!(denied("https://www.eventbrite.com/e/some-event"), None);
    assert_eq!(
        denied("https://www.lonelyplanet.com/thorntree/forums"),
        Some("lonelyplanet.com/thorntree".into())
    );
    assert_eq!(denied("https://www.lonelyplanet.com/portugal/lisbon"), None);
    // Case-sensitive, canonical path.
    assert_eq!(denied("https://www.google.com/TRAVEL"), None);
}

#[test]
fn except_hosts_are_not_covered() {
    assert_eq!(denied("https://developers.booking.com/demand/docs"), None);
    assert_eq!(
        denied("https://demandapi.booking.com/3.1/accommodations"),
        None
    );
    assert_eq!(denied("https://eu.demandapi.booking.com/"), None);
    assert_eq!(denied("https://api.viator.com/partner"), None);
    assert_eq!(denied("https://partner.getyourguide.com/"), None);
    assert_eq!(denied("https://api.getyourguide.com/1/tours"), None);
    // The rest of the organisation stays denied.
    assert!(denied("https://www.viator.com/tours").is_some());
    assert!(denied("https://www.getyourguide.de/").is_some());
    assert!(denied("https://secure.booking.com/").is_some());
}

#[test]
fn deny_reason_carries_yaml_reason() {
    let r = policy()
        .denied(&url("https://www.airbnb.fr/rooms/1"))
        .unwrap();
    assert_eq!(r.rule, "airbnb.*");
    assert!(r.reason.contains("ToS"));
}

#[test]
fn ip_hosts_never_match_brands() {
    assert_eq!(denied("http://93.184.216.34/"), None);
}

fn allow() -> SubresourceAllowlist {
    serde_norway::from_str("version: 1\nhosts: []\n").unwrap()
}

fn entry(host: &str) -> DenyEntry {
    DenyEntry {
        host: host.into(),
        reason: "r".into(),
        source: "brand".into(),
        path_prefix: None,
        except_hosts: vec![],
    }
}

#[test]
fn invalid_denylists_are_rejected() {
    let bad = [
        entry("exa*mple.com"),
        entry("foo.bar.*"),
        entry("https://x.com"),
        entry(""),
        DenyEntry {
            path_prefix: Some("travel".into()),
            ..entry("google.com")
        },
    ];
    for e in bad {
        let list = Denylist {
            version: 1,
            entries: vec![e.clone()],
        };
        assert!(
            VoyagerPolicy::from_parts(PolicyConfig::default(), list, allow()).is_err(),
            "{e:?}"
        );
    }
    let v2 = Denylist {
        version: 2,
        entries: vec![],
    };
    assert!(VoyagerPolicy::from_parts(PolicyConfig::default(), v2, allow()).is_err());
}

#[test]
fn missing_policy_files_fail_startup() {
    let cfg = PolicyConfig {
        denylist_path: "/nonexistent/denylist.yaml".into(),
        ..PolicyConfig::default()
    };
    assert!(VoyagerPolicy::new(cfg).is_err());
}

#[test]
fn subresource_allowlist_is_exact_host() {
    let p = policy();
    assert!(p.subresource_host_allowed("cdnjs.cloudflare.com"));
    assert!(p.subresource_host_allowed("CDN.jsdelivr.net."));
    assert!(!p.subresource_host_allowed("evil.cdnjs.cloudflare.com"));
    assert!(!p.subresource_host_allowed("fonts.gstatic.com"));
    assert!(!p.subresource_host_allowed("www.booking.com"));
}

#[test]
fn host_denied_ignores_path_entries() {
    let p = policy();
    assert!(p.host_denied("www.expedia.de"));
    assert!(!p.host_denied("www.google.com"));
    assert!(!p.host_denied("developers.booking.com"));
}

#[test]
fn subresource_types_enforced() {
    let p = policy();
    assert!(p.subresource_allowed("cdnjs.cloudflare.com", "script"));
    assert!(p.subresource_allowed("CDNJS.cloudflare.com.", "Stylesheet"));
    assert!(p.subresource_allowed("cdn.jsdelivr.net", "xhr"));
    assert!(!p.subresource_allowed("cdnjs.cloudflare.com", "image"));
    assert!(!p.subresource_allowed("cdnjs.cloudflare.com", "document"));
    assert!(p.subresource_allowed("ajax.googleapis.com", "script"));
    assert!(!p.subresource_allowed("ajax.googleapis.com", "xhr"));
    assert!(!p.subresource_allowed("ajax.googleapis.com", "fetch"));
    assert!(p.subresource_allowed("fonts.googleapis.com", "stylesheet"));
    assert!(!p.subresource_allowed("fonts.googleapis.com", "script"));
    assert!(!p.subresource_allowed("fonts.gstatic.com", "font"));
    assert!(!p.subresource_allowed("evil.cdnjs.cloudflare.com", "script"));
    assert!(!p.subresource_allowed("www.booking.com", "script"));
}
