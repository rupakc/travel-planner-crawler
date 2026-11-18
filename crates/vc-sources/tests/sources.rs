//! Spec loading/validation over the real `sources/` directory, and seeding (frontier rows,
//! the `sources` table on a Postgres testcontainer).

mod common;

use std::sync::Arc;

use common::repo_root;
use vc_core::frontier::Frontier;
use vc_core::licence::LicenceClass;
use vc_core::media::{FetchMode, MediaType};
use vc_core::sources::Method;
use vc_sources::{ValidateOpts, adapters, validate};

#[test]
fn all_sources_yaml_load_and_validate() {
    let dir = repo_root().join("sources");
    let specs = vc_sources::load_dir(&dir).unwrap();
    let files = std::fs::read_dir(&dir)
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "yaml")
        })
        .count();
    assert_eq!(specs.len(), files);
    assert!(specs.len() >= 100);

    let denylist = vc_sources::load_denylist(&repo_root().join("policy/denylist.yaml")).unwrap();
    let ids = adapters::ids();
    let mut used = std::collections::BTreeSet::new();
    for s in &specs {
        let v = validate(
            s,
            &ValidateOpts {
                file_stem: Some(&s.id.0),
                adapters: &ids,
                denylist: Some(&denylist),
            },
        );
        assert!(v.is_ok(), "{}: {:?}", s.id.0, v.errors);
        if let Some(a) = &s.adapter {
            used.insert(a.as_str());
        }
        if s.licence.class == LicenceClass::Restricted {
            assert!(!s.enabled, "{}", s.id.0);
        }
    }
    // Every built-in adapter is used by a source, and every adapter a source names exists.
    assert_eq!(used.into_iter().collect::<Vec<_>>(), {
        let mut v = ids.clone();
        v.sort();
        v
    });
}

fn base() -> vc_core::sources::SourceSpec {
    vc_sources::parse_spec(
        "id: test-src\nname: Test\ncategory: tips\nsource_class: editorial\n\
         licence: {class: facts_only, attribution: Test}\nmethod: rss\n\
         feeds: [https://example.org/travel/feed]\ncadence: daily\n\
         relevance: {sections: [tips], why: Travel tips from a test feed for the Tips section.}\n",
    )
    .unwrap()
}

fn errors(s: &vc_core::sources::SourceSpec) -> Vec<String> {
    let ids = adapters::ids();
    validate(
        s,
        &ValidateOpts {
            file_stem: Some("test-src"),
            adapters: &ids,
            denylist: None,
        },
    )
    .errors
}

#[test]
fn relevance_is_required_and_checked() {
    use vc_core::sources::AppSection;
    assert!(errors(&base()).is_empty());

    let mut s = base();
    s.relevance = None;
    assert!(
        errors(&s)[0].contains("relevance is missing"),
        "{:?}",
        errors(&s)
    );
    s.enabled = false;
    assert!(
        errors(&s)[0].contains("relevance is missing"),
        "disabled sources need it too"
    );

    let mut s = base();
    s.relevance.as_mut().unwrap().sections.clear();
    assert!(errors(&s)[0].contains("sections is empty"));
    let mut s = base();
    s.relevance.as_mut().unwrap().sections = vec![AppSection::Tips, AppSection::Tips];
    assert!(errors(&s)[0].contains("duplicate tips"));
    let mut s = base();
    s.relevance.as_mut().unwrap().why = "  too short   ".into();
    assert!(errors(&s)[0].contains("too short"));
    let mut s = base();
    s.relevance.as_mut().unwrap().why = "Line one is long enough\nand line two".into();
    assert!(errors(&s)[0].contains("one line"));

    // Sections are a closed list; unknown spellings do not parse.
    assert!(
        vc_sources::parse_spec(
            "id: test-src\nname: T\ncategory: x\nsource_class: reference\n\
             licence: {class: open, attribution: T}\nmethod: html\ncadence: daily\n\
             relevance: {sections: [nightlife], why: Nightlife listings for travellers here.}\n"
        )
        .is_err()
    );
    assert!(
        vc_sources::parse_spec(
            "id: test-src\nname: T\ncategory: x\nsource_class: reference\n\
             licence: {class: open, attribution: T}\nmethod: html\ncadence: daily\n\
             relevance: {sections: [tips], why: Tips for travellers from this site., extra: 1}\n"
        )
        .is_err(),
        "relevance is strict"
    );
}

#[test]
fn news_and_blog_feeds_must_be_travel_focused() {
    let feed = |category: &str, url: &str| {
        let mut s = base();
        s.category = category.into();
        s.feeds = vec![url.parse().unwrap()];
        s
    };
    for (cat, url) in [
        ("news", "https://www.cnn.com/rss/edition.rss"),
        ("news", "https://www.example-news.com/world/feed"),
        ("blogs", "https://my-cooking-blog.example/feed/"),
        (
            "news",
            "https://www.cnbc.com/id/100003114/device/rss/rss.html",
        ),
    ] {
        let e = errors(&feed(cat, url));
        assert!(
            e.iter().any(|e| e.contains("not travel-focused")),
            "{url}: {e:?}"
        );
    }
    for (cat, url) in [
        ("news", "http://rss.cnn.com/rss/edition_travel.rss"),
        (
            "news",
            "https://www.euronews.com/rss?level=vertical&name=travel",
        ),
        (
            "news",
            "https://rss.nytimes.com/services/xml/rss/nyt/Travel.xml",
        ),
        (
            "news",
            "https://www.cnbc.com/id/10000739/device/rss/rss.html",
        ),
        ("news", "https://www.wanderlust.co.uk/rss"),
        ("blogs", "https://www.nomadicmatt.com/feed/"),
        ("events", "https://www.cnn.com/rss/edition.rss"),
    ] {
        let e = errors(&feed(cat, url));
        assert!(e.is_empty(), "{url}: {e:?}");
    }
    // A disabled feed is never fetched: the gate does not apply.
    let mut s = feed("news", "https://www.cnn.com/rss/edition.rss");
    s.enabled = false;
    assert!(errors(&s).is_empty());
}

/// Sustainable and budget travel publications (2026-11-18) pass the gate; FeedBurner is
/// travel-focused only under Instinct Voyageur's feed path.
#[test]
fn sustainable_and_budget_travel_publications_pass_the_gate() {
    let feed = |category: &str, url: &str| {
        let mut s = base();
        s.category = category.into();
        s.feeds = vec![url.parse().unwrap()];
        s
    };
    for (cat, url) in [
        ("blogs", "https://ecophiles.com/feed/"),
        ("news", "https://www.aeroroutes.com/eng?format=rss"),
        ("blogs", "https://www.mexicodesconocido.com.mx/feed"),
        ("blogs", "https://www.eurocheapo.com/blog/feed/"),
        ("blogs", "https://tokyocheapo.com/feed/"),
        ("blogs", "https://www.pointhacks.com.au/feed/"),
        ("blogs", "https://coasttocoast.co.za/feed/"),
        (
            "blogs",
            "https://www.melhoresdestinos.com.br/sitemap_index.xml",
        ),
        ("blogs", "https://feeds.feedburner.com/InstinctVoyageur"),
    ] {
        let e = errors(&feed(cat, url));
        assert!(e.is_empty(), "{url}: {e:?}");
    }
    for url in [
        "https://feeds.feedburner.com/SomeCookingBlog",
        "https://thesmartlocal.com/feed/",
    ] {
        let e = errors(&feed("blogs", url));
        assert!(
            e.iter().any(|e| e.contains("not travel-focused")),
            "{url}: {e:?}"
        );
    }
}

/// UNESCO's terms ban automated access (2026-11-18): every UNESCO host is denied, while
/// hosts next to new denylist entries stay allowed.
#[test]
fn unesco_is_denied_and_neighbours_of_new_entries_are_not() {
    let dl = vc_sources::load_denylist(&repo_root().join("policy/denylist.yaml")).unwrap();
    let url = |u: &str| vc_core::CanonicalUrl::parse(u).unwrap();
    for denied in [
        "https://whc.unesco.org/en/list/xml/",
        "https://www.unesco.org/en/terms-use",
        "https://www.visitfinland.com/en/",
        "https://gtfs.gis.flix.tech/gtfs_generic_eu.zip",
        "https://www.flixbus.de/",
        "https://www.nyc.gov/site/dcla/",
        "https://data.buenosaires.gob.ar/",
        "https://lnt.org/why/7-principles/",
    ] {
        assert!(
            vc_sources::deny_match(&dl, &url(denied)).is_some(),
            "{denied}"
        );
    }
    for ok in [
        "https://datahub.visitfinland.com/",
        "https://transport.data.gouv.fr/resources/11681",
        "https://www.nycgovparks.org/xml/events_300_rss.xml",
        "https://turismo.buenosaires.gob.ar/en",
        "https://www.smithsonianmag.com/travel/",
        "https://www.green-tourism.com/our-members/",
    ] {
        assert!(vc_sources::deny_match(&dl, &url(ok)).is_none(), "{ok}");
    }
}

#[test]
fn travel_state_gov_is_denied_but_the_advisories_api_is_not() {
    let dl = vc_sources::load_denylist(&repo_root().join("policy/denylist.yaml")).unwrap();
    let url = |u: &str| vc_core::CanonicalUrl::parse(u).unwrap();
    assert!(
        vc_sources::deny_match(&dl, &url("https://travel.state.gov/_res/rss/TAsTWs.xml")).is_some()
    );
    for ok in [
        "https://cadataapi.state.gov/api/TravelAdvisories",
        "https://www.state.gov/",
    ] {
        assert!(vc_sources::deny_match(&dl, &url(ok)).is_none(), "{ok}");
    }
    let specs = vc_sources::load_dir(&repo_root().join("sources")).unwrap();
    let on = |id: &str| specs.iter().find(|s| s.id.0 == id).unwrap().enabled;
    assert!(!on("state-dept-advisories-rss") && !on("state-dept-country-pages"));
    assert!(on("state-dept-advisories-api"));
}

#[test]
fn validation_rules() {
    assert!(errors(&base()).is_empty());
    assert!(
        vc_sources::parse_spec("id: x\nbogus: 1\n").is_err(),
        "unknown fields rejected"
    );

    let mut s = base();
    s.licence.class = LicenceClass::Restricted;
    assert!(errors(&s)[0].contains("restricted"));
    s.enabled = false;
    assert!(errors(&s).is_empty());

    let mut s = base();
    s.min_delay_s = 1;
    assert!(errors(&s)[0].contains("min_delay_s"));
    let mut s = base();
    s.daily_budget = 10_000;
    assert!(errors(&s)[0].contains("daily_budget"));

    let mut s = base();
    s.kinds = vec![vc_core::KindId::new("visa_rule")];
    assert!(
        errors(&s)[0].contains("official"),
        "high-stakes kind from editorial"
    );
    let mut s = base();
    s.llm_kinds = vec![vc_core::KindId::new("emergency_numbers")];
    assert!(!errors(&s).is_empty());

    let mut s = base();
    s.method = Method::Api;
    s.adapter = Some("no_such_adapter".into());
    s.seeds = vec!["https://example.org/api".parse().unwrap()];
    assert!(errors(&s)[0].contains("not registered"));

    let mut s = base();
    s.feeds.clear();
    assert!(!errors(&s).is_empty());

    let mut s = base();
    s.include = vec!["(".into()];
    assert!(errors(&s)[0].contains("regex"));

    let mut s = base();
    s.needs_key = true;
    assert!(errors(&s)[0].contains("key_secret"));

    let ids = adapters::ids();
    let v = validate(
        &base(),
        &ValidateOpts {
            file_stem: Some("other"),
            adapters: &ids,
            denylist: None,
        },
    );
    assert!(v.errors[0].contains("file name"));

    let mut s = base();
    s.kinds = vec![vc_core::KindId::new("tip"), vc_core::KindId::new("tip")];
    let v = validate(
        &s,
        &ValidateOpts {
            file_stem: None,
            adapters: &ids,
            denylist: None,
        },
    );
    assert!(v.is_ok());
    assert!(v.warnings[0].contains("duplicate"));
}

#[test]
fn denylisted_seed_is_an_error() {
    let dl: vc_core::policy::Denylist = serde_norway::from_str(
        "version: 1\nentries:\n\
         - {host: booking.com, reason: ToS, source: brand, except_hosts: [distribution-xml.booking.com]}\n\
         - {host: 'expedia.*', reason: brand, source: brand}\n\
         - {host: google.com, path_prefix: /travel, reason: ToS, source: seed:x}\n",
    )
    .unwrap();
    let hit =
        |u: &str| vc_sources::deny_match(&dl, &vc_core::CanonicalUrl::parse(u).unwrap()).is_some();
    assert!(hit("https://www.booking.com/x"));
    assert!(!hit("https://distribution-xml.booking.com/api"));
    assert!(hit("https://www.expedia.co.uk/"));
    assert!(!hit("https://notexpedia.com/"));
    assert!(hit("https://www.google.com/travel/flights"));
    assert!(!hit("https://www.google.com/maps"));

    let mut s = base();
    s.feeds = vec!["https://www.booking.com/feed".parse().unwrap()];
    let ids = adapters::ids();
    let v = validate(
        &s,
        &ValidateOpts {
            file_stem: None,
            adapters: &ids,
            denylist: Some(&dl),
        },
    );
    assert!(v.errors[0].contains("denylisted"));
}

#[tokio::test]
async fn seed_frontier_enqueues_adapter_rows() {
    let specs = vc_sources::load_dir(&repo_root().join("sources")).unwrap();
    let ids = adapters::ids();
    let ecb = specs.iter().find(|s| s.id.0 == "ecb-eurofxref").unwrap();
    let rows = vc_sources::seed_urls(ecb, &ids).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].fetch_mode, FetchMode::Adapter("ecb_fx".into()));
    let blog = specs
        .iter()
        .find(|s| s.method == Method::Rss && s.enabled)
        .unwrap();
    assert!(
        vc_sources::seed_urls(blog, &ids)
            .unwrap()
            .iter()
            .all(|r| r.fetch_mode == FetchMode::Http)
    );
    let sm = specs.iter().find(|s| !s.sitemaps.is_empty() && s.enabled);
    if let Some(sm) = sm {
        assert!(
            vc_sources::seed_urls(sm, &ids)
                .unwrap()
                .iter()
                .any(|r| r.expect == Some(MediaType::Sitemap))
        );
    }
    for s in specs
        .iter()
        .filter(|s| !s.enabled || !s.licence.class.ingestible())
    {
        assert!(
            vc_sources::seed_urls(s, &ids).unwrap().is_empty(),
            "{}",
            s.id.0
        );
    }
    for s in specs
        .iter()
        .filter(|s| matches!(s.method, Method::Api | Method::Dump) && s.adapter.is_none())
    {
        assert!(
            vc_sources::seed_urls(s, &ids).unwrap().is_empty(),
            "{}",
            s.id.0
        );
    }

    let clock = Arc::new(testkit::FakeClock::new(common::t0()));
    let f = testkit::MemFrontier::new(clock);
    let n = vc_sources::seed_frontier(&f, &specs).await.unwrap();
    assert!(n > 50);
    assert_eq!(
        vc_sources::seed_frontier(&f, &specs).await.unwrap(),
        0,
        "idempotent"
    );
    assert!(f.claim(&[0], 5).await.unwrap().is_some());
}

#[tokio::test]
async fn seed_sources_upserts_into_postgres() {
    let pool = testkit::pg().await;
    let mut specs = vc_sources::load_dir(&repo_root().join("sources")).unwrap();
    let r = vc_sources::seed_sources(&pool, &specs, true).await.unwrap();
    assert_eq!(r.inserted as usize, specs.len());
    let r = vc_sources::seed_sources(&pool, &specs, true).await.unwrap();
    assert_eq!(
        (r.inserted, r.updated, r.unchanged as usize),
        (0, 0, specs.len())
    );

    let i = specs
        .iter()
        .position(|s| s.id.0 == "ecb-eurofxref")
        .unwrap();
    specs[i].notes = Some("changed".into());
    let gone = specs.pop().unwrap();
    let r = vc_sources::seed_sources(&pool, &specs, true).await.unwrap();
    assert_eq!((r.updated, r.disabled), (1, 1));
    let (enabled, spec): (bool, serde_json::Value) =
        sqlx::query_as("SELECT enabled, spec FROM sources WHERE source_id = $1")
            .bind(&gone.id.0)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!enabled);
    assert_eq!(spec["id"], gone.id.0);
    let (notes,): (Option<String>,) =
        sqlx::query_as("SELECT spec->>'notes' FROM sources WHERE source_id = 'ecb-eurofxref'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(notes.as_deref(), Some("changed"));
}

/// `sources/ramp-2.txt` (go-live ramp, step 2): ramp-1 first, then only sources that exist,
/// are enabled, validate (relevance, denylist, politeness) and need no adapter or key.
#[test]
fn ramp_2_ids_exist_are_enabled_and_validate() {
    let ids_of = |name: &str| -> Vec<String> {
        std::fs::read_to_string(repo_root().join("sources").join(name))
            .unwrap()
            .lines()
            .map(|l| l.split('#').next().unwrap_or("").trim().to_string())
            .filter(|l| !l.is_empty())
            .collect()
    };
    let (r1, r2) = (ids_of("ramp-1.txt"), ids_of("ramp-2.txt"));
    assert!(r2.len() > r1.len());
    assert_eq!(&r2[..r1.len()], &r1[..], "ramp-2 starts with ramp-1");
    let mut seen = std::collections::BTreeSet::new();
    let denylist = vc_sources::load_denylist(&repo_root().join("policy/denylist.yaml")).unwrap();
    let ids = adapters::ids();
    let specs = vc_sources::load_dir(&repo_root().join("sources")).unwrap();
    for id in &r2 {
        assert!(seen.insert(id.clone()), "{id} listed twice");
        let s = specs
            .iter()
            .find(|s| s.id.0 == *id)
            .unwrap_or_else(|| panic!("{id}: no sources/{id}.yaml"));
        assert!(s.enabled, "{id} is disabled");
        let v = validate(
            s,
            &ValidateOpts {
                file_stem: Some(id),
                adapters: &ids,
                denylist: Some(&denylist),
            },
        );
        assert!(v.is_ok(), "{id}: {:?}", v.errors);
        assert!(!s.needs_key, "{id} needs a key");
    }
    for id in &r2[r1.len()..] {
        let s = specs.iter().find(|s| s.id.0 == *id).unwrap();
        assert!(
            matches!(s.method, Method::Html | Method::Rss | Method::Sitemap),
            "{id}: batch-2 ramp sources need no adapter"
        );
    }
}
