//! # vc-extract (Task 9, stub)
//!
//! The extraction pipeline (structured first, LLM second, schema validation + one repair,
//! verbatim/grounding checks, source-class gate) and `FilePlaceResolver`.

use async_trait::async_trait;
use vc_config::ExtractConfig;
use vc_core::Result;
use vc_core::extract::{PlaceMatch, PlaceQuery, PlaceResolver};

/// Resolves places from the exported GeoNames subset.
#[derive(Debug, Clone)]
pub struct FilePlaceResolver {
    /// Settings.
    pub config: ExtractConfig,
}

#[async_trait]
impl PlaceResolver for FilePlaceResolver {
    async fn resolve(&self, _q: &PlaceQuery) -> Result<Vec<PlaceMatch>> {
        todo!("Task 9")
    }
}
