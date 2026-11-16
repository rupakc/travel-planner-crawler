//! Adapter tests against recorded fixtures (`testkit/fixtures/vc-sources/`) served by
//! wiremock: `<adapter>_parses_fixture` and `<adapter>_writes_raw_first` for every built-in
//! adapter, plus streaming, lease-extension, redaction and politeness edge cases.

mod common;

use std::time::Duration;

use common::{Harness, assert_raw_first, fixture, spec, t0};
use vc_core::Error;
use vc_core::media::{FetchMode, MediaType};
use vc_core::sources::SourceAdapter;
use vc_core::store::{CaptureRole, SpoolPayload};
use vc_sources::adapters::{self, DumpSettings};
use wiremock::matchers::{method, path, query_param, query_param_contains};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok(body: Vec<u8>, ct: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", ct)
        .set_body_bytes(body)
}

async fn serve(server: &MockServer, p: &str, body: Vec<u8>, ct: &str) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(ok(body, ct))
        .mount(server)
        .await;
}

/// Runs `a` on `url` twice: once normally (asserting the raw-first event order) and once with a
/// failing sink (asserting nothing is enqueued and the run fails).
async fn raw_first_case(a: &dyn SourceAdapter, source: &str, url: &str, secrets: &[(&str, &str)]) {
    let mut h = Harness::new(spec(source), a.id(), url, t0()).await;
    for (k, v) in secrets {
        h.secrets.0.insert(k.to_string(), v.to_string());
    }
    h.run(a).await.unwrap();
    assert_raw_first(&h.events.all());
    assert!(h.sink.records.iter().all(|r| r.meta.payload_hash.is_some()));

    let mut h = Harness::new(spec(source), a.id(), url, t0()).await;
    for (k, v) in secrets {
        h.secrets.0.insert(k.to_string(), v.to_string());
    }
    h.sink.fail = true;
    let err = h.run(a).await.unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err:?}");
    assert!(
        !h.events.all().iter().any(|e| e.starts_with("enqueue")),
        "nothing may be derived when the spool append fails: {:?}",
        h.events.all()
    );
}

// ---------------------------------------------------------------------------------------
// ecb_fx

#[tokio::test]
async fn ecb_fx_parses_fixture() {
    let rates = adapters::ecb_fx::parse(&fixture("ecb_eurofxref_daily.xml")).unwrap();
    assert_eq!(rates.len(), 29);
    let usd = rates.iter().find(|r| r.currency == "USD").unwrap();
    assert_eq!((usd.date.as_str(), usd.rate), ("2026-11-13", 1.1052));

    let s = MockServer::start().await;
    serve(
        &s,
        "/stats/eurofxref/eurofxref-daily.xml",
        fixture("ecb_eurofxref_daily.xml"),
        "text/xml",
    )
    .await;
    let url = format!("{}/stats/eurofxref/eurofxref-daily.xml", s.uri());
    let mut h = Harness::new(spec("ecb-eurofxref"), "ecb_fx", &url, t0()).await;
    let rep = h.run(&adapters::ecb_fx::EcbFx).await.unwrap();
    assert_eq!((rep.requests, rep.records, rep.complete), (1, 1, true));
    let r = &h.sink.records[0];
    assert_eq!(r.meta.role, CaptureRole::Primary);
    assert_eq!(r.meta.media_type, MediaType::Xml);
    assert_eq!(r.meta.fetch_mode, FetchMode::Adapter("ecb_fx".into()));
    assert_eq!(r.meta.epoch, h.claim.permit.epoch());
    assert_eq!(r.meta.source_id.as_ref().unwrap().0, "ecb-eurofxref");
}

#[tokio::test]
async fn ecb_fx_writes_raw_first() {
    let s = MockServer::start().await;
    serve(&s, "/d.xml", fixture("ecb_eurofxref_daily.xml"), "text/xml").await;
    raw_first_case(
        &adapters::ecb_fx::EcbFx,
        "ecb-eurofxref",
        &format!("{}/d.xml", s.uri()),
        &[],
    )
    .await;
}

#[tokio::test]
async fn ecb_fx_garbage_is_spooled_then_rejected() {
    let s = MockServer::start().await;
    serve(&s, "/d.xml", b"<x/>".to_vec(), "text/xml").await;
    let mut h = Harness::new(
        spec("ecb-eurofxref"),
        "ecb_fx",
        &format!("{}/d.xml", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(
        h.run(&adapters::ecb_fx::EcbFx).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        h.sink.records.len(),
        1,
        "raw is kept even when parsing fails"
    );
}

// ---------------------------------------------------------------------------------------
// fcdo_content_api

async fn fcdo_server() -> MockServer {
    let s = MockServer::start().await;
    serve(
        &s,
        "/api/content/foreign-travel-advice",
        fixture("fcdo_index.json"),
        "application/json",
    )
    .await;
    serve(
        &s,
        "/api/content/foreign-travel-advice/france",
        fixture("fcdo_france.json"),
        "application/json",
    )
    .await;
    serve(
        &s,
        "/foreign-travel-advice.atom",
        fixture("fcdo_changes.atom"),
        "application/atom+xml",
    )
    .await;
    s
}

#[tokio::test]
async fn fcdo_content_api_parses_fixture() {
    use adapters::fcdo_content_api as f;
    let idx = f::parse_index(&fixture("fcdo_index.json")).unwrap();
    let mut slugs: Vec<_> = idx.iter().map(|c| c.slug.as_str()).collect();
    slugs.sort();
    assert_eq!(slugs, ["france", "japan", "usa"]);
    let fr = f::parse_country(&fixture("fcdo_france.json")).unwrap();
    assert_eq!(fr.slug, "france");
    assert!(fr.parts.iter().any(|(s, _)| s == "entry-requirements"));
    let ch = f::parse_changes(&fixture("fcdo_changes.atom")).unwrap();
    assert_eq!(ch.len(), 3);
    assert_eq!(ch[0].slug, "usa");

    let s = fcdo_server().await;
    let base = s.uri();
    let mut h = Harness::new(
        spec("fcdo-content-api"),
        "fcdo_content_api",
        &format!("{base}/api/content/foreign-travel-advice"),
        t0(),
    )
    .await;
    let rep = h.run(&f::FcdoContentApi).await.unwrap();
    assert_eq!(rep.enqueued, 4);
    let urls = h.frontier.enqueued_urls();
    assert!(urls.contains(&format!("{base}/api/content/foreign-travel-advice/france")));
    assert!(urls.contains(&format!("{base}/foreign-travel-advice.atom")));
    for n in h.frontier.enqueued.lock().unwrap().iter() {
        assert_eq!(n.fetch_mode, FetchMode::Adapter("fcdo_content_api".into()));
        assert_eq!(n.source_id.as_ref().unwrap().0, "fcdo-content-api");
        assert_eq!(n.depth, 1);
    }

    let mut h = Harness::new(
        spec("fcdo-content-api"),
        "fcdo_content_api",
        &format!("{base}/foreign-travel-advice.atom"),
        t0(),
    )
    .await;
    h.run(&f::FcdoContentApi).await.unwrap();
    assert_eq!(h.sink.records[0].meta.media_type, MediaType::Atom);
    assert!(
        h.frontier
            .enqueued_urls()
            .contains(&format!("{base}/api/content/foreign-travel-advice/usa"))
    );

    let mut h = Harness::new(
        spec("fcdo-content-api"),
        "fcdo_content_api",
        &format!("{base}/api/content/foreign-travel-advice/france"),
        t0(),
    )
    .await;
    let rep = h.run(&f::FcdoContentApi).await.unwrap();
    assert_eq!((rep.records, rep.enqueued), (1, 0));
}

#[tokio::test]
async fn fcdo_content_api_writes_raw_first() {
    let s = fcdo_server().await;
    let a = adapters::fcdo_content_api::FcdoContentApi;
    raw_first_case(
        &a,
        "fcdo-content-api",
        &format!("{}/api/content/foreign-travel-advice", s.uri()),
        &[],
    )
    .await;
    raw_first_case(
        &a,
        "fcdo-content-api",
        &format!("{}/foreign-travel-advice.atom", s.uri()),
        &[],
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// state_dept_advisories, canada_advisories

#[tokio::test]
async fn state_dept_advisories_parses_fixture() {
    let v = adapters::state_dept_advisories::parse(&fixture("state_dept_advisories.json")).unwrap();
    assert_eq!(v.len(), 2);
    assert_eq!(v[0].country_codes, ["IR"]);
    assert_eq!(v[0].level, Some(4));
    assert!(v[0].updated.is_some());

    let s = MockServer::start().await;
    serve(
        &s,
        "/api/TravelAdvisories",
        fixture("state_dept_advisories.json"),
        "application/json; charset=utf-8",
    )
    .await;
    let mut h = Harness::new(
        spec("state-dept-advisories-api"),
        "state_dept_advisories",
        &format!("{}/api/TravelAdvisories", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::state_dept_advisories::StateDeptAdvisories)
        .await
        .unwrap();
    assert!(rep.complete);
    assert_eq!(h.sink.records[0].meta.media_type, MediaType::Json);
}

#[tokio::test]
async fn state_dept_advisories_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/api/TravelAdvisories",
        fixture("state_dept_advisories.json"),
        "application/json",
    )
    .await;
    raw_first_case(
        &adapters::state_dept_advisories::StateDeptAdvisories,
        "state-dept-advisories-api",
        &format!("{}/api/TravelAdvisories", s.uri()),
        &[],
    )
    .await;
}

#[tokio::test]
async fn canada_advisories_parses_fixture() {
    let v = adapters::canada_advisories::parse(&fixture("canada_index.json")).unwrap();
    assert_eq!(v.len(), 3);
    let af = v.iter().find(|a| a.iso == "AF").unwrap();
    assert_eq!(af.advisory_state, 3);
    assert_eq!(af.advisory_text.as_deref(), Some("Avoid all travel"));

    let s = MockServer::start().await;
    serve(
        &s,
        "/travel-voyage/index-alpha-eng.json",
        fixture("canada_index.json"),
        "application/json",
    )
    .await;
    let mut h = Harness::new(
        spec("canada-travel-json"),
        "canada_advisories",
        &format!("{}/travel-voyage/index-alpha-eng.json", s.uri()),
        t0(),
    )
    .await;
    assert!(
        h.run(&adapters::canada_advisories::CanadaAdvisories)
            .await
            .unwrap()
            .complete
    );
}

#[tokio::test]
async fn canada_advisories_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/i.json",
        fixture("canada_index.json"),
        "application/json",
    )
    .await;
    raw_first_case(
        &adapters::canada_advisories::CanadaAdvisories,
        "canada-travel-json",
        &format!("{}/i.json", s.uri()),
        &[],
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// met_norway

#[tokio::test]
async fn met_norway_parses_fixture() {
    let f = adapters::met_norway::parse(&fixture("met_norway_compact.json")).unwrap();
    assert_eq!(f.steps.len(), 3);
    assert_eq!(&f.coordinates[..2], &[10.75, 59.91]);
    assert!(f.steps[0].air_temperature.is_some());

    let s = MockServer::start().await;
    serve(
        &s,
        "/weatherapi/locationforecast/2.0/compact",
        fixture("met_norway_compact.json"),
        "application/json",
    )
    .await;
    let url = format!(
        "{}/weatherapi/locationforecast/2.0/compact?lat=59.91&lon=10.75",
        s.uri()
    );
    let mut h = Harness::new(spec("met-norway"), "met_norway", &url, t0()).await;
    assert!(
        h.run(&adapters::met_norway::MetNorway)
            .await
            .unwrap()
            .complete
    );
    assert_eq!(h.client.uas.lock().unwrap()[0], vc_core::USER_AGENT);
}

#[tokio::test]
async fn met_norway_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/c",
        fixture("met_norway_compact.json"),
        "application/json",
    )
    .await;
    raw_first_case(
        &adapters::met_norway::MetNorway,
        "met-norway",
        &format!("{}/c?lat=59.91&lon=10.75", s.uri()),
        &[],
    )
    .await;
}

#[tokio::test]
async fn met_norway_refuses_bad_coordinates_without_a_request() {
    let s = MockServer::start().await;
    for q in ["", "?lat=59.91", "?lat=59.123456&lon=10.75"] {
        let mut h = Harness::new(
            spec("met-norway"),
            "met_norway",
            &format!("{}/c{q}", s.uri()),
            t0(),
        )
        .await;
        assert!(matches!(
            h.run(&adapters::met_norway::MetNorway).await,
            Err(Error::Invalid(_))
        ));
        assert!(h.client.requested.lock().unwrap().is_empty());
    }
}

// ---------------------------------------------------------------------------------------
// nager_holidays

#[tokio::test]
async fn nager_holidays_parses_fixture() {
    use adapters::nager_holidays as n;
    let c = n::parse_countries(&fixture("nager_available_countries.json")).unwrap();
    assert_eq!(c.len(), 2);
    let hs = n::parse_holidays(&fixture("nager_public_holidays_2026_fr.json")).unwrap();
    assert_eq!(hs[1].date, "2026-04-06");
    assert_eq!(hs[1].local_name, "Lundi de Pâques");

    let s = MockServer::start().await;
    serve(
        &s,
        "/api/v3/AvailableCountries",
        fixture("nager_available_countries.json"),
        "application/json",
    )
    .await;
    serve(
        &s,
        "/api/v3/PublicHolidays/2026/FR",
        fixture("nager_public_holidays_2026_fr.json"),
        "application/json",
    )
    .await;
    let base = s.uri();
    let mut h = Harness::new(
        spec("nager-date"),
        "nager_holidays",
        &format!("{base}/api/v3/AvailableCountries"),
        t0(),
    )
    .await;
    let rep = h.run(&n::NagerHolidays).await.unwrap();
    assert_eq!(rep.enqueued, 4);
    assert_eq!(
        h.frontier.enqueued_urls(),
        [2026, 2027]
            .iter()
            .flat_map(|y| ["FR", "JP"].map(|c| format!("{base}/api/v3/PublicHolidays/{y}/{c}")))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
    );
    let mut h = Harness::new(
        spec("nager-date"),
        "nager_holidays",
        &format!("{base}/api/v3/PublicHolidays/2026/FR"),
        t0(),
    )
    .await;
    assert_eq!(h.run(&n::NagerHolidays).await.unwrap().enqueued, 0);
}

#[tokio::test]
async fn nager_holidays_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/api/v3/AvailableCountries",
        fixture("nager_available_countries.json"),
        "application/json",
    )
    .await;
    raw_first_case(
        &adapters::nager_holidays::NagerHolidays,
        "nager-date",
        &format!("{}/api/v3/AvailableCountries", s.uri()),
        &[],
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// ticketmaster_discovery

const TM_KEY: &str = "tm-secret-key-123";

async fn tm_server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/discovery/v2/events.json"))
        .and(query_param("apikey", TM_KEY))
        .and(query_param("page", "1"))
        .respond_with(ok(br#"{"_embedded":{"events":[]},"page":{"size":1,"totalElements":1,"totalPages":2,"number":1}}"#.to_vec(), "application/json"))
        .with_priority(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/discovery/v2/events.json"))
        .and(query_param("apikey", TM_KEY))
        .respond_with(ok(
            fixture("ticketmaster_events.json"),
            "application/json;charset=utf-8",
        ))
        .with_priority(2)
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn ticketmaster_discovery_parses_fixture() {
    use adapters::ticketmaster_discovery as t;
    let p = t::parse(&fixture("ticketmaster_events.json")).unwrap();
    assert_eq!(p.events.len(), 1);
    let e = &p.events[0];
    assert_eq!(e.id, "vvG1VZKS5pr1qy");
    assert_eq!(e.segment.as_deref(), Some("Sports"));
    let v = e.venue.as_ref().unwrap();
    assert_eq!(
        (v.city.as_deref(), v.country_code.as_deref()),
        (Some("Miami"), Some("US"))
    );
    assert_eq!(
        p.next.as_deref(),
        Some("/discovery/v2/events.json?page=1&size=1")
    );

    let s = tm_server().await;
    let mut h = Harness::new(
        spec("ticketmaster"),
        "ticketmaster_discovery",
        &format!("{}/discovery/v2/events.json?size=1", s.uri()),
        t0(),
    )
    .await;
    h.secrets
        .0
        .insert("crawler-src-ticketmaster-key".into(), TM_KEY.into());
    let rep = h.run(&t::TicketmasterDiscovery::DEFAULT).await.unwrap();
    assert_eq!((rep.requests, rep.records, rep.complete), (2, 2, true));
}

#[tokio::test]
async fn ticketmaster_discovery_writes_raw_first() {
    let s = tm_server().await;
    raw_first_case(
        &adapters::ticketmaster_discovery::TicketmasterDiscovery::DEFAULT,
        "ticketmaster",
        &format!("{}/discovery/v2/events.json?size=1", s.uri()),
        &[("crawler-src-ticketmaster-key", TM_KEY)],
    )
    .await;
}

#[tokio::test]
async fn ticketmaster_key_never_reaches_the_spool() {
    let s = tm_server().await;
    let mut h = Harness::new(
        spec("ticketmaster"),
        "ticketmaster_discovery",
        &format!("{}/discovery/v2/events.json?size=1", s.uri()),
        t0(),
    )
    .await;
    h.secrets
        .0
        .insert("crawler-src-ticketmaster-key".into(), TM_KEY.into());
    h.run(&adapters::ticketmaster_discovery::TicketmasterDiscovery::DEFAULT)
        .await
        .unwrap();
    assert!(
        h.client
            .requested
            .lock()
            .unwrap()
            .iter()
            .all(|u| u.contains(TM_KEY))
    );
    for r in &h.sink.records {
        assert!(!r.meta.url.as_str().contains(TM_KEY));
        if let SpoolPayload::Response(e) = &r.payload {
            assert!(!e.url.as_str().contains(TM_KEY));
            assert!(!format!("{:?}", e.request_headers).contains(TM_KEY));
        }
    }
}

#[tokio::test]
async fn ticketmaster_page_cap_leaves_a_cursor() {
    let s = tm_server().await;
    let mut h = Harness::new(
        spec("ticketmaster"),
        "ticketmaster_discovery",
        &format!("{}/discovery/v2/events.json?size=1", s.uri()),
        t0(),
    )
    .await;
    h.secrets
        .0
        .insert("crawler-src-ticketmaster-key".into(), TM_KEY.into());
    let a = adapters::ticketmaster_discovery::TicketmasterDiscovery { max_pages: 1 };
    let rep = h.run(&a).await.unwrap();
    assert!(!rep.complete);
    let c = rep.cursor.unwrap();
    assert!(c.contains("page=1") && !c.contains(TM_KEY), "{c}");
}

#[tokio::test]
async fn ticketmaster_resumes_from_its_cursor() {
    use adapters::ticketmaster_discovery::{TicketmasterDiscovery, resume_url};
    let s = tm_server().await;
    let row = format!("{}/discovery/v2/events.json?size=1", s.uri());
    let mut h = Harness::new(spec("ticketmaster"), "ticketmaster_discovery", &row, t0()).await;
    h.secrets
        .0
        .insert("crawler-src-ticketmaster-key".into(), TM_KEY.into());
    let first = h
        .run_from(&TicketmasterDiscovery { max_pages: 1 }, None)
        .await
        .unwrap();
    let cursor = first.cursor.expect("cursor");
    // A new run (another process, after a restart) continues at page 1, not page 0.
    let mut h2 = Harness::new(spec("ticketmaster"), "ticketmaster_discovery", &row, t0()).await;
    h2.secrets
        .0
        .insert("crawler-src-ticketmaster-key".into(), TM_KEY.into());
    let rep = h2
        .run_from(&TicketmasterDiscovery::DEFAULT, Some(&cursor))
        .await
        .unwrap();
    assert_eq!((rep.requests, rep.complete, rep.cursor), (1, true, None));
    let req = h2.client.requested.lock().unwrap()[0].clone();
    assert!(req.contains("page=1") && req.contains(TM_KEY), "{req}");
    // Cursors off the row's endpoint (or not a path) start over.
    let row_url = vc_core::CanonicalUrl::parse(&row).unwrap();
    assert!(resume_url(&row_url, "/discovery/v2/venues.json?page=3").is_none());
    assert!(resume_url(&row_url, "https://evil.test/discovery/v2/events.json").is_none());
    assert!(resume_url(&row_url, "//evil.test/discovery/v2/events.json").is_none());
    let mut h3 = Harness::new(spec("ticketmaster"), "ticketmaster_discovery", &row, t0()).await;
    h3.secrets
        .0
        .insert("crawler-src-ticketmaster-key".into(), TM_KEY.into());
    let rep = h3
        .run_from(
            &TicketmasterDiscovery::DEFAULT,
            Some("/discovery/v2/venues.json?page=3"),
        )
        .await
        .unwrap();
    assert_eq!(rep.requests, 2, "started over");
}

#[tokio::test]
async fn ticketmaster_without_key_makes_no_request() {
    let s = tm_server().await;
    let mut h = Harness::new(
        spec("ticketmaster"),
        "ticketmaster_discovery",
        &format!("{}/discovery/v2/events.json", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(
        h.run(&adapters::ticketmaster_discovery::TicketmasterDiscovery::DEFAULT)
            .await,
        Err(Error::NotFound(_))
    ));
    assert!(h.client.requested.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------------------
// wikidata_sparql

async fn wikidata_server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/sparql"))
        .and(query_param_contains("query", "P2852"))
        .respond_with(ok(
            fixture("wikidata_emergency_numbers.srj"),
            "application/sparql-results+json",
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path("/sparql"))
        .respond_with(ok(
            fixture("wikidata_empty.srj"),
            "application/sparql-results+json",
        ))
        .with_priority(10)
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn wikidata_sparql_parses_fixture() {
    let rows =
        adapters::wikidata_sparql::parse(&fixture("wikidata_emergency_numbers.srj")).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["iso2"], "FR");
    assert_eq!(rows[0]["numberLabel"], "112");

    let s = wikidata_server().await;
    let mut h = Harness::new(
        spec("wikidata-aviation"),
        "wikidata_sparql",
        &format!("{}/sparql", s.uri()),
        t0(),
    )
    .await;
    let a = adapters::wikidata_sparql::WikidataSparql {
        page_size: 2,
        max_pages: 3,
    };
    let rep = h.run(&a).await.unwrap();
    // emergency numbers: a full page (2 rows) then 2 more (the mock repeats) then the cap.
    assert!(!rep.complete);
    assert_eq!(rep.cursor.as_deref(), Some("emergency_numbers@6"));
    assert_eq!(rep.requests, 3);
    let q = h.client.requested.lock().unwrap()[1].clone();
    assert!(q.contains("OFFSET+2") || q.contains("OFFSET%202"), "{q}");
    assert_eq!(h.sink.records[0].meta.media_type, MediaType::Json);

    let a = adapters::wikidata_sparql::WikidataSparql {
        page_size: 5,
        max_pages: 3,
    };
    let mut h = Harness::new(
        spec("wikidata-aviation"),
        "wikidata_sparql",
        &format!("{}/sparql", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&a).await.unwrap();
    assert!(rep.complete);
    assert_eq!(
        rep.requests,
        adapters::wikidata_sparql::QUERIES.len() as u32
    );
}

#[tokio::test]
async fn wikidata_sparql_resumes_from_its_cursor() {
    use adapters::wikidata_sparql::{WikidataSparql, parse_cursor};
    assert_eq!(parse_cursor("emergency_numbers@6"), Some((0, 6)));
    assert_eq!(parse_cursor("airports@10"), Some((2, 10)));
    assert_eq!(parse_cursor("gone_query@6"), None);
    assert_eq!(parse_cursor("airports@x"), None);
    let s = wikidata_server().await;
    let a = WikidataSparql {
        page_size: 2,
        max_pages: 3,
    };
    let mut h = Harness::new(
        spec("wikidata-aviation"),
        "wikidata_sparql",
        &format!("{}/sparql", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run_from(&a, Some("emergency_numbers@6")).await.unwrap();
    // Continues at offset 6 (then 8, 10) and stops at the cap again with the next offset.
    assert_eq!(rep.cursor.as_deref(), Some("emergency_numbers@12"));
    let q = h.client.requested.lock().unwrap()[0].clone();
    assert!(q.contains("OFFSET+6") || q.contains("OFFSET%206"), "{q}");
    // A later query: earlier ones are skipped, the stream completes.
    let mut h = Harness::new(
        spec("wikidata-aviation"),
        "wikidata_sparql",
        &format!("{}/sparql", s.uri()),
        t0(),
    )
    .await;
    let last = adapters::wikidata_sparql::QUERIES.last().unwrap().name;
    let rep = h.run_from(&a, Some(&format!("{last}@0"))).await.unwrap();
    assert!(rep.complete);
    assert_eq!(rep.requests, 1);
    // An unknown cursor starts over (same as no cursor).
    let mut h = Harness::new(
        spec("wikidata-aviation"),
        "wikidata_sparql",
        &format!("{}/sparql", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run_from(&a, Some("gone_query@6")).await.unwrap();
    assert_eq!(rep.cursor.as_deref(), Some("emergency_numbers@6"));
}

#[tokio::test]
async fn wikidata_sparql_writes_raw_first() {
    let s = wikidata_server().await;
    raw_first_case(
        &adapters::wikidata_sparql::WikidataSparql::DEFAULT,
        "wikidata-aviation",
        &format!("{}/sparql", s.uri()),
        &[],
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// Dumps

fn csv_body(h: &Harness) -> String {
    h.resources()
        .iter()
        .map(|(ct, b)| {
            assert_eq!(ct, "text/csv");
            String::from_utf8(b.to_vec()).unwrap()
        })
        .collect()
}

#[tokio::test]
async fn ourairports_csv_parses_fixture() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/ourairports-data/airports.csv",
        fixture("ourairports_airports.csv"),
        "text/csv; charset=utf-8",
    )
    .await;
    let mut h = Harness::new(
        spec("ourairports"),
        "ourairports_csv",
        &format!("{}/ourairports-data/airports.csv", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::ourairports_csv::OurAirportsCsv::DEFAULT)
        .await
        .unwrap();
    assert!(rep.complete);
    let out = csv_body(&h);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines[0].starts_with("\"id\",\"ident\",\"type\""));
    assert_eq!(lines.len(), 1 + 38, "only the 38 small airports are kept");
    assert!(!out.contains("heliport") && !out.contains("seaplane_base"));
    let m = &h.manifests()[0];
    assert_eq!(m["type"], "dump_manifest");
    assert_eq!(m["units_seen"], 57);
    assert_eq!(m["units_kept"], 38);
    assert_eq!(m["filter_version"], "ourairports-airports-v1");
    assert_eq!(m["complete"], true);
    let raw = fixture("ourairports_airports.csv");
    assert_eq!(m["size"], raw.len());
    assert_eq!(
        m["sha256"],
        vc_sources::sha256::hex(&vc_sources::sha256::Sha256::digest(&raw))
    );
    assert_eq!(
        h.sink.records.last().unwrap().meta.role,
        CaptureRole::Manifest
    );
}

#[tokio::test]
async fn ourairports_csv_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/a.csv",
        fixture("ourairports_airports.csv"),
        "text/csv",
    )
    .await;
    raw_first_case(
        &adapters::ourairports_csv::OurAirportsCsv::DEFAULT,
        "ourairports",
        &format!("{}/a.csv", s.uri()),
        &[],
    )
    .await;
}

#[tokio::test]
async fn mobility_database_catalog_parses_fixture() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/feeds_v2.csv",
        fixture("mobility_feeds_v2.csv"),
        "text/csv",
    )
    .await;
    let mut h = Harness::new(
        spec("mobility-database"),
        "mobility_database_catalog",
        &format!("{}/feeds_v2.csv", s.uri()),
        t0(),
    )
    .await;
    h.client.chunk = 50; // records (one with a quoted newline) straddle chunk boundaries
    h.run(&adapters::mobility_database_catalog::MobilityDatabaseCatalog::DEFAULT)
        .await
        .unwrap();
    let out = csv_body(&h);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[1].starts_with("mdb-1,gtfs,"));
    assert_eq!(h.manifests()[0]["units_seen"], 4);
}

#[tokio::test]
async fn mobility_database_catalog_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/feeds_v2.csv",
        fixture("mobility_feeds_v2.csv"),
        "text/csv",
    )
    .await;
    raw_first_case(
        &adapters::mobility_database_catalog::MobilityDatabaseCatalog::DEFAULT,
        "mobility-database",
        &format!("{}/feeds_v2.csv", s.uri()),
        &[],
    )
    .await;
}

#[tokio::test]
async fn wikivoyage_dump_parses_fixture() {
    let s = MockServer::start().await;
    let p = "/enwikivoyage/latest/enwikivoyage-latest-pages-articles.xml.bz2";
    serve(
        &s,
        p,
        fixture("wikivoyage_pages_articles.xml.bz2"),
        "application/octet-stream",
    )
    .await;
    let mut h = Harness::new(
        spec("wikivoyage-dump"),
        "wikivoyage_dump",
        &format!("{}{p}", s.uri()),
        t0(),
    )
    .await;
    h.client.chunk = 1000;
    let rep = h
        .run(&adapters::wikivoyage_dump::WikivoyageDump::DEFAULT)
        .await
        .unwrap();
    assert!(rep.complete);
    let res = h.resources();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].0, "application/json");
    let pages: Vec<serde_json::Value> = serde_json::from_slice(&res[0].1).unwrap();
    let titles: Vec<&str> = pages.iter().map(|p| p["title"].as_str().unwrap()).collect();
    assert_eq!(
        titles,
        ["Abakan", "Abersoch", "Akyab"],
        "project-namespace page dropped"
    );
    assert_eq!(pages[0]["page_id"], 53);
    assert_eq!(pages[0]["rev_id"], 5389031);
    assert_eq!(pages[0]["url"], "https://en.wikivoyage.org/wiki/Abakan");
    assert!(pages[0]["text"].as_str().unwrap().contains("'''Abakan'''"));
    assert!(
        !res[0].1.windows(10).any(|w| w == b"Brycehughe"),
        "contributors are not kept"
    );
    assert_eq!(pages[2]["redirect"], "Sittwe");
    assert!(pages[2].get("text").is_none());
    let m = &h.manifests()[0];
    assert_eq!(
        (m["units_seen"].as_u64(), m["units_kept"].as_u64()),
        (Some(4), Some(3))
    );
    assert_eq!(
        h.resources()[0].1.len() as u64,
        h.sink.records[0].meta.payload_len
    );
}

#[tokio::test]
async fn wikivoyage_dump_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/wv.xml.bz2",
        fixture("wikivoyage_pages_articles.xml.bz2"),
        "application/octet-stream",
    )
    .await;
    raw_first_case(
        &adapters::wikivoyage_dump::WikivoyageDump::DEFAULT,
        "wikivoyage-dump",
        &format!("{}/wv.xml.bz2", s.uri()),
        &[],
    )
    .await;
}

#[tokio::test]
async fn wikivoyage_truncated_dump_is_incomplete() {
    let s = MockServer::start().await;
    let full = fixture("wikivoyage_pages_articles.xml.bz2");
    serve(
        &s,
        "/wv.xml.bz2",
        full[..full.len() / 2].to_vec(),
        "application/octet-stream",
    )
    .await;
    let mut h = Harness::new(
        spec("wikivoyage-dump"),
        "wikivoyage_dump",
        &format!("{}/wv.xml.bz2", s.uri()),
        t0(),
    )
    .await;
    assert!(
        h.run(&adapters::wikivoyage_dump::WikivoyageDump::DEFAULT)
            .await
            .is_err()
    );
    let m = h.manifests();
    assert_eq!(m.len(), 1);
    assert_eq!(m[0]["complete"], false);
    assert!(m[0]["sha256"].is_null());
}

#[tokio::test]
async fn se_travel_dump_parses_fixture() {
    let s = MockServer::start().await;
    let p = "/download/stackexchange_20251231/stackexchange_20251231/travel.stackexchange.com.7z/Posts.xml";
    serve(
        &s,
        p,
        fixture("se_travel_posts.xml"),
        "text/xml;charset=UTF-8",
    )
    .await;
    let mut h = Harness::new(
        spec("se-dump"),
        "se_travel_dump",
        &format!("{}/details/stackexchange_20251231", s.uri()),
        t0(),
    )
    .await;
    h.client.chunk = 300;
    let rep = h
        .run(&adapters::se_travel_dump::SeTravelDump::DEFAULT)
        .await
        .unwrap();
    assert!(rep.complete);
    let posts: Vec<serde_json::Value> = serde_json::from_slice(&h.resources()[0].1).unwrap();
    assert_eq!(
        posts.len(),
        3,
        "tag wiki dropped; question + 2 answers kept"
    );
    assert_eq!(posts[0]["post_type"], "question");
    assert_eq!(posts[0]["id"], 9112);
    assert_eq!(posts[0]["url"], "https://travel.stackexchange.com/q/9112");
    assert_eq!(posts[1]["parent_id"], 9112);
    assert!(
        posts[0]["body_html"]
            .as_str()
            .unwrap()
            .starts_with("<p>We will be visiting Cape Town")
    );
    assert!(posts[0]["attribution"]["author"].is_string());
    let body = String::from_utf8(h.resources()[0].1.to_vec()).unwrap();
    assert!(!body.contains("LastEditorDisplayName") && !body.contains("LastEditor"));
    assert!(
        h.manifests()[0]["url"]
            .as_str()
            .unwrap()
            .ends_with("/Posts.xml")
    );
}

#[tokio::test]
async fn se_travel_dump_writes_raw_first() {
    let s = MockServer::start().await;
    serve(&s, "/Posts.xml", fixture("se_travel_posts.xml"), "text/xml").await;
    raw_first_case(
        &adapters::se_travel_dump::SeTravelDump::DEFAULT,
        "se-dump",
        &format!("{}/Posts.xml", s.uri()),
        &[],
    )
    .await;
}

// ---------------------------------------------------------------------------------------
// Streaming, leases and politeness

fn tight(extend_every_s: u64) -> DumpSettings {
    DumpSettings {
        extend_every: Duration::from_secs(extend_every_s),
        batch_bytes: 1024,
        ..DumpSettings::DEFAULT
    }
}

#[tokio::test]
async fn dump_stream_respects_extend() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/a.csv",
        fixture("ourairports_airports.csv"),
        "text/csv",
    )
    .await;
    let url = format!("{}/a.csv", s.uri());

    // ~33 chunks × 20 s ≈ 11 min of streaming against a 60 s lease: only extension keeps it.
    let mut h = Harness::with_budget(
        spec("ourairports"),
        "ourairports_csv",
        &url,
        t0(),
        Duration::from_secs(60),
    )
    .await;
    h.client.chunk = 256;
    h.client.per_chunk = Duration::from_secs(20);
    let a = adapters::ourairports_csv::OurAirportsCsv::new(tight(30));
    let rep = h.run(&a).await.unwrap();
    assert!(rep.complete);
    assert!(*h.frontier.extends.lock().unwrap() >= 10);
    assert!(
        h.resources().len() > 1,
        "batches are written while streaming"
    );
    // Resources are spooled between chunks, not only at the end.
    let ev = h.events.all();
    let first_res = ev.iter().position(|e| e == "append Primary").unwrap();
    let last_extend = ev.iter().rposition(|e| e == "extend").unwrap();
    assert!(first_res < last_extend, "{ev:?}");

    // Without extending, the lease expires mid-stream and the run aborts (partial manifest).
    let mut h = Harness::with_budget(
        spec("ourairports"),
        "ourairports_csv",
        &url,
        t0(),
        Duration::from_secs(60),
    )
    .await;
    h.client.chunk = 256;
    h.client.per_chunk = Duration::from_secs(20);
    let a = adapters::ourairports_csv::OurAirportsCsv::new(tight(3600));
    let err = h.run(&a).await.unwrap_err();
    assert!(
        matches!(err, Error::Fetch(vc_core::fetch::FetchError::Lease(_))),
        "{err:?}"
    );
    assert_eq!(*h.frontier.extends.lock().unwrap(), 0);
    let m = h.manifests();
    assert_eq!(m[0]["complete"], false);
    assert_eq!(m[0]["error"], "Lease");
}

#[tokio::test]
async fn dump_outside_window_makes_no_request() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/a.csv",
        fixture("ourairports_airports.csv"),
        "text/csv",
    )
    .await;
    let noon = t0() + chrono::Duration::hours(9);
    let mut h = Harness::new(
        spec("ourairports"),
        "ourairports_csv",
        &format!("{}/a.csv", s.uri()),
        noon,
    )
    .await;
    let rep = h
        .run(&adapters::ourairports_csv::OurAirportsCsv::DEFAULT)
        .await
        .unwrap();
    assert!(!rep.complete);
    assert_eq!(rep.requests, 0);
    assert!(h.client.requested.lock().unwrap().is_empty());
    assert!(h.sink.records.is_empty());
}

#[tokio::test]
async fn dump_byte_cap_aborts() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/a.csv",
        fixture("ourairports_airports.csv"),
        "text/csv",
    )
    .await;
    let mut h = Harness::new(
        spec("ourairports"),
        "ourairports_csv",
        &format!("{}/a.csv", s.uri()),
        t0(),
    )
    .await;
    let a = adapters::ourairports_csv::OurAirportsCsv::new(DumpSettings {
        max_bytes: 1000,
        ..DumpSettings::DEFAULT
    });
    let err = h.run(&a).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Fetch(vc_core::fetch::FetchError::BodyTooLarge { .. })
        ),
        "{err:?}"
    );
    assert_eq!(h.manifests()[0]["complete"], false);
}

#[tokio::test]
async fn challenge_is_an_error_not_a_retry() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(403))
        .expect(1)
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("state-dept-advisories-api"),
        "state_dept_advisories",
        &format!("{}/api/TravelAdvisories", s.uri()),
        t0(),
    )
    .await;
    let err = h
        .run(&adapters::state_dept_advisories::StateDeptAdvisories)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        Error::Fetch(vc_core::fetch::FetchError::Challenge { status: 403 })
    ));
    assert_eq!(
        h.client.requested.lock().unwrap().len(),
        1,
        "exactly one request, no retry"
    );
}

#[tokio::test]
async fn http_errors_are_spooled_then_reported() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503).set_body_string("busy"))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("canada-travel-json"),
        "canada_advisories",
        &format!("{}/i.json", s.uri()),
        t0(),
    )
    .await;
    let err = h
        .run(&adapters::canada_advisories::CanadaAdvisories)
        .await
        .unwrap_err();
    assert_eq!(err.class(), vc_core::ErrorClass::Transient);
    assert_eq!(h.responses()[0].status.as_u16(), 503);
}

#[tokio::test]
async fn every_request_takes_a_rate_token() {
    let s = wikidata_server().await;
    let mut h = Harness::new(
        spec("wikidata-aviation"),
        "wikidata_sparql",
        &format!("{}/sparql", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::wikidata_sparql::WikidataSparql::DEFAULT)
        .await
        .unwrap();
    assert_eq!(
        h.frontier.inner.rate_tokens_taken(),
        u64::from(rep.requests)
    );
}

#[test]
fn registry_lists_every_adapter_once() {
    let ids = adapters::ids();
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), 12);
    assert_eq!(ids.len(), 12);
    for id in &ids {
        assert_eq!(adapters::get(id).unwrap().id(), *id);
    }
    let cfg_ids: Vec<&str> = adapters::registry().iter().map(|a| a.id()).collect();
    assert_eq!(cfg_ids, ids);
}
