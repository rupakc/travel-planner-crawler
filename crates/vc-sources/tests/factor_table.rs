//! `factor_table` (2026-11-19): emission-factor and grid-intensity tables for the trip carbon
//! footprint, against small excerpts of the real files (`testkit/fixtures/vc-sources/README.md`):
//! the UK DESNZ 2026 flat file (XLSX), OWID food GHG per kg and OWID grid carbon intensity
//! (CSV).

mod common;

use common::{Harness, assert_raw_first, fixture, spec, t0};
use serde_json::{Value, json};
use vc_core::{CanonicalUrl, Error};
use vc_sources::adapters;
use vc_sources::adapters::factor_table::{self, FactorTable, MAPPINGS, mapping_for, slug};
use vc_sources::adapters::records::{AdapterItem, MAPPED, records};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const DESNZ: &str = "https://assets.publishing.service.gov.uk/media/6a6c9748862aaf18d9c62ac9/ghg-conversion-factors-2026-flat-format-revised.xlsx";
const FOOD: &str = "https://ourworldindata.org/grapher/ghg-per-kg-poore.csv?v=1&csvType=full&useColumnShortNames=true";
const GRID: &str = "https://ourworldindata.org/grapher/electricity-mix.csv?frequency=annual&metric=carbon_intensity&source=total&v=1&csvType=full&useColumnShortNames=true";

fn items(url: &str, body: &[u8]) -> Vec<AdapterItem> {
    records("factor_table", &CanonicalUrl::parse(url).unwrap(), body)
        .expect("factor_table has a record mapping")
        .expect("body maps")
}

fn fit(items: &[AdapterItem]) {
    assert!(!items.is_empty());
    for i in items {
        let schema: Value = serde_json::from_str(
            &std::fs::read_to_string(
                common::repo_root().join(format!("schemas/{}.v1.json", i.kind)),
            )
            .unwrap(),
        )
        .unwrap();
        let v = jsonschema::validator_for(&schema).unwrap();
        assert!(v.is_valid(&i.data), "{}: {}", i.kind, i.data);
    }
}

#[test]
fn factor_table_is_registered_and_mapped() {
    assert!(adapters::get("factor_table").is_some());
    assert!(MAPPED.contains(&"factor_table"));
    assert!(
        adapters::registry()
            .iter()
            .any(|a| a.id() == "factor_table")
    );
    for u in [DESNZ, FOOD, GRID] {
        assert!(
            mapping_for(&CanonicalUrl::parse(u).unwrap()).is_some(),
            "{u}"
        );
    }
    assert!(
        mapping_for(&CanonicalUrl::parse("https://ourworldindata.org/grapher/other.csv").unwrap())
            .is_none()
    );
    let ids: Vec<&str> = MAPPINGS.iter().map(|m| m.dataset).collect();
    let mut dedup = ids.clone();
    dedup.dedup();
    assert_eq!(ids, dedup, "unique dataset ids");
}

#[test]
fn slugs_are_snake_case_ascii() {
    assert_eq!(
        slug("Flights / Long-haul, to/from UK / Economy class / With RF"),
        "flights_long_haul_to_from_uk_economy_class_with_rf"
    );
    assert_eq!(slug("Berries & Grapes"), "berries_and_grapes");
    assert_eq!(
        slug("Hotel stay / Côte d'Ivoire"),
        "hotel_stay_cote_divoire"
    );
}

#[test]
fn desnz_flat_file_yields_travel_factors_only() {
    let it = items(DESNZ, &fixture("factor_desnz_flat_2026.xlsx"));
    fit(&it);
    assert!(it.iter().all(|i| i.kind == "emission_factor"));
    // Fuels and the per-gas splits are dropped; a blank factor (Argentina) is never a 0.
    assert!(it.iter().all(|i| i.data["category"] != "fuel"));
    assert!(it.iter().all(|i| i.data["geography"] != "Argentina"));
    assert_eq!(it.len(), 17, "{it:#?}");
    let lh = it
        .iter()
        .find(|i| i.id.as_deref() == Some("desnz:2026:21_316_3170_11_1"))
        .unwrap();
    assert_eq!(
        lh.data,
        json!({
            "activity": "flights_long_haul_to_from_uk_economy_class_with_rf",
            "activity_label": "Flights / Long-haul, to/from UK / Economy class / With RF",
            "category": "flights", "geography": "United Kingdom", "country_code": "GB",
            "unit": "kg CO2e per passenger.km", "value": 0.11704, "gas_basis": "co2e_with_rf",
            "scope": "Scope 3", "year": 2026,
            "publisher": "UK Department for Energy Security and Net Zero (DESNZ)",
            "dataset": "UK GHG conversion factors for company reporting, flat file 2026",
            "factor_id": "21_316_3170_11_1"
        })
    );
    let without = it
        .iter()
        .find(|i| i.id.as_deref() == Some("desnz:2026:21_316_3171_11_1"))
        .unwrap();
    assert_eq!(without.data["gas_basis"], "co2e_without_rf");
    let france = it
        .iter()
        .find(|i| i.data["activity"] == "hotel_stay_france")
        .unwrap();
    assert_eq!(france.data["category"], "accommodation");
    assert_eq!(france.data["country_code"], "FR");
    assert_eq!(france.data["value"], 6.7);
    assert_eq!(france.data["unit"], "kg CO2e per Room per night");
    let cats: std::collections::BTreeSet<&str> = it
        .iter()
        .map(|i| i.data["category"].as_str().unwrap())
        .collect();
    assert_eq!(
        cats.into_iter().collect::<Vec<_>>(),
        [
            "accommodation",
            "bus",
            "electricity",
            "ferry",
            "flights",
            "rail",
            "taxi"
        ]
    );
    assert!(it.iter().all(|i| i.series_key.is_none()));
}

#[test]
fn owid_food_rows_are_global_food_factors() {
    let it = items(FOOD, &fixture("factor_owid_food_poore.csv"));
    fit(&it);
    assert_eq!(it.len(), 7);
    assert_eq!(
        it[3].id.as_deref(),
        Some("owid-food-poore:2010:beef_beef_herd")
    );
    assert_eq!(
        it[3].data,
        json!({
            "activity": "beef_beef_herd", "activity_label": "Beef (beef herd)",
            "category": "food", "geography": "global", "unit": "kg CO2e per kg",
            "value": 99.48, "gas_basis": "co2e", "year": 2010,
            "publisher": "Our World in Data, based on Poore & Nemecek (2018)",
            "dataset": "Our World in Data: greenhouse gas emissions per kilogram of food product (Poore & Nemecek 2018) 2010"
        })
    );
}

#[test]
fn owid_grid_rows_are_country_observations_without_aggregates() {
    let it = items(GRID, &fixture("factor_owid_grid_ci.csv"));
    fit(&it);
    // ASEAN and World are aggregates (no ISO code): skipped.
    assert_eq!(it.len(), 4);
    assert!(it.iter().all(|i| i.kind == "grid_carbon_intensity"));
    assert_eq!(
        it[1].series_key.as_deref(),
        Some("grid_ci:FR:annual:lifecycle")
    );
    assert_eq!(
        it[1].data,
        json!({
            "country": "France", "country_code": "FR", "period": "annual", "year": 2024,
            "date": "2024-01-01", "intensity_g_per_kwh": 40.48, "basis": "lifecycle",
            "publisher": "Our World in Data, based on Ember and Energy Institute",
            "dataset": "Our World in Data: carbon intensity of electricity generation"
        })
    );
    assert_eq!(it[1].id.as_deref(), Some("owid-grid-ci:FR:2024"));
}

#[test]
fn a_file_without_the_required_columns_is_refused() {
    let r = records(
        "factor_table",
        &CanonicalUrl::parse(FOOD).unwrap(),
        b"entity,year,other\nApples,2010,1\n",
    )
    .unwrap();
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    let xl = factor_table::read_table(
        mapping_for(&CanonicalUrl::parse(DESNZ).unwrap()).unwrap(),
        b"not a workbook",
    );
    assert!(xl.is_err());
}

#[tokio::test]
async fn one_conditional_request_spooled_raw() {
    let p = "/grapher/ghg-per-kg-poore.csv";
    let s = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(p))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/csv")
                .set_body_bytes(fixture("factor_owid_food_poore.csv")),
        )
        .expect(1)
        .mount(&s)
        .await;
    let mut h = Harness::new(
        spec("od-owid-food-ghg"),
        "factor_table",
        &format!("{}{p}", s.uri()),
        t0(),
    )
    .await;
    let rep = h.run(&FactorTable).await.unwrap();
    assert_eq!(rep.requests, 1);
    assert!(rep.complete);
    assert_eq!(h.responses().len(), 1);
    assert_raw_first(&h.events.all());
}

#[tokio::test]
async fn a_row_without_a_mapping_makes_no_request() {
    let s = MockServer::start().await;
    let mut h = Harness::new(
        spec("od-owid-food-ghg"),
        "factor_table",
        &format!("{}/unknown.csv", s.uri()),
        t0(),
    )
    .await;
    let err = h.run(&FactorTable).await.unwrap_err();
    assert!(matches!(err, Error::Invalid(_)), "{err:?}");
    assert!(s.received_requests().await.unwrap().is_empty());
}
