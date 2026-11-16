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
        "id: test-src\nname: Test\ncategory: blogs\nsource_class: editorial\n\
         licence: {class: facts_only, attribution: Test}\nmethod: rss\n\
         feeds: [https://example.org/feed]\ncadence: daily\n",
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
