//! `mobility_database_catalog`: the Mobility Database catalog CSV (`feeds_v2.csv`, CC0),
//! streamed. Filter v1 keeps GTFS Schedule feeds that are not deprecated or inactive; rows
//! are stored verbatim under the original header.
//!
//! **Note:** `files.mobilitydatabase.org/robots.txt` disallows `/` (checked 2026-11-15), so
//! the permit client refuses the download until the operator allows it or another route is
//! chosen; the adapter never works around robots.

use async_trait::async_trait;
use vc_core::Result;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};

use super::csv_filter::{CsvFilter, Row};
use super::dump::{DumpFilter, DumpSettings, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "mobility-catalog-gtfs-v1";

fn keep(r: &Row<'_>) -> bool {
    r.get("data_type") == "gtfs" && !matches!(r.get("status"), "deprecated" | "inactive")
}

/// The filter.
pub(crate) fn filter() -> impl DumpFilter {
    CsvFilter::new(FILTER_VERSION, &["id", "data_type", "status"], keep)
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct MobilityDatabaseCatalog {
    settings: DumpSettings,
}

impl MobilityDatabaseCatalog {
    /// Default settings.
    pub const DEFAULT: MobilityDatabaseCatalog = MobilityDatabaseCatalog {
        settings: DumpSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        MobilityDatabaseCatalog { settings }
    }
}

#[async_trait]
impl SourceAdapter for MobilityDatabaseCatalog {
    fn id(&self) -> &'static str {
        "mobility_database_catalog"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        let mut f = filter();
        run_dump(ctx, self.id(), url, &mut f, &self.settings).await
    }
}
