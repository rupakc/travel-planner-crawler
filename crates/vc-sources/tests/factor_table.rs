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
fn egrid_puerto_rico_is_its_own_country() {
    // Synthetic values in the real layout (README): PRMS and PR are ISO 3166-1 PR, not US.
    let it = items(EGRID, &fixture("factor_egrid_puerto_rico_synthetic.xlsx"));
    fit(&it);
    assert_eq!(it.len(), 5, "{it:#?}");
    let prms = by_id(&it, "egrid-subregion:2023:PRMS");
    assert_eq!(prms.data["country_code"], "PR");
    assert_eq!(prms.data["region_code"], "PRMS");
    assert_eq!(
        prms.data["geography"],
        "Puerto Rico Miscellaneous (eGRID subregion PRMS)"
    );
    let pr = by_id(&it, "egrid-state:2023:PR");
    assert_eq!(pr.data["country_code"], "PR");
    assert_eq!(pr.data["geography"], "Puerto Rico");
    assert!(pr.data.get("region_code").is_none());
    assert_eq!(by_id(&it, "egrid-state:2023:CA").data["country_code"], "US");
    assert_eq!(
        by_id(&it, "egrid-subregion:2023:CAMX").data["country_code"],
        "US"
    );
    // One national record per country.
    let mut national: Vec<&str> = it
        .iter()
        .filter(|i| i.data.get("region_code").is_none())
        .map(|i| i.data["country_code"].as_str().unwrap())
        .collect();
    national.sort_unstable();
    assert_eq!(national, ["PR", "US"]);
}

/// A one-member ZIP (deflate) of `data`, written with sizes in the local header.
fn zip_of(name: &str, data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    e.write_all(data).unwrap();
    let c = e.finish().unwrap();
    let mut z = Vec::new();
    z.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
    z.extend_from_slice(&20u16.to_le_bytes()); // version
    z.extend_from_slice(&0u16.to_le_bytes()); // flags
    z.extend_from_slice(&8u16.to_le_bytes()); // deflate
    z.extend_from_slice(&[0; 4]); // time, date
    z.extend_from_slice(&[0; 4]); // crc (not checked)
    z.extend_from_slice(&u32::try_from(c.len()).unwrap().to_le_bytes());
    z.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
    z.extend_from_slice(&u16::try_from(name.len()).unwrap().to_le_bytes());
    z.extend_from_slice(&0u16.to_le_bytes());
    z.extend_from_slice(name.as_bytes());
    z.extend_from_slice(&c);
    z.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    z.extend_from_slice(&[0; 18]);
    z
}

#[test]
fn unzip_member_stops_a_highly_compressible_member_near_the_cap() {
    let cap = 4 * 1024 * 1024;
    let body = zip_of("vehicles.csv", &vec![b'a'; cap + 1]);
    let r = factor_table::unzip_member_capped(&["vehicles.csv"], &body, cap);
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
    // Just under the cap decodes in full.
    let ok = factor_table::unzip_member_capped(
        &["vehicles.csv"],
        &zip_of("vehicles.csv", &vec![b'a'; cap]),
        cap,
    )
    .unwrap();
    assert_eq!(ok.len(), cap);
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

// ---------------------------------------------------------------------------------------------
// Second batch (2026-11-19): eGRID, EF Hub, OWID travel mode and CO2, ORR (ODS), FuelEconomy.gov
// (zipped CSV), CONCITO, RIVM, and Eurostat sdg_13_31 (EEA new-car CO2 averages).
// ---------------------------------------------------------------------------------------------

const EGRID: &str = "https://www.epa.gov/system/files/documents/2025-06/summary_tables_rev2.xlsx";
const EF_HUB: &str =
    "https://www.epa.gov/system/files/other-files/2025-01/ghg-emission-factors-hub-2025.xlsx";
const TRAVEL: &str = "https://ourworldindata.org/grapher/carbon-footprint-travel-mode.csv?v=1&csvType=full&useColumnShortNames=true";
const CO2: &str = "https://raw.githubusercontent.com/owid/co2-data/master/owid-co2-data.csv";
const ORR: &str = "https://dataportal.orr.gov.uk/media/1527/table-6100-estimates-of-normalised-passenger-carbon-dioxide-equivalent-emissions.ods";
const FUEL: &str = "https://www.fueleconomy.gov/feg/epadata/vehicles.csv.zip";
const CONCITO: &str =
    "https://concito.dk/files/media/document/Version%201.2_DK_ENG_rev_18122025_0.xlsx";
const RIVM: &str = "https://www.rivm.nl/sites/default/files/2026-03/Database-milieubelasting-voedingsmiddelen-database-versie-23-september-2024-met-tabblad-per-voedingsmiddelengroep.xlsx";
const SDG: &str = "https://ec.europa.eu/eurostat/api/dissemination/statistics/1.0/data/sdg_13_31?format=JSON&lang=EN";

fn by_id<'a>(it: &'a [AdapterItem], id: &str) -> &'a AdapterItem {
    it.iter()
        .find(|i| i.id.as_deref() == Some(id))
        .unwrap_or_else(|| panic!("{id} missing: {it:#?}"))
}

#[test]
fn every_new_file_url_has_its_mappings() {
    let n = |u: &str| factor_table::mappings_for(&CanonicalUrl::parse(u).unwrap()).len();
    assert_eq!(n(EGRID), 2, "Table 1 and Table 3");
    assert_eq!(n(EF_HUB), 2, "Table 10 and Table 2");
    assert_eq!(n(TRAVEL), 1);
    assert_eq!(n(CO2), 3, "CO2, consumption CO2, GHG per capita");
    assert_eq!(n(ORR), 1);
    assert_eq!(n(FUEL), 1);
    assert_eq!(n(CONCITO), 5, "one sheet per country");
    assert_eq!(n(RIVM), 2, "to retail and to consumption");
    assert_eq!(
        factor_table::file_name_year(&CanonicalUrl::parse(CONCITO).unwrap()),
        Some(2025)
    );
    assert_eq!(
        factor_table::file_name_year(&CanonicalUrl::parse(RIVM).unwrap()),
        Some(2024)
    );
    assert_eq!(
        factor_table::first_year("Apr 2011 to Mar 2012 [b]"),
        Some(2011)
    );
    assert_eq!(
        factor_table::strip_footnote("Passenger Car A"),
        "Passenger Car"
    );
    assert_eq!(factor_table::strip_footnote("Bus"), "Bus");
}

#[test]
fn egrid_subregions_and_states_carry_region_codes() {
    let it = items(EGRID, &fixture("factor_egrid_summary_2023.xlsx"));
    fit(&it);
    // 4 + 4 rows, but the U.S. total (in both tables) is stored once, from Table 3.
    assert_eq!(it.len(), 7, "{it:#?}");
    let national: Vec<&str> = it
        .iter()
        .filter(|i| i.data.get("region_code").is_none())
        .map(|i| i.id.as_deref().unwrap())
        .collect();
    assert_eq!(national, ["egrid-state:2023:U.S."]);
    let camx = by_id(&it, "egrid-subregion:2023:CAMX");
    assert_eq!(
        camx.data,
        json!({
            "activity": "electricity_generation_total_output_emission_rate_wecc_california",
            "activity_label": "Electricity generation, total output emission rate / WECC California",
            "category": "electricity", "geography": "WECC California (eGRID subregion CAMX)",
            "country_code": "US", "region_code": "CAMX", "unit": "lb CO2e per MWh",
            "value": 429.983, "gas_basis": "co2e", "scope": "total output emission rate",
            "year": 2023, "publisher": "US Environmental Protection Agency (EPA)",
            "dataset": "US EPA eGRID summary tables (output emission rates) 2023",
            "factor_id": "CAMX"
        })
    );
    // The first CO₂e column (total output), not the non-baseload one.
    let vt = by_id(&it, "egrid-state:2023:VT");
    assert_eq!(vt.data["value"], 52.239);
    assert_eq!(vt.data["geography"], "US state VT");
    assert_eq!(vt.data["region_code"], "VT");
    // The U.S. total is the national figure (no region code).
    let us = by_id(&it, "egrid-state:2023:U.S.");
    assert_eq!(us.data["geography"], "United States");
    assert!(us.data.get("region_code").is_none());
    assert_eq!(us.data["value"], 770.884);
    assert!(it.iter().all(|i| i.series_key.is_none()));
}

#[test]
fn ef_hub_reads_the_two_stacked_tables_only() {
    let it = items(EF_HUB, &fixture("factor_epa_ef_hub_2025.xlsx"));
    fit(&it);
    // Table 10: 12 vehicle rows; Table 2: 10 fuels; Table 11 (GWP) is never read.
    assert_eq!(it.len(), 22, "{it:#?}");
    assert!(it.iter().all(|i| i.data["gas_basis"] == "co2"));
    let long = by_id(
        &it,
        "epa-ef-hub-travel:2025:air_travel_long_haul_2300_miles",
    );
    assert_eq!(
        long.data,
        json!({
            "activity": "air_travel_long_haul_2300_miles",
            "activity_label": "Air Travel - Long Haul (>= 2300 miles)",
            "category": "flights", "geography": "United States", "country_code": "US",
            "unit": "kg CO2 per passenger-mile", "value": 0.163, "gas_basis": "co2",
            "scope": "Scope 3 cat. 6 and 7, tank-to-wheel", "year": 2025,
            "publisher": "US Environmental Protection Agency (EPA)",
            "dataset": "US EPA GHG Emission Factors Hub 2025"
        })
    );
    // Footnote letters are dropped from the labels.
    let car = by_id(&it, "epa-ef-hub-travel:2025:passenger_car");
    assert_eq!(car.data["unit"], "kg CO2 per vehicle-mile");
    assert_eq!(car.data["category"], "car");
    let jet = by_id(
        &it,
        "epa-ef-hub-mobile-co2:2025:mobile_combustion_kerosene_type_jet_fuel",
    );
    assert_eq!(jet.data["value"], 9.75);
    assert_eq!(jet.data["unit"], "kg CO2 per gallon");
    assert_eq!(jet.data["category"], "fuel");
    assert!(it.iter().all(|i| i.data["activity"] != "co2"));
}

#[test]
fn owid_travel_mode_is_uk_g_per_pkm_with_rf_for_flights() {
    let it = items(TRAVEL, &fixture("factor_owid_travel_mode.csv"));
    fit(&it);
    assert_eq!(it.len(), 16);
    let d = by_id(&it, "owid-travel-mode:2022:domestic_flight");
    assert_eq!(d.data["gas_basis"], "co2e_with_rf");
    assert_eq!(d.data["unit"], "g CO2e per passenger.km");
    assert_eq!(d.data["value"], 245.87);
    assert_eq!(d.data["country_code"], "GB");
    assert_eq!(d.data["scope"], "secondary copy of UK DESNZ factors");
    let cats: std::collections::BTreeMap<&str, &str> = it
        .iter()
        .map(|i| {
            (
                i.data["activity"].as_str().unwrap(),
                i.data["category"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(cats["coach_bus"], "coach");
    assert_eq!(cats["bus_average"], "bus");
    assert_eq!(cats["eurostar_to_paris"], "rail");
    assert_eq!(cats["london_underground"], "rail");
    assert_eq!(cats["plug_in_hybrid"], "car");
    assert_eq!(cats["ferry_foot_passenger"], "ferry");
    assert_eq!(
        by_id(&it, "owid-travel-mode:2022:national_rail").data["gas_basis"],
        "co2e"
    );
}

#[test]
fn owid_co2_per_capita_by_country_from_2010_without_aggregates() {
    let it = items(CO2, &fixture("factor_owid_co2_data.csv"));
    fit(&it);
    // Afghanistan 2023 and 2024 (2009 is before 2010; Africa is an aggregate); the consumption
    // column is blank for those rows, so only CO2 and GHG per capita.
    assert_eq!(it.len(), 4, "{it:#?}");
    let co2 = by_id(
        &it,
        "owid-co2-per-capita:2024:AF:co2_emissions_per_capita_afghanistan",
    );
    assert_eq!(co2.data["geography"], "Afghanistan");
    assert_eq!(co2.data["country_code"], "AF");
    assert_eq!(co2.data["unit"], "t CO2 per person per year");
    assert_eq!(co2.data["category"], "other");
    assert_eq!(co2.data["gas_basis"], "co2");
    let ghg = by_id(
        &it,
        "owid-ghg-per-capita:2024:AF:greenhouse_gas_emissions_per_capita_afghanistan",
    );
    assert_eq!(ghg.data["gas_basis"], "co2e");
    assert_eq!(ghg.data["unit"], "t CO2e per person per year");
    assert!(
        it.iter()
            .all(|i| i.data["year"].as_i64().unwrap() >= factor_table::OWID_CO2_FROM)
    );
}

#[test]
fn orr_ods_rows_are_gb_rail_factors_per_financial_year() {
    let it = items(ORR, &fixture("factor_orr_6100.ods"));
    fit(&it);
    // 21 financial years 2005-06 .. 2025-26, minus 2010-11 (not available, "[x]").
    assert_eq!(it.len(), 20, "{it:#?}");
    assert!(
        it.iter()
            .all(|i| i.data["year"] != 2010 && i.data["category"] == "rail")
    );
    let y = by_id(&it, "orr-6100:2011:passenger_rail_traction_energy");
    assert_eq!(y.data["value"], 48.33);
    assert_eq!(y.data["unit"], "g CO2e per passenger.km");
    assert_eq!(y.data["geography"], "Great Britain");
    assert_eq!(y.data["country_code"], "GB");
}

#[test]
fn fueleconomy_zip_rows_from_model_year_2022() {
    let it = items(FUEL, &fixture("factor_fueleconomy_vehicles.csv.zip"));
    fit(&it);
    // The 2021 Prius is before 2022; the 2026 Prius and the Model Y (0 g tailpipe) stay.
    assert_eq!(it.len(), 2, "{it:#?}");
    let prius = by_id(&it, "fueleconomy:2026:49301");
    assert_eq!(prius.data["value"], 155.0);
    assert_eq!(prius.data["unit"], "g CO2 per mile");
    assert_eq!(
        prius.data["activity_label"],
        "Toyota / Prius / Midsize Cars / Automatic (variable gear ratios)"
    );
    assert_eq!(by_id(&it, "fueleconomy:2026:49743").data["value"], 0.0);
    // A zip without the member is refused.
    let empty = records(
        "factor_table",
        &CanonicalUrl::parse(FUEL).unwrap(),
        b"PK\x05\x06\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0",
    )
    .unwrap();
    assert!(empty.is_err());
}

#[test]
fn concito_sheets_are_country_food_factors_without_zero_totals() {
    let it = items(CONCITO, &fixture("factor_concito_v1_2.xlsx"));
    fit(&it);
    let beef_fr = by_id(&it, "concito-fr:2025:Ra00010");
    assert_eq!(beef_fr.data["geography"], "France");
    assert_eq!(beef_fr.data["country_code"], "FR");
    assert_eq!(beef_fr.data["unit"], "kg CO2e per kg");
    assert_eq!(beef_fr.data["category"], "food");
    // The GB gherkin row publishes 0 for every stage: no figure, skipped.
    assert!(
        it.iter()
            .all(|i| i.id.as_deref() != Some("concito-gb:2025:Ra00008"))
    );
    assert!(
        it.iter()
            .any(|i| i.id.as_deref() == Some("concito-dk:2025:Ra00008"))
    );
    for c in ["dk", "gb", "nl", "fr", "es"] {
        assert!(
            it.iter()
                .any(|i| i.id.as_deref() == Some(&format!("concito-{c}:2025:Ra00001")))
        );
    }
}

#[test]
fn rivm_to_retail_and_to_consumption() {
    let it = items(RIVM, &fixture("factor_rivm_2024.xlsx"));
    fit(&it);
    assert_eq!(it.len(), 6);
    assert!(
        it.iter()
            .all(|i| i.data["country_code"] == "NL" && i.data["year"] == 2024)
    );
    let scopes: std::collections::BTreeSet<&str> = it
        .iter()
        .map(|i| i.data["scope"].as_str().unwrap())
        .collect();
    assert_eq!(
        scopes.into_iter().collect::<Vec<_>>(),
        ["cradle to consumption (NL)", "cradle to retail (NL)"]
    );
    let r = it
        .iter()
        .find(|i| {
            i.id.as_deref()
                .is_some_and(|s| s.starts_with("rivm-retail:2024:almonds_salted"))
        })
        .unwrap();
    assert_eq!(r.data["value"], 2.94140895368816);
}

#[test]
fn a_workbook_missing_one_mapped_sheet_is_refused() {
    // The eGRID file must hold both Table 1 and Table 3.
    let only_concito = fixture("factor_concito_v1_2.xlsx");
    let r = records(
        "factor_table",
        &CanonicalUrl::parse(EGRID).unwrap(),
        &only_concito,
    )
    .unwrap();
    assert!(matches!(r, Err(Error::Invalid(_))), "{r:?}");
}

#[test]
fn eurostat_sdg_13_31_is_new_car_co2_per_country() {
    let it = records(
        "eurostat_jsonstat",
        &CanonicalUrl::parse(SDG).unwrap(),
        &fixture("eurostat_sdg_13_31.json"),
    )
    .unwrap()
    .unwrap();
    fit(&it);
    assert!(it.iter().all(|i| i.kind == "emission_factor"));
    // No EU27_2020 aggregate; EL and UK codes map to GR and GB.
    assert!(
        it.iter()
            .all(|i| i.data["country_code"].as_str().unwrap().len() == 2)
    );
    let at = by_id(&it, "eurostat-sdg-13-31:2000:AT");
    assert_eq!(at.data["unit"], "g CO2 per km");
    assert_eq!(at.data["category"], "car");
    assert_eq!(at.data["geography"], "Austria");
    assert_eq!(at.data["as_of"], "2026-09-08");
    assert_eq!(at.data["scope"], "new registrations, type approval");
    // Status flags: the 2017 break (`be`) and the estimates (`e`) are named in the scope;
    // provisional 2025 cells (`p`) are not stored.
    assert_eq!(
        by_id(&it, "eurostat-sdg-13-31:2017:AT").data["scope"],
        "type approval; series break, estimated"
    );
    assert_eq!(
        by_id(&it, "eurostat-sdg-13-31:2018:AT").data["scope"],
        "type approval; estimated"
    );
    assert_eq!(
        by_id(&it, "eurostat-sdg-13-31:2016:AT").data["scope"],
        "new registrations, type approval"
    );
    assert!(
        it.iter().all(|i| i.data["year"] != 2025),
        "provisional 2025 cells are skipped"
    );
}
