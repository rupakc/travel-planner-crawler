//! The generic, mapping-driven open-data readers (2026-11-18): `socrata`, `udata_resource` and
//! `wfs_geojson`, against recorded fixtures (`testkit/fixtures/vc-sources/README.md`) served by
//! wiremock, and their row mappings (`restaurant`, `hotel`, `attraction`, `amenity`).

mod common;

use common::{Harness, assert_raw_first, fixture, spec, t0};
use serde_json::{Value, json};
use vc_core::{CanonicalUrl, Error};
use vc_sources::adapters;
use vc_sources::adapters::records::{AdapterItem, MAPPED, records};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok(body: Vec<u8>, content_type: &str) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", content_type)
        .set_body_bytes(body)
}

fn items(adapter: &str, url: &str, body: &[u8]) -> Vec<AdapterItem> {
    records(adapter, &CanonicalUrl::parse(url).unwrap(), body)
        .expect("adapter has a record mapping")
        .expect("body maps")
}

fn validator(kind: &str) -> jsonschema::Validator {
    let schema: Value = serde_json::from_str(
        &std::fs::read_to_string(common::repo_root().join(format!("schemas/{kind}.v1.json")))
            .unwrap(),
    )
    .unwrap();
    jsonschema::validator_for(&schema).unwrap()
}

/// Every item fits its kind's schema.
fn fit(items: &[AdapterItem]) {
    assert!(!items.is_empty());
    for i in items {
        let v = validator(i.kind);
        assert!(v.is_valid(&i.data), "{}: {}", i.kind, i.data);
    }
}

#[test]
fn readers_are_registered_mapped_and_socrata_resumes() {
    for id in ["socrata", "udata_resource", "wfs_geojson"] {
        assert!(adapters::get(id).is_some(), "{id} registered");
        assert!(MAPPED.contains(&id), "{id} has a record mapping");
        assert!(
            adapters::registry().iter().any(|a| a.id() == id),
            "{id} in the configured registry"
        );
    }
    assert!(
        adapters::resumable().iter().any(|a| a.id() == "socrata"),
        "socrata pages resume"
    );
}

// ---------------------------------------------------------------------------------------
// socrata

const CHICAGO: &str = "/resource/4ijn-s7e5.json";

#[tokio::test]
async fn socrata_asks_only_for_the_mapped_columns_and_window() {
    let m = adapters::socrata::MAPPINGS
        .iter()
        .find(|m| m.path == CHICAGO)
        .unwrap();
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(CHICAGO))
        .and(query_param("$select", m.select))
        .and(query_param(
            "$where",
            "inspection_date >= '2023-11-17' AND license_ > 0 AND results in('Pass','Pass w/ Conditions','Fail') AND facility_type in('Restaurant','Bakery','TAVERN','Golden Diner')",
        ))
        .and(query_param("$order", "license_,inspection_date DESC"))
        .and(query_param("$limit", "2000"))
        .and(query_param("$offset", "0"))
        .respond_with(ok(
            fixture("socrata_chicago_food_inspections.json"),
            "application/json",
        ))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "socrata",
        &format!("{}{CHICAGO}", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&adapters::socrata::Socrata::DEFAULT).await.unwrap();
    assert_eq!(
        (rep.requests, rep.records, rep.complete),
        (1, 1, true),
        "a short page ends the list"
    );
    assert_raw_first(&h.events.all());
}

#[tokio::test]
async fn socrata_refuses_a_dataset_without_a_mapping() {
    let s = MockServer::start().await;
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "socrata",
        &format!("{}/resource/zzzz-zzzz.json", s.uri()),
        t0(),
    )
    .await;
    let err = h
        .run(&adapters::socrata::Socrata::DEFAULT)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
    assert!(h.client.requested.lock().unwrap().is_empty());
}

#[tokio::test]
async fn socrata_follows_full_pages_and_leaves_a_cursor() {
    const CAT: &str = "/resource/t2h3-cgys.json";
    let s = MockServer::start().await;
    // A full page (limit 2000) is simulated by a cursor page of limit 7 that is full.
    Mock::given(method("GET"))
        .and(path(CAT))
        .and(query_param("$offset", "7"))
        .respond_with(ok(
            fixture("socrata_catalonia_tourism_register.json"),
            "application/json",
        ))
        .mount(&s)
        .await;
    let a = adapters::socrata::Socrata { max_pages: 1 };
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "socrata",
        &format!("{}{CAT}", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run_from(
            &a,
            Some(&format!("{CAT}?$order=n_mero_inscripci&$limit=7&$offset=7")),
        )
        .await
        .unwrap();
    assert!(!rep.complete);
    let cursor = rep.cursor.expect("cursor");
    let next = CanonicalUrl::parse(&format!("{}{cursor}", s.uri())).unwrap();
    assert_eq!(
        adapters::inspections::window(&next, "$offset", "$limit"),
        Some((14, 7)),
        "{cursor}"
    );
    // A cursor for another dataset is ignored (the run starts over).
    assert!(
        adapters::socrata::resume_url(
            &CanonicalUrl::parse(&format!("{}{CAT}", s.uri())).unwrap(),
            "/resource/43nn-pn8j.json?$limit=7"
        )
        .is_none()
    );
}

#[test]
fn socrata_grouped_pages_never_split_an_establishment() {
    use adapters::socrata as so;
    let m = so::MAPPINGS.iter().find(|m| m.path == CHICAGO).unwrap();
    let rows = so::parse(&fixture("socrata_chicago_food_inspections.json")).unwrap();
    assert_eq!(rows.len(), 7);
    let u = CanonicalUrl::parse(&format!(
        "https://data.cityofchicago.org{CHICAGO}?$limit=7&$offset=0"
    ))
    .unwrap();
    let (kept, next) = so::page(m, &u, &rows).unwrap();
    assert_eq!(kept.len(), 5, "licence 207 waits for the next page");
    assert_eq!(
        adapters::inspections::window(&next.unwrap(), "$offset", "$limit"),
        Some((5, 7))
    );
    // The mapping makes the same cut.
    assert_eq!(
        items(
            "socrata",
            u.as_str(),
            &fixture("socrata_chicago_food_inspections.json")
        )
        .len(),
        1
    );
}

#[test]
fn socrata_restaurants_take_each_licences_latest_result() {
    let it = items(
        "socrata",
        &format!("https://data.cityofchicago.org{CHICAGO}?$limit=2000&$offset=0"),
        &fixture("socrata_chicago_food_inspections.json"),
    );
    fit(&it);
    assert_eq!(it.len(), 2);
    assert_eq!(it[0].kind, "restaurant");
    assert_eq!(it[0].id.as_deref(), Some("chicago-cdph:149"));
    assert_eq!(
        it[0].data,
        json!({
            "name": "FOX'S BEVERLY PUB", "establishment_type": "restaurant",
            "category_label": "Restaurant",
            "address": "9956 S WESTERN AVE, 60643 Chicago",
            "city": "Chicago", "country": "United States", "country_code": "US",
            "lat": 41.71201213322909, "lng": -87.68193540843004,
            "hygiene": {"scheme": "other", "rating": "Pass", "rating_date": "2026-07-30",
                        "authority": "Chicago Department of Public Health"},
            "as_of": "2026-07-30"
        })
    );
    assert_eq!(it[1].data["hygiene"]["rating_date"], "2024-12-18");

    let austin = items(
        "socrata",
        "https://data.austintexas.gov/resource/ecmv-9xxi.json?$limit=2000&$offset=0",
        &fixture("socrata_austin_food_inspections.json"),
    );
    fit(&austin);
    assert_eq!(
        austin.len(),
        5,
        "Golden Boy's two inspections are one record"
    );
    let golden = austin
        .iter()
        .find(|i| i.id.as_deref() == Some("austin-aph:177715"))
        .unwrap();
    assert_eq!(
        golden.data["hygiene"]["rating"], "88",
        "score as the register shows it"
    );
    assert_eq!(golden.data["hygiene"]["rating_date"], "2026-09-22");
    assert_eq!(
        golden.data["address"], "111 Congress Ave Austin, TX 78701-4050",
        "the postcode is already in the address"
    );
}

#[test]
fn socrata_tourism_register_maps_hotels_without_owner_data() {
    let m = adapters::socrata::MAPPINGS
        .iter()
        .find(|m| m.path == "/resource/t2h3-cgys.json")
        .unwrap();
    for col in [
        "nom_del_titular",
        "primer_cognom",
        "segon_cognom",
        "cif",
        "ra_social",
    ] {
        assert!(!m.select.contains(col), "{col} is never fetched");
    }
    assert!(m.filter.contains("'Hotels'") && !m.filter.contains("Habitatge"));
    let it = items(
        "socrata",
        "https://analisi.transparenciacatalunya.cat/resource/t2h3-cgys.json?$limit=2000&$offset=0",
        &fixture("socrata_catalonia_tourism_register.json"),
    );
    fit(&it);
    assert_eq!(it.len(), 7);
    let by = |id: &str| {
        it.iter()
            .find(|i| i.id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("{id}"))
    };
    assert_eq!(
        by("catalonia-rtc:hb-000002").data,
        json!({
            "name": "Suizo", "property_type": "hotel", "star_rating": 3.0,
            "address": "Plaça Del Ángel 12, 08002 Barcelona", "neighbourhood": "Barcelonès",
            "city": "Barcelona", "country": "Spain", "country_code": "ES"
        })
    );
    assert_eq!(
        by("catalonia-rtc:hb-000005").data["property_type"],
        "guesthouse"
    );
    assert_eq!(
        by("catalonia-rtc:atb-000001").data["property_type"],
        "apartment"
    );
    assert_eq!(
        by("catalonia-rtc:kb-000004").data["property_type"],
        "campsite"
    );
    assert!(
        by("catalonia-rtc:atb-000001")
            .data
            .get("star_rating")
            .is_none()
    );
}

#[test]
fn socrata_public_restrooms_are_amenities() {
    let it = items(
        "socrata",
        "https://data.cityofnewyork.us/resource/i7jb-7jku.json?$limit=2000&$offset=0",
        &fixture("socrata_nyc_public_restrooms.json"),
    );
    fit(&it);
    assert_eq!(it.len(), 5);
    assert_eq!(it[0].kind, "amenity");
    assert_eq!(
        it[0].data,
        json!({
            "name": "Ben Abrams Playground", "amenity_type": "toilet",
            "city": "New York City", "country": "United States", "country_code": "US",
            "lat": 40.85438, "lng": -73.8697,
            "opening_hours": "8am-4pm, Open later seasonally",
            "wheelchair_accessible": true, "operator": "NYC Parks"
        })
    );
    assert_eq!(
        it[0].id.as_deref(),
        Some("nyc-restrooms:ben abrams playground:40.854380:-73.869700")
    );
}

// ---------------------------------------------------------------------------------------
// udata_resource

const DATASET: &str = "/api/1/datasets/hebergements-touristiques-classes-en-france/";
const FILE: &str = "/static/exportHebergementsClasses/hebergements_classes.csv";

async fn udata_server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(DATASET))
        .respond_with(ok(
            fixture("udata_atout_france_dataset.json"),
            "application/json",
        ))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(FILE))
        .respond_with(ok(fixture("udata_atout_france_classes.csv"), "text/csv"))
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn udata_dataset_row_enqueues_the_current_file_on_an_owned_site() {
    let s = udata_server().await;
    // The source lists the file's host among its seeds: the file is followed (as its own
    // adapter row, under that host's politeness key and robots.txt).
    let mut owned = spec("nyc-restaurant-inspections");
    owned.seeds = vec![
        url::Url::parse(&format!("{}{DATASET}", s.uri())).unwrap(),
        url::Url::parse(&format!("https://data.classement.atout-france.fr{FILE}")).unwrap(),
    ];
    let mut h = Harness::new(
        owned,
        "udata_resource",
        &format!("{}{DATASET}", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::udata_resource::UdataResource::DEFAULT)
        .await
        .unwrap();
    assert_eq!((rep.requests, rep.enqueued, rep.complete), (1, 1, true));
    assert_eq!(
        h.frontier.enqueued_urls(),
        vec![format!("https://data.classement.atout-france.fr{FILE}")]
    );
    assert_raw_first(&h.events.all());
    let req = h.client.requested.lock().unwrap().clone();
    assert!(!req.iter().any(|u| u.contains("/datasets/r/")), "{req:?}");

    // Not on the source's own sites: not followed.
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "udata_resource",
        &format!("{}{DATASET}", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::udata_resource::UdataResource::DEFAULT)
        .await
        .unwrap();
    assert_eq!((rep.requests, rep.enqueued), (1, 0));
    assert!(h.frontier.enqueued_urls().is_empty());
}

#[test]
fn udata_picks_the_newest_matching_resource() {
    let m = &adapters::udata_resource::MAPPINGS[0];
    let body = json!({"resources": [
        {"format": "csv", "url": "https://static.data.gouv.fr/resources/x/1/a.csv", "last_modified": "2026-11-18"},
        {"format": "json", "url": format!("https://data.classement.atout-france.fr{FILE}"), "last_modified": "2026-11-18"},
        {"format": "CSV", "url": format!("https://data.classement.atout-france.fr{FILE}?v=old"), "last_modified": "2026-01-01"},
        {"format": "csv", "url": format!("https://data.classement.atout-france.fr{FILE}"), "last_modified": "2026-11-17"},
        {"format": "csv", "url": format!("http://data.classement.atout-france.fr{FILE}?v=plain"), "last_modified": "2026-12-01"}
    ]});
    let got = adapters::udata_resource::pick_resource(m, &serde_json::to_vec(&body).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        got.as_str(),
        format!("https://data.classement.atout-france.fr{FILE}")
    );
}

#[tokio::test]
async fn udata_file_row_streams_the_csv_and_maps_the_register() {
    let s = udata_server().await;
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "udata_resource",
        &format!("{}{FILE}", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::udata_resource::UdataResource::DEFAULT)
        .await
        .unwrap();
    assert_eq!(rep.requests, 1);
    let res = h.resources();
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].0, "text/csv");
    let m = h.manifests();
    assert_eq!(m[0]["filter_version"], "atout-france-classes-v1");

    let it = items(
        "udata_resource",
        &format!("https://data.classement.atout-france.fr{FILE}"),
        &res[0].1,
    );
    fit(&it);
    assert_eq!(it.len(), 7);
    assert_eq!(
        it[0].data,
        json!({
            "name": "1924 HÔTEL", "property_type": "hotel", "star_rating": 3.0,
            "address": "2 Rue Gabriel Péri, 38000 GRENOBLE", "city": "GRENOBLE",
            "country": "France", "country_code": "FR",
            "website_url": "https://www.1924hotel.com/", "as_of": "2023-06-19"
        })
    );
    assert_eq!(
        it[0].id.as_deref(),
        Some("atout-france:hôtel de tourisme:1924 hôtel:38000")
    );
    let types: Vec<&str> = it
        .iter()
        .map(|i| i.data["property_type"].as_str().unwrap())
        .collect();
    assert_eq!(
        types,
        [
            "hotel",
            "hotel",
            "hotel",
            "campsite",
            "apartment",
            "resort",
            "hostel"
        ]
    );
    assert_eq!(
        it[5].data["website_url"],
        "https://www.amucchiatana.corsica"
    );
    assert!(
        it[6].data.get("star_rating").is_none(),
        "hostels have no stars"
    );
    // The dataset API response itself yields nothing.
    assert!(
        items(
            "udata_resource",
            &format!("https://www.data.gouv.fr{DATASET}"),
            &fixture("udata_atout_france_dataset.json")
        )
        .is_empty()
    );
}

#[tokio::test]
async fn udata_file_without_the_required_columns_is_refused() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(FILE))
        .respond_with(ok(b"a;b\n1;2\n".to_vec(), "text/csv"))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "udata_resource",
        &format!("{}{FILE}", s.uri()),
        t0(),
    )
    .await;
    let err = h
        .run(&adapters::udata_resource::UdataResource::DEFAULT)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
}

// ---------------------------------------------------------------------------------------
// wfs_geojson

fn wien(layer: &str) -> String {
    format!(
        "https://data.wien.gv.at/daten/geo?service=WFS&request=GetFeature&version=1.1.0&typeName=ogdwien:{layer}&srsName=EPSG:4326&outputFormat=json"
    )
}

fn zuerich(layer: &str) -> String {
    format!(
        "https://www.ogd.stadt-zuerich.ch/wfs/geoportal/Zueri_WC?service=WFS&version=1.1.0&request=GetFeature&typename={layer}&outputFormat=GeoJSON&srsName=EPSG:4326"
    )
}

#[tokio::test]
async fn wfs_geojson_is_one_request_per_layer() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/daten/geo"))
        .and(query_param("typeName", "ogdwien:WCANLAGEOGD"))
        .respond_with(ok(fixture("wfs_wien_toilets.json"), "application/json"))
        .mount(&s)
        .await;
    let row = wien("WCANLAGEOGD").replace("https://data.wien.gv.at", &s.uri());
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "wfs_geojson",
        &row,
        t0(),
    )
    .await;
    let rep = h.run(&adapters::wfs_geojson::WfsGeojson).await.unwrap();
    assert_eq!((rep.requests, rep.records, rep.complete), (1, 1, true));
    assert_raw_first(&h.events.all());

    // An unmapped layer is refused before any request.
    let row = wien("TRINKBRUNNENOGD").replace("https://data.wien.gv.at", &s.uri());
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "wfs_geojson",
        &row,
        t0(),
    )
    .await;
    assert!(h.run(&adapters::wfs_geojson::WfsGeojson).await.is_err());
    assert!(h.client.requested.lock().unwrap().is_empty());
}

#[test]
fn wfs_geojson_maps_vienna_and_zurich_layers() {
    let sights = items(
        "wfs_geojson",
        &wien("SEHENSWUERDIGOGD"),
        &fixture("wfs_wien_sights.json"),
    );
    fit(&sights);
    assert_eq!(sights.len(), 3);
    assert_eq!(
        sights[0].data,
        json!({
            "name": "Ankeruhr", "category": "landmark", "address": "Hoher Markt 10/",
            "city": "Vienna", "country": "Austria", "country_code": "AT",
            "lat": 48.21076056, "lng": 16.37367575,
            "info_url": "http://www.wien.info/article.asp?IDArticle=2018"
        })
    );
    assert_eq!(
        sights[0].id.as_deref(),
        Some("wien-ogd:sehenswuerdigogd.643934")
    );
    let museums = items(
        "wfs_geojson",
        &wien("MUSEUMOGD"),
        &fixture("wfs_wien_museums.json"),
    );
    fit(&museums);
    assert_eq!(museums[0].data["category"], "museum");

    let wc = items(
        "wfs_geojson",
        &wien("WCANLAGEOGD"),
        &fixture("wfs_wien_toilets.json"),
    );
    fit(&wc);
    assert_eq!(wc[0].kind, "amenity");
    assert_eq!(
        wc[0].data,
        json!({
            "name": "Public toilet, Aßmayergasse / Karl-Löwe-Gasse", "amenity_type": "toilet",
            "description": "Pissoir", "address": "Aßmayergasse / Karl-Löwe-Gasse",
            "city": "Vienna", "country": "Austria", "country_code": "AT",
            "lat": 48.17764084, "lng": 16.33842195, "phone": "+43 (1) 546 48",
            "opening_hours": "Mo-So: 0-24 Uhr", "wheelchair_accessible": false,
            "info_url": "https://www.wien.gv.at/umwelt/ma48/sauberestadt/wc/"
        })
    );
    assert_eq!(wc[1].data["wheelchair_accessible"], true);

    let wifi = items(
        "wfs_geojson",
        &wien("WLANWIENATOGD"),
        &fixture("wfs_wien_wifi.json"),
    );
    fit(&wifi);
    assert_eq!(wifi[0].data["amenity_type"], "wifi_hotspot");
    assert_eq!(wifi[0].data["name"], "Altes AKH");

    let zh = items(
        "wfs_geojson",
        &zuerich("poi_zueriwc_view"),
        &fixture("wfs_zuerich_wc.json"),
    );
    fit(&zh);
    assert_eq!(zh[0].data["name"], "Bürkliplatz (nicht rollstuhlgängig)");
    assert_eq!(zh[0].data["wheelchair_accessible"], false);
    assert_eq!(zh[0].data["city"], "Zurich");
    assert!(
        zh[0].data["description"]
            .as_str()
            .unwrap()
            .contains("Nachts geschlossen. Gebührenfrei."),
        "HTML line breaks become spaces: {}",
        zh[0].data["description"]
    );
    let rs = items(
        "wfs_geojson",
        &zuerich("poi_zueriwc_rs_view"),
        &fixture("wfs_zuerich_wc_rs.json"),
    );
    fit(&rs);
    assert_eq!(rs[0].data["wheelchair_accessible"], true);
}
