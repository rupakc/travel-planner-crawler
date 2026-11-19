//! # vc-api (Task 12)
//!
//! The internal `/v1` read API (spec §5.11, §17.5, §17.9; contract `docs/Crawler-API.md`):
//!
//! * axum router over the vc-core [`IndexReader`] and [`PlaceResolver`];
//! * per-client bearer tokens (`<client_id>.<secret>`, argon2id-hashed in `api_clients`) and a
//!   per-client token-bucket rate limit;
//! * licence shaping on every item ([`shape`]): per-client licence classes, use policy, and
//!   `facts_only` text capped at the 300-character excerpt;
//! * OpenAPI 3.1 ([`openapi`], served at `/v1/openapi.json`; an insta snapshot guards it);
//! * `/v1/changes` with the `(xid, seq)` cursor;
//! * TLS from secrets and a localhost-only `/metrics` listener ([`tls::serve`]).
//!
//! Block 16xx (no migration needed: `api_clients` and `changes` are in `0010_api`).

pub mod auth;
pub mod catalog;
pub mod dto;
pub mod error;
mod handlers;
pub mod metrics;
pub mod pg;
mod ratelimit;
pub mod shape;
pub mod tls;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use utoipa::OpenApi;
use utoipa::openapi::security::{Http, HttpAuthScheme, SecurityScheme};
use vc_config::ApiConfig;
use vc_core::extract::PlaceResolver;
use vc_core::index::IndexReader;

use crate::auth::{Authenticator, ClientStore};
use crate::catalog::Catalog;
use crate::dto::StatsDto;
use crate::error::ApiError;
use crate::metrics::Metrics;
use crate::ratelimit::RateLimiter;

/// How long `/v1/stats` answers are reused.
const STATS_TTL: Duration = Duration::from_secs(60);

struct Internals {
    auth: Authenticator,
    limiter: RateLimiter,
    metrics: Arc<Metrics>,
    stats: tokio::sync::Mutex<Option<(Instant, StatsDto)>>,
}

/// The process health `/healthz` reports (`true` = healthy); see [`ApiState::with_health`].
pub type HealthProbe = Arc<dyn Fn() -> bool + Send + Sync>;

/// Shared state of the API.
#[derive(Clone)]
pub struct ApiState {
    /// Settings.
    pub config: ApiConfig,
    /// The read model.
    pub index: Arc<dyn IndexReader>,
    /// Place resolution.
    pub places: Arc<dyn PlaceResolver>,
    /// API clients.
    pub clients: Arc<dyn ClientStore>,
    /// Sources, stats, readiness.
    pub catalog: Arc<dyn Catalog>,
    inner: Arc<Internals>,
    health: Option<HealthProbe>,
}

impl ApiState {
    /// Builds the state.
    pub fn new(
        config: ApiConfig,
        index: Arc<dyn IndexReader>,
        places: Arc<dyn PlaceResolver>,
        clients: Arc<dyn ClientStore>,
        catalog: Arc<dyn Catalog>,
    ) -> Self {
        ApiState {
            config,
            index,
            places,
            clients,
            catalog,
            inner: Arc::new(Internals {
                auth: Authenticator::new(),
                limiter: RateLimiter::default(),
                metrics: Arc::new(Metrics::default()),
                stats: tokio::sync::Mutex::new(None),
            }),
            health: None,
        }
    }

    /// `/healthz` answers 503 while `probe` returns `false` (the process hosting the API is
    /// unhealthy; e.g. `vcrawl run` with a role down or stalled). Without a probe it is pure
    /// liveness.
    pub fn with_health(mut self, probe: HealthProbe) -> Self {
        self.health = Some(probe);
        self
    }

    /// Whether the health probe (if any) passes.
    pub fn healthy(&self) -> bool {
        self.health.as_ref().is_none_or(|h| h())
    }

    /// The request counters.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.inner.metrics.clone()
    }

    async fn cached_stats(&self) -> Result<StatsDto, ApiError> {
        let mut slot = self.inner.stats.lock().await;
        if let Some((at, s)) = slot.as_ref()
            && at.elapsed() < STATS_TTL
        {
            return Ok(s.clone());
        }
        let s = self.catalog.stats().await?;
        *slot = Some((Instant::now(), s.clone()));
        Ok(s)
    }
}

/// Authenticates, rate-limits and attaches the [`auth::Client`].
async fn require_client(State(state): State<ApiState>, mut req: Request, next: Next) -> Response {
    let client = match state
        .inner
        .auth
        .authenticate(state.clients.as_ref(), req.headers())
        .await
    {
        Ok(c) => c,
        Err(e) => {
            if e == ApiError::Unauthorized {
                state
                    .inner
                    .metrics
                    .auth_failures
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            return e.into_response();
        }
    };
    if let Err(retry_after) = state
        .inner
        .limiter
        .check(&client.client_id, client.rate_per_min)
    {
        state
            .inner
            .metrics
            .rate_limited
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        return ApiError::RateLimited { retry_after }.into_response();
    }
    req.extensions_mut().insert(client);
    next.run(req).await
}

/// Builds the API router (serve it with `ConnectInfo<SocketAddr>`). `/metrics` is **not** on
/// it: see [`metrics_router`].
pub fn router(state: ApiState) -> Router {
    let protected = Router::new()
        .route("/v1/search", get(handlers::search))
        .route("/v1/entities", get(handlers::entities))
        .route("/v1/entities/{id}", get(handlers::entity))
        .route("/v1/records/{id}/versions", get(handlers::record_versions))
        .route("/v1/articles", get(handlers::articles))
        .route("/v1/observations", get(handlers::observations))
        .route("/v1/carbon/factors", get(handlers::carbon_factors))
        .route("/v1/carbon/grid", get(handlers::carbon_grid))
        .route("/v1/places/resolve", get(handlers::places_resolve))
        .route("/v1/changes", get(handlers::changes))
        .route("/v1/sources", get(handlers::sources))
        .route("/v1/stats", get(handlers::stats))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_client,
        ));
    let public = Router::new()
        .route("/healthz", get(handlers::healthz))
        .route("/readyz", get(handlers::readyz))
        .route("/v1/openapi.json", get(handlers::openapi_json));
    let timeout = Duration::from_secs(state.config.request_timeout_s.max(1));
    let metrics = state.inner.metrics.clone();
    protected
        .merge(public)
        .fallback(|| async { ApiError::NotFound.into_response() })
        .layer(middleware::from_fn_with_state(metrics, metrics::track))
        .layer(TimeoutLayer::with_status_code(
            StatusCode::SERVICE_UNAVAILABLE,
            timeout,
        ))
        .layer(RequestBodyLimitLayer::new(16 * 1024))
        .with_state(state)
}

/// The `/metrics` router for the localhost-only listener (loopback peers only).
pub fn metrics_router(state: &ApiState) -> Router {
    Router::new()
        .route("/metrics", get(metrics::serve_metrics))
        .with_state(state.inner.metrics.clone())
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Voyager Crawler read API",
        description = "Private, internal-only read API of the Voyager crawler. Frozen /v1 contract: additive changes only (Crawler-API.md).",
        license(name = "MIT", identifier = "MIT"),
    ),
    paths(
        handlers::search,
        handlers::entities,
        handlers::entity,
        handlers::record_versions,
        handlers::articles,
        handlers::observations,
        handlers::carbon_factors,
        handlers::carbon_grid,
        handlers::places_resolve,
        handlers::changes,
        handlers::sources,
        handlers::stats,
        handlers::healthz,
        handlers::readyz,
        handlers::openapi_json,
    ),
    modifiers(&BearerScheme),
    tags(
        (name = "read", description = "Entities, records, articles, observations, places"),
        (name = "sync", description = "Change feed"),
        (name = "meta", description = "Sources, statistics, this document"),
        (name = "probe", description = "Health probes (no auth)"),
    )
)]
struct ApiDoc;

struct BearerScheme;

impl utoipa::Modify for BearerScheme {
    fn modify(&self, doc: &mut utoipa::openapi::OpenApi) {
        let comps = doc.components.get_or_insert_with(Default::default);
        comps.add_security_scheme(
            "bearer",
            SecurityScheme::Http(Http::new(HttpAuthScheme::Bearer)),
        );
    }
}

/// The OpenAPI document of `/v1` (`info.version` is the contract version, `1`).
pub fn openapi() -> utoipa::openapi::OpenApi {
    let mut doc = ApiDoc::openapi();
    doc.info.version = "1".into();
    doc
}
