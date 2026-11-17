//! Sources: the `sources/<id>.yaml` spec and API/dump adapters (spec §5.10, §5.12, r5 Q6).
//! Implemented by `vc-sources`.
//!
//! [`SourceSpec`] is mirrored verbatim by `schemas/source.schema.json` (Task 0B). Unknown
//! fields are rejected everywhere (`deny_unknown_fields`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::clock::Clock;
use crate::error::{LeaseLost, Result};
use crate::fetch::PermitClient;
use crate::frontier::{ClaimPermit, Frontier, LeasedUrl};
use crate::ids::{KindId, SourceId};
use crate::licence::{Licence, SourceClass};
use crate::secrets::SecretSource;
use crate::store::CaptureSink;

/// How a source is crawled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Method {
    /// HTML pages from seeds (and links).
    Html,
    /// RSS/Atom feeds.
    Rss,
    /// Sitemaps.
    Sitemap,
    /// An API through an adapter.
    Api,
    /// A dump through an adapter.
    Dump,
}

/// When to use Chromium.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RenderMode {
    /// Never render (default).
    #[default]
    Never,
    /// Render captures the processor flags (little text, SPA markers).
    Auto,
    /// Always render.
    Always,
}

/// Recrawl cadence (upper bound for the change-rate estimator).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cadence {
    /// Every hour.
    Hourly,
    /// Every 2 hours.
    H2,
    /// Every 6 hours.
    H6,
    /// Every 12 hours.
    H12,
    /// Daily.
    Daily,
    /// Weekly.
    Weekly,
    /// Monthly.
    Monthly,
    /// Quarterly.
    Quarterly,
    /// Yearly.
    Yearly,
    /// Only when triggered by an operator.
    OnDemand,
}

impl Cadence {
    /// The interval, or `None` for [`Cadence::OnDemand`].
    pub fn interval(&self) -> Option<std::time::Duration> {
        const H: u64 = 3600;
        let s = match self {
            Cadence::Hourly => H,
            Cadence::H2 => 2 * H,
            Cadence::H6 => 6 * H,
            Cadence::H12 => 12 * H,
            Cadence::Daily => 24 * H,
            Cadence::Weekly => 7 * 24 * H,
            Cadence::Monthly => 30 * 24 * H,
            Cadence::Quarterly => 91 * 24 * H,
            Cadence::Yearly => 365 * 24 * H,
            Cadence::OnDemand => return None,
        };
        Some(std::time::Duration::from_secs(s))
    }
}

/// A section of the Voyager app a source feeds (`relevance.sections`). Closed list: a source
/// that serves none of these is not pertinent to trip planning and is not added.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppSection {
    /// Flights (airports, routes).
    Flights,
    /// Weather.
    Weather,
    /// Hotels and places to stay.
    Hotels,
    /// Activities.
    Activities,
    /// Places to see.
    Places,
    /// Free walking tours.
    WalkingTours,
    /// Events.
    Events,
    /// Visa and entry rules, travel advisories.
    Visa,
    /// SIM cards and eSIMs.
    Sim,
    /// Travel tips.
    Tips,
    /// Emergency card (numbers, embassies).
    EmergencyCard,
    /// Local transportation (getting around).
    GettingAround,
    /// Forex (exchange rates).
    Forex,
    /// Packing list.
    Packing,
    /// Day-by-day itinerary.
    Itinerary,
}

impl AppSection {
    /// Every section, in app order.
    pub const ALL: [AppSection; 15] = [
        AppSection::Flights,
        AppSection::Weather,
        AppSection::Hotels,
        AppSection::Activities,
        AppSection::Places,
        AppSection::WalkingTours,
        AppSection::Events,
        AppSection::Visa,
        AppSection::Sim,
        AppSection::Tips,
        AppSection::EmergencyCard,
        AppSection::GettingAround,
        AppSection::Forex,
        AppSection::Packing,
        AppSection::Itinerary,
    ];

    /// The snake_case spelling used in `sources/*.yaml`.
    pub fn as_str(&self) -> &'static str {
        match self {
            AppSection::Flights => "flights",
            AppSection::Weather => "weather",
            AppSection::Hotels => "hotels",
            AppSection::Activities => "activities",
            AppSection::Places => "places",
            AppSection::WalkingTours => "walking_tours",
            AppSection::Events => "events",
            AppSection::Visa => "visa",
            AppSection::Sim => "sim",
            AppSection::Tips => "tips",
            AppSection::EmergencyCard => "emergency_card",
            AppSection::GettingAround => "getting_around",
            AppSection::Forex => "forex",
            AppSection::Packing => "packing",
            AppSection::Itinerary => "itinerary",
        }
    }
}

/// Why a source is pertinent to trip planning (`relevance:` in the spec). Required for every
/// source: `vc_sources::validate` rejects a spec without it. It is optional in serde only so
/// specs stored before the field existed still deserialize.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Relevance {
    /// The app sections the source feeds (at least one).
    pub sections: Vec<AppSection>,
    /// One plain sentence: what traveller-facing data it gives which section.
    pub why: String,
}

fn default_max_depth() -> u8 {
    2
}
fn default_min_delay_s() -> u32 {
    5
}
fn default_daily_budget() -> u32 {
    3000
}
fn default_true() -> bool {
    true
}

/// A source (`sources/<id>.yaml`). Required: `id`, `name`, `category`, `source_class`,
/// `licence` (`class`, `attribution`), `method`, `cadence`; everything else has a default.
/// `relevance` is required as well, by validation (`vc_sources::validate`), not by serde.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceSpec {
    /// Stable id: the file stem, `^[a-z0-9][a-z0-9-]{1,62}$` (e.g. `fcdo-content-api`).
    pub id: SourceId,
    /// Display name.
    pub name: String,
    /// Category (e.g. `visa`, `events`, `blog`; free-form, from the research).
    pub category: String,
    /// Publisher class.
    pub source_class: SourceClass,
    /// Licence.
    pub licence: Licence,
    /// Crawl method.
    pub method: Method,
    /// Seed URLs.
    #[serde(default)]
    pub seeds: Vec<Url>,
    /// Sitemap URLs.
    #[serde(default)]
    pub sitemaps: Vec<Url>,
    /// Feed URLs.
    #[serde(default)]
    pub feeds: Vec<Url>,
    /// Adapter id for `api`/`dump` methods.
    #[serde(default)]
    pub adapter: Option<String>,
    /// URL include regexes (empty = everything under the seeds' hosts).
    #[serde(default)]
    pub include: Vec<String>,
    /// URL exclude regexes.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Max link depth from a seed.
    #[serde(default = "default_max_depth")]
    pub max_depth: u8,
    /// Kinds produced by structured extraction / adapters.
    #[serde(default)]
    pub kinds: Vec<KindId>,
    /// Kinds the LLM may extract from this source.
    #[serde(default)]
    pub llm_kinds: Vec<KindId>,
    /// Rendering.
    #[serde(default)]
    pub render: RenderMode,
    /// Recrawl cadence.
    pub cadence: Cadence,
    /// Minimum delay between requests, seconds (sources can only raise the default).
    #[serde(default = "default_min_delay_s")]
    pub min_delay_s: u32,
    /// Daily request budget for the key (sources can only lower the default).
    #[serde(default = "default_daily_budget")]
    pub daily_budget: u32,
    /// Group several domains under one politeness key.
    #[serde(default)]
    pub politeness_key: Option<String>,
    /// Enabled (restricted-licence sources must be `false`).
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Needs an API key.
    #[serde(default)]
    pub needs_key: bool,
    /// Secret Manager name of the key.
    #[serde(default)]
    pub key_secret: Option<String>,
    /// Max retention of derived data (api_terms sources).
    #[serde(default)]
    pub max_retention_days: Option<u32>,
    /// Display requirements from the provider's terms.
    #[serde(default)]
    pub display_requirements: Option<String>,
    /// Notes.
    #[serde(default)]
    pub notes: Option<String>,
    /// The app sections the source serves and why (required by `vc_sources::validate`).
    #[serde(default)]
    pub relevance: Option<Relevance>,
}

/// What an adapter run did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterReport {
    /// Requests made.
    pub requests: u32,
    /// Spool records written.
    pub records: u32,
    /// Decoded bytes read.
    pub bytes: u64,
    /// URLs enqueued.
    pub enqueued: u64,
    /// Whether the run finished (false = resume from `cursor` next time).
    pub complete: bool,
    /// Resume cursor (adapter-defined, no page content).
    pub cursor: Option<String>,
}

/// Everything an adapter may use. Adapters never claim: the fetcher (or maintenance, for
/// dumps) claims the key and hands over the permit.
pub struct AdapterCtx<'a> {
    /// The permit for the source's key.
    pub permit: &'a ClaimPermit,
    /// The permit-gated client (paces pagination).
    pub client: &'a dyn PermitClient,
    /// Where raw records go (raw first).
    pub sink: &'a mut dyn CaptureSink,
    /// The frontier (for `extend` and `enqueue`).
    pub frontier: &'a dyn Frontier,
    /// The source spec.
    pub source: &'a SourceSpec,
    /// The leased URL row (`fetch_mode = adapter:<id>`).
    pub url: &'a LeasedUrl,
    /// Secrets (API keys).
    pub secrets: &'a dyn SecretSource,
    /// Local clock.
    pub clock: &'a dyn Clock,
}

impl AdapterCtx<'_> {
    /// Extends the lease (call between dump chunks; without it the lease expires and the
    /// stream aborts).
    pub async fn extend(&self, by: std::time::Duration) -> Result<(), LeaseLost> {
        self.frontier.extend(self.permit, by).await
    }
}

/// An API or dump adapter, registered in `vc_sources::adapters::REGISTRY`.
#[async_trait]
pub trait SourceAdapter: Send + Sync {
    /// The adapter id (`adapter:` in the spec and the fetch mode).
    fn id(&self) -> &'static str;

    /// Runs one unit of work under the permit, writing raw records to `ctx.sink`.
    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport>;
}
