//! Test harness for the adapters: a permit client that talks HTTP/1.1 to wiremock (lease
//! check + rate token per request, scope check, per-chunk lease check on streams with clock
//! advance), a recording capture sink, an event-logging frontier wrapper and a secrets map.

#![allow(dead_code, missing_docs)]

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use chrono::{DateTime, TimeZone, Utc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vc_core::error::LeaseLost;
use vc_core::fetch::{
    Exchange, FetchError, FetchRequest, LimitedBodyStream, PermitClient, ResponseHead,
};
use vc_core::frontier::{Claim, ClaimPermit, Frontier, KeyOutcome, LeaseCheck, NewUrl, ParkReason};
use vc_core::media::FetchMode;
use vc_core::policy::RobotsEntry;
use vc_core::secrets::{Secret, SecretSource};
use vc_core::sources::{AdapterCtx, AdapterReport, SourceAdapter, SourceSpec};
use vc_core::store::{CaptureSink, SpoolRecord};
use vc_core::{CanonicalUrl, CaptureId, Clock, Error, OriginKey, PolitenessKey, Result, UrlKey};
use wiremock::http::{HeaderMap, HeaderName, HeaderValue, Method};

pub fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(repo_root().join("testkit/fixtures/vc-sources").join(name))
        .unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

pub fn spec(id: &str) -> SourceSpec {
    vc_sources::load_file(&repo_root().join("sources").join(format!("{id}.yaml"))).unwrap()
}

/// Shared ordered event log ("fetch <path>", "append <role>", "enqueue <n>", "extend").
#[derive(Clone, Default)]
pub struct Events(Arc<Mutex<Vec<String>>>);

impl Events {
    pub fn push(&self, s: impl Into<String>) {
        self.0.lock().unwrap().push(s.into());
    }
    pub fn all(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

// ---------------------------------------------------------------------------------------
// Minimal HTTP/1.1 client (test only; production uses vc-fetch).

pub struct RawResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub async fn http_get(
    url: &url::Url,
    headers: &[(String, String)],
) -> std::io::Result<RawResponse> {
    let host = url.host_str().unwrap_or("127.0.0.1");
    let port = url.port_or_known_default().unwrap_or(80);
    let mut s = tokio::net::TcpStream::connect((host, port)).await?;
    let mut path = url.path().to_string();
    if let Some(q) = url.query() {
        path.push('?');
        path.push_str(q);
    }
    let mut req = format!("GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes()).await?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).await?;
    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http head");
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let mut raw_body = buf[split + 4..].to_vec();
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .expect("status");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    if headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.contains("chunked"))
    {
        let mut out = Vec::new();
        let mut rest = &raw_body[..];
        loop {
            let nl = rest.windows(2).position(|w| w == b"\r\n").expect("chunk");
            let n = usize::from_str_radix(String::from_utf8_lossy(&rest[..nl]).trim(), 16)
                .expect("chunk size");
            if n == 0 {
                break;
            }
            out.extend_from_slice(&rest[nl + 2..nl + 2 + n]);
            rest = &rest[nl + 4 + n..];
        }
        raw_body = out;
    }
    Ok(RawResponse {
        status,
        headers,
        body: raw_body,
    })
}

// ---------------------------------------------------------------------------------------
// Permit client.

pub struct TestClient {
    pub frontier: Arc<TestFrontier>,
    pub clock: Arc<testkit::FakeClock>,
    pub events: Events,
    /// Stream chunk size.
    pub chunk: usize,
    /// Simulated time per streamed chunk.
    pub per_chunk: Duration,
    /// Every URL requested (as sent, secrets included: test only).
    pub requested: Mutex<Vec<String>>,
    /// Every User-Agent sent.
    pub uas: Mutex<Vec<String>>,
    /// The extra request headers of every request (as sent, secrets included: test only).
    pub sent_headers: Mutex<Vec<Vec<(String, String)>>>,
    /// The headers of every request (lower-case names; test only).
    pub sent: Mutex<Vec<Vec<(String, String)>>>,
}

impl TestClient {
    pub fn new(
        frontier: Arc<TestFrontier>,
        clock: Arc<testkit::FakeClock>,
        events: Events,
    ) -> Self {
        TestClient {
            frontier,
            clock,
            events,
            chunk: 4096,
            per_chunk: Duration::ZERO,
            requested: Mutex::new(Vec::new()),
            uas: Mutex::new(Vec::new()),
            sent_headers: Mutex::new(Vec::new()),
            sent: Mutex::new(Vec::new()),
        }
    }

    async fn get(
        &self,
        p: &ClaimPermit,
        req: &FetchRequest,
    ) -> std::result::Result<RawResponse, FetchError> {
        if !p.scope().covers(&req.url) {
            return Err(FetchError::OutOfScope(req.url.clone()));
        }
        self.frontier.take_rate_token(p).await?;
        let mut headers = vec![("user-agent".to_string(), vc_core::USER_AGENT.to_string())];
        if let Some(a) = &req.accept {
            headers.push(("accept".into(), a.clone()));
        }
        // Conditional GET from the leased row's validators (as vc-fetch sends them).
        if let Some(e) = &req.validators.etag {
            headers.push(("if-none-match".into(), e.clone()));
        }
        if let Some(lm) = &req.validators.last_modified {
            headers.push(("if-modified-since".into(), lm.clone()));
        }
        // Extra headers (API keys), as vc-fetch sends them.
        let extra: Vec<(String, String)> = req
            .headers
            .iter()
            .filter(|(k, _)| *k != "user-agent" && *k != "cookie")
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        headers.extend(extra.iter().cloned());
        self.sent_headers.lock().unwrap().push(extra);
        self.sent.lock().unwrap().push(headers.clone());
        self.requested
            .lock()
            .unwrap()
            .push(req.url.as_str().to_string());
        self.uas
            .lock()
            .unwrap()
            .push(vc_core::USER_AGENT.to_string());
        self.events
            .push(format!("fetch {}", req.url.as_url().path()));
        let r = http_get(req.url.as_url(), &headers)
            .await
            .map_err(|e| FetchError::Transport(e.to_string()))?;
        p.record_request_end(std::time::Instant::now());
        if r.status == 401 || r.status == 403 {
            return Err(FetchError::Challenge { status: r.status });
        }
        Ok(r)
    }

    fn header_map(r: &RawResponse) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in &r.headers {
            if let (Ok(k), Ok(v)) = (
                HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                h.append(k, v);
            }
        }
        h
    }
}

#[async_trait]
impl PermitClient for TestClient {
    async fn fetch(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> std::result::Result<Exchange, FetchError> {
        let start = self.clock.now();
        let r = self.get(p, &req).await?;
        // vc-fetch keeps the headers it sent (they go into the WARC `request` record).
        let mut request_headers = req.headers.clone();
        request_headers.insert("user-agent", HeaderValue::from_static(vc_core::USER_AGENT));
        Ok(Exchange {
            url: req.url.clone(),
            redirects: Vec::new(),
            method: Method::GET,
            request_headers,
            status: r.status.try_into().expect("status"),
            response_headers: Self::header_map(&r),
            body: Bytes::from(r.body),
            original_encoding: None,
            truncated: false,
            peer_ip: None,
            started_at: start,
            ended_at: self.clock.now(),
            purpose: req.purpose,
        })
    }

    async fn stream(
        &self,
        p: &ClaimPermit,
        req: FetchRequest,
    ) -> std::result::Result<LimitedBodyStream, FetchError> {
        let start = self.clock.now();
        let r = self.get(p, &req).await?;
        let head = ResponseHead {
            url: req.url.clone(),
            redirects: Vec::new(),
            request_headers: HeaderMap::new(),
            status: r.status.try_into().expect("status"),
            headers: Self::header_map(&r),
            original_encoding: None,
            peer_ip: None,
            started_at: start,
        };
        if req.max_body_bytes > 0 && r.body.len() as u64 > req.max_body_bytes {
            let limit = req.max_body_bytes;
            let s = futures::stream::iter(vec![Err(FetchError::BodyTooLarge { limit })]);
            return Ok(LimitedBodyStream {
                head,
                body: Box::pin(s),
            });
        }
        // Each chunk: advance the clock, then check the lease (like the real client, which
        // aborts the stream when the lease is gone).
        let chunks: Vec<Bytes> = r
            .body
            .chunks(self.chunk.max(1))
            .map(Bytes::copy_from_slice)
            .collect();
        let frontier = self.frontier.clone();
        let clock = self.clock.clone();
        let per_chunk = self.per_chunk;
        let lease = (p.key().0.clone(), p.epoch().0);
        let s = futures::stream::unfold((chunks.into_iter(), false), move |(mut it, dead)| {
            let frontier = frontier.clone();
            let clock = clock.clone();
            let lease = lease.clone();
            async move {
                if dead {
                    return None;
                }
                let c = it.next()?;
                clock.advance(per_chunk);
                match frontier.lease_live(&lease) {
                    Ok(()) => Some((Ok(c), (it, false))),
                    Err(e) => Some((Err(FetchError::Lease(e)), (it, true))),
                }
            }
        });
        Ok(LimitedBodyStream {
            head,
            body: Box::pin(s),
        })
    }
}

/// A frontier wrapper: delegates to `MemFrontier`, logs `enqueue`/`extend` into the event
/// log, keeps the enqueued rows, and mirrors each permit's deadline so a stream can check its
/// lease without borrowing the permit.
pub struct TestFrontier {
    pub inner: Arc<testkit::MemFrontier>,
    pub clock: Arc<testkit::FakeClock>,
    pub events: Events,
    pub enqueued: Mutex<Vec<NewUrl>>,
    pub extends: Mutex<u32>,
    deadlines: Mutex<HashMap<(String, i64), DateTime<Utc>>>,
}

impl TestFrontier {
    pub fn new(
        inner: Arc<testkit::MemFrontier>,
        clock: Arc<testkit::FakeClock>,
        events: Events,
    ) -> Self {
        TestFrontier {
            inner,
            clock,
            events,
            enqueued: Mutex::new(Vec::new()),
            extends: Mutex::new(0),
            deadlines: Mutex::new(HashMap::new()),
        }
    }

    pub fn track(&self, p: &ClaimPermit) {
        self.deadlines
            .lock()
            .unwrap()
            .insert((p.key().0.clone(), p.epoch().0), p.deadline().0);
    }

    pub fn lease_live(&self, lease: &(String, i64)) -> std::result::Result<(), LeaseLost> {
        let d = self.deadlines.lock().unwrap().get(lease).copied();
        match d {
            Some(d) if self.clock.now() < d => Ok(()),
            _ => Err(LeaseLost {
                key: lease.0.clone(),
                epoch: lease.1,
                reason: vc_core::LeaseLostReason::Expired,
            }),
        }
    }

    pub fn enqueued_urls(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .enqueued
            .lock()
            .unwrap()
            .iter()
            .map(|n| n.url.as_str().to_string())
            .collect();
        v.sort();
        v
    }
}

#[async_trait]
impl LeaseCheck for TestFrontier {
    async fn check(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        self.inner.check(p).await
    }
}

#[async_trait]
impl Frontier for TestFrontier {
    async fn claim(&self, shard: &[i16], max_urls: usize) -> Result<Option<Claim>> {
        let c = self.inner.claim(shard, max_urls).await?;
        if let Some(c) = &c {
            self.track(&c.permit);
        }
        Ok(c)
    }
    async fn extend(&self, p: &ClaimPermit, by: Duration) -> std::result::Result<(), LeaseLost> {
        self.inner.extend(p, by).await?;
        self.track(p);
        *self.extends.lock().unwrap() += 1;
        self.events.push("extend");
        Ok(())
    }
    async fn take_rate_token(&self, p: &ClaimPermit) -> std::result::Result<(), LeaseLost> {
        self.inner.take_rate_token(p).await
    }
    async fn mark_fetched(
        &self,
        p: &ClaimPermit,
        url: &UrlKey,
        next: DateTime<Utc>,
    ) -> std::result::Result<(), LeaseLost> {
        self.inner.mark_fetched(p, url, next).await
    }
    async fn complete(&self, p: ClaimPermit, o: KeyOutcome) -> std::result::Result<(), LeaseLost> {
        self.inner.complete(p, o).await
    }
    async fn enqueue(&self, batch: &[NewUrl]) -> Result<u64> {
        self.events.push(format!("enqueue {}", batch.len()));
        self.enqueued.lock().unwrap().extend(batch.iter().cloned());
        self.inner.enqueue(batch).await
    }
    async fn park(&self, key: &PolitenessKey, reason: ParkReason) -> Result<()> {
        self.inner.park(key, reason).await
    }
    async fn robots(&self, origin: &OriginKey) -> Result<Option<RobotsEntry>> {
        self.inner.robots(origin).await
    }
    async fn store_robots(&self, p: &ClaimPermit, entry: RobotsEntry) -> Result<()> {
        self.inner.store_robots(p, entry).await
    }
}

/// Records every appended spool record; can be told to fail.
#[derive(Default)]
pub struct RecSink {
    pub records: Vec<SpoolRecord>,
    pub events: Events,
    pub fail: bool,
}

#[async_trait]
impl CaptureSink for RecSink {
    async fn append(&mut self, r: SpoolRecord) -> Result<CaptureId> {
        if self.fail {
            return Err(Error::Storage("spool full (test)".into()));
        }
        self.events.push(format!("append {:?}", r.meta.role));
        let id = r.meta.capture_id;
        self.records.push(r);
        Ok(id)
    }
}

/// Secrets from a map.
#[derive(Default)]
pub struct MapSecrets(pub HashMap<String, String>);

#[async_trait]
impl SecretSource for MapSecrets {
    async fn get(&self, name: &str) -> Result<Secret> {
        self.0
            .get(name)
            .map(|v| Secret::new(v.clone()))
            .ok_or_else(|| Error::NotFound(name.to_string()))
    }
}

/// Everything needed to run one adapter against one leased URL.
pub struct Harness {
    pub clock: Arc<testkit::FakeClock>,
    pub frontier: Arc<TestFrontier>,
    pub client: TestClient,
    pub sink: RecSink,
    pub secrets: MapSecrets,
    pub events: Events,
    pub spec: SourceSpec,
    pub claim: Claim,
}

/// 03:00 UTC (inside the default dump window).
pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, 3, 0, 0).unwrap()
}

impl Harness {
    /// Leases `url` (fetch mode `adapter:<adapter>`) for `spec` at `now`.
    pub async fn new(spec: SourceSpec, adapter: &str, url: &str, now: DateTime<Utc>) -> Harness {
        Self::with_budget(spec, adapter, url, now, Duration::from_secs(240)).await
    }

    pub async fn with_budget(
        spec: SourceSpec,
        adapter: &str,
        url: &str,
        now: DateTime<Utc>,
        per_url_budget: Duration,
    ) -> Harness {
        let clock = Arc::new(testkit::FakeClock::new(now));
        let mem =
            Arc::new(testkit::MemFrontier::new(clock.clone()).with_per_url_budget(per_url_budget));
        let events = Events::default();
        let frontier = Arc::new(TestFrontier::new(
            mem.clone(),
            clock.clone(),
            events.clone(),
        ));
        let mut nu = NewUrl::new(CanonicalUrl::parse(url).unwrap());
        nu.source_id = Some(spec.id.clone());
        nu.fetch_mode = FetchMode::Adapter(adapter.to_string());
        mem.enqueue(&[nu]).await.unwrap();
        let claim = frontier.claim(&[0], 1).await.unwrap().expect("claim");
        let client = TestClient::new(frontier.clone(), clock.clone(), events.clone());
        let sink = RecSink {
            events: events.clone(),
            ..RecSink::default()
        };
        Harness {
            clock,
            frontier,
            client,
            sink,
            secrets: MapSecrets::default(),
            events,
            spec,
            claim,
        }
    }

    pub async fn run(&mut self, a: &dyn SourceAdapter) -> Result<AdapterReport> {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let mut ctx = AdapterCtx {
            permit: &self.claim.permit,
            client: &self.client,
            sink: &mut self.sink,
            frontier: &*self.frontier,
            source: &self.spec,
            url: &self.claim.urls[0],
            secrets: &self.secrets,
            clock: &*clock,
        };
        a.run(&mut ctx).await
    }

    pub async fn run_from(
        &mut self,
        a: &dyn vc_sources::adapters::ResumableAdapter,
        cursor: Option<&str>,
    ) -> Result<AdapterReport> {
        let clock: Arc<dyn Clock> = self.clock.clone();
        let mut ctx = AdapterCtx {
            permit: &self.claim.permit,
            client: &self.client,
            sink: &mut self.sink,
            frontier: &*self.frontier,
            source: &self.spec,
            url: &self.claim.urls[0],
            secrets: &self.secrets,
            clock: &*clock,
        };
        a.run_from(&mut ctx, cursor).await
    }

    /// Bodies of records with a given payload kind.
    pub fn resources(&self) -> Vec<(String, Bytes)> {
        self.sink
            .records
            .iter()
            .filter_map(|r| match &r.payload {
                vc_core::store::SpoolPayload::Resource { content_type, body } => {
                    Some((content_type.clone(), body.clone()))
                }
                _ => None,
            })
            .collect()
    }

    pub fn manifests(&self) -> Vec<serde_json::Value> {
        self.sink
            .records
            .iter()
            .filter_map(|r| match &r.payload {
                vc_core::store::SpoolPayload::Metadata { fields } => Some(fields.clone()),
                _ => None,
            })
            .collect()
    }

    pub fn responses(&self) -> Vec<&Exchange> {
        self.sink
            .records
            .iter()
            .filter_map(|r| match &r.payload {
                vc_core::store::SpoolPayload::Response(e) => Some(e),
                _ => None,
            })
            .collect()
    }
}

/// Asserts the raw-first order: the first `append` comes right after the first `fetch`, and
/// every `enqueue` comes after at least one `append`.
pub fn assert_raw_first(events: &[String]) {
    let first_append = events
        .iter()
        .position(|e| e.starts_with("append"))
        .expect("something was spooled");
    if let Some(first_enqueue) = events.iter().position(|e| e.starts_with("enqueue")) {
        assert!(
            first_append < first_enqueue,
            "enqueue before raw: {events:?}"
        );
    }
    let first_fetch = events.iter().position(|e| e.starts_with("fetch")).unwrap();
    assert!(first_fetch < first_append, "{events:?}");
}
