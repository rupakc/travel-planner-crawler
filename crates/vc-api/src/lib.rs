//! # vc-api (Task 12, stub)
//!
//! The internal `/v1` read API (axum, TLS, per-client tokens, OpenAPI). Block 16xx.

use std::sync::Arc;

use vc_config::ApiConfig;
use vc_core::extract::PlaceResolver;
use vc_core::index::IndexReader;

/// Shared state of the API.
#[derive(Clone)]
pub struct ApiState {
    /// Settings.
    pub config: ApiConfig,
    /// The read model.
    pub index: Arc<dyn IndexReader>,
    /// Place resolution.
    pub places: Arc<dyn PlaceResolver>,
}

/// Builds the router.
pub fn router(_state: ApiState) -> axum::Router {
    todo!("Task 12")
}
