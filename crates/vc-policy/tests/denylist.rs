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

/// Batch-2 additions (research 2026-11-17): terms bans, bot walls and a hijacked domain.
#[test]
fn batch2_entries_are_denied() {
    for u in [
        "https://library.traveldoc.aero/",
        "https://timaticweb2.com/",
        "https://www.ica.gov.sg/",
        "https://www.henleyglobal.com/",
        "https://www.passportindex.org/",
        "https://bus-data.dft.gov.uk/",
        "https://www.transit.dot.gov/",
        "https://www.mofa.go.jp/",
        "https://www.dha.gov.za/",
        "https://www.mpi.govt.nz/",
        "https://www.usembassy.gov/",
        "https://fr.usembassy.gov/",
        "https://www.ricksteves.com/",
        "https://www.welcomenepal.com/",
        "https://www.timeout.com/london",
        "https://guide.michelin.com/en",
        "https://www.visitbritain.com/",
        "https://download.gtfs.de/germany/fv_free/latest.zip",
        "https://static.data.gov.hk/td/x.zip",
        "https://www.iata.org/",
        // scam-safety research 2026-11-17: 403 to an honest bot
        "https://www.worldpackers.com/articles/travel-scams",
        "https://www.moneycrashers.com/travel-scams/",
        "https://www.eurochange.co.uk/travel-money/guides",
        "https://travelersunited.org/",
        "https://consumer.ftc.gov/articles/travel-scams",
        // bot walls met in production, 2026-11-17
        "https://taxation-customs.ec.europa.eu/travelling_en",
        "https://www.mfat.govt.nz/en/embassies",
        "https://mfat.govt.nz/",
        "https://secretldn.com/feed/",
        "https://www.secretldn.com/",
        // bot walls met in production, 2026-11-18
        "https://philippines.travel/sitemap.xml",
        "https://www.philippines.travel/",
        "https://www.dubaicustoms.gov.ae/en/",
        "https://dubaicustoms.gov.ae/",
        // alternatives research 2026-11-18: refused, challenged or terms forbid copying
        "https://www.visitoslo.com/sitemaps-1-sitemap.xml",
        "https://dados.gov.br/",
        "https://www.sanparks.org/",
        "https://www.biosecurity.govt.nz/",
        "https://www.data.govt.nz/",
        "https://www.dubai.ae/",
        "https://www.us.emb-japan.go.jp/",
        "https://intramuros.gov.ph/",
        "https://www.agenziadoganemonopoli.gov.it/",
        "https://www.newzealand.com/",
        "https://www.douane.gouv.fr/",
        "https://www.mfa.gov.sg/",
    ] {
        assert!(denied(u).is_some(), "{u} must be denied");
    }
}

/// Hosts the crawler uses must never be caught by a denylist entry (siblings and
/// look-alikes of denied hosts included).
#[test]
fn approved_hosts_stay_allowed() {
    for u in [
        "https://www.gov.uk/duty-free-goods",
        "https://www.gov.uk/api/content/foreign-travel-advice",
        "https://cadataapi.state.gov/api/TravelAdvisories",
        "https://data.international.gc.ca/travel-voyage/x.json",
        "https://www.canada.ca/en/immigration-refugees-citizenship/",
        "https://www.cbp.gov/travel/",
        "https://home-affairs.ec.europa.eu/",
        // europa.eu: only taxation-customs.ec.europa.eu is denied (2026-11-17)
        "https://www.ecb.europa.eu/stats/eurofxref/eurofxref-daily.xml",
        "https://travel-europe.europa.eu/en/etias",
        "https://home-affairs.ec.europa.eu/policies/schengen/visa-policy_en",
        "https://food.ec.europa.eu/animals/movement-pets_en",
        "https://eur-lex.europa.eu/eli/reg/2018/1806",
        "https://ec.europa.eu/eurostat/api/dissemination/statistics/1.0/data/prc_ppp_ind",
        "https://www.immigration.govt.nz/",
        "https://www.customs.govt.nz/",
        "https://www.customs.go.jp/english/",
        "https://www.maff.go.jp/aqs/english/",
        "https://www.nps.gov/sitemap.xml",
        "https://www.discoverireland.ie/",
        "https://whatson.melbourne.vic.gov.au/",
        "https://data.gov.hk/en-data/",
        "https://gtfs.de/",
        "https://chile.travel/",
        // scam-safety sources (2026-11-17); consumer.ftc.gov is denied, ftc.gov is not
        "https://travelerset.com/blog/common-travel-scams-and-how-to-avoid-them",
        "https://avoidtravelscam.com/resources/",
        "https://twopassportspacked.com/common-travel-scams/",
        "https://www.smartraveller.gov.au/news-and-updates/stop-check-protect-and-keep-scammers-out-your-travel-plans",
        "https://www.ftc.gov/",
        "https://reportfraud.ftc.gov/",
        // alternatives to bot-walled sites (2026-11-18); denied siblings stay apart
        "https://www.safetravel.govt.nz/destinations/japan",
        "https://europa.eu/youreurope/citizens/travel/carry/index_en.htm",
        "https://www.revenue.ie/en/customs/individuals/info-travelling-duty-free/index.aspx",
        "https://icp.gov.ae/sitemap.xml",
        "https://egymonuments.gov.eg/en/sitemap.aspx",
        "https://kws.go.ke/wp-sitemap-posts-park-1.xml",
        "https://dirco.gov.za/page-sitemap.xml",
        "https://www.capetown.gov.za/sitemap.xml",
        "https://cadataapi.state.gov/api/CountryTravelInformation",
        "https://api.ratings.food.gov.uk/Establishments",
        "https://data.cityofnewyork.us/resource/43nn-pn8j.json",
        "https://catalogue.data.govt.nz/",
        "https://www.customs.gov.sg/",
        "https://www.gov.ph/",
    ] {
        assert_eq!(denied(u), None, "{u} must stay allowed");
    }
}
