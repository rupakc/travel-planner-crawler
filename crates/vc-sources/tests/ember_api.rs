//! `ember_api` (2026-11-19): Ember API carbon intensity per country (yearly and monthly), the
//! owner's key in the `api_key` query parameter, against small excerpts of real responses
//! (`testkit/fixtures/vc-sources/README.md`).

mod common;

use common::{Harness, assert_raw_first, fixture, spec, t0};
use vc_core::store::SpoolPayload;
use vc_core::{CanonicalUrl, Error};
use vc_sources::adapters;
use vc_sources::adapters::ember_api::{self, EmberApi, KEY_PARAM, Period, period};
use vc_sources::adapters::records::{AdapterItem, MAPPED, records};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "ember-test-key-0000";
const SECRET: &str = "crawler-src-ember-key";
const YEARLY: &str = "/v1/carbon-intensity/yearly?is_aggregate_entity=false&start_date=2000";
const MONTHLY: &str = "/v1/carbon-intensity/monthly?is_aggregate_entity=false&start_date=2019-01";

fn url(p: &str) -> CanonicalUrl {
    CanonicalUrl::parse(&format!("https://api.ember-energy.org{p}")).unwrap()
}

fn items(p: &str, file: &str) -> Vec<AdapterItem> {
    records("ember_api", &url(p), &fixture(file))
        .expect("ember_api has a record mapping")
        .expect("fixture maps")
}

async fn server() -> MockServer {
    let s = MockServer::start().await;
    for (p, file) in [
        ("/v1/carbon-intensity/yearly", "ember_ci_yearly.json"),
        ("/v1/carbon-intensity/monthly", "ember_ci_monthly.json"),
    ] {
        Mock::given(method("GET"))
            .and(path(p))
            .and(query_param(KEY_PARAM, KEY))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_bytes(fixture(file)),
            )
            .mount(&s)
            .await;
    }
    s
}

async fn harness(s: &MockServer, p: &str) -> Harness {
    let mut h = Harness::new(
        spec("carbon-ember-api"),
        "ember_api",
        &format!("{}{p}", s.uri()),
        t0(),
    )
    .await;
    h.secrets.0.insert(SECRET.into(), KEY.into());
    h
}

#[test]
fn ember_api_is_registered_and_mapped() {
    assert!(adapters::get("ember_api").is_some());
    assert!(adapters::registry().iter().any(|a| a.id() == "ember_api"));
    assert!(MAPPED.contains(&"ember_api"));
    let s = spec("carbon-ember-api");
    assert!(s.enabled && s.needs_key);
    assert_eq!(s.adapter.as_deref(), Some("ember_api"));
    assert_eq!(s.key_secret.as_deref(), Some(SECRET));
}

#[test]
fn rows_are_classified_and_the_key_is_refused() {
    assert_eq!(period(&url(YEARLY)).unwrap(), Period::Annual);
    assert_eq!(period(&url(MONTHLY)).unwrap(), Period::Monthly);
    assert!(period(&url("/v1/electricity-generation/yearly")).is_err());
    assert!(period(&url(&format!("{YEARLY}&api_key=x"))).is_err());
    assert!(period(&url(&format!("{YEARLY}&other=1"))).is_err());
}

#[test]
fn yearly_fixture_maps_countries_and_skips_nothing_valid() {
    let it = items(YEARLY, "ember_ci_yearly.json");
    assert_eq!(it.len(), 7);
    let fr = it
        .iter()
        .find(|i| i.id.as_deref() == Some("ember:annual:FR:2024-01-01"))
        .unwrap();
    assert_eq!(fr.kind, "grid_carbon_intensity");
    assert_eq!(
        fr.series_key.as_deref(),
        Some("grid_ci:FR:annual:lifecycle")
    );
    assert_eq!(fr.data["intensity_g_per_kwh"], 40.34);
    assert_eq!(fr.data["year"], 2024);
    assert_eq!(fr.data["date"], "2024-01-01");
    assert_eq!(fr.data["publisher"], "Ember");
    assert_eq!(fr.data["country"], "France");
    // Kosovo's user-assigned XKX maps to XK.
    assert!(it.iter().any(|i| i.data["country_code"] == "XK"));
}

#[test]
fn monthly_fixture_maps_first_day_of_month() {
    let it = items(MONTHLY, "ember_ci_monthly.json");
    assert_eq!(it.len(), 4);
    let nz = it
        .iter()
        .find(|i| i.id.as_deref() == Some("ember:monthly:NZ:2025-02-01"))
        .unwrap();
    assert_eq!(
        nz.series_key.as_deref(),
        Some("grid_ci:NZ:monthly:lifecycle")
    );
    assert_eq!(nz.data["period"], "monthly");
    assert_eq!(nz.data["date"], "2025-02-01");
}

#[test]
fn aggregates_unknown_codes_bad_values_and_dates_are_skipped() {
    let body = br#"{"stats":{},"data":[
        {"entity":"World","entity_code":null,"is_aggregate_entity":true,"date":"2024","emissions_intensity_gco2_per_kwh":473.0},
        {"entity":"EU","entity_code":"EUU","is_aggregate_entity":true,"date":"2024","emissions_intensity_gco2_per_kwh":200.0},
        {"entity":"Nowhere","entity_code":"ZZZ","is_aggregate_entity":false,"date":"2024","emissions_intensity_gco2_per_kwh":1.0},
        {"entity":"France","entity_code":"FRA","is_aggregate_entity":false,"date":"2024","emissions_intensity_gco2_per_kwh":null},
        {"entity":"France","entity_code":"FRA","is_aggregate_entity":false,"date":"2024","emissions_intensity_gco2_per_kwh":-3.0},
        {"entity":"France","entity_code":"FRA","is_aggregate_entity":false,"date":"24","emissions_intensity_gco2_per_kwh":40.0},
        {"entity":"France","entity_code":"FRA","is_aggregate_entity":false,"date":"2024","emissions_intensity_gco2_per_kwh":40.34}
    ]}"#;
    let it = records("ember_api", &url(YEARLY), body).unwrap().unwrap();
    assert_eq!(it.len(), 1);
    assert_eq!(it[0].data["country_code"], "FR");
    assert!(
        records("ember_api", &url(YEARLY), b"not json")
            .unwrap()
            .is_err()
    );
    assert!(
        records("ember_api", &url("/v1/other"), body)
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn one_request_per_row_raw_first() {
    let s = server().await;
    for p in [YEARLY, MONTHLY] {
        let mut h = harness(&s, p).await;
        let rep = h.run(&EmberApi).await.unwrap();
        assert_eq!((rep.requests, rep.records, rep.complete), (1, 1, true));
        assert_raw_first(&h.events.all());
    }
    let mut h = harness(&s, YEARLY).await;
    h.sink.fail = true;
    let err = h.run(&EmberApi).await.unwrap_err();
    assert!(matches!(err, Error::Storage(_)), "{err:?}");
}

#[tokio::test]
async fn the_key_never_reaches_the_spool() {
    let s = server().await;
    let mut h = harness(&s, YEARLY).await;
    h.run(&EmberApi).await.unwrap();
    let sent = h.client.requested.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert!(sent[0].contains(KEY), "the request carries the key");
    assert!(!h.sink.records.is_empty());
    for r in &h.sink.records {
        assert!(!r.meta.url.as_str().contains(KEY));
        assert!(!r.meta.url.as_str().contains(KEY_PARAM));
        if let SpoolPayload::Response(e) = &r.payload {
            assert!(!e.url.as_str().contains(KEY));
            assert!(!format!("{:?}", e.request_headers).contains(KEY));
            assert!(e.redirects.iter().all(|x| !x.url.as_str().contains(KEY)));
        }
    }
}

#[tokio::test]
async fn a_row_carrying_a_key_is_refused_without_a_request() {
    let s = server().await;
    let mut h = harness(&s, &format!("{YEARLY}&api_key=leaked")).await;
    assert!(h.run(&EmberApi).await.is_err());
    assert!(h.client.requested.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_secret_fails_without_a_request() {
    let s = server().await;
    let mut h = Harness::new(
        spec("carbon-ember-api"),
        "ember_api",
        &format!("{}{YEARLY}", s.uri()),
        t0(),
    )
    .await;
    assert!(h.run(&EmberApi).await.is_err());
    assert!(h.client.requested.lock().unwrap().is_empty());
}

#[test]
fn parse_reads_the_documented_shape() {
    let rows = ember_api::parse(&fixture("ember_ci_monthly.json")).unwrap();
    assert_eq!(rows.len(), 4);
    assert!(rows.iter().all(|r| !r.is_aggregate_entity));
}
