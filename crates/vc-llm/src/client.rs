//! The shared Vertex REST client: ADC bearer tokens, the 6 h billing breaker, error mapping.
//!
//! Every Vertex call (generate, embed, batch create/get/list/cancel) goes through
//! [`VertexClient::call`], which refuses with [`LlmError::Disabled`] before any network I/O when
//! `llm.enabled = false`, and with [`LlmError::BreakerOpen`] while the breaker is open. Request
//! and response bodies are never logged.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::{Method, Url};
use serde_json::Value;
use vc_config::LlmConfig;
use vc_core::llm::{LlmError, Purpose};
use vc_core::{Clock, Result};

/// The `app` label on every Vertex request and job.
pub const APP_LABEL: &str = "voyager-crawler";
/// OAuth scope for Vertex AI.
pub const CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";

/// The labels every request carries: `{app: voyager-crawler, purpose: <p>}`.
pub fn labels(purpose: Purpose) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert("app".to_string(), APP_LABEL.to_string());
    m.insert("purpose".to_string(), purpose.as_str().to_string());
    m
}

/// A source of OAuth access tokens for Vertex.
#[async_trait]
pub trait TokenSource: Send + Sync + fmt::Debug {
    /// A valid bearer token.
    async fn token(&self) -> std::result::Result<String, LlmError>;
}

/// A fixed token (tests, local development).
#[derive(Clone)]
pub struct StaticToken(pub String);

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken([censored])")
    }
}

#[async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> std::result::Result<String, LlmError> {
        Ok(self.0.clone())
    }
}

/// Application Default Credentials (the VM's service account via the metadata server), scope
/// `cloud-platform`. Built lazily on first use; google-cloud-auth caches and refreshes tokens.
#[derive(Default)]
pub struct AdcToken {
    creds: tokio::sync::OnceCell<google_cloud_auth::credentials::AccessTokenCredentials>,
}

impl AdcToken {
    /// A lazily initialised ADC token source.
    pub fn new() -> Self {
        AdcToken::default()
    }
}

impl fmt::Debug for AdcToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AdcToken")
    }
}

#[async_trait]
impl TokenSource for AdcToken {
    async fn token(&self) -> std::result::Result<String, LlmError> {
        let creds = self
            .creds
            .get_or_try_init(|| async {
                google_cloud_auth::credentials::Builder::default()
                    .with_scopes([CLOUD_PLATFORM_SCOPE])
                    .build_access_token_credentials()
                    .map_err(|e| LlmError::Transport(format!("adc credentials: {e}")))
            })
            .await?;
        creds
            .access_token()
            .await
            .map(|t| t.token)
            .map_err(|e| LlmError::Transport(format!("adc token: {e}")))
    }
}

/// The spend breaker: a Vertex 403/429 that names billing or budget opens it for `breaker_h`
/// hours; while open, no call reaches the network.
#[derive(Debug)]
pub struct Breaker {
    until: Mutex<Option<DateTime<Utc>>>,
    hours: u64,
    clock: Arc<dyn Clock>,
}

impl Breaker {
    /// A closed breaker that opens for `hours`.
    pub fn new(hours: u64, clock: Arc<dyn Clock>) -> Self {
        Breaker {
            until: Mutex::new(None),
            hours,
            clock,
        }
    }

    /// When the breaker closes, if it is open now.
    pub fn open_until(&self) -> Option<DateTime<Utc>> {
        let g = self.until.lock().expect("breaker lock");
        g.filter(|u| *u > self.clock.now())
    }

    /// `Err(BreakerOpen)` while open.
    pub fn check(&self) -> std::result::Result<(), LlmError> {
        match self.open_until() {
            Some(until) => Err(LlmError::BreakerOpen { until }),
            None => Ok(()),
        }
    }

    /// Opens the breaker now; returns when it closes.
    pub fn trip(&self) -> DateTime<Utc> {
        let until = self.clock.now() + chrono::Duration::hours(self.hours as i64);
        let mut g = self.until.lock().expect("breaker lock");
        let until = g.map_or(until, |u| u.max(until));
        *g = Some(until);
        until
    }
}

/// Whether an error body of a 403/429 names billing or budget (spend cap, billing disabled).
pub fn is_billing_error(status: u16, body: &str) -> bool {
    if status != 403 && status != 429 {
        return false;
    }
    let b = body.to_ascii_lowercase();
    b.contains("billing") || b.contains("budget")
}

struct Inner {
    http: reqwest::Client,
    tokens: Arc<dyn TokenSource>,
    breaker: Breaker,
    project: String,
    endpoint: String,
    enabled: bool,
}

/// A cheap-to-clone Vertex REST client shared by [`crate::VertexLlm`],
/// [`crate::VertexEmbedder`] and [`crate::VertexBatchRunner`] (one breaker for all).
#[derive(Clone)]
pub struct VertexClient {
    inner: Arc<Inner>,
}

impl fmt::Debug for VertexClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VertexClient")
            .field("project", &self.inner.project)
            .field("endpoint", &self.inner.endpoint)
            .field("enabled", &self.inner.enabled)
            .finish_non_exhaustive()
    }
}

impl VertexClient {
    /// A client for `config` (timeout `request_timeout_s`).
    pub fn new(
        config: &LlmConfig,
        tokens: Arc<dyn TokenSource>,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.request_timeout_s.max(1)))
            .build()
            .map_err(|e| LlmError::Transport(format!("http client: {e}")))?;
        Ok(VertexClient {
            inner: Arc::new(Inner {
                http,
                tokens,
                breaker: Breaker::new(config.breaker_h, clock),
                project: config.gcp_project.clone(),
                endpoint: config.api_endpoint.trim_end_matches('/').to_string(),
                enabled: config.enabled,
            }),
        })
    }

    /// The breaker.
    pub fn breaker(&self) -> &Breaker {
        &self.inner.breaker
    }

    /// Whether calls are enabled at all.
    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    /// `https://aiplatform.googleapis.com` for `global`, `https://{loc}-aiplatform.googleapis.com`
    /// otherwise (or the configured override).
    pub fn base_url(&self, location: &str) -> String {
        if !self.inner.endpoint.is_empty() {
            self.inner.endpoint.clone()
        } else if location == "global" {
            "https://aiplatform.googleapis.com".to_string()
        } else {
            format!("https://{location}-aiplatform.googleapis.com")
        }
    }

    /// `{base}/v1/projects/{project}/locations/{location}`.
    pub fn location_url(&self, location: &str) -> String {
        format!(
            "{}/v1/projects/{}/locations/{location}",
            self.base_url(location),
            self.inner.project
        )
    }

    /// `{location_url}/publishers/google/models/{model}:{method}`.
    pub fn model_url(&self, location: &str, model: &str, method: &str) -> String {
        format!(
            "{}/publishers/google/models/{model}:{method}",
            self.location_url(location)
        )
    }

    /// `{base}/v1/{resource name}` (e.g. a batch job name).
    pub fn resource_url(&self, location: &str, name: &str) -> String {
        format!("{}/v1/{name}", self.base_url(location))
    }

    /// One Vertex call. Refuses when disabled or while the breaker is open; a billing/budget
    /// 403/429 trips the breaker and is returned as [`LlmError::BreakerOpen`].
    pub async fn call(
        &self,
        method: Method,
        url: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> std::result::Result<Value, LlmError> {
        if !self.inner.enabled {
            return Err(LlmError::Disabled);
        }
        self.inner.breaker.check()?;
        let url = if query.is_empty() {
            Url::parse(url)
        } else {
            Url::parse_with_params(url, query)
        }
        .map_err(|e| LlmError::Transport(format!("bad url: {e}")))?;
        let token = self.inner.tokens.token().await?;
        let mut rb = self
            .inner
            .http
            .request(method, url)
            .bearer_auth(token)
            .header(reqwest::header::USER_AGENT, vc_core::USER_AGENT);
        if let Some(b) = body {
            rb = rb.json(b);
        }
        let started = Instant::now();
        let resp = rb.send().await.map_err(map_reqwest)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(map_reqwest)?;
        tracing::debug!(
            status,
            latency_ms = started.elapsed().as_millis() as u64,
            "vertex call"
        );
        if (200..300).contains(&status) {
            if text.trim().is_empty() {
                return Ok(Value::Object(Default::default()));
            }
            return serde_json::from_str(&text)
                .map_err(|e| LlmError::InvalidResponse(format!("vertex json: {e}")));
        }
        Err(self.map_status(status, &text))
    }

    fn map_status(&self, status: u16, body: &str) -> LlmError {
        if is_billing_error(status, body) {
            let until = self.inner.breaker.trip();
            tracing::warn!(status, %until, "vertex billing/budget refusal: breaker open");
            return LlmError::BreakerOpen { until };
        }
        let message = error_message(body);
        match status {
            429 => LlmError::RateLimited,
            404 => LlmError::ModelUnavailable(message),
            _ => LlmError::Http { status, message },
        }
    }
}

/// `error.message` of a Google error body, at most 300 characters (never request text).
fn error_message(body: &str) -> String {
    let msg = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();
    msg.chars().take(300).collect()
}

fn map_reqwest(e: reqwest::Error) -> LlmError {
    if e.is_timeout() {
        LlmError::Timeout
    } else {
        LlmError::Transport(e.without_url().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billing_detection() {
        assert!(is_billing_error(
            403,
            r#"{"error":{"message":"This API method requires billing to be enabled","status":"PERMISSION_DENIED"}}"#
        ));
        assert!(is_billing_error(429, "budget exceeded"));
        assert!(!is_billing_error(429, "Resource exhausted"));
        assert!(!is_billing_error(500, "billing"));
    }

    #[test]
    fn error_message_is_truncated() {
        let long = "x".repeat(1000);
        let body = format!(r#"{{"error":{{"message":"{long}"}}}}"#);
        assert_eq!(error_message(&body).len(), 300);
        assert_eq!(error_message("not json"), "");
    }
}
