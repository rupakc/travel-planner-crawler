//! US State Department country information and the official food-hygiene registers
//! (2026-11-18): `state_dept_country_info`, `fsa_fhrs`, `nyc_restaurant_inspections`,
//! `toronto_dinesafe`, against recorded fixtures (`testkit/fixtures/vc-sources/README.md`)
//! served by wiremock; and their record mappings (`embassy`, `visa_rule`, `tip`, `restaurant`).

mod common;

use common::{Harness, assert_raw_first, fixture, spec, t0};
use serde_json::{Value, json};
use vc_core::store::SpoolPayload;
use vc_core::{CanonicalUrl, Error};
use vc_sources::adapters;
use vc_sources::adapters::records::{AdapterItem, records};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn ok(body: Vec<u8>) -> ResponseTemplate {
    ResponseTemplate::new(200)
        .insert_header("content-type", "application/json")
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

/// Every item fits its kind's schema and is a kind its source lists.
fn fit(source: &str, items: &[AdapterItem]) {
    let s = spec(source);
    for i in items {
        assert!(
            s.kinds.iter().any(|k| k.0.as_ref() == i.kind),
            "{source} does not list {}",
            i.kind
        );
        let v = validator(i.kind);
        assert!(v.is_valid(&i.data), "{}: {}", i.kind, i.data);
    }
}

fn sent(h: &Harness, i: usize, name: &str) -> Option<String> {
    h.client.sent.lock().unwrap()[i]
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.clone())
}

#[test]
fn new_adapters_are_registered_mapped_and_resume() {
    for id in [
        "state_dept_country_info",
        "fsa_fhrs",
        "nyc_restaurant_inspections",
        "toronto_dinesafe",
    ] {
        assert!(adapters::get(id).is_some(), "{id} registered");
        assert!(adapters::records::MAPPED.contains(&id), "{id} mapped");
        assert!(adapters::ids().contains(&id));
    }
    let resumable: Vec<&str> = adapters::resumable().iter().map(|a| a.id()).collect();
    for id in ["fsa_fhrs", "nyc_restaurant_inspections", "toronto_dinesafe"] {
        assert!(resumable.contains(&id), "{id} resumes its paging");
    }
}

// ---------------------------------------------------------------------------------------
// state_dept_country_info

const CTI: &str = "/api/CountryTravelInformation";

#[tokio::test]
async fn state_dept_country_info_is_one_request_to_the_listed_endpoint() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(CTI))
        .respond_with(ok(fixture("state_dept_country_info.json")))
        .mount(&s)
        .await;
    let a = adapters::state_dept_country_info::StateDeptCountryInfo;
    let mut h = Harness::new(
        spec("state-dept-country-info"),
        "state_dept_country_info",
        &format!("{}{CTI}", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&a).await.unwrap();
    assert_eq!(
        (rep.requests, rep.records, rep.enqueued, rep.complete),
        (1, 1, 0, true)
    );
    assert_raw_first(&h.events.all());
    assert!(h.frontier.enqueued_urls().is_empty(), "never fans out");

    // Any other path on the host (a Cloudflare block page) is refused before any request.
    for other in ["/swagger", "/Help", "/api/CountryTravelInformation/JA", "/"] {
        let mut h = Harness::new(
            spec("state-dept-country-info"),
            "state_dept_country_info",
            &format!("{}{other}", s.uri()),
            t0(),
        )
        .await;
        assert!(matches!(h.run(&a).await, Err(Error::Invalid(_))), "{other}");
        assert!(h.client.requested.lock().unwrap().is_empty(), "{other}");
    }
}

#[tokio::test]
async fn state_dept_country_info_sends_validators_and_keeps_a_304() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(CTI))
        .and(header("if-none-match", "\"v1\""))
        .respond_with(ResponseTemplate::new(304).insert_header("etag", "\"v1\""))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("state-dept-country-info"),
        "state_dept_country_info",
        &format!("{}{CTI}", s.uri()),
        t0(),
    )
    .await;
    let u = &mut h.claim.urls[0];
    u.validators.etag = Some("\"v1\"".into());
    u.last_capture = Some(vc_core::CaptureRef {
        capture_id: vc_core::CaptureId::at(t0()),
        captured_at: t0(),
    });
    let rep = h
        .run(&adapters::state_dept_country_info::StateDeptCountryInfo)
        .await
        .unwrap();
    assert!(rep.complete);
    assert_eq!(sent(&h, 0, "if-none-match").as_deref(), Some("\"v1\""));
    assert!(matches!(
        h.sink.records[0].payload,
        SpoolPayload::Revisit(_)
    ));
}

#[test]
fn state_dept_country_info_maps_embassies_visa_rules_and_tips() {
    let body = fixture("state_dept_country_info.json");
    let parsed = adapters::state_dept_country_info::parse(&body).unwrap();
    let codes: Vec<_> = parsed
        .iter()
        .map(|c| (c.tag.as_str(), c.country_code))
        .collect();
    assert_eq!(
        codes,
        [
            ("AF", Some("AF")),
            ("AS", Some("AU")),
            ("HK", Some("HK")),
            ("JA", Some("JP"))
        ],
        "FIPS tags are not ISO codes"
    );
    let all = items(
        "state_dept_country_info",
        &format!("https://cadataapi.state.gov{CTI}"),
        &body,
    );
    fit("state-dept-country-info", &all);
    let by = |kind: &str, cc: &str| -> Vec<&AdapterItem> {
        all.iter()
            .filter(|i| {
                i.kind == kind
                    && (i.data["country_code"] == cc || i.data["destination_country_code"] == cc)
            })
            .collect()
    };

    // Japan: the embassy block, phones as written (no E.164 for a number written without +).
    let jp = by("embassy", "JP");
    assert_eq!(jp.len(), 1);
    assert_eq!(jp[0].id.as_deref(), Some("state-dept:embassy:JA"));
    assert_eq!(
        jp[0].data,
        json!({
            "name": "U.S. Embassy Tokyo", "mission_type": "embassy",
            "of_country": "United States", "of_country_code": "US",
            "country": "Japan", "country_code": "JP",
            "address": "1-10-5 Akasaka, Minato-ku, Tokyo 107-8420 Japan",
            "phones": [
                {"type": "main", "label": "Phone", "number": "81-3-3224-5000"},
                {"type": "emergency", "label": "Emergency Phone", "number": "81-3-3224-5000"},
                {"type": "fax", "label": "Fax", "number": "81-3-3224-5856"}
            ],
            "emails": ["TokyoACS@state.gov"],
            "website_url": "https://jp.usembassy.gov/",
            "as_of": "2025-08-11"
        })
    );
    // Australia (FIPS AS): written internationally → E.164.
    let au = by("embassy", "AU");
    let ph = &au[0].data["phones"][0];
    assert!(ph["number"].as_str().unwrap().starts_with('+'), "{ph}");
    assert!(ph["e164"].as_str().unwrap().starts_with("+61"), "{ph}");
    // Afghanistan: the suspension notice is kept with the office.
    let af = by("embassy", "AF");
    assert_eq!(
        af[0].data["notes"],
        json!(["OPERATIONS SUSPENDED - AUGUST 31, 2021"])
    );

    // Visa rules for US passport holders, with the deciding sentences as evidence.
    let v = by("visa_rule", "JP");
    assert_eq!(v.len(), 1);
    let d = &v[0].data;
    assert_eq!(
        (
            &d["visa_type"],
            &d["max_stay_days"],
            &d["purpose"],
            &d["nationality_code"]
        ),
        (
            &json!("visa_free"),
            &json!(90),
            &json!("tourism"),
            &json!("US")
        )
    );
    assert_eq!(
        d["evidence"],
        json!(["For tourist or business “visa free” stays up to 90 days:"])
    );
    assert_eq!(d["as_of"], "2025-08-11");
    assert_eq!(by("visa_rule", "AU")[0].data["visa_type"], "eta");
    assert_eq!(
        by("visa_rule", "HK")[0].data["visa_type"],
        "visa_free",
        "\"you only need a visa if\" makes the later \"must get a visa\" conditional"
    );
    assert_eq!(by("visa_rule", "AF")[0].data["visa_type"], "visa_required");

    // Tips: titled paragraphs of local laws, health and safety, with unique ids.
    let tips: Vec<_> = all.iter().filter(|i| i.kind == "tip").collect();
    assert!(tips.len() >= 8, "{}", tips.len());
    let mut ids: Vec<_> = tips.iter().map(|t| t.id.clone().unwrap()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), tips.len());
    let crim = tips
        .iter()
        .find(|t| t.data["country_code"] == "JP" && t.data["title"] == "Criminal penalties")
        .expect("Japan criminal penalties");
    assert_eq!(crim.data["category"], "safety");
    assert_eq!(crim.data["severity"], "warning");
    assert!(
        crim.data["body"]
            .as_str()
            .unwrap()
            .starts_with("U.S. citizens must follow local laws.")
    );
    assert!(
        !tips
            .iter()
            .any(|t| t.data["title"].as_str().unwrap().contains("Resources")),
        "link lists are not tips"
    );
}

// ---------------------------------------------------------------------------------------
// fsa_fhrs

const FSA_P1: &str = "/Establishments";

async fn fsa_server() -> MockServer {
    let s = MockServer::start().await;
    for (n, f) in [
        ("1", "fsa_establishments_p1.json"),
        ("2", "fsa_establishments_p2.json"),
    ] {
        Mock::given(method("GET"))
            .and(path(FSA_P1))
            .and(query_param("localAuthorityId", "120"))
            .and(query_param("businessTypeId", "1"))
            .and(query_param("pageNumber", n))
            .and(query_param("pageSize", "1000"))
            .and(header("x-api-version", "2"))
            .respond_with(ok(fixture(f)))
            .mount(&s)
            .await;
    }
    s
}

#[tokio::test]
async fn fsa_fhrs_pages_each_authority_with_the_api_version_header() {
    let s = fsa_server().await;
    let a = adapters::fsa_fhrs::FsaFhrs { max_pages: 2 };
    let seed = format!("{}{FSA_P1}", s.uri());
    let mut h = Harness::new(spec("fsa-food-hygiene"), "fsa_fhrs", &seed, t0()).await;
    let rep = h.run(&a).await.unwrap();
    assert_eq!((rep.requests, rep.records), (2, 2));
    assert!(!rep.complete);
    assert_eq!(sent(&h, 0, "x-api-version").as_deref(), Some("2"));
    assert_eq!(sent(&h, 1, "x-api-version").as_deref(), Some("2"));
    assert_raw_first(&h.events.all());
    // After the last page of (Westminster, restaurants): the next business type.
    let cursor = rep.cursor.clone().unwrap();
    assert_eq!(
        cursor,
        "/Establishments?businessTypeId=7843&localAuthorityId=120&pageNumber=1&pageSize=1000",
        "canonical (sorted) query"
    );
    // A cursor resumes there; one outside the scope starts over.
    let u = CanonicalUrl::parse(&seed).unwrap();
    assert!(adapters::fsa_fhrs::resume_url(&u, &cursor).is_some());
    assert!(
        adapters::fsa_fhrs::resume_url(
            &u,
            "/Establishments?localAuthorityId=999&businessTypeId=1&pageNumber=1&pageSize=1000"
        )
        .is_none()
    );
    assert!(adapters::fsa_fhrs::resume_url(&u, "/Authorities/basic").is_none());
}

#[test]
fn fsa_fhrs_records_are_rated_restaurants() {
    let p1 = items(
        "fsa_fhrs",
        "https://api.ratings.food.gov.uk/Establishments?localAuthorityId=120&businessTypeId=1&pageNumber=1&pageSize=1000",
        &fixture("fsa_establishments_p1.json"),
    );
    assert_eq!(p1.len(), 3);
    fit("fsa-food-hygiene", &p1);
    assert!(p1.iter().all(|i| i.kind == "restaurant"));
    assert_eq!(p1[2].id.as_deref(), Some("fhrs:1559410"));
    assert_eq!(
        p1[2].data,
        json!({
            "name": "1 Warwick", "establishment_type": "restaurant",
            "category_label": "Restaurant/Cafe/Canteen",
            "address": "1 WARWICK STREET, LONDON, W1B 5LR", "district": "Westminster",
            "city": "London", "country": "United Kingdom", "country_code": "GB",
            "lat": 51.5107175, "lng": -0.1372907,
            "hygiene": {"scheme": "uk_fhrs", "rating": "5", "rating_value": 5.0,
                        "rating_date": "2025-04-29", "authority": "Westminster"},
            "as_of": "2025-04-29"
        })
    );
    // Awaiting inspection (no rating, placeholder 1901 date) is not a record.
    let p2 = items(
        "fsa_fhrs",
        "https://api.ratings.food.gov.uk/Establishments?localAuthorityId=95&businessTypeId=7843&pageNumber=1&pageSize=1000",
        &fixture("fsa_establishments_p2.json"),
    );
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0].data["establishment_type"], "pub_bar");
    assert_eq!(p2[0].data["hygiene"]["rating_date"], "2022-11-22");
    // An authority outside the scope maps to nothing.
    assert!(
        items(
            "fsa_fhrs",
            "https://api.ratings.food.gov.uk/Establishments?localAuthorityId=999&businessTypeId=1&pageNumber=1&pageSize=1000",
            &fixture("fsa_establishments_p1.json"),
        )
        .is_empty()
    );
}

#[test]
fn fsa_fhrs_scottish_passes_and_town_filter() {
    let body = json!({"establishments": [
        {"FHRSID": 1, "BusinessName": "Cafe A", "BusinessType": "Restaurant/Cafe/Canteen",
         "AddressLine1": "1 High St", "AddressLine3": "EDINBURGH", "PostCode": "EH1 1AA",
         "RatingValue": "Pass", "RatingDate": "2025-03-04T00:00:00",
         "LocalAuthorityName": "Edinburgh (City of)",
         "geocode": {"latitude": "55.95", "longitude": "-3.19"}},
        {"FHRSID": 2, "BusinessName": "Cafe B", "RatingValue": "Exempt",
         "RatingDate": "2025-03-04T00:00:00"}
    ], "meta": {"pageNumber": 1, "totalPages": 1}});
    let it = items(
        "fsa_fhrs",
        "https://api.ratings.food.gov.uk/Establishments?localAuthorityId=210&businessTypeId=1&pageNumber=1&pageSize=1000",
        body.to_string().as_bytes(),
    );
    assert_eq!(it.len(), 1);
    assert_eq!(
        it[0].data["hygiene"],
        json!({"scheme": "uk_fhis", "rating": "Pass", "rating_date": "2025-03-04",
               "authority": "Edinburgh (City of)"})
    );
    assert_eq!(it[0].data["city"], "Edinburgh");
    // Bath and North East Somerset: only establishments with a Bath (BA1, BA2) postcode.
    let body = json!({"establishments": [
        {"FHRSID": 3, "BusinessName": "In Bath", "AddressLine1": "1 Milsom St",
         "AddressLine2": "Bath", "PostCode": "BA1 1BZ", "RatingValue": "4",
         "RatingDate": "2025-01-01T00:00:00"},
        {"FHRSID": 4, "BusinessName": "In Keynsham", "AddressLine1": "2 Bath Hill",
         "AddressLine2": "Keynsham", "PostCode": "BS31 1HG", "RatingValue": "4",
         "RatingDate": "2025-01-01T00:00:00"}
    ]});
    let it = items(
        "fsa_fhrs",
        "https://api.ratings.food.gov.uk/Establishments?localAuthorityId=326&businessTypeId=1&pageNumber=1&pageSize=1000",
        body.to_string().as_bytes(),
    );
    let names: Vec<_> = it.iter().map(|i| i.data["name"].clone()).collect();
    assert_eq!(names, [json!("In Bath")]);
    assert_eq!(it[0].data["city"], "Bath");
}

// ---------------------------------------------------------------------------------------
// nyc_restaurant_inspections

const NYC: &str = "/resource/43nn-pn8j.json";

#[tokio::test]
async fn nyc_inspections_asks_for_recent_letter_grades_grouped() {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(NYC))
        .and(query_param(
            "$where",
            "grade in('A','B','C') AND grade_date >= '2023-11-17'",
        ))
        .and(query_param("$order", "camis,grade_date DESC"))
        .and(query_param(
            "$group",
            adapters::nyc_restaurant_inspections::COLUMNS,
        ))
        .and(query_param(
            "$select",
            adapters::nyc_restaurant_inspections::COLUMNS,
        ))
        .and(query_param("$limit", "2000"))
        .and(query_param("$offset", "0"))
        .respond_with(ok(fixture("nyc_restaurant_inspections.json")))
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("nyc-restaurant-inspections"),
        "nyc_restaurant_inspections",
        &format!("{}{NYC}", s.uri()),
        t0(),
    )
    .await;
    let rep = h
        .run(&adapters::nyc_restaurant_inspections::NycRestaurantInspections::DEFAULT)
        .await
        .unwrap();
    assert_eq!(
        (rep.requests, rep.records, rep.complete),
        (1, 1, true),
        "short page ends it"
    );
    assert_raw_first(&h.events.all());
}

#[test]
fn nyc_full_pages_never_split_a_restaurant() {
    use adapters::nyc_restaurant_inspections as n;
    let rows = n::parse(&fixture("nyc_restaurant_inspections.json")).unwrap();
    assert_eq!(rows.len(), 6);
    let u = CanonicalUrl::parse(&format!(
        "https://data.cityofnewyork.us{NYC}?$order=camis&$limit=6&$offset=12"
    ))
    .unwrap();
    let (kept, next) = n::page(&u, &rows).unwrap();
    assert_eq!(
        kept.len(),
        4,
        "the last restaurant's rows wait for the next page"
    );
    let next = next.unwrap();
    assert_eq!(
        adapters::inspections::window(&next, "$offset", "$limit"),
        Some((16, 6))
    );
    // The mapping makes the same cut.
    let full = items(
        "nyc_restaurant_inspections",
        u.as_str(),
        &fixture("nyc_restaurant_inspections.json"),
    );
    assert_eq!(full.len(), 2);
}

#[test]
fn nyc_records_take_each_restaurants_latest_grade() {
    let it = items(
        "nyc_restaurant_inspections",
        &format!("https://data.cityofnewyork.us{NYC}?$limit=2000&$offset=0"),
        &fixture("nyc_restaurant_inspections.json"),
    );
    assert_eq!(it.len(), 3);
    fit("nyc-restaurant-inspections", &it);
    assert_eq!(it[0].id.as_deref(), Some("nyc-dohmh:30075445"));
    assert_eq!(
        it[0].data,
        json!({
            "name": "MORRIS PARK BAKE SHOP", "establishment_type": "restaurant",
            "cuisine": "Bakery Products/Desserts",
            "address": "1007 MORRIS PARK AVENUE, Bronx NY 10462", "district": "Bronx",
            "city": "New York City", "country": "United States", "country_code": "US",
            "lat": 40.848231224526, "lng": -73.855971889932, "phone": "7188924968",
            "hygiene": {"scheme": "nyc_letter_grade", "rating": "A", "rating_date": "2026-02-27",
                        "authority": "NYC Department of Health and Mental Hygiene"},
            "as_of": "2026-02-27"
        })
    );
    assert_eq!(
        it[1].data["address"],
        "351 WEST 57 STREET, Manhattan NY 10019"
    );
}

// ---------------------------------------------------------------------------------------
// toronto_dinesafe

const PKG: &str = "/api/3/action/package_show";
const DS: &str = "/api/3/action/datastore_search";
const RES: &str = "75dbb3ff-8281-4f03-9de2-bd1214761302";

async fn toronto_server() -> MockServer {
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(PKG))
        .and(query_param("id", "dinesafe"))
        .respond_with(ok(fixture("toronto_dinesafe_package.json")))
        .mount(&s)
        .await;
    Mock::given(method("GET"))
        .and(path(DS))
        .and(query_param("resource_id", RES))
        .and(query_param("distinct", "true"))
        .and(query_param("fields", adapters::toronto_dinesafe::FIELDS))
        .and(query_param("limit", "2000"))
        .respond_with(ok(fixture("toronto_dinesafe_page.json")))
        .mount(&s)
        .await;
    s
}

#[tokio::test]
async fn toronto_dinesafe_finds_the_datastore_resource_then_pages_it() {
    let s = toronto_server().await;
    let row = format!("{}{PKG}?id=dinesafe", s.uri());
    let a = adapters::toronto_dinesafe::TorontoDinesafe::DEFAULT;
    let mut h = Harness::new(spec("toronto-dinesafe"), "toronto_dinesafe", &row, t0()).await;
    let rep = h.run(&a).await.unwrap();
    assert_eq!((rep.requests, rep.records, rep.complete), (2, 2, true));
    assert_raw_first(&h.events.all());
    let req = h.client.requested.lock().unwrap().clone();
    assert!(req[1].contains(DS) && req[1].contains(RES), "{req:?}");
    assert!(
        !req.iter().any(|u| u.contains("/download/")),
        "robots.txt disallows the download URLs"
    );

    // A saved cursor skips package_show.
    let base = CanonicalUrl::parse(&row).unwrap();
    let cursor = adapters::toronto_dinesafe::first_page(&base, RES)
        .unwrap()
        .path_and_query();
    let mut h = Harness::new(spec("toronto-dinesafe"), "toronto_dinesafe", &row, t0()).await;
    let rep = h.run_from(&a, Some(&cursor)).await.unwrap();
    assert_eq!((rep.requests, rep.complete), (1, true));
    let req = h.client.requested.lock().unwrap().clone();
    assert!(req[0].contains(DS), "{req:?}");
}

#[test]
fn toronto_records_take_each_establishments_latest_inspection() {
    let pkg = items(
        "toronto_dinesafe",
        &format!("https://ckan0.cf.opendata.inter.prod-toronto.ca{PKG}?id=dinesafe"),
        &fixture("toronto_dinesafe_package.json"),
    );
    assert!(pkg.is_empty());
    assert_eq!(
        adapters::toronto_dinesafe::datastore_resource(&fixture("toronto_dinesafe_package.json"))
            .unwrap(),
        RES
    );
    let it = items(
        "toronto_dinesafe",
        &format!(
            "https://ckan0.cf.opendata.inter.prod-toronto.ca{DS}?resource_id={RES}&limit=2000&offset=0"
        ),
        &fixture("toronto_dinesafe_page.json"),
    );
    assert_eq!(it.len(), 2);
    fit("toronto-dinesafe", &it);
    assert_eq!(
        it[0].data,
        json!({
            "name": "GALERIA PILATES", "address": "998 Queen St E M4M 1K1",
            "city": "Toronto", "country": "Canada", "country_code": "CA",
            "lat": 43.66133456, "lng": -79.33952183, "phone": "4374211171",
            "hygiene": {"scheme": "toronto_dinesafe", "rating": "Pass",
                        "rating_date": "2026-02-25", "authority": "Toronto Public Health"},
            "as_of": "2026-02-25"
        })
    );
    assert_eq!(it[1].data["hygiene"]["rating_date"], "2026-07-16");
    assert_eq!(
        it[1].id.as_deref(),
        Some("toronto-dinesafe:001Vo000013PoSbIAK")
    );
}

#[tokio::test]
async fn food_adapters_write_raw_first_and_stop_on_a_failed_append() {
    let s = toronto_server().await;
    let mut h = Harness::new(
        spec("toronto-dinesafe"),
        "toronto_dinesafe",
        &format!("{}{PKG}?id=dinesafe", s.uri()),
        t0(),
    )
    .await;
    h.sink.fail = true;
    let err = h
        .run(&adapters::toronto_dinesafe::TorontoDinesafe::DEFAULT)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err:?}");
    assert_eq!(
        h.client.requested.lock().unwrap().len(),
        1,
        "nothing is derived (no datastore page) before package_show is spooled"
    );
}
