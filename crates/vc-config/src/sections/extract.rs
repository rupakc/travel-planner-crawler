//! `[extract]`: extraction pipeline and place resolution (owner: vc-extract).

use serde::{Deserialize, Serialize};

/// Extraction settings. LLM chunking limits (`max_input_tokens`, `max_chunks`,
/// `max_output_tokens`, `chars_per_token`) live in `[llm]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractConfig {
    /// Exported GeoNames subset used for place resolution (`crates/vc-extract/data/`).
    pub places_file: String,
    /// Country codes, names and folded names (country hints).
    pub countries_file: String,
    /// First-level admin codes and names.
    pub admin1_file: String,
    /// Schema-repair attempts after a validation failure.
    pub repair_attempts: u32,
    /// Excerpt length, characters (≤ 300).
    pub excerpt_chars: usize,
    /// Records below this quality are dropped.
    pub min_quality: f32,
    /// Canary size for a new extraction run.
    pub canary_records: u32,
    /// Max records accepted from one LLM call (the rest are ignored).
    pub max_records_per_call: usize,
    /// Input list price used for reservation estimates, USD per 1M tokens.
    pub est_input_usd_per_mtok: f64,
    /// Output list price used for reservation estimates, USD per 1M tokens.
    pub est_output_usd_per_mtok: f64,
    /// A place match below this score leaves `place_ids` empty.
    pub place_min_score: f32,
    /// Top-two score gap under which LLM place disambiguation is tried (under budget).
    pub place_ambiguity_margin: f32,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        ExtractConfig {
            places_file: "data/places.tsv.gz".into(),
            countries_file: "data/countries.tsv.gz".into(),
            admin1_file: "data/admin1.tsv.gz".into(),
            repair_attempts: 1,
            excerpt_chars: 300,
            min_quality: 0.3,
            canary_records: 200,
            max_records_per_call: 20,
            est_input_usd_per_mtok: 0.25,
            est_output_usd_per_mtok: 1.50,
            place_min_score: 0.5,
            place_ambiguity_margin: 0.15,
        }
    }
}
