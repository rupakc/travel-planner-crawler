//! A fake web behind the `PermitClient` contract: routes by canonical URL, takes a lease check +
//! rate token per request (through the frontier) and logs every request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use chrono::Utc;
use http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use vc_core::fetch::{
    Exchange, FetchError, FetchRequest, LimitedBodyStream, PermitClient, RedirectHop,
    RequestPurpose,
};
use vc_core::frontier::{ClaimPermit, Frontier};
use vc_core::{CanonicalUrl, USER_AGENT};

/// A canned response.
#[derive(Debug, Clone)]
pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
    /// A same-key redirect followed inline: the exchange reports this as its final URL.
    pub final_url: Option<CanonicalUrl>,
    pub error: Option<FetchError>,
}

/// One logged request.
#[derive(Debug, Clone)]
pub struct Logged {
    pub url: String,
    pub purpose: RequestPurpose,
    pub at: Instant,
    pub max_body_bytes: u64,
    pub accept: Option<String>,
}

/// The fake web.
pub struct FakeWeb {
    frontier: Arc<dyn Frontier>,
    routes: Mutex<HashMap<String, Resp>>,
    log: Mutex<Vec<Logged>>,
    latency: Duration,
}

fn canon(u: &str) -> String {
    CanonicalUrl::parse(u)
        .expect("valid url")
        .as_str()
        .to_string()
}

impl FakeWeb {
    pub fn new(frontier: Arc<dyn Frontier>) -> Self {
        FakeWeb {
            frontier,
            routes: Mutex::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            latency: Duration::from_millis(1),
        }
    }

    pub fn page(&self, url: &str, content_type: &str, body: &str) {
        self.respond(url, 200, &[("content-type", content_type)], body);
    }

    pub fn respond(&self, url: &str, status: u16, headers: &[(&str, &str)], body: &str) {
        self.routes.lock().unwrap().insert(
            canon(url),
            Resp {
                status,
                headers: headers
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
                body: Bytes::from(body.to_string()),
                final_url: None,
                error: None,
            },
        );
    }

    /// `url` redirects (inline, same key) to `target`, which serves `body` as HTML.
    pub fn redirect(&self, url: &str, target: &str, body: &str) {
        self.routes.lock().unwrap().insert(
            canon(url),
            Resp {
                status: 200,
                headers: vec![("content-type".into(), "text/html".into())],
                body: Bytes::from(body.to_string()),
                final_url: Some(CanonicalUrl::parse(target).unwrap()),
                error: None,
            },
        );
    }

    pub fn error(&self, url: &str, e: FetchError) {
        self.routes.lock().unwrap().insert(
            canon(url),
            Resp {
                status: 0,
                headers: Vec::new(),
                body: Bytes::new(),
                final_url: None,
                error: Some(e),
            },
        );
    }

    pub fn requests(&self) -> Vec<Logged> {
        self.log.lock().unwrap().clone()
    }

    pub fn urls(&self) -> Vec<String> {
        self.requests().into_iter().map(|l| l.url).collect()
    }

    pub fn count(&self, url: &str) -> usize {
        let c = canon(url);
        self.urls().iter().filter(|u| **u == c).count()
    }
}

#[async_trait]
impl PermitClient for FakeWeb {
    async fn fetch(&self, p: &ClaimPermit, req: FetchRequest) -> Result<Exchange, FetchError> {
        // Lease check + global rate token before every request, like vc-fetch.
        self.frontier.take_rate_token(p).await?;
        self.log.lock().unwrap().push(Logged {
            url: req.url.as_str().to_string(),
            purpose: req.purpose,
            at: Instant::now(),
            max_body_bytes: req.max_body_bytes,
            accept: req.accept.clone(),
        });
        let started_at = Utc::now();
        tokio::time::sleep(self.latency).await;
        let resp = self.routes.lock().unwrap().get(req.url.as_str()).cloned();
        let resp = resp.unwrap_or(Resp {
            status: 404,
            headers: vec![("content-type".into(), "text/plain".into())],
            body: Bytes::from_static(b"not found"),
            final_url: None,
            error: None,
        });
        p.record_request_end(Instant::now());
        if let Some(e) = resp.error {
            return Err(e);
        }
        if req.max_body_bytes > 0 && resp.body.len() as u64 > req.max_body_bytes {
            return Err(FetchError::BodyTooLarge {
                limit: req.max_body_bytes,
            });
        }
        let mut request_headers = HeaderMap::new();
        request_headers.insert(
            http::header::USER_AGENT,
            HeaderValue::from_static(USER_AGENT),
        );
        let mut response_headers = HeaderMap::new();
        for (k, v) in &resp.headers {
            response_headers.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        let (url, redirects) = match resp.final_url {
            Some(f) => (
                f,
                vec![RedirectHop {
                    url: req.url.clone(),
                    status: StatusCode::MOVED_PERMANENTLY,
                    peer_ip: None,
                }],
            ),
            None => (req.url.clone(), Vec::new()),
        };
        Ok(Exchange {
            url,
            redirects,
            method: Method::GET,
            request_headers,
            status: StatusCode::from_u16(resp.status).unwrap(),
            response_headers,
            body: resp.body,
            original_encoding: None,
            truncated: false,
            peer_ip: None,
            started_at,
            ended_at: Utc::now(),
            purpose: req.purpose,
        })
    }

    async fn stream(
        &self,
        _p: &ClaimPermit,
        _req: FetchRequest,
    ) -> Result<LimitedBodyStream, FetchError> {
        Err(FetchError::Transport(
            "stream not supported by FakeWeb".into(),
        ))
    }
}
