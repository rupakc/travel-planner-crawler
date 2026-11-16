//! Shared harness for the crawl-path and failpoint suites: Postgres (testkit), a fetcher wired
//! like production except for the policy (a stub that lets loopback through the SSRF guard),
//! a static resolver pointing the test hosts at the local fake server, and a chosen blob store.
#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::Arc;

use sqlx::PgPool;
use testkit::vc_fetch_fixtures::StubPolicy;
use vc_config::Config;
use vc_core::frontier::Frontier;
use vc_core::policy::Policy;
use vc_core::sources::SourceSpec;
use vc_core::store::BlobStore;
use vc_core::{Clock, SystemClock};
use vc_fetch::StaticResolver;
use vc_frontier::PgFrontier;
use vcrawl::roles::fetcher::{Fetcher, FetcherParts, RobotsTombstoner, SpoolAdoption};
use vcrawl::wiring;
use vcrawl::wiring::rawcap::PgRawBytes;
use vcrawl::wiring::sources::Sources;

/// A test configuration: 1 s key delay, short leases, a generous global rate.
pub fn cfg() -> Config {
    let mut c = Config::default();
    c.frontier.min_delay_s = 1;
    c.frontier.per_url_budget_s = 10;
    c.frontier.global_rate_per_s = 100.0;
    c.frontier.global_rate_burst = 100.0;
    c.frontier.holder = Some("test-fetcher".into());
    c.store.zstd_level = 3;
    c.vcrawl.recrawl_min_s = 3600;
    c
}

/// A source spec from YAML fields (id, method and the rest).
pub fn spec(yaml_body: &str) -> SourceSpec {
    let y = format!(
        "name: test\ncategory: test\nsource_class: official\n\
         licence: {{class: open, attribution: test}}\n{yaml_body}"
    );
    vc_sources::parse_spec(&y).expect("spec")
}

/// The fetcher's parts against `pool`, `hosts` resolved to 127.0.0.1.
pub fn parts(
    cfg: &Config,
    pool: &PgPool,
    hosts: &[&str],
    specs: Vec<SourceSpec>,
    spool_dir: &Path,
    store: Arc<dyn BlobStore>,
) -> FetcherParts {
    let cfg = Arc::new(cfg.clone());
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let pg = Arc::new(PgFrontier::new(pool.clone(), cfg.frontier.clone()));
    let frontier: Arc<dyn Frontier> = pg.clone();
    let policy: Arc<dyn Policy> = Arc::new(StubPolicy::loopback_ok());
    let mut r = StaticResolver::new();
    for h in hosts {
        r = r.with(h, IpAddr::V4(Ipv4Addr::LOCALHOST));
    }
    let client = wiring::fetch::permit_client(
        &cfg,
        frontier.clone(),
        policy.clone(),
        clock.clone(),
        Some(Arc::new(r)),
    );
    let adopt = SpoolAdoption {
        store: store.clone(),
        committer: wiring::store::committer(pool.clone()),
    };
    let (spool, roller) = wiring::store::spool_and_roller(
        &cfg,
        spool_dir.to_path_buf(),
        store,
        wiring::store::committer(pool.clone()),
        clock.clone(),
    )
    .expect("spool");
    FetcherParts {
        recrawl: pg,
        frontier,
        client,
        renderer: None,
        policy,
        spool,
        roller,
        sources: Arc::new(Sources::new(specs)),
        adapters: Arc::new(wiring::sources::adapters(&cfg)),
        resumable: Arc::new(wiring::sources::resumable_adapters()),
        cursors: Some(vc_sources::cursors::PgAdapterCursors::new(pool.clone())),
        secrets: Arc::new(vc_secrets::EnvSource {
            prefix: "VC_TEST_SECRET_".into(),
        }),
        raw_bytes: Arc::new(PgRawBytes::new(pool.clone(), cfg.fetch.daily_raw_bytes_cap)),
        tombstoner: Some(RobotsTombstoner {
            pool: pool.clone(),
            sink: Arc::new(vc_index::PgIndex::new(pool.clone())),
        }),
        pool: Some(pool.clone()),
        clock,
        metrics: None,
        tracker: None,
        adopt: Some(adopt),
        cfg,
    }
}

/// A fetcher (see [`parts`]).
pub fn fetcher(
    cfg: &Config,
    pool: &PgPool,
    hosts: &[&str],
    specs: Vec<SourceSpec>,
    spool_dir: &Path,
    store: Arc<dyn BlobStore>,
) -> Fetcher {
    Fetcher::new(parts(cfg, pool, hosts, specs, spool_dir, store))
}

/// "Time passes": dead leases expire, every key is ready again (unless parked), every due URL
/// is due now. Politeness is still enforced by the code under test; this only fast-forwards.
pub async fn fast_forward(pool: &PgPool) {
    for q in [
        "UPDATE claims SET deadline = least(deadline, now() - interval '1 second')",
        "UPDATE politeness_keys SET next_allowed_at = now() - interval '1 second'
         WHERE parked_until IS NULL OR parked_until <= now()",
        "UPDATE url_state SET next_fetch_at = now() - interval '1 second'
         WHERE state IN ('due', 'leased')",
    ] {
        sqlx::query(q).execute(pool).await.expect("fast forward");
    }
}

/// The URL `http://<host>:<port><path>`.
pub fn url(host: &str, port: u16, path: &str) -> vc_core::CanonicalUrl {
    vc_core::CanonicalUrl::parse(&format!("http://{host}:{port}{path}")).expect("url")
}

/// `(state, etag, last_capture_id, payload_hash)` of a URL.
pub async fn url_state(
    pool: &PgPool,
    u: &vc_core::CanonicalUrl,
) -> (String, Option<String>, Option<String>, Option<Vec<u8>>) {
    sqlx::query_as(
        "SELECT state, etag, last_capture_id, payload_hash FROM url_state WHERE url_key = $1",
    )
    .bind(u.key().as_bytes().as_slice())
    .fetch_one(pool)
    .await
    .expect("url_state row")
}

/// Committed captures as `(capture_id, role, status, revisit_of_id, url)`, oldest first.
pub async fn captures(pool: &PgPool) -> Vec<(String, String, Option<i16>, Option<String>, String)> {
    sqlx::query_as(
        "SELECT capture_id, role, status, revisit_of_id, url FROM captures ORDER BY capture_id",
    )
    .fetch_all(pool)
    .await
    .expect("captures")
}
