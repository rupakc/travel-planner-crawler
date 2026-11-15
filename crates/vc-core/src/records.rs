//! Extracted records and their provenance (spec §5.6, §5.7, r5 Q11). `vc-extract` produces
//! [`ExtractedRecord`]s; `vc-index` (`RecordSink`) persists them.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::{CaptureRef, ExtractionRunId, KindId, ModelId, RecordKey, SourceId, UrlKey};
use crate::licence::{Licence, SourceClass};
use crate::url::CanonicalUrl;
use crate::usepolicy::UsePolicy;

/// Max characters of an excerpt served for `facts_only` content.
pub const MAX_EXCERPT_CHARS: usize = 300;

/// How a record was extracted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionMethod {
    /// JSON-LD / microdata mapping (free).
    Structured,
    /// A source adapter's parser.
    Adapter,
    /// LLM extraction.
    Llm,
}

/// Who/what extracted a record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtractorInfo {
    /// Extractor version (crate version + mapping version).
    pub version: String,
    /// Method.
    pub method: ExtractionMethod,
    /// Model, for LLM extraction.
    pub model: Option<ModelId>,
    /// blake3 hex of the prompt, for LLM extraction.
    pub prompt_hash: Option<String>,
    /// The extraction run (canary/rollout), if any.
    pub run_id: Option<ExtractionRunId>,
}

/// One point of an observation series (kinds with `storage = observation`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationPoint {
    /// Series key, e.g. `fx:EUR:USD`, `weather_normal:<geonameid>:07`.
    pub series_key: String,
    /// Observation time.
    pub observed_at: DateTime<Utc>,
    /// Value.
    pub value: serde_json::Value,
}

/// A record extracted from one capture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractedRecord {
    /// Kind.
    pub kind: KindId,
    /// Schema version of the kind.
    pub schema_version: u32,
    /// Stable per-page key.
    pub record_key: RecordKey,
    /// The validated data (JSON of the kind's schema).
    pub data: serde_json::Value,
    /// Headline / name.
    pub title: Option<String>,
    /// ≤ [`MAX_EXCERPT_CHARS`] characters of excerpt.
    pub excerpt: Option<String>,
    /// Language.
    pub lang: Option<String>,
    /// Resolved GeoNames ids (empty when unresolved).
    pub place_ids: Vec<i64>,
    /// Quality score in `[0, 1]`.
    pub quality: f32,
    /// Cross-source disagreement on a high-stakes field.
    pub conflict: bool,
    /// Use policy (meet of the capture's final policy and anything stricter).
    pub use_policy: UsePolicy,
    /// Extractor.
    pub extractor: ExtractorInfo,
    /// For observation kinds.
    pub observation: Option<ObservationPoint>,
}

/// Where records came from (one per capture).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provenance {
    /// Source.
    pub source_id: SourceId,
    /// Publisher class.
    pub source_class: SourceClass,
    /// Licence.
    pub licence: Licence,
    /// URL key.
    pub url_key: UrlKey,
    /// URL.
    pub url: CanonicalUrl,
    /// The capture.
    pub capture: CaptureRef,
    /// The capture's final use policy.
    pub use_policy: UsePolicy,
}
