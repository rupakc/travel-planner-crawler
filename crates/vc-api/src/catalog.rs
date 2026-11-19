//! Sources, statistics and readiness (`/v1/sources`, `/v1/stats`, `/readyz`), and the carbon
//! factor reads (`/v1/carbon/factors`, `/v1/carbon/grid`).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use vc_core::Result;
use vc_core::index::{Item, ObservationView};
use vc_core::licence::LicenceClass;
use vc_core::usepolicy::{IndexUse, ServeUse};

use crate::dto::{SourceDto, StatsDto};

/// Filters of `/v1/carbon/factors` (current `emission_factor` records). Every filter is an
/// exact match on the record's field except `activity` (a substring of the slug); `None` = any.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FactorQuery {
    /// `category` (`flights`, `rail`, `accommodation`, `electricity`, `food`, …).
    pub category: Option<String>,
    /// `country_code` (ISO 3166-1 alpha-2).
    pub country: Option<String>,
    /// `region_code` (eGRID subregion, US state, …).
    pub region: Option<String>,
    /// `year`.
    pub year: Option<i32>,
    /// `gas_basis`.
    pub gas_basis: Option<String>,
    /// The `activity` slug contains this text (lower-case snake_case).
    pub activity: Option<String>,
    /// The source id (`od-desnz-ghg-factors`, `od-epa-egrid`, …).
    pub source: Option<String>,
    /// Licence classes the client may receive (empty = none).
    pub licence_classes: Vec<LicenceClass>,
    /// Page size.
    pub limit: u32,
    /// Rows to skip.
    pub offset: u32,
}

impl FactorQuery {
    /// Whether an item passes the filters (the in-memory form of the SQL `WHERE`).
    pub fn accepts(&self, it: &Item) -> bool {
        let f = |k: &str| it.data.get(k).and_then(|v| v.as_str());
        let eq = |want: &Option<String>, k: &str| want.as_deref().is_none_or(|w| f(k) == Some(w));
        it.kind.0.as_ref() == "emission_factor"
            && self.licence_classes.contains(&it.licence.class)
            && it.use_policy.index == IndexUse::Full
            && it.use_policy.serve != ServeUse::LinkOnly
            && eq(&self.category, "category")
            && eq(&self.country, "country_code")
            && eq(&self.region, "region_code")
            && eq(&self.gas_basis, "gas_basis")
            && self
                .year
                .is_none_or(|y| it.data.get("year").and_then(|v| v.as_i64()) == Some(i64::from(y)))
            && self
                .activity
                .as_deref()
                .is_none_or(|a| f("activity").is_some_and(|s| s.contains(a)))
            && self
                .source
                .as_deref()
                .is_none_or(|src| it.provenance.iter().any(|p| p.source_id.0 == src))
    }
}

/// Filters of `/v1/carbon/grid` (`grid_carbon_intensity` points of one country, newest
/// first).
#[derive(Debug, Clone, PartialEq)]
pub struct GridQuery {
    /// ISO 3166-1 alpha-2 country.
    pub country: String,
    /// `annual` or `monthly` (`None` = both).
    pub period: Option<String>,
    /// `lifecycle` or `direct` (`None` = both).
    pub basis: Option<String>,
    /// From (inclusive).
    pub from: DateTime<Utc>,
    /// To (exclusive).
    pub to: DateTime<Utc>,
    /// Max points.
    pub limit: u32,
}

impl GridQuery {
    /// The `LIKE` pattern of the matching series keys (`grid_ci:FR:%:lifecycle`). The parts
    /// are validated by the handler (no wildcard can come from a client).
    pub fn series_pattern(&self) -> String {
        format!(
            "grid_ci:{}:{}:{}",
            self.country,
            self.period.as_deref().unwrap_or("%"),
            self.basis.as_deref().unwrap_or("%")
        )
    }

    /// Whether a point passes the filters (the in-memory form of the SQL `WHERE`).
    pub fn accepts(&self, o: &ObservationView) -> bool {
        let mut p = o.series_key.splitn(4, ':');
        let (Some("grid_ci"), Some(cc), Some(period), Some(basis)) =
            (p.next(), p.next(), p.next(), p.next())
        else {
            return false;
        };
        cc == self.country
            && self.period.as_deref().is_none_or(|w| w == period)
            && self.basis.as_deref().is_none_or(|w| w == basis)
            && o.observed_at >= self.from
            && o.observed_at < self.to
    }
}

/// Catalogue data that is not part of the vc-core read model.
#[async_trait]
pub trait Catalog: Send + Sync {
    /// Every source, by id.
    async fn sources(&self) -> Result<Vec<SourceDto>>;
    /// Counts.
    async fn stats(&self) -> Result<StatsDto>;
    /// Readiness of the backing store.
    async fn ready(&self) -> Result<()>;
    /// Current, servable `emission_factor` records matching `q`, newest year first, then by
    /// id; at most `q.limit` items after `q.offset`.
    async fn carbon_factors(&self, q: &FactorQuery) -> Result<Vec<Item>> {
        let _ = q;
        Ok(Vec::new())
    }
    /// `grid_carbon_intensity` points matching `q`, newest first.
    async fn carbon_grid(&self, q: &GridQuery) -> Result<Vec<ObservationView>> {
        let _ = q;
        Ok(Vec::new())
    }
}

/// An in-memory [`Catalog`].
#[derive(Debug, Default)]
pub struct MemCatalog {
    /// Sources.
    pub sources: Vec<SourceDto>,
    /// `emission_factor` items served by [`Catalog::carbon_factors`].
    pub factors: Vec<Item>,
    /// `grid_carbon_intensity` points served by [`Catalog::carbon_grid`].
    pub grid: Vec<ObservationView>,
}

fn year_of(it: &Item) -> i64 {
    it.data
        .get("year")
        .and_then(|v| v.as_i64())
        .unwrap_or(i64::MIN)
}

#[async_trait]
impl Catalog for MemCatalog {
    async fn sources(&self) -> Result<Vec<SourceDto>> {
        Ok(self.sources.clone())
    }
    async fn stats(&self) -> Result<StatsDto> {
        Ok(StatsDto {
            sources_enabled: i64::try_from(self.sources.iter().filter(|s| s.enabled).count())
                .unwrap_or(i64::MAX),
            records_active: 0,
            entities_active: 0,
            changes_seq_max: None,
            generated_at: Utc::now(),
        })
    }
    async fn ready(&self) -> Result<()> {
        Ok(())
    }
    async fn carbon_factors(&self, q: &FactorQuery) -> Result<Vec<Item>> {
        let mut v: Vec<Item> = self
            .factors
            .iter()
            .filter(|i| q.accepts(i))
            .cloned()
            .collect();
        v.sort_by(|a, b| year_of(b).cmp(&year_of(a)).then_with(|| a.id.cmp(&b.id)));
        Ok(v.into_iter()
            .skip(q.offset as usize)
            .take(q.limit as usize)
            .collect())
    }
    async fn carbon_grid(&self, q: &GridQuery) -> Result<Vec<ObservationView>> {
        let mut v: Vec<ObservationView> =
            self.grid.iter().filter(|o| q.accepts(o)).cloned().collect();
        v.sort_by(|a, b| {
            b.observed_at
                .cmp(&a.observed_at)
                .then_with(|| a.series_key.cmp(&b.series_key))
        });
        v.truncate(q.limit as usize);
        Ok(v)
    }
}
