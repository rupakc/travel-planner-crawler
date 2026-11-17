//! Every long-running role serves `/metrics` (and `/healthz`) on `obs.metrics_addr` and writes
//! its heartbeat, so otelcol's scrape of `<role>:9464` (deploy/otelcol/config.yaml) finds it.
//! Each test runs the role's real `run` against a test database and local stores, scrapes
//! `/metrics` until the role's `crawler_heartbeat` series shows up, then stops the role.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;
use vcrawl::Ctx;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// The test database as a `[database]` URL (maintenance opens its own pool from config).
fn database_url(opts: &PgConnectOptions) -> String {
    let password = std::env::var("VC_TEST_DATABASE_URL")
        .ok()
        .and_then(|u| url::Url::parse(&u).ok())
        .and_then(|u| u.password().map(str::to_string))
        .unwrap_or_else(|| "postgres".into());
    format!(
        "postgres://{}:{password}@{}:{}/{}",
        opts.get_username(),
        opts.get_host(),
        opts.get_port(),
        opts.get_database().unwrap_or("postgres")
    )
}

/// Production defaults, with paths into the repository and a scratch directory, local blob
/// storage, rendering and the LLM off, and `/metrics` on a free loopback port.
fn cfg(tmp: &Path, opts: &PgConnectOptions, instance: &str) -> (vc_config::Config, SocketAddr) {
    let r = root();
    let p = |rel: &str| r.join(rel).display().to_string();
    let t = |rel: &str| {
        let d = tmp.join(rel);
        std::fs::create_dir_all(&d).expect("scratch dir");
        d.display().to_string()
    };
    let addr: SocketAddr = format!("127.0.0.1:{}", free_port()).parse().unwrap();
    let mut c = vc_config::Config::default();
    c.obs.metrics_addr = addr.to_string();
    c.obs.heartbeat_s = 1;
    c.vcrawl.instance = Some(instance.into());
    c.vcrawl.data_dir = t("data");
    c.database.url = database_url(opts);
    c.database.password_secret = None;
    c.secrets.backend = vc_config::SecretBackend::Env;
    c.secrets.env_prefix = "VC_TEST_ROLE_METRICS_NO_SUCH_SECRET_".into();
    c.store.backend = vc_config::BlobBackend::Fs;
    c.store.fs_root = t("blobs");
    c.store.spool_dir = t("spool");
    c.render.enabled = false;
    c.render.cache_dir = t("render-cache");
    c.llm.enabled = false;
    c.sources.dir = t("sources");
    c.policy.denylist_path = p("policy/denylist.yaml");
    c.policy.subresource_allowlist_path = p("policy/subresource_allowlist.yaml");
    c.kinds.kinds_toml = p("kinds.toml");
    c.kinds.schemas_dir = p("schemas");
    c.kinds.prompts_dir = p("prompts");
    c.extract.places_file = p("crates/vc-extract/data/places.tsv.gz");
    c.extract.countries_file = p("crates/vc-extract/data/countries.tsv.gz");
    c.extract.admin1_file = p("crates/vc-extract/data/admin1.tsv.gz");
    c.api.bind = format!("127.0.0.1:{}", free_port());
    c.api.metrics_bind = format!("127.0.0.1:{}", free_port());
    (c, addr)
}

/// Scrapes `addr` until `/metrics` carries `role`'s heartbeat (the role is up and beating)
/// and `/healthz` answers 200; fails if the role exits first or after 60 s.
async fn assert_serves_metrics(
    role: &str,
    addr: SocketAddr,
    pool: &PgPool,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let want = format!("crawler_heartbeat{{role=\"{role}\"}} 1");
    let start = Instant::now();
    let mut last = String::new();
    loop {
        assert!(
            !task.is_finished(),
            "{role} exited before serving /metrics: {:?}",
            task.await
        );
        if let Ok((200, body)) = vc_obs::server::get(addr, "/metrics").await {
            if body.contains(&want) {
                break;
            }
            last = body;
        }
        assert!(
            start.elapsed() < Duration::from_secs(60),
            "{role}: no {want} on {addr}/metrics; last body:\n{last}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (status, _) = vc_obs::server::get(addr, "/healthz")
        .await
        .expect("/healthz");
    assert_eq!(status, 200, "{role} /healthz");
    // The heartbeat is in Postgres too (crawler-down alert, `vcrawl admin status`).
    let (beats,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM heartbeats WHERE role = $1 AND instance = $2")
            .bind(role)
            .bind(format!("{role}-test"))
            .fetch_one(pool)
            .await
            .expect("heartbeats");
    assert_eq!(beats, 1, "{role} heartbeat row");
    task.abort();
}

async fn setup(role: &str) -> (tempfile::TempDir, PgPool, Ctx, SocketAddr) {
    let (pool, opts) = testkit::pg_with_url().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (c, addr) = cfg(tmp.path(), &opts, &format!("{role}-test"));
    let ctx = Ctx::new(c).with_pool(pool.clone());
    (tmp, pool, ctx, addr)
}

#[tokio::test]
async fn scheduler_serves_metrics() {
    let (_tmp, pool, ctx, addr) = setup("scheduler").await;
    let task = tokio::spawn(vcrawl::roles::scheduler::run(ctx));
    assert_serves_metrics("scheduler", addr, &pool, task).await;
}

#[tokio::test]
async fn fetcher_serves_metrics() {
    let (_tmp, pool, ctx, addr) = setup("fetcher").await;
    let task = tokio::spawn(vcrawl::roles::fetcher::run(ctx));
    assert_serves_metrics("fetcher", addr, &pool, task).await;
}

#[tokio::test]
async fn processor_serves_metrics() {
    let (_tmp, pool, ctx, addr) = setup("processor").await;
    let task = tokio::spawn(vcrawl::roles::processor::run(ctx));
    assert_serves_metrics("processor", addr, &pool, task).await;
}

#[tokio::test]
async fn embedder_serves_metrics_while_the_llm_is_off() {
    let (_tmp, pool, ctx, addr) = setup("embedder").await;
    assert!(!ctx.cfg.llm.enabled);
    let task = tokio::spawn(vcrawl::roles::embedder::run(ctx));
    assert_serves_metrics("embedder", addr, &pool, task).await;
}

#[tokio::test]
async fn api_serves_metrics_on_the_obs_address() {
    let (tmp, pool, ctx, addr) = setup("api").await;
    let tls = tmp.path().join("tls");
    std::fs::create_dir_all(&tls).unwrap();
    let fixtures = root().join("crates/vc-api/tests/fixtures/tls");
    std::fs::copy(fixtures.join("a.crt"), tls.join(vc_api::tls::CERT_FILE)).unwrap();
    std::fs::copy(fixtures.join("a.key"), tls.join(vc_api::tls::KEY_FILE)).unwrap();
    assert_ne!(
        ctx.cfg.obs.metrics_addr, ctx.cfg.api.metrics_bind,
        "the crawler's /metrics is not vc-api's localhost counter listener"
    );
    let task = tokio::spawn(async move { vcrawl::roles::api::run_with_tls_dir(ctx, &tls).await });
    assert_serves_metrics("api", addr, &pool, task).await;
}

#[tokio::test]
async fn maintenance_serves_metrics() {
    let (_tmp, pool, ctx, addr) = setup("maintenance").await;
    let task = tokio::spawn(vcrawl::roles::maintenance::run(ctx));
    assert_serves_metrics("maintenance", addr, &pool, task).await;
}
