//! **fake-web**: a scriptable "mini internet" (spec §12) served by one axum server.
//!
//! * Sites are chosen by the `Host` header, so one listener serves every site. In-process tests
//!   use [`FakeWebServer::client`] (a reqwest client that resolves every site host to the
//!   listener); E2E points the sites' DNS names at the `testkit-fake-web` container.
//! * Every route has one scripted [`Reply`] per **cycle** (the last one repeats), so content,
//!   robots.txt and status can change between crawl cycles ([`FakeWeb::set_cycle`],
//!   `POST /__admin/cycle`).
//! * Every request to a site (known or not) is appended to the frozen [`RequestLog`]. The fake
//!   web also remembers which robots.txt each origin was served, and which responses were
//!   challenges or carried `Retry-After`, so [`FakeWeb::policy_snapshot`] can hand the
//!   politeness oracle exactly what the crawler saw.
//! * Admin endpoints (never logged): `GET /__admin/log` (JSONL), `GET /__admin/snapshot`
//!   (JSON [`PolicySnapshot`]), `GET|POST /__admin/cycle` (`{"cycle": n}`; an empty POST moves
//!   to the next cycle), `POST /__admin/reset`, `GET /__admin/health`.
//!
//! The seven scripted sites live in [`sites`].

pub mod sites;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use bytes::Bytes;
use chrono::{Duration as ChronoDuration, Utc};
use http::{HeaderName, HeaderValue, Method, StatusCode, header};
use vc_core::PolitenessKey;

use crate::oracle::{Backoff, KeyRules, PolicySnapshot, RobotsServed};
use crate::request_log::{RequestLog, RequestLogEntry};
use crate::server::FakeServer;

/// One scripted HTTP response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    /// Status code.
    pub status: u16,
    /// Extra headers (`Content-Type` included).
    pub headers: Vec<(String, String)>,
    /// Body.
    pub body: Bytes,
    /// The response is a bot challenge (the oracle parks the key after it).
    pub challenge: bool,
    /// Added latency before the response is sent (on top of the site latency).
    pub latency: Duration,
    /// Never answer: the request hangs until the client gives up (a timeout, as the crawler
    /// sees it). Logged with status 0 when the client drops the connection.
    pub no_response: bool,
}

impl Reply {
    /// A response with a status, a content type and a body.
    pub fn new(status: u16, content_type: &str, body: impl Into<Bytes>) -> Reply {
        Reply {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
            challenge: false,
            latency: Duration::ZERO,
            no_response: false,
        }
    }

    /// No response at all: the server holds the connection and never sends a status line.
    pub fn no_response() -> Reply {
        let mut r = Reply::status(0);
        r.no_response = true;
        r
    }

    /// `200 text/html`.
    pub fn html(body: impl Into<String>) -> Reply {
        Reply::new(200, "text/html; charset=utf-8", body.into())
    }

    /// `200 text/plain`.
    pub fn text(body: impl Into<String>) -> Reply {
        Reply::new(200, "text/plain; charset=utf-8", body.into())
    }

    /// `200 application/xml`.
    pub fn xml(body: impl Into<String>) -> Reply {
        Reply::new(200, "application/xml; charset=utf-8", body.into())
    }

    /// `200 application/rss+xml`.
    pub fn rss(body: impl Into<String>) -> Reply {
        Reply::new(200, "application/rss+xml; charset=utf-8", body.into())
    }

    /// `200 application/json`.
    pub fn json(v: &serde_json::Value) -> Reply {
        Reply::new(200, "application/json", v.to_string())
    }

    /// `200 application/javascript`.
    pub fn js(body: impl Into<String>) -> Reply {
        Reply::new(200, "application/javascript", body.into())
    }

    /// `200 text/css`.
    pub fn css(body: impl Into<String>) -> Reply {
        Reply::new(200, "text/css", body.into())
    }

    /// An empty response with a status.
    pub fn status(status: u16) -> Reply {
        Reply::new(status, "text/plain; charset=utf-8", Bytes::new())
    }

    /// A redirect (`301`, `302`, `307`, `308`) to `location`.
    pub fn redirect(status: u16, location: &str) -> Reply {
        Reply::status(status).header("location", location)
    }

    /// A Cloudflare-style bot challenge: `403`, `cf-mitigated: challenge`, an interstitial body.
    pub fn challenge() -> Reply {
        let mut r = Reply::new(403, "text/html; charset=UTF-8", sites::CHALLENGE_HTML)
            .header("cf-mitigated", "challenge")
            .header("server", "cloudflare");
        r.challenge = true;
        r
    }

    /// Adds a header.
    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.to_ascii_lowercase(), v.to_string()));
        self
    }

    /// Adds latency.
    pub fn with_latency(mut self, d: Duration) -> Reply {
        self.latency = d;
        self
    }

    /// Marks the reply as a challenge (e.g. a `200` captcha page).
    pub fn as_challenge(mut self) -> Reply {
        self.challenge = true;
        self
    }

    fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// A route's replies by cycle: index = cycle, the last entry repeats; `None` = 404.
pub type Script = Vec<Option<Reply>>;

/// One scripted site.
#[derive(Debug, Clone)]
pub struct Site {
    /// Short name (`blog`, `spa`, …).
    pub name: String,
    /// Host name (lowercase, no port).
    pub host: String,
    /// Latency added to every response.
    pub latency: Duration,
    /// Routes by path (or path + `?query`, matched first).
    pub routes: BTreeMap<String, Script>,
    /// The reply for unknown paths (default: 404).
    pub fallback: Option<Script>,
    /// Path prefixes that are render sub-resources (oracle: short gap).
    pub subresource_paths: Vec<String>,
    /// Page delay for the key in the oracle (default: the snapshot default, 5 s).
    pub min_delay_ms: Option<u64>,
}

impl Site {
    /// An empty site.
    pub fn new(name: &str, host: &str) -> Site {
        Site {
            name: name.to_string(),
            host: host.to_ascii_lowercase(),
            latency: Duration::ZERO,
            routes: BTreeMap::new(),
            fallback: None,
            subresource_paths: Vec::new(),
            min_delay_ms: None,
        }
    }

    /// The same reply in every cycle.
    pub fn route(mut self, path: &str, r: Reply) -> Site {
        self.routes.insert(path.to_string(), vec![Some(r)]);
        self
    }

    /// One reply per cycle (the last repeats; `None` = 404).
    pub fn route_cycles(mut self, path: &str, script: Script) -> Site {
        self.routes.insert(path.to_string(), script);
        self
    }

    /// The reply for unknown paths.
    pub fn fallback(mut self, r: Reply) -> Site {
        self.fallback = Some(vec![Some(r)]);
        self
    }

    /// Latency for every response.
    pub fn latency(mut self, d: Duration) -> Site {
        self.latency = d;
        self
    }

    /// Declares render sub-resource path prefixes.
    pub fn subresources(mut self, prefixes: &[&str]) -> Site {
        self.subresource_paths = prefixes.iter().map(|p| p.to_string()).collect();
        self
    }

    /// The politeness key of the site.
    pub fn key(&self) -> String {
        PolitenessKey::for_host(&self.host).0
    }

    /// The reply for `path_and_query` in `cycle`.
    pub fn reply(&self, path_and_query: &str, cycle: usize) -> Option<Reply> {
        let path = path_and_query.split('?').next().unwrap_or("/");
        let script = self
            .routes
            .get(path_and_query)
            .or_else(|| self.routes.get(path))
            .or(self.fallback.as_ref())?;
        let i = cycle.min(script.len().saturating_sub(1));
        script.get(i).cloned().flatten()
    }
}

#[derive(Debug, Default)]
struct Served {
    robots: Vec<RobotsServed>,
    backoffs: Vec<Backoff>,
}

/// The fake web: sites, the current cycle, the request log and what was served.
#[derive(Debug)]
pub struct FakeWeb {
    sites: RwLock<BTreeMap<String, Site>>,
    cycle: AtomicUsize,
    log: RequestLog,
    served: Mutex<Served>,
    denylist: RwLock<Vec<String>>,
    park_ms: u64,
}

impl Default for FakeWeb {
    fn default() -> Self {
        FakeWeb::new()
    }
}

impl FakeWeb {
    /// A fake web with no sites.
    pub fn new() -> FakeWeb {
        FakeWeb {
            sites: RwLock::new(BTreeMap::new()),
            cycle: AtomicUsize::new(0),
            log: RequestLog::new(),
            served: Mutex::new(Served::default()),
            denylist: RwLock::new(Vec::new()),
            park_ms: PolicySnapshot::default().park_ms,
        }
    }

    /// The seven scripted sites of spec §12 with default options.
    pub fn with_default_sites() -> FakeWeb {
        FakeWeb::with_sites(sites::all(&sites::SiteOptions::default()))
    }

    /// A fake web serving `sites` (plus [`sites::DENIED`] on the oracle's denylist).
    pub fn with_sites(sites: Vec<Site>) -> FakeWeb {
        let w = FakeWeb::new();
        for s in sites {
            w.add_site(s);
        }
        w.deny(sites::DENIED);
        w
    }

    /// Adds (or replaces) a site.
    pub fn add_site(&self, s: Site) {
        self.sites
            .write()
            .expect("sites lock")
            .insert(s.host.clone(), s);
    }

    /// Replaces one route's script at runtime.
    pub fn set_route(&self, host: &str, path: &str, script: Script) {
        let mut g = self.sites.write().expect("sites lock");
        let s = g
            .entry(host.to_ascii_lowercase())
            .or_insert_with(|| Site::new(host, host));
        s.routes.insert(path.to_string(), script);
    }

    /// Adds a denylist pattern to the snapshot.
    pub fn deny(&self, pattern: &str) {
        self.denylist
            .write()
            .expect("denylist lock")
            .push(pattern.to_string());
    }

    /// Hosts served.
    pub fn hosts(&self) -> Vec<String> {
        self.sites
            .read()
            .expect("sites lock")
            .keys()
            .cloned()
            .collect()
    }

    /// A copy of one site.
    pub fn site(&self, host: &str) -> Option<Site> {
        self.sites.read().expect("sites lock").get(host).cloned()
    }

    /// The current cycle.
    pub fn cycle(&self) -> usize {
        self.cycle.load(Ordering::SeqCst)
    }

    /// Sets the cycle.
    pub fn set_cycle(&self, n: usize) {
        self.cycle.store(n, Ordering::SeqCst);
    }

    /// Moves to the next cycle and returns it.
    pub fn next_cycle(&self) -> usize {
        self.cycle.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The request log (clones share entries).
    pub fn log(&self) -> RequestLog {
        self.log.clone()
    }

    /// Clears the log and what was served, and goes back to cycle 0.
    pub fn reset(&self) {
        let mut g = self.served.lock().expect("served lock");
        *g = Served::default();
        drop(g);
        self.log.clear();
        self.set_cycle(0);
    }

    /// What the politeness oracle needs: robots.txt bodies served per origin, challenge parks,
    /// `Retry-After` windows, per-key delays and sub-resource paths, and the denylist.
    pub fn policy_snapshot(&self) -> PolicySnapshot {
        let mut snap = PolicySnapshot::new();
        for s in self.sites.read().expect("sites lock").values() {
            let e = snap.keys.entry(s.key()).or_default();
            if s.min_delay_ms.is_some() {
                e.min_delay_ms = e.min_delay_ms.max(s.min_delay_ms);
            }
            for p in &s.subresource_paths {
                if !e.subresource_paths.contains(p) {
                    e.subresource_paths.push(p.clone());
                }
            }
        }
        snap.keys.retain(|_, r| *r != KeyRules::default());
        let g = self.served.lock().expect("served lock");
        snap.robots = g.robots.clone();
        snap.backoffs = g.backoffs.clone();
        snap.denylist = self.denylist.read().expect("denylist lock").clone();
        snap
    }

    /// The axum router (site traffic + `/__admin/*`).
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(handle).with_state(self)
    }

    /// Serves on `127.0.0.1:<random port>`.
    pub async fn start(self: Arc<Self>) -> FakeWebServer {
        let server = FakeServer::spawn(self.clone().router()).await;
        FakeWebServer { server, web: self }
    }

    fn record_served(&self, key: &str, url: &str, r: &Reply, ended: chrono::DateTime<Utc>) {
        let mut g = self.served.lock().expect("served lock");
        let path = url::Url::parse(url)
            .map(|u| u.path().to_string())
            .unwrap_or_default();
        if path == "/robots.txt" {
            g.robots.push(RobotsServed {
                origin: crate::oracle::origin_of(url).unwrap_or_default(),
                at: ended,
                status: r.status,
                body: String::from_utf8_lossy(&r.body).into_owned(),
                challenge: r.challenge,
                no_response: r.no_response,
            });
        }
        if r.challenge {
            g.backoffs.push(Backoff {
                key: key.to_string(),
                from: ended,
                until: ended + ChronoDuration::milliseconds(self.park_ms as i64),
                reason: "challenge".into(),
            });
        }
        if matches!(r.status, 429 | 503)
            && let Some(secs) = r
                .header_value("retry-after")
                .and_then(|v| v.trim().parse::<i64>().ok())
        {
            g.backoffs.push(Backoff {
                key: key.to_string(),
                from: ended,
                until: ended + ChronoDuration::seconds(secs),
                reason: "retry_after".into(),
            });
        }
    }
}

/// A running fake web.
#[derive(Debug)]
pub struct FakeWebServer {
    server: FakeServer,
    web: Arc<FakeWeb>,
}

impl std::ops::Deref for FakeWebServer {
    type Target = FakeWeb;
    fn deref(&self) -> &FakeWeb {
        &self.web
    }
}

impl FakeWebServer {
    /// The listener address.
    pub fn addr(&self) -> std::net::SocketAddr {
        self.server.addr()
    }

    /// `http://<addr>` (admin endpoints).
    pub fn base_url(&self) -> String {
        self.server.base_url()
    }

    /// The shared fake web.
    pub fn web(&self) -> Arc<FakeWeb> {
        self.web.clone()
    }

    /// `(host, addr)` pairs for DNS overrides (e.g. `reqwest::ClientBuilder::resolve`).
    pub fn resolve_overrides(&self) -> Vec<(String, std::net::SocketAddr)> {
        self.web
            .hosts()
            .into_iter()
            .chain([sites::DENIED.to_string()])
            .map(|h| (h, self.addr()))
            .collect()
    }

    /// A reqwest client that sends [`vc_core::USER_AGENT`] and resolves every site host to
    /// this server (no redirects followed, like the crawler).
    pub fn client(&self) -> reqwest::Client {
        let mut b = reqwest::Client::builder()
            .user_agent(vc_core::USER_AGENT)
            .redirect(reqwest::redirect::Policy::none());
        for (h, a) in self.resolve_overrides() {
            b = b.resolve(&h, a);
        }
        b.build().expect("reqwest client")
    }

    /// `http://<host><path>` (resolved by [`FakeWebServer::client`]).
    pub fn url(&self, host: &str, path: &str) -> String {
        format!("http://{host}{path}")
    }
}

fn host_of(req: &Request) -> String {
    req.headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .map(str::to_string)
        .or_else(|| req.uri().authority().map(|a| a.to_string()))
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn strip_port(host: &str) -> &str {
    if host.starts_with('[') {
        return host.find(']').map_or(host, |i| &host[..=i]);
    }
    host.rsplit_once(':')
        .filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit()))
        .map_or(host, |(h, _)| h)
}

async fn handle(State(web): State<Arc<FakeWeb>>, req: Request) -> Response {
    let pq = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".into());
    if pq.starts_with("/__admin/") {
        return admin(&web, req, &pq).await;
    }
    let started = Utc::now();
    let host_port = host_of(&req);
    let host = strip_port(&host_port).to_string();
    let url = format!("http://{host_port}{pq}");
    let key = PolitenessKey::for_host(&host).0;
    let ua = req
        .headers()
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let cycle = web.cycle();
    let (site_latency, reply) = {
        let g = web.sites.read().expect("sites lock");
        match g.get(&host) {
            Some(s) => (s.latency, s.reply(&pq, cycle)),
            None => (Duration::ZERO, None),
        }
    };
    let reply = reply.unwrap_or_else(|| Reply::status(404));
    if reply.no_response {
        // Recorded when the client gives up and the connection (and this future) is dropped.
        let _guard = NoResponseGuard {
            web: web.clone(),
            key,
            url,
            started,
            ua,
            reply,
        };
        return std::future::pending::<Response>().await;
    }
    let latency = site_latency + reply.latency;
    if !latency.is_zero() {
        tokio::time::sleep(latency).await;
    }
    let head_only = req.method() == Method::HEAD;
    let mut resp = Response::new(if head_only {
        Body::empty()
    } else {
        Body::from(reply.body.clone())
    });
    *resp.status_mut() = StatusCode::from_u16(reply.status).unwrap_or(StatusCode::OK);
    for (k, v) in &reply.headers {
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            resp.headers_mut().append(k, v);
        }
    }
    let ended = Utc::now();
    web.record_served(&key, &url, &reply, ended);
    web.log.record(RequestLogEntry {
        key,
        url,
        started_at: started,
        ended_at: ended,
        status: reply.status,
        ua,
    });
    resp
}

/// Logs a request that was never answered once the client has dropped it.
struct NoResponseGuard {
    web: Arc<FakeWeb>,
    key: String,
    url: String,
    started: chrono::DateTime<Utc>,
    ua: String,
    reply: Reply,
}

impl Drop for NoResponseGuard {
    fn drop(&mut self) {
        let ended = Utc::now();
        self.web
            .record_served(&self.key, &self.url, &self.reply, ended);
        self.web.log.record(RequestLogEntry {
            key: std::mem::take(&mut self.key),
            url: std::mem::take(&mut self.url),
            started_at: self.started,
            ended_at: ended,
            status: 0,
            ua: std::mem::take(&mut self.ua),
        });
    }
}

fn json_response(status: StatusCode, v: serde_json::Value) -> Response {
    let mut r = Response::new(Body::from(v.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    r
}

async fn admin(web: &FakeWeb, req: Request, pq: &str) -> Response {
    let path = pq.split('?').next().unwrap_or(pq);
    let method = req.method().clone();
    match (method, path) {
        (Method::GET, "/__admin/health") => {
            json_response(StatusCode::OK, serde_json::json!({"ok": true}))
        }
        (Method::GET, "/__admin/log") => {
            let mut r = Response::new(Body::from(web.log.to_jsonl()));
            r.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-ndjson"),
            );
            r
        }
        (Method::GET, "/__admin/snapshot") => json_response(
            StatusCode::OK,
            serde_json::to_value(web.policy_snapshot()).unwrap_or_default(),
        ),
        (Method::GET, "/__admin/cycle") => {
            json_response(StatusCode::OK, serde_json::json!({"cycle": web.cycle()}))
        }
        (Method::POST, "/__admin/cycle") => {
            let body = to_bytes(req.into_body(), 64 * 1024)
                .await
                .unwrap_or_default();
            let n = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("cycle").and_then(|c| c.as_u64()));
            let c = match n {
                Some(n) => {
                    web.set_cycle(n as usize);
                    n as usize
                }
                None => web.next_cycle(),
            };
            json_response(StatusCode::OK, serde_json::json!({"cycle": c}))
        }
        (Method::POST, "/__admin/reset") => {
            web.reset();
            json_response(StatusCode::OK, serde_json::json!({"ok": true}))
        }
        _ => json_response(
            StatusCode::NOT_FOUND,
            serde_json::json!({"error": "unknown admin endpoint"}),
        ),
    }
}
