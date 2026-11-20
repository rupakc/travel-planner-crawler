//! The `/v1` wire format. These types are the frozen contract (`docs/Crawler-API.md`,
//! additive changes only); they are converted from the vc-core read model so a change in
//! vc-core never silently changes the API.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use vc_core::extract::PlaceMatch;
use vc_core::index::{ChangeOp, ChangeView, ClaimView, ObservationView, OwnerType, ProvenanceView};
use vc_core::licence::{Licence, LicenceClass};
use vc_core::records::{ExtractionMethod, ExtractorInfo};
use vc_core::usepolicy::{IndexUse, ServeUse, UsePolicy};

/// Licence class of served data (`restricted` data is never ingested or served).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum LicenceClassDto {
    /// CC0, public domain, OGL, CC BY.
    Open,
    /// CC BY-SA, ODbL: never merged with other licences; keep the attribution.
    ShareAlike,
    /// Editorial: facts + headline + ≤ 300-char excerpt + link only.
    FactsOnly,
    /// Usable per the provider's API terms.
    ApiTerms,
}

impl LicenceClassDto {
    /// The served class, `None` for `restricted`.
    pub fn from_core(c: LicenceClass) -> Option<Self> {
        match c {
            LicenceClass::Open => Some(LicenceClassDto::Open),
            LicenceClass::ShareAlike => Some(LicenceClassDto::ShareAlike),
            LicenceClass::FactsOnly => Some(LicenceClassDto::FactsOnly),
            LicenceClass::ApiTerms => Some(LicenceClassDto::ApiTerms),
            LicenceClass::Restricted => None,
        }
    }
}

/// `licence {class, spdx, attribution, url}`, on every item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct LicenceDto {
    /// Licence class.
    pub class: LicenceClassDto,
    /// SPDX id, when there is one.
    pub spdx: Option<String>,
    /// Attribution to display.
    pub attribution: String,
    /// Licence or terms URL.
    pub url: Option<String>,
}

impl LicenceDto {
    /// Converts; `None` for `restricted`.
    pub fn from_core(l: &Licence) -> Option<Self> {
        Some(LicenceDto {
            class: LicenceClassDto::from_core(l.class)?,
            spdx: l.spdx.clone(),
            attribution: l.attribution.clone(),
            url: l.url.as_ref().map(|u| u.to_string()),
        })
    }
}

/// How a record was extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionMethodDto {
    /// JSON-LD / microdata mapping.
    Structured,
    /// A source adapter.
    Adapter,
    /// LLM extraction (grounding-checked).
    Llm,
}

/// Who/what extracted a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ExtractorDto {
    /// Extractor version.
    pub version: String,
    /// Method.
    pub method: ExtractionMethodDto,
    /// Model id, for LLM extraction.
    pub model: Option<String>,
    /// blake3 hex of the prompt, for LLM extraction.
    pub prompt_hash: Option<String>,
    /// Extraction run id (ULID), if any.
    pub run_id: Option<String>,
}

impl From<&ExtractorInfo> for ExtractorDto {
    fn from(e: &ExtractorInfo) -> Self {
        ExtractorDto {
            version: e.version.clone(),
            method: match e.method {
                ExtractionMethod::Structured => ExtractionMethodDto::Structured,
                ExtractionMethod::Adapter => ExtractionMethodDto::Adapter,
                ExtractionMethod::Llm => ExtractionMethodDto::Llm,
            },
            model: e.model.as_ref().map(|m| m.0.to_string()),
            prompt_hash: e.prompt_hash.clone(),
            run_id: e.run_id.map(|r| r.to_string()),
        }
    }
}

/// Where a served item came from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ProvenanceDto {
    /// Source id (`/v1/sources`).
    pub source_id: String,
    /// Page URL (the link to show).
    pub url: String,
    /// Capture time.
    pub captured_at: DateTime<Utc>,
    /// Extractor.
    pub extractor: ExtractorDto,
}

impl From<&ProvenanceView> for ProvenanceDto {
    fn from(p: &ProvenanceView) -> Self {
        ProvenanceDto {
            source_id: p.source_id.0.clone(),
            url: p.url.clone(),
            captured_at: p.captured_at,
            extractor: (&p.extractor).into(),
        }
    }
}

/// What may be indexed and served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum IndexUseDto {
    /// Title and link only.
    TitleAndLink,
    /// Fully indexed.
    Full,
}

/// What may be shown to Voyager's agents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServeUseDto {
    /// A link only (no data, no excerpt).
    LinkOnly,
    /// Facts + link, no excerpt.
    FactsOnly,
    /// Facts, excerpt and link.
    Full,
}

/// The effective use policy of an item (the API has already applied it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UsePolicyDto {
    /// Index use.
    pub index: IndexUseDto,
    /// Serve use.
    pub serve: ServeUseDto,
}

impl UsePolicyDto {
    /// Converts; `None` when the item may not be served at all (`index: none`).
    pub fn from_core(u: &UsePolicy) -> Option<Self> {
        Some(UsePolicyDto {
            index: match u.index {
                IndexUse::None => return None,
                IndexUse::TitleAndLink => IndexUseDto::TitleAndLink,
                IndexUse::Full => IndexUseDto::Full,
            },
            serve: match u.serve {
                ServeUse::LinkOnly => ServeUseDto::LinkOnly,
                ServeUse::FactsOnly => ServeUseDto::FactsOnly,
                ServeUse::Full => ServeUseDto::Full,
            },
        })
    }
}

/// One served item (entity or record).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ItemDto {
    /// Entity id (ULID) or record id (decimal).
    pub id: String,
    /// Kind (e.g. `attraction`, `event`, `article`).
    pub kind: String,
    /// `kind@version` of `data`.
    pub schema: String,
    /// Title / headline.
    pub title: Option<String>,
    /// ≤ 300-character excerpt (the only text served for `facts_only`).
    pub excerpt: Option<String>,
    /// Facts, per the kind's JSON schema.
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
    /// GeoNames ids.
    pub place_ids: Vec<i64>,
    /// Licence.
    pub licence: LicenceDto,
    /// One entry per contributing record.
    pub provenance: Vec<ProvenanceDto>,
    /// A cross-source conflict on a high-stakes field: show with care.
    pub conflict: bool,
    /// Effective use policy.
    pub use_policy: UsePolicyDto,
    /// Last update.
    pub updated_at: DateTime<Utc>,
    /// Ranking score (search only).
    pub score: Option<f32>,
}

/// A page of items.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ItemPage {
    /// Items.
    pub items: Vec<ItemDto>,
    /// Pass as `cursor` for the next page; absent on the last page.
    pub next_cursor: Option<String>,
}

/// One per-attribute claim of an entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ClaimDto {
    /// Attribute.
    pub attribute: String,
    /// Value.
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
    /// Contributing record.
    pub record_id: i64,
    /// Record version.
    pub version: i32,
    /// Confidence in `[0, 1]`.
    pub confidence: f32,
}

impl From<&ClaimView> for ClaimDto {
    fn from(c: &ClaimView) -> Self {
        ClaimDto {
            attribute: c.attribute.clone(),
            value: c.value.clone(),
            record_id: c.record_id.0,
            version: c.version,
            confidence: c.confidence,
        }
    }
}

/// An entity with its claims.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct EntityDto {
    /// The entity.
    pub item: ItemDto,
    /// Per-attribute provenance.
    pub claims: Vec<ClaimDto>,
}

/// Body of a `301` for a merged entity (`Location` carries the same path).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RedirectDto {
    /// The entity it was merged into.
    pub redirect_to: String,
    /// Its path.
    pub location: String,
}

/// One version of a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct RecordVersionDto {
    /// Record id.
    pub record_id: i64,
    /// Version (1-based).
    pub version: i32,
    /// `kind@version`.
    pub schema: String,
    /// Data.
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
    /// blake3 hex of the canonical JSON.
    pub data_hash: String,
    /// Capture time.
    pub captured_at: DateTime<Utc>,
    /// Extractor.
    pub extractor: ExtractorDto,
    /// Licence class.
    pub licence_class: LicenceClassDto,
    /// Effective use policy.
    pub use_policy: UsePolicyDto,
    /// Valid from.
    pub valid_from: DateTime<Utc>,
    /// Superseded at (absent = current).
    pub superseded_at: Option<DateTime<Utc>>,
}

/// A page of record versions, newest first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct VersionPage {
    /// Versions.
    pub items: Vec<RecordVersionDto>,
    /// Next page cursor.
    pub next_cursor: Option<String>,
}

/// `changes.owner_type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OwnerTypeDto {
    /// A record.
    Record,
    /// An entity.
    Entity,
    /// An observation series.
    Observation,
    /// A source.
    Source,
}

/// `changes.op`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOpDto {
    /// Created or updated: re-fetch it.
    Upsert,
    /// Tombstoned: delete any copy.
    Tombstone,
    /// Entity merged: re-fetch (the old id answers 301).
    Redirect,
}

/// One change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ChangeDto {
    /// Opaque position (`<xid>-<seq>`).
    pub cursor: String,
    /// Owner type.
    pub owner_type: OwnerTypeDto,
    /// Owner id.
    pub owner_id: String,
    /// Operation.
    pub op: ChangeOpDto,
    /// Kind, when applicable.
    pub kind: Option<String>,
    /// When.
    pub at: DateTime<Utc>,
}

impl From<&ChangeView> for ChangeDto {
    fn from(c: &ChangeView) -> Self {
        ChangeDto {
            cursor: c.cursor.to_string(),
            owner_type: match c.owner_type {
                OwnerType::Record => OwnerTypeDto::Record,
                OwnerType::Entity => OwnerTypeDto::Entity,
                OwnerType::Observation => OwnerTypeDto::Observation,
                OwnerType::Source => OwnerTypeDto::Source,
            },
            owner_id: c.owner_id.clone(),
            op: match c.op {
                ChangeOp::Upsert => ChangeOpDto::Upsert,
                ChangeOp::Tombstone => ChangeOpDto::Tombstone,
                ChangeOp::Redirect => ChangeOpDto::Redirect,
            },
            kind: c.kind.as_ref().map(|k| k.0.to_string()),
            at: c.at,
        }
    }
}

/// A page of the change feed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ChangesDto {
    /// Changes in cursor order.
    pub changes: Vec<ChangeDto>,
    /// Pass as `since` next time (the last change, or the input cursor when empty).
    pub next: Option<String>,
}

/// One observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ObservationDto {
    /// Series (e.g. `fx:USD:EUR`).
    pub series_key: String,
    /// Time.
    pub observed_at: DateTime<Utc>,
    /// Value.
    #[schema(value_type = Object)]
    pub value: serde_json::Value,
    /// Source id.
    pub source_id: String,
    /// Licence.
    pub licence: LicenceDto,
}

impl ObservationDto {
    /// Converts; `None` for `restricted`.
    pub fn from_core(o: &ObservationView) -> Option<Self> {
        Some(ObservationDto {
            series_key: o.series_key.clone(),
            observed_at: o.observed_at,
            value: o.value.clone(),
            source_id: o.source_id.0.clone(),
            licence: LicenceDto::from_core(&o.licence)?,
        })
    }
}

/// Observations of a series.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct ObservationList {
    /// Points, oldest first.
    pub items: Vec<ObservationDto>,
}

/// A candidate place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PlaceDto {
    /// GeoNames id.
    pub geoname_id: i64,
    /// Canonical name.
    pub name: String,
    /// ISO 3166-1 alpha-2 country.
    pub country: String,
    /// First-level admin region.
    pub admin1: Option<String>,
    /// Latitude.
    pub lat: f64,
    /// Longitude.
    pub lon: f64,
    /// Confidence in `[0, 1]`.
    pub score: f32,
}

impl From<&PlaceMatch> for PlaceDto {
    fn from(p: &PlaceMatch) -> Self {
        PlaceDto {
            geoname_id: p.geoname_id,
            name: p.name.clone(),
            country: p.country.clone(),
            admin1: p.admin1.clone(),
            lat: p.point.lat,
            lon: p.point.lon,
            score: p.score,
        }
    }
}

/// Place candidates, best first.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct PlaceList {
    /// Candidates (empty when unresolved).
    pub items: Vec<PlaceDto>,
}

/// One configured source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SourceDto {
    /// Source id.
    pub source_id: String,
    /// Name.
    pub name: String,
    /// Category.
    pub category: String,
    /// `official`, `reference`, `editorial`, `community` or `commercial_api`.
    pub source_class: String,
    /// Licence.
    pub licence: LicenceDto,
    /// `html`, `rss`, `sitemap`, `api` or `dump`.
    pub method: String,
    /// Whether it is crawled.
    pub enabled: bool,
}

/// The sources.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SourceList {
    /// Sources, by id.
    pub items: Vec<SourceDto>,
}

/// Index statistics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StatsDto {
    /// Enabled sources.
    pub sources_enabled: i64,
    /// Active records.
    pub records_active: i64,
    /// Active entities.
    pub entities_active: i64,
    /// Highest change-feed sequence (absent when empty).
    pub changes_seq_max: Option<i64>,
    /// When these numbers were computed (cached up to 60 s).
    pub generated_at: DateTime<Utc>,
}

/// Probe answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct StatusDto {
    /// `ok` or `ready`.
    pub status: String,
}

/// The place of a fact (`/v1/facts`): the resolved place (a hook takes the Wikidata place it
/// was resolved to).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FactPlaceDto {
    /// GeoNames id (the primary join key).
    pub geonames_id: Option<i64>,
    /// Wikidata QID.
    pub wikidata_qid: Option<String>,
    /// `country`, `admin1`, `admin2`, `locality`, `village` or `feature`, when known.
    pub admin_level: Option<String>,
    /// ISO 3166-1 alpha-2 country.
    pub country: Option<String>,
    /// Name in the source.
    pub name: Option<String>,
    /// Language of `name`.
    pub name_lang: Option<String>,
}

/// A figure behind a record or statistic.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FactQuantityDto {
    /// The figure.
    pub value: f64,
    /// Unit (`m`, `km2`, `people`, `sites`, …).
    pub unit: String,
    /// What a record is a record of (`highest point of Germany`).
    pub rank_scope: Option<String>,
}

/// Where a fact comes from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FactSourceDto {
    /// Source id (`sources/*.yaml`).
    pub source_id: String,
    /// Deep link to the fact at the source.
    pub url: String,
    /// Name of the work (else the source's registry name).
    pub name: Option<String>,
    /// Publisher.
    pub publisher: Option<String>,
    /// `official`, `reference`, `editorial`, `community` or `commercial_api`.
    pub source_class: Option<String>,
    /// Whether the source cites its own sources for it: `yes`, `partial` or `no`.
    pub sourced: Option<String>,
}

/// One place fact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FactDto {
    /// Record id.
    pub id: String,
    /// The place.
    pub place: FactPlaceDto,
    /// `historical`, `fun`, `popular`, `unbelievable` or `little_known`.
    pub fact_type: String,
    /// One sentence in our own words (≤ 280 characters), or a short share-alike hook when
    /// `text_origin` is `source_hook`.
    pub text: String,
    /// Language of `text` (BCP 47).
    pub lang: Option<String>,
    /// Machine translation to English, when one was made at ingest.
    pub text_en: Option<String>,
    /// `templated`, `rewritten` or `source_hook`.
    pub text_origin: String,
    /// Whether `text` may be given to a model: always false for share-alike text, hooks and
    /// AI-reserved sources.
    pub llm_ok: bool,
    /// Year or date the fact refers to (`1244`, `1806-10-27`, `-0500`).
    pub date_ref: Option<String>,
    /// The figure behind it (structured sources only).
    pub quantity: Option<FactQuantityDto>,
    /// Source.
    pub source: FactSourceDto,
    /// Licence (share-alike: never merge with other text; keep the attribution).
    pub licence: LicenceDto,
    /// Crawl, edit or edition date (YYYY-MM-DD).
    pub as_of: Option<String>,
    /// `structured`, `extracted` or `corroborated`.
    pub confidence: String,
    /// The source's robots.txt blocks AI bots.
    pub ai_bot_blocked: bool,
    /// Last update of the record.
    pub updated_at: DateTime<Utc>,
}

/// A page of facts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct FactPage {
    /// Facts, by record id.
    pub items: Vec<FactDto>,
    /// Pass back as `cursor` for the next page.
    pub next_cursor: Option<String>,
}
