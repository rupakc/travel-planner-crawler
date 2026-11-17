//! Open-data adapters (2026-11-18): `openholidays`, `json_events` (NYC Parks via Socrata,
//! Visit Stockholm via DRF), `eurostat_jsonstat` and the `wdi_csv` ZIP dump, against fixtures
//! built from each provider's documented format (`testkit/fixtures/vc-sources/README.md`),
//! served by wiremock; and their record mappings (`public_holiday`, `event`, `price_level`).

mod common;

use common::{Harness, assert_raw_first, fixture, spec, t0};
use serde_json::json;
use vc_core::media::{FetchMode, MediaType};
use vc_core::sources::SourceAdapter;
use vc_core::store::CaptureRole;
use vc_core::{CanonicalUrl, Error};
use vc_sources::adapters;
use vc_sources::adapters::records::{AdapterItem, records};
use wiremock::matchers::{method, path, query_param};
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

async fn raw_first_case(a: &dyn SourceAdapter, source: &str, url: &str) {
    let mut h = Harness::new(spec(source), a.id(), url, t0()).await;
    h.run(a).await.unwrap();
    assert_raw_first(&h.events.all());
    assert!(h.sink.records.iter().all(|r| r.meta.payload_hash.is_some()));

    let mut h = Harness::new(spec(source), a.id(), url, t0()).await;
    h.sink.fail = true;
    let err = h.run(a).await.unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err:?}");
    assert!(
        !h.events.all().iter().any(|e| e.starts_with("enqueue")),
        "nothing may be derived when the spool append fails"
    );
}

fn items(adapter: &str, url: &str, body: &[u8]) -> Vec<AdapterItem> {
    records(adapter, &CanonicalUrl::parse(url).unwrap(), body)
        .expect("adapter has a record mapping")
        .expect("body maps")
}

#[test]
fn new_adapters_are_registered_and_mapped() {
    for id in [
        "openholidays",
        "json_events",
        "eurostat_jsonstat",
        "wdi_csv",
    ] {
        assert!(adapters::get(id).is_some(), "{id} registered");
        assert!(
            adapters::records::MAPPED.contains(&id),
            "{id} has a record mapping"
        );
    }
    assert!(
        adapters::resumable()
            .iter()
            .any(|a| a.id() == "json_events"),
        "json_events resumes its paging"
    );
}

// ---------------------------------------------------------------------------------------
// openholidays

#[tokio::test]
async fn openholidays_fans_out_per_country_and_year() {
    use adapters::openholidays as o;
    let c = o::parse_countries(&fixture("openholidays_countries.json")).unwrap();
    assert_eq!(
        c.iter().map(|c| c.iso_code.as_str()).collect::<Vec<_>>(),
        ["DE", "PT"]
    );

    let s = MockServer::start().await;
    serve(
        &s,
        "/Countries",
        fixture("openholidays_countries.json"),
        "application/json",
    )
    .await;
    serve(
        &s,
        "/PublicHolidays",
        fixture("openholidays_public_de.json"),
        "application/json",
    )
    .await;
    let base = s.uri();
    let mut h = Harness::new(
        spec("openholidays"),
        "openholidays",
        &format!("{base}/Countries"),
        t0(),
    )
    .await;
    let rep = h.run(&o::OpenHolidays).await.unwrap();
    assert_eq!((rep.requests, rep.records, rep.enqueued), (1, 1, 4));
    let want: Vec<String> = {
        let mut v: Vec<String> = ["DE", "PT"]
            .iter()
            .flat_map(|cc| {
                ["PublicHolidays", "SchoolHolidays"].map(|p| {
                    format!(
                        "{base}/{p}?countryIsoCode={cc}&languageIsoCode=EN&validFrom=2026-01-01&validTo=2027-12-31"
                    )
                })
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(h.frontier.enqueued_urls(), want);

    let url = format!(
        "{base}/PublicHolidays?countryIsoCode=DE&languageIsoCode=EN&validFrom=2026-01-01&validTo=2027-12-31"
    );
    let mut h = Harness::new(spec("openholidays"), "openholidays", &url, t0()).await;
    let rep = h.run(&o::OpenHolidays).await.unwrap();
    assert_eq!((rep.requests, rep.enqueued, rep.complete), (1, 0, true));
    let r = &h.sink.records[0];
    assert_eq!(r.meta.fetch_mode, FetchMode::Adapter("openholidays".into()));
    assert_eq!(r.meta.media_type, MediaType::Json);

    // An unknown endpoint is refused before any request.
    let mut h = Harness::new(
        spec("openholidays"),
        "openholidays",
        &format!("{base}/Languages"),
        t0(),
    )
    .await;
    assert!(matches!(
        h.run(&o::OpenHolidays).await,
        Err(Error::Invalid(_))
    ));
    assert!(h.client.requested.lock().unwrap().is_empty());
}

#[tokio::test]
async fn openholidays_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        "/Countries",
        fixture("openholidays_countries.json"),
        "application/json",
    )
    .await;
    raw_first_case(
        &adapters::openholidays::OpenHolidays,
        "openholidays",
        &format!("{}/Countries", s.uri()),
    )
    .await;
}

#[test]
fn openholidays_records_are_public_holidays() {
    let q = "countryIsoCode=DE&languageIsoCode=EN&validFrom=2026-01-01&validTo=2027-12-31";
    let pubs = items(
        "openholidays",
        &format!("https://openholidaysapi.org/PublicHolidays?{q}"),
        &fixture("openholidays_public_de.json"),
    );
    assert_eq!(pubs.len(), 3, "BackToSchool days are not holidays");
    assert!(
        pubs.iter()
            .all(|i| i.kind == "public_holiday" && i.series_key.is_none())
    );
    assert_eq!(
        pubs[0].data,
        json!({"name": "New Year's Day", "local_name": "Neujahr", "type": "public",
               "start_date": "2026-01-01", "end_date": "2026-01-01", "half_day": false,
               "country_code": "DE", "nationwide": true, "regional_scope": "national"})
    );
    assert_eq!(
        pubs[0].id.as_deref(),
        Some("openholidays:1d1c5d4e-3c0a-4a8e-9a43-0b5b9f6f0001")
    );
    let epiphany = &pubs[1].data;
    assert_eq!(epiphany["nationwide"], false);
    assert_eq!(
        epiphany["subdivisions"],
        json!([{"code": "DE-BW", "name": "BW"}, {"code": "DE-BY", "name": "BY"},
               {"code": "DE-ST", "name": "ST"}])
    );
    let eve = &pubs[2].data;
    assert_eq!(
        (eve["type"].as_str(), eve["half_day"].as_bool()),
        (Some("optional"), Some(true))
    );
    assert_eq!(eve["comment"], "Shops close at noon.");

    let school = items(
        "openholidays",
        &format!("https://openholidaysapi.org/SchoolHolidays?{q}"),
        &fixture("openholidays_school_de.json"),
    );
    assert_eq!(school.len(), 1);
    assert_eq!(school[0].data["type"], "school");
    assert_eq!(school[0].data["end_date"], "2026-08-22");

    // The country list carries no holidays; a holiday list without its country is refused.
    assert!(
        items(
            "openholidays",
            "https://openholidaysapi.org/Countries",
            &fixture("openholidays_countries.json")
        )
        .is_empty()
    );
    assert!(
        records(
            "openholidays",
            &CanonicalUrl::parse("https://openholidaysapi.org/PublicHolidays").unwrap(),
            &fixture("openholidays_public_de.json"),
        )
        .unwrap()
        .is_err()
    );
}

// ---------------------------------------------------------------------------------------
// json_events: NYC Parks (Socrata)

const NYC: &str = "/resource/fudw-fgrp.json";

#[tokio::test]
async fn json_events_socrata_asks_for_upcoming_rows_in_pages() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(NYC))
        .and(query_param("$where", "date >= '2026-11-16'"))
        .and(query_param("$order", "date,event_id"))
        .and(query_param("$limit", "1000"))
        .and(query_param("$offset", "0"))
        .respond_with(ok(fixture("nyc_parks_events.json"), "application/json"))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("nyc-parks-events"),
        "json_events",
        &format!("{}{NYC}", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::json_events::JsonEvents::DEFAULT)
        .await
        .unwrap();
    assert_eq!(
        (rep.requests, rep.records, rep.complete),
        (1, 1, true),
        "a short page ends the list"
    );
    assert_eq!(h.sink.records[0].meta.status, Some(200));

    // A full page asks for the next one.
    use adapters::json_events as j;
    let m = j::mapping_for(
        &CanonicalUrl::parse(&format!("https://data.cityofnewyork.us{NYC}")).unwrap(),
    )
    .unwrap();
    let full = j::Page {
        items: vec![json!({}); 1000],
        pages: None,
    };
    let u = CanonicalUrl::parse(&format!(
        "https://data.cityofnewyork.us{NYC}?$order=date&$limit=1000&$offset=1000"
    ))
    .unwrap();
    let next = j::next_page(m, &u, &full).unwrap().unwrap();
    assert!(
        next.as_str().contains("%24offset=2000") || next.as_str().contains("$offset=2000"),
        "{next}"
    );
}

#[test]
fn nyc_parks_records_are_events() {
    let ev = items(
        "json_events",
        &format!("https://data.cityofnewyork.us{NYC}?$offset=0"),
        &fixture("nyc_parks_events.json"),
    );
    assert_eq!(ev.len(), 3, "the untitled row is skipped");
    assert!(ev.iter().all(|i| i.kind == "event"));
    assert_eq!(
        ev[0].id.as_deref(),
        Some("nyc-parks-events:201001:2026-11-20")
    );
    assert_eq!(
        ev[0].data,
        json!({"name": "Shakespeare in the Park: Twelfth Night", "category": "other",
               "description": "Free outdoor theatre in Central Park.",
               "start_date": "2026-11-20", "end_date": "2026-11-20", "start_time": "19:30",
               "venue": "Delacorte Theater, Central Park", "city": "New York City",
               "country": "United States", "country_code": "US",
               "price": {"is_free": true},
               "source_url": "https://www.nycgovparks.org/events/2026/11/20/twelfth-night"})
    );
    assert_eq!(ev[1].data["start_time"], "08:00");
    assert_eq!(ev[2].data["price"], json!({"text": "$15 per class"}));
}

// ---------------------------------------------------------------------------------------
// json_events: Visit Stockholm (DRF page numbers)

const STH: &str = "/api/public-v1/events/";

async fn sth_server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(STH))
        .and(query_param("page", "2"))
        .respond_with(ok(fixture("stockholm_events_p2.json"), "application/json"))
        .with_priority(1)
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(STH))
        .and(query_param("format", "json"))
        .respond_with(ok(fixture("stockholm_events_p1.json"), "application/json"))
        .with_priority(2)
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn json_events_follows_page_numbers() {
    let s = sth_server().await;
    let url = format!("{}{STH}?format=json", s.uri());
    let mut h = Harness::new(spec("stockholm-open-api"), "json_events", &url, t0()).await;
    let rep = h
        .run(&adapters::json_events::JsonEvents::DEFAULT)
        .await
        .unwrap();
    assert_eq!((rep.requests, rep.records, rep.complete), (2, 2, true));
    let req = h.client.requested.lock().unwrap().clone();
    assert!(
        req[1].ends_with(&format!("{STH}?format=json&page=2")),
        "{req:?}"
    );
}

#[tokio::test]
async fn json_events_page_cap_leaves_a_cursor_and_resumes() {
    let s = sth_server().await;
    let url = format!("{}{STH}?format=json", s.uri());
    let a = adapters::json_events::JsonEvents { max_pages: 1 };
    let mut h = Harness::new(spec("stockholm-open-api"), "json_events", &url, t0()).await;
    let rep = h.run(&a).await.unwrap();
    assert!(!rep.complete);
    let cursor = rep.cursor.clone().unwrap();
    assert_eq!(cursor, format!("{STH}?format=json&page=2"));

    let mut h = Harness::new(spec("stockholm-open-api"), "json_events", &url, t0()).await;
    let rep = h.run_from(&a, Some(&cursor)).await.unwrap();
    assert_eq!((rep.requests, rep.complete), (1, true));
    // A cursor off the endpoint is ignored (starts over).
    let mut h = Harness::new(spec("stockholm-open-api"), "json_events", &url, t0()).await;
    let rep = h.run_from(&a, Some("/elsewhere?page=9")).await.unwrap();
    assert_eq!(rep.cursor.as_deref(), Some(cursor.as_str()));
}

#[tokio::test]
async fn json_events_writes_raw_first_and_refuses_unknown_endpoints() {
    let s = sth_server().await;
    raw_first_case(
        &adapters::json_events::JsonEvents::DEFAULT,
        "stockholm-open-api",
        &format!("{}{STH}?format=json", s.uri()),
    )
    .await;
    let mut h = Harness::new(
        spec("stockholm-open-api"),
        "json_events",
        &format!("{}/api/other/", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(
        h.run(&adapters::json_events::JsonEvents::DEFAULT).await,
        Err(Error::Invalid(_))
    ));
    assert!(h.client.requested.lock().unwrap().is_empty());
}

#[test]
fn stockholm_records_are_events() {
    let p1 = items(
        "json_events",
        &format!("https://www.visitstockholm.com{STH}?format=json"),
        &fixture("stockholm_events_p1.json"),
    );
    let p2 = items(
        "json_events",
        &format!("https://www.visitstockholm.com{STH}?format=json&page=2"),
        &fixture("stockholm_events_p2.json"),
    );
    let all: Vec<_> = p1.iter().chain(&p2).collect();
    assert_eq!(all.len(), 3);
    let cats: Vec<&str> = all
        .iter()
        .map(|i| i.data["category"].as_str().unwrap())
        .collect();
    assert_eq!(cats, ["exhibition", "music", "market"]);
    let d = &p1[0].data;
    assert_eq!(
        p1[0].id.as_deref(),
        Some("stockholm-open-api:edd6c9c1-13aa-4397-ad7a-41ca97ef7101")
    );
    assert_eq!(d["name"], "Remnants");
    assert_eq!(d["description"], "Remnants at Tensta Konsthall.");
    assert_eq!(
        (d["start_date"].as_str(), d["end_date"].as_str()),
        (Some("2026-11-17"), Some("2027-01-10"))
    );
    assert_eq!(
        (d["city"].as_str(), d["country_code"].as_str()),
        (Some("Spånga"), Some("SE"))
    );
    assert_eq!(d["lat"], 59.32494688747681);
    assert_eq!(d["ticket_url"], "https://example.org/remnants");
    assert_eq!(
        d["source_url"],
        "https://www.visitstockholm.com/events/remnants/next/"
    );
    assert!(d.get("start_time").is_none());
    assert_eq!(p1[1].data["start_time"], "19:00");
}

// ---------------------------------------------------------------------------------------
// eurostat_jsonstat

const ESTAT: &str = "/eurostat/api/dissemination/statistics/1.0/data/prc_ppp_ind";

#[tokio::test]
async fn eurostat_jsonstat_parses_fixture() {
    let cube = adapters::eurostat_jsonstat::parse(&fixture("eurostat_prc_ppp_ind.json")).unwrap();
    assert_eq!(cube.cells.len(), 7);
    assert_eq!(cube.dim("geo"), Some(3));
    let s = MockServer::start().await;
    serve(
        &s,
        ESTAT,
        fixture("eurostat_prc_ppp_ind.json"),
        "application/json",
    )
    .await;
    let url = format!(
        "{}{ESTAT}?format=JSON&lang=EN&na_item=PLI_EU27_2020&sinceTimePeriod=2015",
        s.uri()
    );
    let mut h = Harness::new(spec("eurostat-prices"), "eurostat_jsonstat", &url, t0()).await;
    let rep = h
        .run(&adapters::eurostat_jsonstat::EurostatJsonStat)
        .await
        .unwrap();
    assert_eq!((rep.requests, rep.records, rep.complete), (1, 1, true));
    assert_eq!(h.sink.records[0].meta.role, CaptureRole::Primary);

    // Garbage is spooled, then refused.
    let s = MockServer::start().await;
    serve(&s, ESTAT, br#"{"id":[]}"#.to_vec(), "application/json").await;
    let mut h = Harness::new(
        spec("eurostat-prices"),
        "eurostat_jsonstat",
        &format!("{}{ESTAT}", s.uri()),
        t0(),
    )
    .await;
    assert!(matches!(
        h.run(&adapters::eurostat_jsonstat::EurostatJsonStat).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(h.sink.records.len(), 1);
}

#[tokio::test]
async fn eurostat_jsonstat_writes_raw_first() {
    let s = MockServer::start().await;
    serve(
        &s,
        ESTAT,
        fixture("eurostat_prc_ppp_ind.json"),
        "application/json",
    )
    .await;
    raw_first_case(
        &adapters::eurostat_jsonstat::EurostatJsonStat,
        "eurostat-prices",
        &format!("{}{ESTAT}", s.uri()),
    )
    .await;
}

#[test]
fn eurostat_records_are_price_level_observations() {
    let pl = items(
        "eurostat_jsonstat",
        &format!("https://ec.europa.eu{ESTAT}?format=JSON"),
        &fixture("eurostat_prc_ppp_ind.json"),
    );
    assert_eq!(pl.len(), 6, "the EU27 aggregate is not a country");
    assert!(pl.iter().all(|i| i.kind == "price_level"));
    let keys: Vec<&str> = pl
        .iter()
        .map(|i| i.series_key.as_deref().unwrap())
        .collect();
    assert_eq!(
        keys,
        [
            "price_level:PT:transport:eu27_2020_100",
            "price_level:GR:restaurants_hotels:eu27_2020_100",
            "price_level:PT:restaurants_hotels:eu27_2020_100",
            "price_level:GR:gdp:eu27_2020_100",
            "price_level:GR:gdp:eu27_2020_100",
            "price_level:PT:gdp:eu27_2020_100",
        ]
    );
    let gr = pl
        .iter()
        .find(|i| i.id.as_deref() == Some("GR:restaurants_hotels:eu27_2020_100:2024"))
        .unwrap();
    assert_eq!(
        gr.data,
        json!({"country_code": "GR", "year": 2024, "date": "2024-01-01", "index": 81.2,
               "basis": "eu27_2020_100", "category": "restaurants_hotels",
               "category_code": "A0111", "category_label": "Restaurants and hotels",
               "indicator_code": "PLI_EU27_2020", "flags": "p", "publisher": "Eurostat",
               "as_of": "2026-06-18"})
    );
    let gr23 = pl
        .iter()
        .find(|i| i.id.as_deref() == Some("GR:gdp:eu27_2020_100:2023"))
        .unwrap();
    assert!(gr23.data.get("flags").is_none());
}

#[test]
fn price_categories_match_publisher_labels_exactly() {
    use adapters::records::price_category as c;
    assert_eq!(c("Restaurants and  hotels"), Some("restaurants_hotels"));
    assert_eq!(c("TRANSPORT"), Some("transport"));
    assert_eq!(
        c("Transport services"),
        None,
        "sub-categories are not the category"
    );
    assert_eq!(c("Bread and cereals"), None);
}

// ---------------------------------------------------------------------------------------
// wdi_csv (a ZIP dump)

const WDI: &str = "/data/download/WDI_CSV.zip";

#[tokio::test]
async fn wdi_csv_reads_one_zip_member_and_keeps_price_level_rows() {
    let s = MockServer::start().await;
    serve(
        &s,
        WDI,
        fixture("wdi_csv.zip"),
        "application/x-zip-compressed",
    )
    .await;
    let mut h = Harness::new(
        spec("worldbank-wdi-bulk"),
        "wdi_csv",
        &format!("{}{WDI}", s.uri()),
        t0(),
    )
    .await;
    h.client.chunk = 97; // archive headers and CSV rows straddle chunk boundaries
    let rep = h.run(&adapters::wdi_csv::WdiCsv::DEFAULT).await.unwrap();
    assert!(rep.complete);
    let out: String = h
        .resources()
        .iter()
        .map(|(ct, b)| {
            assert_eq!(ct, "text/csv");
            String::from_utf8(b.to_vec()).unwrap()
        })
        .collect();
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines[0].starts_with("\"Country Name\",\"Country Code\""),
        "{}",
        lines[0]
    );
    assert_eq!(lines.len(), 1 + 5, "only the five price-level rows: {out}");
    assert!(!out.contains("NY.GDP.MKTP.CD") && !out.contains("2-alpha code"));
    let m = &h.manifests()[0];
    assert_eq!(m["units_seen"], 8);
    assert_eq!(m["units_kept"], 5);
    assert_eq!(m["filter_version"], "wdi-price-level-v1");
    let raw = fixture("wdi_csv.zip");
    assert_eq!(m["size"], raw.len());

    let pl = items(
        "wdi_csv",
        &format!("https://databank.worldbank.org{WDI}"),
        out.as_bytes(),
    );
    let ids: Vec<&str> = pl.iter().map(|i| i.id.as_deref().unwrap()).collect();
    assert_eq!(
        ids,
        [
            "PT:gdp:us_1:2022",
            "PT:gdp:us_1:2023",
            "XK:gdp:us_1:2023",
            "US:gdp:us_1:2022",
            "US:gdp:us_1:2023"
        ],
        "aggregates, pre-2000 years and empty cells are left out"
    );
    assert_eq!(pl[1].series_key.as_deref(), Some("price_level:PT:gdp:us_1"));
    assert_eq!(pl[1].data["index"], 0.5861);
    assert_eq!(pl[1].data["date"], "2023-01-01");
    assert_eq!(pl[1].data["indicator_code"], "PA.NUS.PPPC.RF");
}

#[tokio::test]
async fn wdi_csv_without_its_data_member_fails() {
    let s = MockServer::start().await;
    // An archive whose only member is not the data file.
    let z = {
        let raw = fixture("wdi_csv.zip");
        // Cut right after the first member (WDICountry.csv) and its local header.
        let second = raw
            .windows(4)
            .enumerate()
            .skip(4)
            .find(|(_, w)| *w == [0x50, 0x4b, 0x03, 0x04])
            .map(|(i, _)| i)
            .unwrap();
        let mut z = raw[..second].to_vec();
        z.extend([0x50, 0x4b, 0x01, 0x02]);
        z
    };
    serve(&s, WDI, z, "application/zip").await;
    let mut h = Harness::new(
        spec("worldbank-wdi-bulk"),
        "wdi_csv",
        &format!("{}{WDI}", s.uri()),
        t0(),
    )
    .await;
    assert!(h.run(&adapters::wdi_csv::WdiCsv::DEFAULT).await.is_err());
    assert_eq!(h.manifests()[0]["complete"], false);
}

#[tokio::test]
async fn wdi_csv_writes_raw_first() {
    let s = MockServer::start().await;
    serve(&s, WDI, fixture("wdi_csv.zip"), "application/zip").await;
    raw_first_case(
        &adapters::wdi_csv::WdiCsv::DEFAULT,
        "worldbank-wdi-bulk",
        &format!("{}{WDI}", s.uri()),
    )
    .await;
}
