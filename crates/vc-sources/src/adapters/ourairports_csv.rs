//! `ourairports_csv`: OurAirports `airports.csv` (public domain), streamed. Filter v1 keeps
//! large, medium and small airports (drops heliports, seaplane bases, balloonports and closed
//! fields); rows are stored verbatim under the original header.

use async_trait::async_trait;
use vc_core::Result;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};

use super::csv_filter::{CsvFilter, Row};
use super::dump::{DumpFilter, DumpSettings, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "ourairports-airports-v1";

fn keep(r: &Row<'_>) -> bool {
    matches!(
        r.get("type"),
        "large_airport" | "medium_airport" | "small_airport"
    )
}

/// The filter.
pub(crate) fn filter() -> impl DumpFilter {
    CsvFilter::new(FILTER_VERSION, &["ident", "type", "name"], keep)
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct OurAirportsCsv {
    settings: DumpSettings,
}

impl OurAirportsCsv {
    /// Default settings.
    pub const DEFAULT: OurAirportsCsv = OurAirportsCsv {
        settings: DumpSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        OurAirportsCsv { settings }
    }
}

#[async_trait]
impl SourceAdapter for OurAirportsCsv {
    fn id(&self) -> &'static str {
        "ourairports_csv"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        let mut f = filter();
        run_dump(ctx, self.id(), url, &mut f, &self.settings).await
    }
}
