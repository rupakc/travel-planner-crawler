//! Request metrics in the Prometheus text format, served on a separate localhost-only
//! listener (`metrics_bind`, default `127.0.0.1:9100`); never on the API listener. Labels are
//! route templates and status codes only (never URLs, ids or client tokens).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::extract::{ConnectInfo, MatchedPath, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Counters.
#[derive(Default)]
pub struct Metrics {
    requests: Mutex<BTreeMap<(String, u16), u64>>,
    pub(crate) auth_failures: AtomicU64,
    pub(crate) rate_limited: AtomicU64,
}

impl Metrics {
    fn record(&self, route: &str, status: u16) {
        *self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry((route.to_string(), status))
            .or_default() += 1;
    }

    /// The Prometheus text exposition.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str("# HELP vc_api_requests_total Requests by route template and status.\n");
        out.push_str("# TYPE vc_api_requests_total counter\n");
        let reqs = self
            .requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for ((route, status), n) in reqs.iter() {
            let _ = writeln!(
                out,
                "vc_api_requests_total{{route=\"{route}\",status=\"{status}\"}} {n}"
            );
        }
        let _ = writeln!(
            out,
            "# HELP vc_api_auth_failures_total Rejected bearer tokens.\n# TYPE vc_api_auth_failures_total counter\nvc_api_auth_failures_total {}",
            self.auth_failures.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# HELP vc_api_rate_limited_total Requests refused by the per-client rate limit.\n# TYPE vc_api_rate_limited_total counter\nvc_api_rate_limited_total {}",
            self.rate_limited.load(Ordering::Relaxed)
        );
        out
    }
}

/// Middleware counting every response by route template.
pub(crate) async fn track(
    State(m): State<std::sync::Arc<Metrics>>,
    req: Request,
    next: Next,
) -> Response {
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "unmatched".into());
    let resp = next.run(req).await;
    m.record(&route, resp.status().as_u16());
    resp
}

/// `GET /metrics`: loopback peers only.
pub(crate) async fn serve_metrics(
    State(m): State<std::sync::Arc<Metrics>>,
    req: Request,
) -> Response {
    let loopback = req
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|ci| ci.0.ip().is_loopback());
    if !loopback {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        m.render(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_counters() {
        let m = Metrics::default();
        m.record("/v1/search", 200);
        m.record("/v1/search", 200);
        m.rate_limited.fetch_add(1, Ordering::Relaxed);
        let s = m.render();
        assert!(s.contains("vc_api_requests_total{route=\"/v1/search\",status=\"200\"} 2"));
        assert!(s.contains("vc_api_rate_limited_total 1"));
        assert!(s.contains("vc_api_auth_failures_total 0"));
    }
}
