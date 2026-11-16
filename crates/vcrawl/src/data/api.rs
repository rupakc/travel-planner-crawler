//! The `/v1` API state over Postgres: [`PgIndex`] as the read model (with the budgeted query
//! embedder when the LLM is enabled), [`PgApiStore`] as clients **and** catalog, and the
//! GeoNames place resolver.

use std::sync::Arc;

use sqlx::PgPool;
use vc_api::ApiState;
use vc_api::pg::PgApiStore;
use vc_config::{ApiConfig, IndexConfig};
use vc_core::extract::PlaceResolver;
use vc_index::{PgIndex, QueryEmbedder};

/// Builds the API state. `query_embedder = None` keeps search text-only.
pub fn state(
    pool: PgPool,
    api: ApiConfig,
    index: IndexConfig,
    places: Arc<dyn PlaceResolver>,
    query_embedder: Option<Arc<dyn QueryEmbedder>>,
) -> ApiState {
    let mut ix = PgIndex::with_config(pool.clone(), index);
    if let Some(e) = query_embedder {
        ix = ix.with_query_embedder(e);
    }
    let store = Arc::new(PgApiStore::new(pool));
    ApiState::new(api, Arc::new(ix), places, store.clone(), store)
}
