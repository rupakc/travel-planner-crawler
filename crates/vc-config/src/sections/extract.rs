//! `[extract]`: extraction pipeline and place resolution (owner: vc-extract).

use serde::{Deserialize, Serialize};

/// Extraction settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExtractConfig {
    /// Exported GeoNames subset used for place resolution.
    pub places_file: String,
    /// Schema-repair attempts after a validation failure.
    pub repair_attempts: u32,
    /// Excerpt length, characters (≤ 300).
    pub excerpt_chars: usize,
    /// Records below this quality are dropped.
    pub min_quality: f32,
    /// Canary size for a new extraction run.
    pub canary_records: u32,
}

impl Default for ExtractConfig {
    fn default() -> Self {
        ExtractConfig {
            places_file: "data/places.tsv.gz".into(),
            repair_attempts: 1,
            excerpt_chars: 300,
            min_quality: 0.3,
            canary_records: 200,
        }
    }
}
