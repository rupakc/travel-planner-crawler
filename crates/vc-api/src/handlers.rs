//! `/v1` handlers.

use axum::Json;
use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Extension, Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use utoipa::IntoParams;
use vc_core::extract::{GeoPoint, PlaceQuery};
use vc_core::ids::{EntityId, KindId, RecordId};
use vc_core::index::{ChangeCursor, EntityLookup, SearchQuery};

use crate::ApiState;
use crate::auth::Client;
use crate::dto::{
    ChangeDto, ChangesDto, ClaimDto, EntityDto, ItemPage, ObservationDto, ObservationList,
    PlaceDto, PlaceList, RedirectDto, SourceList, StatsDto, StatusDto, VersionPage,
};
use crate::error::{ApiError, ErrorBody};
use crate::shape::Shaper;

type ApiResult<T> = Result<T, ApiError>;

/// Default page size.
pub const DEFAULT_LIMIT: u32 = 20;

fn query<T>(q: Result<Query<T>, QueryRejection>) -> ApiResult<T> {
    q.map(|Query(t)| t)
        .map_err(|e| ApiError::BadRequest(e.body_text()))
}

fn limit(state: &ApiState, l: Option<u32>) -> u32 {
    l.unwrap_or(DEFAULT_LIMIT)
        .clamp(1, state.config.max_page.max(1))
}

fn shaper(state: &ApiState, c: &Client) -> Shaper {
    Shaper::new(&c.licence_classes, state.config.excerpt_chars)
}

fn kinds(s: Option<&str>) -> Vec<KindId> {
    s.map(|s| {
        s.split(',')
            .map(str::trim)
            .filter(|k| !k.is_empty())
            .map(KindId::new)
            .collect()
    })
    .unwrap_or_default()
}

/// Search filters shared by `/v1/search`, `/v1/entities` and `/v1/articles`.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct SearchParams {
    /// Free text.
    pub q: Option<String>,
    /// Exact GeoNames id.
    pub place: Option<i64>,
    /// Comma-separated kinds (ignored by `/v1/articles`, which is always `article`).
    pub kind: Option<String>,
    /// Radius search: latitude (with `lon` and `radius_km`).
    pub lat: Option<f64>,
    /// Radius search: longitude.
    pub lon: Option<f64>,
    /// Radius search: radius in km.
    pub radius_km: Option<f64>,
    /// Only items updated since (RFC 3339).
    pub updated_since: Option<DateTime<Utc>>,
    /// Page size (default 20, max `max_page`).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
}

async fn run_search(
    state: &ApiState,
    client: &Client,
    p: SearchParams,
    force_kind: Option<&str>,
) -> ApiResult<ItemPage> {
    let near = match (p.lat, p.lon, p.radius_km) {
        (None, None, None) => None,
        (Some(lat), Some(lon), Some(r))
            if (-90.0..=90.0).contains(&lat)
                && (-180.0..=180.0).contains(&lon)
                && r > 0.0
                && r <= 20_000.0 =>
        {
            Some((GeoPoint { lat, lon }, r))
        }
        _ => {
            return Err(ApiError::BadRequest(
                "lat, lon and radius_km must be given together and in range".into(),
            ));
        }
    };
    let shaper = shaper(state, client);
    let q = SearchQuery {
        q: p.q.filter(|s| !s.trim().is_empty()),
        place_id: p.place,
        near,
        kinds: match force_kind {
            Some(k) => vec![KindId::new(k)],
            None => kinds(p.kind.as_deref()),
        },
        licence_classes: shaper.allowed().to_vec(),
        updated_since: p.updated_since,
        limit: limit(state, p.limit),
        cursor: p.cursor,
    };
    let page = state.index.search(&q).await?;
    Ok(ItemPage {
        items: page.items.iter().filter_map(|i| shaper.item(i)).collect(),
        next_cursor: page.next_cursor,
    })
}

/// Hybrid search over entities and records.
#[utoipa::path(get, path = "/v1/search", tag = "read", params(SearchParams),
    security(("bearer" = [])),
    responses((status = 200, body = ItemPage), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn search(
    State(state): State<ApiState>,
    Extension(client): Extension<Client>,
    p: Result<Query<SearchParams>, QueryRejection>,
) -> ApiResult<Json<ItemPage>> {
    Ok(Json(run_search(&state, &client, query(p)?, None).await?))
}

/// Entities by place, kind and text.
#[utoipa::path(get, path = "/v1/entities", tag = "read", params(SearchParams),
    security(("bearer" = [])),
    responses((status = 200, body = ItemPage), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn entities(
    State(state): State<ApiState>,
    Extension(client): Extension<Client>,
    p: Result<Query<SearchParams>, QueryRejection>,
) -> ApiResult<Json<ItemPage>> {
    Ok(Json(run_search(&state, &client, query(p)?, None).await?))
}

/// Articles (headline + excerpt + facts + link).
#[utoipa::path(get, path = "/v1/articles", tag = "read", params(SearchParams),
    security(("bearer" = [])),
    responses((status = 200, body = ItemPage), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn articles(
    State(state): State<ApiState>,
    Extension(client): Extension<Client>,
    p: Result<Query<SearchParams>, QueryRejection>,
) -> ApiResult<Json<ItemPage>> {
    Ok(Json(
        run_search(&state, &client, query(p)?, Some("article")).await?,
    ))
}

/// One entity with its claims; a merged entity answers `301` to its successor.
#[utoipa::path(get, path = "/v1/entities/{id}", tag = "read",
    params(("id" = String, Path, description = "Entity id (ULID)")),
    security(("bearer" = [])),
    responses((status = 200, body = EntityDto),
        (status = 301, body = RedirectDto, description = "Merged; `Location` names the successor"),
        (status = 400, body = ErrorBody), (status = 401, body = ErrorBody),
        (status = 404, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn entity(
    State(state): State<ApiState>,
    Extension(client): Extension<Client>,
    id: Result<Path<String>, PathRejection>,
) -> ApiResult<Response> {
    let Path(id) = id.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    let id: EntityId = id
        .parse()
        .map_err(|_| ApiError::BadRequest("entity id must be a ULID".into()))?;
    match state.index.entity(&id).await? {
        EntityLookup::Found(view) => {
            let item = shaper(&state, &client)
                .item(&view.item)
                .ok_or(ApiError::NotFound)?;
            let claims = view.claims.iter().map(ClaimDto::from).collect();
            Ok(Json(EntityDto { item, claims }).into_response())
        }
        EntityLookup::Redirect(to) => {
            let location = format!("/v1/entities/{to}");
            let mut resp = (
                StatusCode::MOVED_PERMANENTLY,
                Json(RedirectDto {
                    redirect_to: to.to_string(),
                    location: location.clone(),
                }),
            )
                .into_response();
            let hv = HeaderValue::from_str(&location).map_err(|_| ApiError::Internal)?;
            resp.headers_mut().insert(header::LOCATION, hv);
            Ok(resp)
        }
        EntityLookup::NotFound => Err(ApiError::NotFound),
    }
}

/// Paging parameters.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PageParams {
    /// Page size (default 20, max `max_page`).
    pub limit: Option<u32>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
}

/// Versions of a record, newest first.
#[utoipa::path(get, path = "/v1/records/{id}/versions", tag = "read",
    params(("id" = i64, Path, description = "Record id"), PageParams),
    security(("bearer" = [])),
    responses((status = 200, body = VersionPage), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn record_versions(
    State(state): State<ApiState>,
    Extension(client): Extension<Client>,
    id: Result<Path<i64>, PathRejection>,
    p: Result<Query<PageParams>, QueryRejection>,
) -> ApiResult<Json<VersionPage>> {
    let Path(id) = id.map_err(|e| ApiError::BadRequest(e.body_text()))?;
    let p = query(p)?;
    let page = state
        .index
        .record_versions(RecordId(id), limit(&state, p.limit), p.cursor)
        .await?;
    let shaper = shaper(&state, &client);
    Ok(Json(VersionPage {
        items: page
            .items
            .iter()
            .filter_map(|v| shaper.version(v))
            .collect(),
        next_cursor: page.next_cursor,
    }))
}

/// Change-feed parameters.
#[derive(Debug, Default, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ChangesParams {
    /// `next` of the previous page (absent = from the start).
    pub since: Option<String>,
    /// Comma-separated kinds (absent = all).
    pub kinds: Option<String>,
    /// Page size (default 20, max `max_page`).
    pub limit: Option<u32>,
}

/// The change feed. Only committed transactions older than every open one are served, so a
/// client that pages with `since = next` never skips a change.
#[utoipa::path(get, path = "/v1/changes", tag = "sync", params(ChangesParams),
    security(("bearer" = [])),
    responses((status = 200, body = ChangesDto), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn changes(
    State(state): State<ApiState>,
    Extension(_client): Extension<Client>,
    p: Result<Query<ChangesParams>, QueryRejection>,
) -> ApiResult<Json<ChangesDto>> {
    let p = query(p)?;
    let since: Option<ChangeCursor> = match p.since.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => Some(
            s.parse()
                .map_err(|_| ApiError::BadRequest("since must be a cursor <xid>-<seq>".into()))?,
        ),
        None => None,
    };
    let page = state
        .index
        .changes(since, &kinds(p.kinds.as_deref()), limit(&state, p.limit))
        .await?;
    Ok(Json(ChangesDto {
        changes: page.changes.iter().map(ChangeDto::from).collect(),
        next: page.next.or(since).map(|c| c.to_string()),
    }))
}

/// Observation parameters.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ObservationParams {
    /// Series key (e.g. `fx:USD:EUR`).
    pub series: String,
    /// From (inclusive, RFC 3339; default `to` − 30 days).
    pub from: Option<DateTime<Utc>>,
    /// To (exclusive, RFC 3339; default now).
    pub to: Option<DateTime<Utc>>,
    /// Max points (default 20, max `max_page`).
    pub limit: Option<u32>,
}

/// Points of an observation series in `[from, to)`.
#[utoipa::path(get, path = "/v1/observations", tag = "read", params(ObservationParams),
    security(("bearer" = [])),
    responses((status = 200, body = ObservationList), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn observations(
    State(state): State<ApiState>,
    Extension(client): Extension<Client>,
    p: Result<Query<ObservationParams>, QueryRejection>,
) -> ApiResult<Json<ObservationList>> {
    let p = query(p)?;
    let to = p.to.unwrap_or_else(Utc::now);
    let from = p.from.unwrap_or(to - Duration::days(30));
    if from >= to {
        return Err(ApiError::BadRequest("from must be before to".into()));
    }
    let shaper = shaper(&state, &client);
    let obs = state
        .index
        .observations(&p.series, from, to, limit(&state, p.limit))
        .await?;
    Ok(Json(ObservationList {
        items: obs
            .iter()
            .filter(|o| shaper.permits(o.licence.class))
            .filter_map(ObservationDto::from_core)
            .collect(),
    }))
}

/// Place-resolution parameters.
#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct PlaceParams {
    /// Place name as written.
    pub name: String,
    /// ISO 3166-1 alpha-2 country hint.
    pub country: Option<String>,
    /// Nearby latitude (with `lon`).
    pub lat: Option<f64>,
    /// Nearby longitude (with `lat`).
    pub lon: Option<f64>,
    /// Language of `name`.
    pub lang: Option<String>,
}

/// Resolves a place name to GeoNames candidates, best first.
#[utoipa::path(get, path = "/v1/places/resolve", tag = "read", params(PlaceParams),
    security(("bearer" = [])),
    responses((status = 200, body = PlaceList), (status = 400, body = ErrorBody),
        (status = 401, body = ErrorBody), (status = 429, body = ErrorBody)))]
pub async fn places_resolve(
    State(state): State<ApiState>,
    Extension(_client): Extension<Client>,
    p: Result<Query<PlaceParams>, QueryRejection>,
) -> ApiResult<Json<PlaceList>> {
    let p = query(p)?;
    if p.name.trim().is_empty() || p.name.chars().count() > 200 {
        return Err(ApiError::BadRequest("name must be 1-200 characters".into()));
    }
    let near = match (p.lat, p.lon) {
        (None, None) => None,
        (Some(lat), Some(lon)) => Some(GeoPoint { lat, lon }),
        _ => return Err(ApiError::BadRequest("lat and lon go together".into())),
    };
    let matches = state
        .places
        .resolve(&PlaceQuery {
            name: p.name,
            country: p.country,
            near,
            lang: p.lang,
        })
        .await?;
    Ok(Json(PlaceList {
        items: matches.iter().map(PlaceDto::from).collect(),
    }))
}

/// The configured sources and their licences.
#[utoipa::path(get, path = "/v1/sources", tag = "meta",
    security(("bearer" = [])),
    responses((status = 200, body = SourceList), (status = 401, body = ErrorBody),
        (status = 429, body = ErrorBody)))]
pub async fn sources(
    State(state): State<ApiState>,
    Extension(_client): Extension<Client>,
) -> ApiResult<Json<SourceList>> {
    Ok(Json(SourceList {
        items: state.catalog.sources().await?,
    }))
}

/// Index statistics (cached up to 60 s).
#[utoipa::path(get, path = "/v1/stats", tag = "meta",
    security(("bearer" = [])),
    responses((status = 200, body = StatsDto), (status = 401, body = ErrorBody),
        (status = 429, body = ErrorBody)))]
pub async fn stats(
    State(state): State<ApiState>,
    Extension(_client): Extension<Client>,
) -> ApiResult<Json<StatsDto>> {
    Ok(Json(state.cached_stats().await?))
}

/// Liveness; 503 while the hosting process reports itself unhealthy (`ApiState::with_health`).
#[utoipa::path(get, path = "/healthz", tag = "probe",
    responses((status = 200, body = StatusDto), (status = 503, body = StatusDto)))]
pub async fn healthz(State(state): State<ApiState>) -> (StatusCode, Json<StatusDto>) {
    if state.healthy() {
        (
            StatusCode::OK,
            Json(StatusDto {
                status: "ok".into(),
            }),
        )
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(StatusDto {
                status: "unhealthy".into(),
            }),
        )
    }
}

/// Readiness (the database answers).
#[utoipa::path(get, path = "/readyz", tag = "probe",
    responses((status = 200, body = StatusDto), (status = 503, body = ErrorBody)))]
pub async fn readyz(State(state): State<ApiState>) -> ApiResult<Json<StatusDto>> {
    if state.catalog.ready().await.is_err() || state.clients.ping().await.is_err() {
        return Err(ApiError::Unavailable);
    }
    Ok(Json(StatusDto {
        status: "ready".into(),
    }))
}

/// This document.
#[utoipa::path(get, path = "/v1/openapi.json", tag = "meta",
    responses((status = 200, description = "The OpenAPI 3.1 document")))]
pub async fn openapi_json() -> Response {
    Json(crate::openapi()).into_response()
}
