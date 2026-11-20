//! Sources, statistics and readiness (`/v1/sources`, `/v1/stats`, `/readyz`), and the carbon
//! reads (`/v1/carbon/factors`, `/v1/carbon/grid`, `/v1/carbon/occupancy`) and the place
//! facts (`/v1/facts`).

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

/// Filters of `/v1/carbon/occupancy` (current `occupancy` records: persons per vehicle). Every
/// filter is an exact match on the record's field; `None` = any.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OccupancyQuery {
    /// `mode` (`car`, `taxi`, `bus`, `coach`, `bus_coach`, `rail`, …).
    pub mode: Option<String>,
    /// `country_code` (ISO 3166-1 alpha-2).
    pub country: Option<String>,
    /// `year`.
    pub year: Option<i32>,
    /// `trip_purpose` (`all`, `work`, `commuting`, `leisure`, `other`).
    pub trip_purpose: Option<String>,
    /// `basis` (`per_vehicle_km`, `per_trip`).
    pub basis: Option<String>,
    /// The source id (`od-dft-tag-databook`, …).
    pub source: Option<String>,
    /// Licence classes the client may receive (empty = none).
    pub licence_classes: Vec<LicenceClass>,
    /// Page size.
    pub limit: u32,
    /// Rows to skip.
    pub offset: u32,
}

impl OccupancyQuery {
    /// Whether an item passes the filters (the in-memory form of the SQL `WHERE`).
    pub fn accepts(&self, it: &Item) -> bool {
        let f = |k: &str| it.data.get(k).and_then(|v| v.as_str());
        let eq = |want: &Option<String>, k: &str| want.as_deref().is_none_or(|w| f(k) == Some(w));
        it.kind.0.as_ref() == "occupancy"
            && self.licence_classes.contains(&it.licence.class)
            && it.use_policy.index == IndexUse::Full
            && it.use_policy.serve != ServeUse::LinkOnly
            && eq(&self.mode, "mode")
            && eq(&self.country, "country_code")
            && eq(&self.trip_purpose, "trip_purpose")
            && eq(&self.basis, "basis")
            && self
                .year
                .is_none_or(|y| it.data.get("year").and_then(|v| v.as_i64()) == Some(i64::from(y)))
            && self
                .source
                .as_deref()
                .is_none_or(|src| it.provenance.iter().any(|p| p.source_id.0 == src))
    }
}

/// Filters of `/v1/facts` (current `place_fact` records). The place filters match the
/// **resolved** place ([`resolve_place`]): a fact keyed only by its country code is found
/// under that country's GeoNames id and QID too. `None` = any.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FactQuery {
    /// GeoNames id of the place.
    pub geonames_id: Option<i64>,
    /// Wikidata QID of the place.
    pub wikidata_qid: Option<String>,
    /// ISO 3166-1 alpha-2 country of the place.
    pub country: Option<String>,
    /// `fact_type`.
    pub fact_type: Option<String>,
    /// Licence classes the client may receive (empty = none).
    pub licence_classes: Vec<LicenceClass>,
    /// Page size.
    pub limit: u32,
    /// Rows to skip.
    pub offset: u32,
}

/// One `/v1/facts` row: the record, its resolved place and its source's registry entry.
#[derive(Debug, Clone, PartialEq)]
pub struct FactRow {
    /// The `place_fact` record.
    pub item: Item,
    /// The place it is about: its own `place`, with a bare country's GeoNames id and QID
    /// filled in from a Wikidata country fact ([`resolve_place`]).
    pub place: serde_json::Value,
    /// The source's name (`sources.name`).
    pub source_name: Option<String>,
    /// The source's class (`sources.source_class`).
    pub source_class: Option<String>,
}

/// Whether a record's own `place` is a country known only by its ISO code (no GeoNames id,
/// no QID; e.g. a Factbook fact), so it takes those keys from a Wikidata country fact.
pub fn is_bare_country(place: &serde_json::Value) -> bool {
    place.get("geonames_id").is_none()
        && place.get("wikidata_qid").is_none()
        && place.get("admin_level").and_then(|v| v.as_str()) == Some("country")
        && place.get("country").and_then(|v| v.as_str()).is_some()
}

/// The place a `place_fact` is about, resolved against `places` (the current place facts):
/// its own `place`; for a bare country ([`is_bare_country`]), that place plus the
/// `geonames_id` and `wikidata_qid` of the first Wikidata country fact (lowest record id) of
/// the same country, so that the GeoNames and QID filters find it too. `None` only when the
/// record has no `place`. The in-memory form of the SQL.
pub fn resolve_place(it: &Item, places: &[Item]) -> Option<serde_json::Value> {
    let own = it.data.get("place")?;
    if !is_bare_country(own) {
        return Some(own.clone());
    }
    let cc = own.get("country");
    let hit = places
        .iter()
        .filter(|p| p.kind.0.as_ref() == "place_fact")
        .filter_map(|p| p.data.get("place").map(|pl| (p, pl)))
        .filter(|(_, pl)| {
            pl.get("wikidata_qid").is_some()
                && pl.get("admin_level").and_then(|v| v.as_str()) == Some("country")
                && pl.get("country") == cc
        })
        .min_by_key(|(p, _)| p.id.parse::<i64>().unwrap_or(i64::MAX));
    let mut out = own.clone();
    if let (Some((_, pl)), Some(o)) = (hit, out.as_object_mut()) {
        for k in ["geonames_id", "wikidata_qid"] {
            if let Some(v) = pl.get(k) {
                o.insert(k.into(), v.clone());
            }
        }
    }
    Some(out)
}

/// Whether a record's text is our own (`text_origin` `templated` or `rewritten`; a missing one
/// is `templated`): source text kept as written is never served.
pub fn own_words(it: &Item) -> bool {
    matches!(
        it.data
            .get("text_origin")
            .map_or(Some("templated"), |v| v.as_str()),
        Some("templated" | "rewritten")
    )
}

impl FactQuery {
    /// Whether an item with its resolved place passes the filters (the in-memory form of the
    /// SQL `WHERE`).
    pub fn accepts(&self, it: &Item, place: &serde_json::Value) -> bool {
        let pf = |k: &str| place.get(k).and_then(|v| v.as_str());
        it.kind.0.as_ref() == "place_fact"
            && self.licence_classes.contains(&it.licence.class)
            && it.use_policy.index == IndexUse::Full
            && it.use_policy.serve != ServeUse::LinkOnly
            && own_words(it)
            && self.geonames_id.is_none_or(|g| {
                place.get("geonames_id").and_then(|v| v.as_i64()) == Some(g)
                    || it.place_ids.contains(&g)
            })
            && self
                .wikidata_qid
                .as_deref()
                .is_none_or(|q| pf("wikidata_qid") == Some(q))
            && self
                .country
                .as_deref()
                .is_none_or(|c| pf("country") == Some(c))
            && self
                .fact_type
                .as_deref()
                .is_none_or(|t| it.data.get("fact_type").and_then(|v| v.as_str()) == Some(t))
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
    /// Current, servable `occupancy` records matching `q`, newest year first, then by id; at
    /// most `q.limit` items after `q.offset`.
    async fn carbon_occupancy(&self, q: &OccupancyQuery) -> Result<Vec<Item>> {
        let _ = q;
        Ok(Vec::new())
    }
    /// Current, servable `place_fact` records matching `q`, by record id; at most `q.limit` rows after `q.offset`.
    async fn facts(&self, q: &FactQuery) -> Result<Vec<FactRow>> {
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
    /// `occupancy` items served by [`Catalog::carbon_occupancy`].
    pub occupancy: Vec<Item>,
    /// `place_fact` items served by [`Catalog::facts`].
    pub facts: Vec<Item>,
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
    async fn carbon_occupancy(&self, q: &OccupancyQuery) -> Result<Vec<Item>> {
        let mut v: Vec<Item> = self
            .occupancy
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
    async fn facts(&self, q: &FactQuery) -> Result<Vec<FactRow>> {
        let mut v: Vec<FactRow> =
            self.facts
                .iter()
                .filter_map(|i| {
                    let place = resolve_place(i, &self.facts)?;
                    q.accepts(i, &place).then(|| {
                        let src = i.provenance.first().and_then(|p| {
                            self.sources.iter().find(|s| s.source_id == p.source_id.0)
                        });
                        FactRow {
                            item: i.clone(),
                            place,
                            source_name: src.map(|s| s.name.clone()),
                            source_class: src.map(|s| s.source_class.clone()),
                        }
                    })
                })
                .collect();
        v.sort_by_key(|r| r.item.id.parse::<i64>().unwrap_or(i64::MAX));
        Ok(v.into_iter()
            .skip(q.offset as usize)
            .take(q.limit as usize)
            .collect())
    }
}
