//! Place resolution (spec §5.7 step 5). Implemented by `vc-extract` (`FilePlaceResolver`) and
//! used by `vc-api` (`/v1/places/resolve`).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A point on Earth.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GeoPoint {
    /// Latitude, degrees.
    pub lat: f64,
    /// Longitude, degrees.
    pub lon: f64,
}

/// A place lookup.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceQuery {
    /// Place name as written.
    pub name: String,
    /// ISO 3166-1 alpha-2 country hint.
    pub country: Option<String>,
    /// A nearby point, if known.
    pub near: Option<GeoPoint>,
    /// Language of `name`.
    pub lang: Option<String>,
}

/// A candidate place.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaceMatch {
    /// GeoNames id.
    pub geoname_id: i64,
    /// Canonical name.
    pub name: String,
    /// ISO 3166-1 alpha-2 country code.
    pub country: String,
    /// First-level admin region, if known.
    pub admin1: Option<String>,
    /// Location.
    pub point: GeoPoint,
    /// Confidence in `[0, 1]`.
    pub score: f32,
}

/// Resolves place names to GeoNames ids. LLM disambiguation only under budget; an unresolved
/// record keeps `place_ids = []`.
#[async_trait]
pub trait PlaceResolver: Send + Sync {
    /// Candidates, best first (empty when unresolved).
    async fn resolve(&self, q: &PlaceQuery) -> Result<Vec<PlaceMatch>>;
}
