//! Built-in API and dump adapters (spec §5.10, r5 Q6).
//!
//! An adapter never claims: the fetcher (maintenance, for dumps) claims the key, leases a URL
//! row whose fetch mode is `adapter:<id>` and calls [`SourceAdapter::run`] with the permit.
//! Every request goes through `ctx.client` (lease check + global rate token + pacing per
//! request). **Raw first:** each API response is spooled (fsync) as a WARC `response` before
//! anything is derived from it; dumps are streamed, filtered on the fly into `resource`
//! records, and named by a `dump_manifest` `metadata` record. Fan-out (one row per country,
//! per year) is done by enqueueing more `adapter:<id>` rows, so the frontier's per-key
//! politeness applies to each of them. The caller marks the leased row fetched.
//!
//! Each adapter module also exposes a pure `parse` function for its responses (what the
//! adapter "yields": items it validates, counts and fans out from), tested against recorded
//! fixtures.

use std::sync::Arc;

use async_trait::async_trait;
use vc_core::Result;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};

pub(crate) mod common;
pub(crate) mod csv_filter;
pub mod dump;
pub(crate) mod gtfs_zip;
pub mod inspections;
pub(crate) mod iso3;
pub(crate) mod zip_stream;

pub mod canada_advisories;
pub mod canada_offices;
pub mod countries;
pub mod ecb_fx;
pub mod enwiki_visa;
pub mod eurostat_jsonstat;
pub mod fcdo_content_api;
pub mod fsa_fhrs;
pub mod govuk_medical_lists;
pub mod govuk_world_orgs;
pub mod gtfs_static;
pub mod html_text;
pub mod json_events;
pub mod met_norway;
pub mod mobility_database_catalog;
pub mod nager_holidays;
pub mod nyc_restaurant_inspections;
pub mod openholidays;
pub mod ourairports_csv;
pub mod phone;
pub mod records;
pub mod row_map;
pub mod se_travel_dump;
pub mod socrata;
pub mod state_dept_advisories;
pub mod state_dept_country_info;
pub mod ticketmaster_discovery;
pub mod toronto_dinesafe;
pub mod travelpayouts_fares;
pub mod udata_resource;
pub mod wdi_csv;
pub mod wfs_geojson;
pub mod wikidata_sparql;
pub mod wikivoyage_dump;

pub use dump::DumpSettings;

/// Every built-in adapter, with default settings.
pub static REGISTRY: &[&dyn SourceAdapter] = &[
    &canada_advisories::CanadaAdvisories,
    &canada_offices::CanadaOfficesAdapter,
    &ecb_fx::EcbFx,
    &enwiki_visa::EnwikiVisa::DEFAULT,
    &eurostat_jsonstat::EurostatJsonStat,
    &fcdo_content_api::FcdoContentApi,
    &fsa_fhrs::FsaFhrs::DEFAULT,
    &govuk_medical_lists::GovukMedicalLists,
    &govuk_world_orgs::GovukWorldOrgs,
    &json_events::JsonEvents::DEFAULT,
    &gtfs_static::GtfsStatic::DEFAULT,
    &met_norway::MetNorway,
    &mobility_database_catalog::MobilityDatabaseCatalog::DEFAULT,
    &nager_holidays::NagerHolidays,
    &nyc_restaurant_inspections::NycRestaurantInspections::DEFAULT,
    &openholidays::OpenHolidays,
    &ourairports_csv::OurAirportsCsv::DEFAULT,
    &se_travel_dump::SeTravelDump::DEFAULT,
    &socrata::Socrata::DEFAULT,
    &state_dept_advisories::StateDeptAdvisories,
    &state_dept_country_info::StateDeptCountryInfo,
    &ticketmaster_discovery::TicketmasterDiscovery::DEFAULT,
    &toronto_dinesafe::TorontoDinesafe::DEFAULT,
    &travelpayouts_fares::TravelpayoutsFares::DEFAULT,
    &udata_resource::UdataResource::DEFAULT,
    &wdi_csv::WdiCsv::DEFAULT,
    &wfs_geojson::WfsGeojson,
    &wikidata_sparql::WikidataSparql::DEFAULT,
    &wikivoyage_dump::WikivoyageDump::DEFAULT,
];

/// An adapter whose paged stream can continue where an earlier run stopped.
///
/// A run that stops early (page cap) reports `complete: false` and a `cursor`; the caller keeps
/// it ([`crate::cursors`]) and hands it back to the next run of the same leased row.
/// [`SourceAdapter::run`] is `run_from(ctx, None)`.
#[async_trait]
pub trait ResumableAdapter: SourceAdapter {
    /// Runs from `cursor` (an earlier report's cursor). `None`, or a cursor this adapter does
    /// not recognise (or that no longer fits the leased row), starts over.
    async fn run_from(
        &self,
        ctx: &mut AdapterCtx<'_>,
        cursor: Option<&str>,
    ) -> Result<AdapterReport>;
}

/// The built-in adapters that resume, with default settings.
pub fn resumable() -> Vec<Arc<dyn ResumableAdapter>> {
    vec![
        Arc::new(fsa_fhrs::FsaFhrs::DEFAULT),
        Arc::new(json_events::JsonEvents::DEFAULT),
        Arc::new(nyc_restaurant_inspections::NycRestaurantInspections::DEFAULT),
        Arc::new(socrata::Socrata::DEFAULT),
        Arc::new(ticketmaster_discovery::TicketmasterDiscovery::DEFAULT),
        Arc::new(toronto_dinesafe::TorontoDinesafe::DEFAULT),
        Arc::new(wikidata_sparql::WikidataSparql::DEFAULT),
    ]
}

/// The shortest time the fetcher may wait before refetching a row of adapter `id` (cached
/// data an adapter must not poll), if the adapter sets one. It raises the recrawl estimator's
/// lower bound; the source cadence still bounds it from above (and is raised to it if lower).
pub fn min_refetch(id: &str) -> Option<std::time::Duration> {
    match id {
        travelpayouts_fares::ID => Some(travelpayouts_fares::MIN_REFETCH),
        _ => None,
    }
}

/// Calls each counting adapter has sent since the process started (`(adapter id, calls)`),
/// for the `crawler_adapter_calls_total{adapter}` metric.
pub fn call_counts() -> Vec<(&'static str, u64)> {
    vec![(travelpayouts_fares::ID, travelpayouts_fares::CALLS.total())]
}

/// The ids of the built-in adapters.
pub fn ids() -> Vec<&'static str> {
    REGISTRY.iter().map(|a| a.id()).collect()
}

/// The built-in adapter with this id (default settings).
pub fn get(id: &str) -> Option<&'static dyn SourceAdapter> {
    REGISTRY.iter().copied().find(|a| a.id() == id)
}

/// Every built-in adapter, with dump settings taken from the `[sources]` config.
pub fn registry_with(cfg: &vc_config::SourcesConfig) -> Vec<Box<dyn SourceAdapter>> {
    let d = DumpSettings::from_config(cfg);
    vec![
        Box::new(canada_advisories::CanadaAdvisories),
        Box::new(canada_offices::CanadaOfficesAdapter),
        Box::new(ecb_fx::EcbFx),
        Box::new(enwiki_visa::EnwikiVisa::new(d.clone())),
        Box::new(eurostat_jsonstat::EurostatJsonStat),
        Box::new(fcdo_content_api::FcdoContentApi),
        Box::new(fsa_fhrs::FsaFhrs::DEFAULT),
        Box::new(govuk_medical_lists::GovukMedicalLists),
        Box::new(govuk_world_orgs::GovukWorldOrgs),
        Box::new(json_events::JsonEvents::DEFAULT),
        Box::new(gtfs_static::GtfsStatic::new(
            gtfs_static::GtfsSettings::from_config(cfg),
        )),
        Box::new(met_norway::MetNorway),
        Box::new(mobility_database_catalog::MobilityDatabaseCatalog::new(
            d.clone(),
        )),
        Box::new(nager_holidays::NagerHolidays),
        Box::new(nyc_restaurant_inspections::NycRestaurantInspections::DEFAULT),
        Box::new(openholidays::OpenHolidays),
        Box::new(ourairports_csv::OurAirportsCsv::new(d.clone())),
        Box::new(se_travel_dump::SeTravelDump::new(d.clone())),
        Box::new(socrata::Socrata::DEFAULT),
        Box::new(state_dept_advisories::StateDeptAdvisories),
        Box::new(state_dept_country_info::StateDeptCountryInfo),
        Box::new(ticketmaster_discovery::TicketmasterDiscovery::DEFAULT),
        Box::new(toronto_dinesafe::TorontoDinesafe::DEFAULT),
        Box::new(travelpayouts_fares::TravelpayoutsFares::DEFAULT),
        Box::new(udata_resource::UdataResource::new(d.clone())),
        Box::new(wdi_csv::WdiCsv::new(d.clone())),
        Box::new(wfs_geojson::WfsGeojson),
        Box::new(wikidata_sparql::WikidataSparql::DEFAULT),
        Box::new(wikivoyage_dump::WikivoyageDump::new(d)),
    ]
}

/// Every built-in adapter with default settings (boxed).
pub fn registry() -> Vec<Box<dyn SourceAdapter>> {
    registry_with(&vc_config::SourcesConfig::default())
}
