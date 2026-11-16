//! `FilePlaceResolver` on the committed GeoNames export, and LLM disambiguation.

mod common;

use std::sync::{Arc, OnceLock};

use chrono::{TimeZone, Utc};
use serde_json::json;
use testkit::{FakeClock, FakeLlm, MemLedger};
use vc_config::ExtractConfig;
use vc_core::Result;
use vc_core::extract::{GeoPoint, PlaceMatch, PlaceQuery, PlaceResolver};
use vc_core::llm::Usd;
use vc_extract::{FilePlaceResolver, LlmPlaceDisambiguator};

fn data_cfg() -> ExtractConfig {
    let d = concat!(env!("CARGO_MANIFEST_DIR"), "/data");
    ExtractConfig {
        places_file: format!("{d}/places.tsv.gz"),
        countries_file: format!("{d}/countries.tsv.gz"),
        admin1_file: format!("{d}/admin1.tsv.gz"),
        ..ExtractConfig::default()
    }
}

fn resolver() -> &'static FilePlaceResolver {
    static R: OnceLock<FilePlaceResolver> = OnceLock::new();
    R.get_or_init(|| FilePlaceResolver::load(&data_cfg()).expect("load place data"))
}

fn q(name: &str, country: Option<&str>) -> PlaceQuery {
    PlaceQuery {
        name: name.into(),
        country: country.map(Into::into),
        near: None,
        lang: None,
    }
}

async fn top(name: &str, country: Option<&str>) -> Option<PlaceMatch> {
    resolver()
        .resolve(&q(name, country))
        .await
        .unwrap()
        .into_iter()
        .next()
}

#[test]
fn data_files_are_small_and_attributed() {
    let d = concat!(env!("CARGO_MANIFEST_DIR"), "/data");
    let total: u64 = ["places.tsv.gz", "countries.tsv.gz", "admin1.tsv.gz"]
        .iter()
        .map(|f| std::fs::metadata(format!("{d}/{f}")).unwrap().len())
        .sum();
    assert!(
        total < 15 * 1024 * 1024,
        "place data must stay under 15 MB compressed"
    );
    let notice = std::fs::read_to_string(format!("{d}/NOTICE")).unwrap();
    assert!(notice.contains(vc_extract::GEONAMES_ATTRIBUTION));
    assert!(resolver().len() > 150_000);
}

#[tokio::test]
async fn resolves_famous_places_and_namesakes() {
    let paris = top("Paris", None).await.unwrap();
    assert_eq!((paris.geoname_id, paris.country.as_str()), (2988507, "FR"));
    assert!(paris.score > 0.8);
    assert_eq!(top("Paris", Some("US")).await.unwrap().country, "US");
    assert_eq!(top("Zürich", None).await.unwrap().geoname_id, 2657896);
    assert_eq!(
        top("zurich", Some("Switzerland")).await.unwrap().geoname_id,
        2657896
    );
    assert_eq!(top("Lisboa", None).await.unwrap().geoname_id, 2267057);
    assert_eq!(
        top("Berlin", Some("Deutschland")).await.unwrap().geoname_id,
        2950159
    );
    assert_eq!(top("Portland, Oregon", None).await.unwrap().country, "US");
    assert_eq!(top("Kyoto, Japan", None).await.unwrap().country, "JP");
    let lisbon = top("Lisbon", None).await.unwrap();
    assert_eq!(lisbon.admin1.as_deref(), Some("Lisbon"));
    // A nearby point picks the namesake.
    let near = resolver()
        .resolve(&PlaceQuery {
            near: Some(GeoPoint {
                lat: 33.66,
                lon: -95.55,
            }),
            ..q("Paris", None)
        })
        .await
        .unwrap();
    assert_eq!(near[0].country, "US");
}

#[tokio::test]
async fn unknown_or_excluded_places_are_unresolved() {
    assert!(top("Atlantis-under-the-sea", None).await.is_none());
    assert!(top("", None).await.is_none());
    assert!(top("Paris", Some("JP")).await.is_none());
}

/// A resolver returning two close candidates.
struct Two;

#[async_trait::async_trait]
impl PlaceResolver for Two {
    async fn resolve(&self, _q: &PlaceQuery) -> Result<Vec<PlaceMatch>> {
        let m = |id, s| PlaceMatch {
            geoname_id: id,
            name: "Springfield".into(),
            country: "US".into(),
            admin1: None,
            point: GeoPoint { lat: 0.0, lon: 0.0 },
            score: s,
        };
        Ok(vec![m(1, 0.5), m(2, 0.45)])
    }
}

fn ledger(cap: f64) -> Arc<MemLedger> {
    let clock = Arc::new(FakeClock::new(
        Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap(),
    ));
    Arc::new(MemLedger::new(
        clock,
        Usd::from_dollars(cap),
        Usd::from_dollars(cap),
    ))
}

#[tokio::test]
async fn disambiguation_only_picks_a_candidate() {
    let llm = Arc::new(FakeLlm::new());
    let led = ledger(1.0);
    let d = LlmPlaceDisambiguator::new(
        Arc::new(Two),
        llm.clone(),
        led.clone(),
        &ExtractConfig::default(),
    );
    llm.push_json(json!({"geoname_id": 2}));
    let r = d.resolve(&q("Springfield", None)).await.unwrap();
    assert_eq!(r[0].geoname_id, 2);
    assert!(r[0].score > r[1].score);
    assert_eq!(llm.requests()[0].purpose, vc_core::llm::Purpose::Place);
    // An id outside the candidates (injected) is ignored.
    llm.push_json(json!({"geoname_id": 666}));
    let r = d.resolve(&q("Springfield", None)).await.unwrap();
    assert_eq!(r[0].geoname_id, 1);
    assert_eq!(led.open_reservations(), 0);
}

#[tokio::test]
async fn disambiguation_needs_budget() {
    let llm = Arc::new(FakeLlm::new());
    let d = LlmPlaceDisambiguator::new(
        Arc::new(Two),
        llm.clone(),
        ledger(0.0),
        &ExtractConfig::default(),
    );
    let r = d.resolve(&q("Springfield", None)).await.unwrap();
    assert_eq!(r[0].geoname_id, 1);
    assert_eq!(llm.calls(), 0);
    // Disabled LLM: unchanged.
    let llm = Arc::new(FakeLlm::new());
    llm.set_enabled(false);
    let d = LlmPlaceDisambiguator::new(Arc::new(Two), llm, ledger(1.0), &ExtractConfig::default());
    assert_eq!(
        d.resolve(&q("Springfield", None)).await.unwrap()[0].geoname_id,
        1
    );
}
