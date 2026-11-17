//! `wdi_csv`: the World Bank World Development Indicators bulk CSV (`WDI_CSV.zip`, CC BY 4.0),
//! streamed. The archive's data member (`WDICSV.csv`; `WDIData.csv` in older releases) is
//! read on the fly and filter v1 keeps only the rows of the indicators in [`INDICATORS`]
//! (one row per country or aggregate, one column per year); rows are stored verbatim under the
//! original header. Nothing else in the ~270 MB archive is kept.

use async_trait::async_trait;
use vc_core::Result;
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter};

use super::csv_filter::{CsvFilter, Row};
use super::dump::{DumpFilter, DumpSettings, run_dump};

/// Filter version recorded in the manifest.
pub const FILTER_VERSION: &str = "wdi-price-level-v1";

/// Indicators kept: price level ratio of the PPP conversion factor (GDP) to the market
/// exchange rate (United States = 1).
pub const INDICATORS: &[&str] = &["PA.NUS.PPPC.RF"];

/// The archive members holding the data, by release.
pub const MEMBERS: &[&str] = &["WDICSV.csv", "WDIData.csv"];

fn keep(r: &Row<'_>) -> bool {
    INDICATORS.contains(&r.get("Indicator Code").trim())
}

/// The filter.
pub(crate) fn filter() -> impl DumpFilter {
    CsvFilter::new(FILTER_VERSION, &["Country Code", "Indicator Code"], keep)
        .with_zip_member(MEMBERS)
}

/// The adapter.
#[derive(Debug, Clone)]
pub struct WdiCsv {
    settings: DumpSettings,
}

impl WdiCsv {
    /// Default settings.
    pub const DEFAULT: WdiCsv = WdiCsv {
        settings: DumpSettings::DEFAULT,
    };

    /// With settings.
    pub fn new(settings: DumpSettings) -> Self {
        WdiCsv { settings }
    }
}

#[async_trait]
impl SourceAdapter for WdiCsv {
    fn id(&self) -> &'static str {
        "wdi_csv"
    }

    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport> {
        let url = ctx.url.url.clone();
        let mut f = filter();
        run_dump(ctx, self.id(), url, &mut f, &self.settings).await
    }
}
