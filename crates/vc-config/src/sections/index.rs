//! `[index]`: records, embeddings, hybrid query (owner: vc-index).

use serde::{Deserialize, Serialize};

/// Index settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// HNSW `m`.
    pub hnsw_m: u32,
    /// HNSW `ef_construction`.
    pub hnsw_ef_construction: u32,
    /// HNSW `ef_search`.
    pub hnsw_ef_search: u32,
    /// Reciprocal rank fusion constant.
    pub rrf_k: u32,
    /// Freshness half-life, days.
    pub freshness_half_life_days: u32,
    /// Entity merge: min trigram similarity of normalised names.
    pub merge_min_trigram: f32,
    /// Entity merge: max distance, metres.
    pub merge_max_distance_m: f64,
    /// A record is `stale` when unseen for this multiple of its cadence.
    pub stale_cadence_factor: u32,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig {
            hnsw_m: 16,
            hnsw_ef_construction: 64,
            hnsw_ef_search: 40,
            rrf_k: 60,
            freshness_half_life_days: 90,
            merge_min_trigram: 0.8,
            merge_max_distance_m: 300.0,
            stale_cadence_factor: 3,
        }
    }
}
