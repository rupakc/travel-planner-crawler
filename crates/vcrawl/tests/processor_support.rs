//! Shared helpers for the data-path tests (`processor.rs`, `api.rs`, `embedder.rs` include
//! this file with `#[path]`). Captures are committed through the real raw-store path: WARC
//! encode → segment → blob upload → `vc_commit_segment` (which enqueues `capture_ready`).

#![allow(dead_code, missing_docs)]

use std::io::Cursor;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use chrono::{DateTime, Duration as ChronoDuration, TimeZone, Utc};
use sqlx::PgPool;
use testkit::FakeClock;
use testkit::blob::MemBlobStore;
use testkit::llm::{FakeLlm, MemLedger};
use vc_config::{ExtractConfig, KindsConfig, LlmConfig};
use vc_core::frontier::{Frontier, Validators};
use vc_core::kinds::KindRegistry;
use vc_core::llm::Usd;
use vc_core::media::{FetchMode, MediaType};
use vc_core::store::{
    BlobStore, CaptureMeta, CaptureRole, SegmentCommitter, SpoolPayload, SpoolRecord,
};
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_core::{CanonicalUrl, CaptureId, CaptureRef, Clock, Epoch, SegmentId, SourceId};
use vc_extract::{Extractor, FilePlaceResolver};
use vcrawl::data::processor::{Processor, ProcessorDeps};
use vcrawl::data::sources::SourceCache;

/// The repository root.
pub fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// A fixture file under `testkit/fixtures/vcrawl/data/`.
pub fn fixture(name: &str) -> String {
    let p = root().join("testkit/fixtures/vcrawl/data").join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("fixture {}: {e}", p.display()))
}

/// Seeds `sources` with fixture specs (`sources/<id>.yaml`).
pub async fn seed(pool: &PgPool, ids: &[&str]) {
    let specs: Vec<_> = ids
        .iter()
        .map(|id| {
            vc_sources::spec::parse_spec(&fixture(&format!("sources/{id}.yaml"))).expect("spec")
        })
        .collect();
    vc_sources::seed::seed_sources(pool, &specs, false)
        .await
        .expect("seed sources");
}

/// The real kind registry of the repository.
pub fn kinds() -> Arc<dyn KindRegistry> {
    let r = root();
    let cfg = KindsConfig {
        kinds_toml: r.join("kinds.toml").display().to_string(),
        schemas_dir: r.join("schemas").display().to_string(),
        prompts_dir: r.join("prompts").display().to_string(),
    };
    Arc::new(vc_kinds::FileKindRegistry::load(&cfg).expect("kind registry"))
}

/// A tiny place file (the format of `data/places.tsv.gz`).
pub fn places() -> FilePlaceResolver {
    let places = "# Contains data from GeoNames (geonames.org), CC BY 4.0\n\
geoname_id\tname\tcountry\tadmin1\tfclass\trank\tlat\tlon\tkeys\n\
2267057\tLisbon\tPT\t14\tP\t500000000\t38.7167\t-9.1333\tlisbon|lisboa\n";
    let countries = "code\tname\tkeys\nPT\tPortugal\tportugal\nES\tSpain\tspain\n";
    let admin1 = "code\tname\nPT.14\tLisbon\n";
    FilePlaceResolver::from_readers(
        Cursor::new(places),
        Cursor::new(countries),
        Cursor::new(admin1),
    )
    .expect("places")
}

/// A fixed test time.
pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 15, 12, 0, 0).unwrap()
}

/// What to commit.
pub struct Cap<'a> {
    pub url: &'a str,
    pub source: Option<&'a str>,
    pub body: &'a str,
    pub at: DateTime<Utc>,
    pub status: Option<u16>,
    pub role: CaptureRole,
    pub media: MediaType,
    /// A 304 revisit of this capture (no body stored).
    pub revisit_of: Option<CaptureRef>,
}

impl<'a> Cap<'a> {
    /// A 200 HTML primary capture.
    pub fn html(url: &'a str, source: &'a str, body: &'a str, at: DateTime<Utc>) -> Self {
        Cap {
            url,
            source: Some(source),
            body,
            at,
            status: Some(200),
            role: CaptureRole::Primary,
            media: MediaType::Html,
            revisit_of: None,
        }
    }

    /// A 304 revisit of `of` (a WARC `revisit` record, no body).
    pub fn revisit(url: &'a str, source: &'a str, of: CaptureRef, at: DateTime<Utc>) -> Self {
        Cap {
            status: Some(304),
            revisit_of: Some(of),
            ..Cap::html(url, source, "", at)
        }
    }
}

fn not_modified(u: &CanonicalUrl, at: DateTime<Utc>) -> vc_core::fetch::Exchange {
    let mut req = http::HeaderMap::new();
    req.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static(vc_core::USER_AGENT),
    );
    vc_core::fetch::Exchange {
        url: u.clone(),
        redirects: vec![],
        method: http::Method::GET,
        request_headers: req,
        status: http::StatusCode::NOT_MODIFIED,
        response_headers: http::HeaderMap::new(),
        body: Bytes::new(),
        original_encoding: None,
        truncated: false,
        peer_ip: None,
        started_at: at,
        ended_at: at,
        purpose: vc_core::fetch::RequestPurpose::Page,
    }
}

/// Commits one capture through the raw-store path and returns its reference.
pub async fn commit(pool: &PgPool, blobs: &Arc<MemBlobStore>, c: Cap<'_>) -> CaptureRef {
    let url = CanonicalUrl::parse(c.url).expect("url");
    let body = Bytes::from(c.body.to_string());
    let meta = CaptureMeta {
        capture_id: CaptureId::at(c.at),
        captured_at: c.at,
        epoch: Epoch(1),
        orphan_epoch: false,
        url_key: url.key(),
        key: url.politeness_key(),
        url,
        source_id: c.source.map(|s| SourceId(s.into())),
        role: c.role,
        fetch_mode: FetchMode::Http,
        status: c.status,
        media_type: c.media,
        payload_hash: c
            .revisit_of
            .is_none()
            .then(|| vc_store::warc::payload_hash(&body)),
        payload_len: body.len() as u64,
        validators: Validators::default(),
        revisit_of: c.revisit_of,
        use_signals: UseSignals::default(),
        use_policy_fetch: UsePolicy::ALLOW_ALL,
        raw_expires_at: None,
        next_fetch_at: None,
    };
    let cref = meta.capture_ref();
    let payload = if c.revisit_of.is_some() {
        SpoolPayload::Revisit(not_modified(&meta.url, c.at))
    } else {
        SpoolPayload::Resource {
            content_type: "text/html; charset=utf-8".into(),
            body,
        }
    };
    let rec = SpoolRecord {
        meta: meta.clone(),
        payload,
    };
    let enc = vc_store::warc::encode(&rec).expect("encode");
    let entry = vc_store::SpoolEntry {
        meta,
        warc_record_id: enc.warc_record_id,
        record_count: enc.record_count,
        warc: enc.bytes,
    };
    let seg = vc_store::build_segment(SegmentId::at(c.at), false, &[entry], 3).expect("segment");
    let crc = crc32c(&seg.bytes);
    blobs
        .put_if_absent(&seg.manifest.path, seg.bytes.clone(), crc)
        .await
        .expect("upload segment");
    vc_store::PgSegmentCommitter::new(pool.clone())
        .commit(&seg.manifest)
        .await
        .expect("commit segment");
    cref
}

fn crc32c(b: &[u8]) -> u32 {
    vc_llm::crc32c(b)
}

/// A processor over a fresh database.
pub struct Rig {
    pub pool: PgPool,
    pub blobs: Arc<MemBlobStore>,
    pub llm: Arc<FakeLlm>,
    pub ledger: Arc<MemLedger>,
    pub clock: Arc<FakeClock>,
    pub proc: Arc<Processor>,
}

/// Options for [`rig`].
#[derive(Default)]
pub struct RigOpts {
    /// Wire the (fake) LLM into the extractor (`VC_LLM_ENABLED=true`).
    pub llm: bool,
    /// Follow links into a `PgFrontier`.
    pub frontier: bool,
    /// Wire link triage over the fake LLM: `Some(enabled)`.
    pub triage: Option<bool>,
    /// Wraps the raw store the processor reads (commits still go to `Rig::blobs`).
    #[allow(clippy::type_complexity)]
    pub wrap_blobs:
        Option<Box<dyn Fn(Arc<MemBlobStore>, PgPool) -> Arc<dyn BlobStore> + Send + Sync>>,
}

/// A processor over `pool` with the given sources seeded.
pub async fn rig_on(pool: PgPool, sources: &[&str], o: RigOpts) -> Rig {
    seed(&pool, sources).await;
    let blobs = Arc::new(MemBlobStore::new());
    let clock = Arc::new(FakeClock::new(t0()));
    let dyn_clock: Arc<dyn Clock> = clock.clone();
    let llm = Arc::new(FakeLlm::new());
    let ledger = Arc::new(MemLedger::new(
        dyn_clock,
        Usd::from_dollars(1.0),
        Usd::from_dollars(10.0),
    ));
    let kinds = kinds();
    let mut ex =
        Extractor::new(kinds.clone(), ExtractConfig::default()).with_places(Arc::new(places()));
    if o.llm {
        ex = ex.with_llm(
            llm.clone(),
            ledger.clone(),
            LlmConfig {
                enabled: true,
                ..LlmConfig::default()
            },
        );
    }
    let frontier: Option<Arc<dyn Frontier>> = o.frontier.then(|| {
        Arc::new(vc_frontier::PgFrontier::new(
            pool.clone(),
            vc_config::FrontierConfig::default(),
        )) as Arc<dyn Frontier>
    });
    let queue = Arc::new(vc_queue::PgWorkQueue {
        pool: pool.clone(),
        config: vc_config::QueueConfig::default(),
    });
    let proc_blobs: Arc<dyn BlobStore> = match &o.wrap_blobs {
        Some(w) => w(blobs.clone(), pool.clone()),
        None => blobs.clone(),
    };
    let proc = Processor::new(ProcessorDeps {
        pool: pool.clone(),
        blobs: proc_blobs,
        queue,
        sink: Arc::new(vc_index::PgIndex::new(pool.clone())),
        kinds,
        extractor: Arc::new(ex),
        frontier,
        handlers: vc_parse::registry(),
        limits: vc_core::parse::Limits::default(),
    })
    .with_sources(SourceCache::with_ttl(
        pool.clone(),
        std::time::Duration::ZERO,
    ));
    let proc = match o.triage {
        Some(enabled) => proc.with_triage(vcrawl::agentic::AgenticLlm::new(
            llm.clone(),
            ledger.clone(),
            LlmConfig {
                enabled,
                ..LlmConfig::default()
            },
        )),
        None => proc,
    };
    Rig {
        pool,
        blobs,
        llm,
        ledger,
        clock,
        proc: Arc::new(proc),
    }
}

/// A processor over a fresh database.
pub async fn rig(sources: &[&str], o: RigOpts) -> Rig {
    rig_on(testkit::pg().await, sources, o).await
}

/// `t0 + minutes`.
pub fn at(minutes: i64) -> DateTime<Utc> {
    t0() + ChronoDuration::minutes(minutes)
}

/// Scalar count.
pub async fn count(pool: &PgPool, sql: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql.to_string()))
        .fetch_one(pool)
        .await
        .expect("count")
}
