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

pub mod canada_advisories;
pub mod ecb_fx;
pub mod fcdo_content_api;
pub mod met_norway;
pub mod mobility_database_catalog;
pub mod nager_holidays;
pub mod ourairports_csv;
pub mod records;
pub mod se_travel_dump;
pub mod state_dept_advisories;
pub mod ticketmaster_discovery;
pub mod wikidata_sparql;
pub mod wikivoyage_dump;

pub use dump::DumpSettings;

/// Every built-in adapter, with default settings.
pub static REGISTRY: &[&dyn SourceAdapter] = &[
    &canada_advisories::CanadaAdvisories,
    &ecb_fx::EcbFx,
    &fcdo_content_api::FcdoContentApi,
    &met_norway::MetNorway,
    &mobility_database_catalog::MobilityDatabaseCatalog::DEFAULT,
    &nager_holidays::NagerHolidays,
    &ourairports_csv::OurAirportsCsv::DEFAULT,
    &se_travel_dump::SeTravelDump::DEFAULT,
    &state_dept_advisories::StateDeptAdvisories,
    &ticketmaster_discovery::TicketmasterDiscovery::DEFAULT,
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
        Arc::new(ticketmaster_discovery::TicketmasterDiscovery::DEFAULT),
        Arc::new(wikidata_sparql::WikidataSparql::DEFAULT),
    ]
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
        Box::new(ecb_fx::EcbFx),
        Box::new(fcdo_content_api::FcdoContentApi),
        Box::new(met_norway::MetNorway),
        Box::new(mobility_database_catalog::MobilityDatabaseCatalog::new(
            d.clone(),
        )),
        Box::new(nager_holidays::NagerHolidays),
        Box::new(ourairports_csv::OurAirportsCsv::new(d.clone())),
        Box::new(se_travel_dump::SeTravelDump::new(d.clone())),
        Box::new(state_dept_advisories::StateDeptAdvisories),
        Box::new(ticketmaster_discovery::TicketmasterDiscovery::DEFAULT),
        Box::new(wikidata_sparql::WikidataSparql::DEFAULT),
        Box::new(wikivoyage_dump::WikivoyageDump::new(d)),
    ]
}

/// Every built-in adapter with default settings (boxed).
pub fn registry() -> Vec<Box<dyn SourceAdapter>> {
    registry_with(&vc_config::SourcesConfig::default())
}
