//! `gtfs_static` against a fake feed host (wiremock): streamed zip reading with malformed
//! rows, the size cap, conditional GET (304), the mapping to `transit_item`, the licence in the
//! manifest, and raw-first order. Feeds are built in the test (a minimal zip writer).

mod common;

use common::{Harness, assert_raw_first, spec, t0};
use serde_json::Value;
use vc_core::Error;
use vc_core::frontier::Validators;
use vc_core::sources::SourceAdapter;
use vc_core::store::{CaptureRole, SpoolPayload};
use vc_sources::adapters::gtfs_static::{GtfsSettings, GtfsStatic};
use vc_sources::adapters::records;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// ---------------------------------------------------------------------------------------
// A minimal zip writer: (name, content, deflate, data descriptor).

fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= u32::from(b);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn zip(entries: &[(&str, &[u8], bool, bool)]) -> Vec<u8> {
    use std::io::Write;
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, content, deflate, dd) in entries {
        let data = if *deflate {
            let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(content).unwrap();
            e.finish().unwrap()
        } else {
            content.to_vec()
        };
        let crc = crc32(content);
        let offset = out.len() as u32;
        let (c, u, h) = if *dd {
            (0, 0, 0)
        } else {
            (data.len() as u32, content.len() as u32, crc)
        };
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes());
        out.extend_from_slice(&(if *dd { 8u16 } else { 0 }).to_le_bytes());
        out.extend_from_slice(&(if *deflate { 8u16 } else { 0 }).to_le_bytes());
        out.extend_from_slice(&[0; 4]);
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&c.to_le_bytes());
        out.extend_from_slice(&u.to_le_bytes());
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);
        if *dd {
            out.extend_from_slice(&0x0807_4b50u32.to_le_bytes());
            out.extend_from_slice(&crc.to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(content.len() as u32).to_le_bytes());
        }
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&[0; 24]);
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    out.extend_from_slice(&central);
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&[0; 18]);
    out
}

/// An Auckland-like feed: one agency, three good routes and two bad ones, two stations, a
/// platform and a bus stop (not kept), a station with bad coordinates, a fare, feed_info, and
/// big `stop_times`/`trips` tables that must never be stored.
fn feed() -> Vec<u8> {
    let agency = "\u{feff}agency_id,agency_name,agency_url,agency_timezone,agency_lang,agency_phone,agency_fare_url\n\
        AT,Auckland Transport,https://at.govt.nz/,Pacific/Auckland,en,09 366 6400,https://at.govt.nz/fares/\n";
    let routes = "route_id,agency_id,route_short_name,route_long_name,route_type\n\
        NX1,AT,NX1,Britomart to Hibiscus Coast,3\n\
        EAST,AT,EAST,Eastern Line,2\n\
        DEV,AT,,Devonport Ferry,4\n\
        BAD1,AT,X,Bad type,99\n\
        ,AT,Y,No id,3\n";
    let stops = "stop_id,stop_code,stop_name,stop_lat,stop_lon,location_type,parent_station,wheelchair_boarding\n\
        BRIT,,Britomart Train Station,-36.8443,174.7676,1,,1\n\
        BRIT-1,9301,Britomart Platform 1,-36.8443,174.7676,0,BRIT,1\n\
        NEWM,,\"Newmarket Train Station, Auckland\",-36.8698,174.7781,1,,2\n\
        BUS1,7001,Some bus stop,-36.85,174.76,0,,0\n\
        BADC,,Broken Station,abc,174.7,1,,0\n";
    let fares = "fare_id,price,currency_type,payment_method,transfers,agency_id,transfer_duration\n\
        adult_1zone,2.40,NZD,0,,AT,3600\n\
        broken,free,NZD,0,,AT,\n";
    let info = "feed_publisher_name,feed_publisher_url,feed_lang,feed_start_date,feed_end_date,feed_version\n\
        Auckland Transport,https://at.govt.nz/,en,20261110,20270201,v1\n";
    let stop_times: String = (0..20_000)
        .map(|i| format!("T{i},08:00:00,08:00:00,BRIT-1,{i}\n"))
        .collect();
    let trips: String = (0..5_000).map(|i| format!("NX1,WD,T{i}\n")).collect();
    zip(&[
        ("agency.txt", agency.as_bytes(), true, false),
        ("calendar.txt", b"service_id,monday\nWD,1\n", false, false),
        ("fare_attributes.txt", fares.as_bytes(), true, true),
        ("feed_info.txt", info.as_bytes(), false, false),
        ("routes.txt", routes.as_bytes(), true, false),
        ("stop_times.txt", stop_times.as_bytes(), true, true),
        ("stops.txt", stops.as_bytes(), true, true),
        ("trips.txt", trips.as_bytes(), true, false),
    ])
}

async fn serve_zip(s: &MockServer, p: &str, body: Vec<u8>) {
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/zip")
                .insert_header("etag", "\"feed-v1\"")
                .insert_header("last-modified", "Tue, 10 Nov 2026 03:00:00 GMT")
                .set_body_bytes(body),
        )
        .mount(s)
        .await;
}

fn items(h: &Harness) -> Vec<Value> {
    h.resources()
        .iter()
        .flat_map(|(ct, b)| {
            assert_eq!(ct, "application/json");
            serde_json::from_slice::<Vec<Value>>(b).unwrap()
        })
        .collect()
}

#[tokio::test]
async fn gtfs_static_keeps_traveller_tables_and_counts_malformed_rows() {
    let s = MockServer::start().await;
    let raw = feed();
    serve_zip(&s, "/gtfs.zip", raw.clone()).await;
    let url = format!("{}/gtfs.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    let rep = h.run(&GtfsStatic::DEFAULT).await.unwrap();
    assert!(rep.complete);
    assert_eq!(rep.requests, 1);

    let it = items(&h);
    let of = |t: &str| it.iter().filter(|i| i["t"] == t).collect::<Vec<_>>();
    // One operator with its modes, route count and fare (the malformed fare is dropped).
    let ops = of("operator");
    assert_eq!(ops.len(), 1);
    assert_eq!(ops[0]["name"], "Auckland Transport");
    assert_eq!(ops[0]["fare_url"], "https://at.govt.nz/fares/");
    assert_eq!(ops[0]["phone"], "09 366 6400");
    assert_eq!(ops[0]["modes"], serde_json::json!(["bus", "ferry", "rail"]));
    assert_eq!(ops[0]["route_count"], 3);
    assert_eq!(ops[0]["fares"].as_array().unwrap().len(), 1);
    assert_eq!(ops[0]["fares"][0]["amount"], 2.4);
    assert_eq!(ops[0]["fares"][0]["unlimited_transfers"], true);
    assert_eq!(ops[0]["valid_from"], "2026-11-10");
    assert_eq!(ops[0]["valid_to"], "2027-02-01");
    // Three lines (bad route type and missing id are malformed).
    let lines = of("line");
    let names: Vec<&str> = lines.iter().map(|l| l["id"].as_str().unwrap()).collect();
    assert_eq!(names, ["NX1", "EAST", "DEV"]);
    assert_eq!(lines[1]["mode"], "rail");
    assert_eq!(lines[0]["operator"], "Auckland Transport");
    // Stations only (no platform, no bus stop; bad coordinates are malformed).
    let st = of("station");
    let names: Vec<&str> = st.iter().map(|l| l["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        [
            "Britomart Train Station",
            "Newmarket Train Station, Auckland"
        ]
    );
    assert_eq!(st[0]["wheelchair"], true);
    assert_eq!(st[1]["wheelchair"], false);
    assert_eq!(st[0]["timezone"], "Pacific/Auckland");
    // Nothing of stop_times or trips is stored.
    let all: String = h
        .resources()
        .iter()
        .map(|(_, b)| String::from_utf8_lossy(b).into_owned())
        .collect();
    assert!(!all.contains("08:00:00") && !all.contains("T4999"));

    // The manifest: the whole archive's digest, the per-feed report, the licence.
    let m = h
        .sink
        .records
        .iter()
        .find_map(|r| match (&r.payload, r.meta.role) {
            (SpoolPayload::Metadata { fields }, CaptureRole::Manifest) => Some(fields.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(m["complete"], true);
    assert_eq!(m["filter_version"], "gtfs-static-v1");
    assert_eq!(m["size"], raw.len());
    assert_eq!(
        m["sha256"],
        vc_sources::sha256::hex(&vc_sources::sha256::Sha256::digest(&raw))
    );
    assert_eq!(m["entries"], 8);
    let f = &m["feeds"][0];
    assert_eq!(f["malformed_rows"], 4, "2 routes, 1 station, 1 fare: {f}");
    assert_eq!(
        (f["routes"].as_u64(), f["stations"].as_u64()),
        (Some(3), Some(2))
    );
    let lic = &h.spec.licence;
    assert_eq!(m["licence"]["class"], "open");
    assert_eq!(m["licence"]["spdx"], "CC-BY-4.0");
    assert_eq!(m["licence"]["attribution"], lic.attribution.as_str());

    // Last: the status capture of the leased URL, with the validators for the next run.
    let last = h.sink.records.last().unwrap();
    assert_eq!(last.meta.role, CaptureRole::Primary);
    assert_eq!(last.meta.status, Some(200));
    assert_eq!(last.meta.validators.etag.as_deref(), Some("\"feed-v1\""));
    assert!(matches!(last.payload, SpoolPayload::Metadata { .. }));
    assert_eq!(
        last.meta.payload_hash.as_ref().map(|p| p.0),
        Some(vc_sources::sha256::Sha256::digest(&raw))
    );
}

#[tokio::test]
async fn gtfs_static_items_map_to_transit_records() {
    let s = MockServer::start().await;
    serve_zip(&s, "/gtfs.zip", feed()).await;
    let url = format!("{}/gtfs.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    h.run(&GtfsStatic::DEFAULT).await.unwrap();
    let (_, body) = &h.resources()[0];
    let u = vc_core::CanonicalUrl::parse(&url).unwrap();
    let recs = records::records("gtfs_static", &u, body).unwrap().unwrap();
    assert_eq!(recs.len(), 1 + 3 + 2);
    assert!(recs.iter().all(|r| r.kind == "transit_item"));
    let op = &recs[0];
    assert_eq!(op.id.as_deref(), Some("operator:AT"));
    assert_eq!(op.data["item_type"], "operator");
    assert_eq!(op.data["country_code"], "NZ");
    assert_eq!(op.data["website_url"], "https://at.govt.nz/");
    assert_eq!(op.data["fares"][0]["price"]["text"], "2.40 NZD");
    assert_eq!(op.data["fares"][0]["transfer_window_min"], 60);
    let line = recs.iter().find(|r| r.data["mode"] == "ferry").unwrap();
    assert_eq!(line.data["name"], "Devonport Ferry");
    let nx1 = recs
        .iter()
        .find(|r| r.id.as_deref() == Some("line:NX1"))
        .unwrap();
    assert_eq!(nx1.data["name"], "NX1: Britomart to Hibiscus Coast");
    let st = recs
        .iter()
        .find(|r| r.data["item_type"] == "station")
        .unwrap();
    assert_eq!(st.data["wheelchair_accessible"], true);
    assert!(st.data["lat"].as_f64().unwrap() < -36.0);
    // Every record fits the kind's schema.
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(common::repo_root().join("schemas/transit_item.v1.json")).unwrap(),
    )
    .unwrap();
    let v = jsonschema::validator_for(&schema).unwrap();
    for r in &recs {
        assert!(v.is_valid(&r.data), "{}", r.data);
    }
}

#[tokio::test]
async fn gtfs_static_conditional_get_costs_one_304() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/gtfs.zip"))
        .and(header("if-none-match", "\"feed-v1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"feed-v1\""))
        .expect(1)
        .mount(&s)
        .await;
    let url = format!("{}/gtfs.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    h.claim.urls[0].validators = Validators {
        etag: Some("\"feed-v1\"".into()),
        last_modified: Some("Tue, 10 Nov 2026 03:00:00 GMT".into()),
    };
    let rep = h.run(&GtfsStatic::DEFAULT).await.unwrap();
    assert!(rep.complete);
    assert_eq!(rep.requests, 1);
    let sent = h.client.sent.lock().unwrap().clone();
    assert!(sent[0].contains(&("if-none-match".into(), "\"feed-v1\"".into())));
    assert!(sent[0].contains(&(
        "if-modified-since".into(),
        "Tue, 10 Nov 2026 03:00:00 GMT".into()
    )));
    assert!(
        h.resources().is_empty(),
        "an unchanged feed stores no items"
    );
    let m = &h.manifests()[0];
    assert_eq!(m["not_modified"], true);
    let last = h.sink.records.last().unwrap();
    assert_eq!(
        (last.meta.role, last.meta.status),
        (CaptureRole::Primary, Some(304))
    );
    assert_eq!(last.meta.validators.etag.as_deref(), Some("\"feed-v1\""));
    assert!(last.meta.payload_hash.is_none());
}

#[tokio::test]
async fn gtfs_static_skips_a_feed_over_the_size_cap() {
    let s = MockServer::start().await;
    serve_zip(&s, "/big.zip", feed()).await;
    let url = format!("{}/big.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    let small = GtfsStatic::new(GtfsSettings {
        max_bytes: 1024,
        ..GtfsSettings::DEFAULT
    });
    let rep = h.run(&small).await.unwrap();
    assert!(
        rep.complete,
        "a skipped feed is a finished run, not an error"
    );
    assert!(h.resources().is_empty());
    let m = &h.manifests()[0];
    assert_eq!(m["skipped"], "over_size_cap");
    assert_eq!(m["size_cap"], 1024);
    assert_eq!(h.sink.records.len(), 1, "only the manifest");
}

#[tokio::test]
async fn gtfs_static_stores_nothing_from_a_truncated_or_non_zip_download() {
    let s = MockServer::start().await;
    let raw = feed();
    serve_zip(&s, "/cut.zip", raw[..raw.len() / 2].to_vec()).await;
    Mock::given(method("GET"))
        .and(path("/page.zip"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/octet-stream")
                .set_body_bytes(b"<html>login</html>".to_vec()),
        )
        .mount(&s)
        .await;
    for p in ["/cut.zip", "/page.zip"] {
        let url = format!("{}{p}", s.uri());
        let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
        let err = h.run(&GtfsStatic::DEFAULT).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{p}: {err:?}");
        assert!(h.resources().is_empty(), "{p}");
        assert_eq!(h.sink.records.len(), 1, "{p}: only the manifest");
        assert_eq!(h.manifests()[0]["complete"], false);
    }
}

#[tokio::test]
async fn gtfs_static_maps_http_errors_and_parks_on_403() {
    let s = MockServer::start().await;
    Mock::given(path("/gone.zip"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&s)
        .await;
    Mock::given(path("/wall.zip"))
        .respond_with(ResponseTemplate::new(403))
        .mount(&s)
        .await;
    let url = format!("{}/gone.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    assert!(matches!(
        h.run(&GtfsStatic::DEFAULT).await.unwrap_err(),
        Error::NotFound(_)
    ));
    let url = format!("{}/wall.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    let err = h.run(&GtfsStatic::DEFAULT).await.unwrap_err();
    assert!(
        matches!(
            err,
            Error::Fetch(vc_core::fetch::FetchError::Challenge { status: 403 })
        ),
        "{err:?}"
    );
    assert_eq!(
        s.received_requests().await.unwrap().len(),
        2,
        "never retried"
    );
}

#[tokio::test]
async fn gtfs_static_writes_raw_first_and_fails_closed_on_spool_errors() {
    let s = MockServer::start().await;
    serve_zip(&s, "/gtfs.zip", feed()).await;
    let url = format!("{}/gtfs.zip", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    h.run(&GtfsStatic::DEFAULT).await.unwrap();
    assert_raw_first(&h.events.all());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    h.sink.fail = true;
    assert!(matches!(
        h.run(&GtfsStatic::DEFAULT).await.unwrap_err(),
        Error::Storage(_)
    ));
}

#[tokio::test]
async fn gtfs_static_waits_for_the_dump_window() {
    let s = MockServer::start().await;
    serve_zip(&s, "/gtfs.zip", feed()).await;
    let url = format!("{}/gtfs.zip", s.uri());
    let noon = t0() + chrono::Duration::hours(9);
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, noon).await;
    let rep = h.run(&GtfsStatic::DEFAULT).await.unwrap();
    assert_eq!(rep.requests, 0);
    assert!(s.received_requests().await.unwrap().is_empty());
}

#[test]
fn gtfs_sources_are_wired_with_licence_and_politeness() {
    // Keyless feeds on, keyed ones wired but off, robots-blocked and unread-terms ones off.
    let on = [
        "ovapi-gtfs-nl",
        "opentransportdata-swiss",
        "vic-gtfs",
        "translink-seq-gtfs",
        "entur-gtfs",
        "auckland-gtfs",
        // 2026-11-18 open-data batch.
        "prague-pid-gtfs",
        "budapest-bkk-gtfs",
        "mbta-gtfs",
        "cta-gtfs",
        "king-county-metro-gtfs",
        "ireland-nta-gtfs",
        "lithuania-gtfs",
        "luxembourg-gtfs",
    ];
    let off = [
        "stm-montreal-gtfs",
        "tfnsw-opendata",
        "sfbay-511",
        "trafiklab-gtfs-sverige",
        "digitransit",
        "delhi-otd",
        "delfi-opendata-oepnv",
        "spain-nap",
        "mta-gtfs",
        "gtfs-de",
        "hk-td-gtfs",
    ];
    for id in on.iter().chain(&off) {
        let sp = spec(id);
        assert_eq!(sp.adapter.as_deref(), Some("gtfs_static"), "{id}");
        assert_eq!(sp.enabled, on.contains(id), "{id}");
        assert!(
            sp.kinds.iter().any(|k| k.0.as_ref() == "transit_item"),
            "{id}"
        );
        assert!(sp.llm_kinds.is_empty(), "{id}: no model on timetables");
        assert!(!sp.licence.attribution.is_empty(), "{id}");
        assert!(sp.min_delay_s >= 5, "{id}");
    }
    // Crawl-delay 10 on data.opentransportdata.swiss.
    assert!(spec("opentransportdata-swiss").min_delay_s >= 10);
    assert_eq!(
        spec("entur-gtfs").licence.attribution,
        "Data made available by Entur"
    );
    for id in [
        "tfnsw-opendata",
        "sfbay-511",
        "trafiklab-gtfs-sverige",
        "digitransit",
        "delhi-otd",
    ] {
        let sp = spec(id);
        assert!(sp.needs_key && sp.key_secret.is_some(), "{id}");
        let host = sp.seeds[0].host_str().unwrap().to_string();
        assert!(
            vc_sources::adapters::gtfs_static::auth_for(&host).is_some(),
            "{id}: the adapter knows where {host} takes its key"
        );
    }
}

#[test]
fn gtfs_static_settings_follow_config() {
    let mut c = vc_config::SourcesConfig::default();
    assert_eq!(
        GtfsSettings::from_config(&c).max_bytes,
        GtfsSettings::DEFAULT.max_bytes
    );
    c.gtfs_max_bytes = 42;
    assert_eq!(GtfsSettings::from_config(&c).max_bytes, 42);
    let a: &dyn SourceAdapter = vc_sources::adapters::get("gtfs_static").unwrap();
    assert_eq!(a.id(), "gtfs_static");
}

// ---------------------------------------------------------------------------------------
// "Latest file" resolver: an index (udata/CKAN JSON, or an HTML page) names the current zip.

fn udata_index(base: &str) -> String {
    serde_json::json!({
        "id": "5a2a58b9111e9b7f34fc6606",
        "license": "cc-by",
        "resources": [
            {"id": "a", "title": "gtfs-20261104-20261212.zip", "type": "main", "format": "zip",
             "url": format!("{base}/resources/lu/20261105-053451/gtfs-20261104-20261212.zip"),
             "last_modified": "2026-11-05T05:34:55.712000+00:00"},
            {"id": "b", "title": "gtfs-20261111-20261212.zip", "type": "main", "format": "zip",
             "url": "/resources/lu/20261112-064412/gtfs-20261111-20261212.zip",
             "last_modified": "2026-11-12T06:44:37.368000+00:00"},
            {"id": "c", "title": "documentation.zip", "type": "documentation", "format": "zip",
             "url": format!("{base}/resources/lu/doc.zip"),
             "last_modified": "2026-11-17T00:00:00+00:00"},
            {"id": "d", "title": "stops.csv", "type": "main", "format": "csv",
             "url": format!("{base}/resources/lu/stops.csv"),
             "last_modified": "2026-11-18T00:00:00+00:00"}
        ]
    })
    .to_string()
}

#[test]
fn resolve_latest_picks_the_newest_main_zip_of_a_udata_or_ckan_index() {
    use vc_core::CanonicalUrl;
    use vc_sources::adapters::gtfs_static::resolve_latest;
    let idx = CanonicalUrl::parse("https://data.public.lu/api/1/datasets/x/").unwrap();
    let got = resolve_latest(
        &idx,
        Some("application/json"),
        udata_index("https://download.data.public.lu").as_bytes(),
    )
    .unwrap();
    // The newest `main` zip (documentation and the CSV are ignored); relative URLs resolve
    // against the index.
    assert_eq!(
        got.as_str(),
        "https://data.public.lu/resources/lu/20261112-064412/gtfs-20261111-20261212.zip"
    );
    let ckan = serde_json::json!({"success": true, "result": {"resources": [
        {"url": "https://ckan.test/d/old.zip", "format": "GTFS", "metadata_modified": "2026-10-01T00:00:00"},
        {"url": "https://ckan.test/d/new.zip", "format": "ZIP", "metadata_modified": "2026-11-01T00:00:00"},
        {"url": "https://ckan.test/d/readme.pdf", "format": "PDF", "metadata_modified": "2026-11-15T00:00:00"}
    ]}});
    let idx = CanonicalUrl::parse("https://ckan.test/api/3/action/package_show?id=d").unwrap();
    assert_eq!(
        resolve_latest(
            &idx,
            Some("application/json; charset=utf-8"),
            ckan.to_string().as_bytes()
        )
        .unwrap()
        .as_str(),
        "https://ckan.test/d/new.zip"
    );
    // Nothing qualifies → None (the adapter stores nothing and fails).
    let none = serde_json::json!({"resources": [{"url": "https://x.test/a.csv", "format": "csv"}]});
    assert!(resolve_latest(&idx, Some("application/json"), none.to_string().as_bytes()).is_none());
    assert!(resolve_latest(&idx, Some("application/json"), b"not json").is_none());
    assert!(resolve_latest(&idx, Some("application/zip"), b"PK").is_none());
}

#[test]
fn resolve_latest_reads_date_stamped_zip_links_of_an_html_index() {
    use vc_core::CanonicalUrl;
    use vc_sources::adapters::gtfs_static::resolve_latest;
    let idx = CanonicalUrl::parse("https://feeds.test/gtfs/").unwrap();
    let html = br#"<html><body><pre>
        <a href="/gtfs/feed_20261111.zip">feed_20261111.zip</a>
        <a href='feed_20261118.zip'>feed_20261118.zip</a>
        <a href="/gtfs/feed_20261104.zip">feed_20261104.zip</a>
        <a href="/gtfs/feed_20261118.zip-validation-report.html">report</a>
        <a href="/gtfs/notes.txt">notes</a></pre></body></html>"#;
    assert_eq!(
        resolve_latest(&idx, Some("text/html; charset=UTF-8"), html)
            .unwrap()
            .as_str(),
        "https://feeds.test/gtfs/feed_20261118.zip"
    );
    assert!(resolve_latest(&idx, Some("text/html"), b"<html>no feeds</html>").is_none());
}

#[tokio::test]
async fn gtfs_static_resolves_an_index_then_downloads_the_current_zip() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/1/datasets/x/"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("etag", "\"index-v7\"")
                .set_body_raw(udata_index(&s.uri()), "application/json"),
        )
        .expect(1)
        .mount(&s)
        .await;
    let raw = feed();
    serve_zip(
        &s,
        "/resources/lu/20261112-064412/gtfs-20261111-20261212.zip",
        raw.clone(),
    )
    .await;
    let url = format!("{}/api/1/datasets/x/", s.uri());
    let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
    let rep = h.run(&GtfsStatic::DEFAULT).await.unwrap();
    assert!(rep.complete);
    assert_eq!(rep.requests, 2, "the index, then the zip it names");
    let got: Vec<String> = s
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(
        got,
        [
            "/api/1/datasets/x/",
            "/resources/lu/20261112-064412/gtfs-20261111-20261212.zip"
        ]
    );
    // Raw first: the index is spooled (manifest role, never mapped) before any item.
    let first = &h.sink.records[0];
    assert_eq!(first.meta.role, CaptureRole::Manifest);
    assert!(matches!(first.payload, SpoolPayload::Response(_)));
    assert_raw_first(&h.events.all());
    assert_eq!(items(&h).iter().filter(|i| i["t"] == "operator").count(), 1);
    let m = h
        .manifests()
        .into_iter()
        .find(|m| m["type"] == "dump_manifest")
        .unwrap();
    assert_eq!(m["complete"], true);
    assert_eq!(m["resolved_from_index"], true);
    assert!(
        m["resolved_url"]
            .as_str()
            .unwrap()
            .ends_with("/gtfs-20261111-20261212.zip")
    );
    assert_eq!(
        m["sha256"],
        vc_sources::sha256::hex(&vc_sources::sha256::Sha256::digest(&raw))
    );
    // The leased row (the index) keeps the index's validators for the next conditional GET.
    let last = h.sink.records.last().unwrap();
    assert_eq!(
        (last.meta.role, last.meta.status),
        (CaptureRole::Primary, Some(200))
    );
    assert_eq!(last.meta.validators.etag.as_deref(), Some("\"index-v7\""));
}

#[tokio::test]
async fn gtfs_static_never_follows_an_index_to_another_site() {
    let s = MockServer::start().await;
    let other = serde_json::json!({"resources": [
        {"type": "main", "format": "zip", "url": "https://elsewhere.example/feed.zip",
         "last_modified": "2026-11-12T00:00:00+00:00"}
    ]});
    Mock::given(path("/api/1/datasets/y/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(other.to_string(), "application/json"),
        )
        .mount(&s)
        .await;
    Mock::given(path("/empty/"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html>nothing here</html>", "text/html"),
        )
        .mount(&s)
        .await;
    for p in ["/api/1/datasets/y/", "/empty/"] {
        let url = format!("{}{p}", s.uri());
        let mut h = Harness::new(spec("auckland-gtfs"), "gtfs_static", &url, t0()).await;
        let err = h.run(&GtfsStatic::DEFAULT).await.unwrap_err();
        assert!(matches!(err, Error::Invalid(_)), "{p}: {err:?}");
        assert!(h.resources().is_empty(), "{p}");
        let m = h
            .manifests()
            .into_iter()
            .find(|m| m["type"] == "dump_manifest")
            .unwrap();
        assert_eq!(m["complete"], false, "{p}");
    }
    assert_eq!(
        s.received_requests().await.unwrap().len(),
        2,
        "one index request each, nothing followed"
    );
}
