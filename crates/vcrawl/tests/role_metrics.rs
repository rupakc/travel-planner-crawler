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

/// The API's TLS pair (vc-api's fixtures) in `<tmp>/tls`.
fn tls_dir(tmp: &Path) -> PathBuf {
    let tls = tmp.join("tls");
    std::fs::create_dir_all(&tls).unwrap();
    let fixtures = root().join("crates/vc-api/tests/fixtures/tls");
    std::fs::copy(fixtures.join("a.crt"), tls.join(vc_api::tls::CERT_FILE)).unwrap();
    std::fs::copy(fixtures.join("a.key"), tls.join(vc_api::tls::KEY_FILE)).unwrap();
    tls
}

/// Scrapes `addr/metrics` until every role in `roles` has a fresh heartbeat series.
async fn wait_for_heartbeats(addr: SocketAddr, roles: &[&str]) {
    let start = Instant::now();
    let mut last = String::new();
    loop {
        if let Ok((200, body)) = vc_obs::server::get(addr, "/metrics").await {
            if roles
                .iter()
                .all(|r| body.contains(&format!("crawler_heartbeat{{role=\"{r}\"}} 1")))
            {
                return;
            }
            last = body;
        }
        assert!(
            start.elapsed() < Duration::from_secs(90),
            "not every heartbeat of {roles:?} on {addr}/metrics; last body:\n{last}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// `GET https://<api.bind>/healthz` (the MIG autohealing check; self-signed test cert).
async fn api_healthz(bind: &str) -> u16 {
    let http = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .expect("client");
    let port = bind.rsplit(':').next().unwrap();
    let start = Instant::now();
    loop {
        if let Ok(r) = http
            .get(format!("https://127.0.0.1:{port}/healthz"))
            .send()
            .await
        {
            return r.status().as_u16();
        }
        assert!(start.elapsed() < Duration::from_secs(30), "api not serving");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const ALL_ROLES: [&str; 6] = [
    "scheduler",
    "fetcher",
    "processor",
    "embedder",
    "api",
    "maintenance",
];

#[tokio::test]
async fn run_starts_every_role_in_one_process_and_each_writes_its_heartbeat() {
    let (tmp, pool, ctx, addr) = setup("run").await;
    let api_bind = ctx.cfg.api.bind.clone();
    let opts = vcrawl::run::RunOptions {
        tls_dir: tls_dir(tmp.path()),
        ..Default::default()
    };
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(vcrawl::run::run_until(
        ctx,
        vcrawl::run::RoleName::ALL.to_vec(),
        opts,
        rx,
    ));
    // One listener for the whole process: every role's heartbeat on the same /metrics.
    wait_for_heartbeats(addr, &ALL_ROLES).await;
    let (status, body) = vc_obs::server::get(addr, "/healthz")
        .await
        .expect("/healthz");
    assert_eq!(status, 200, "{body}");
    let h: serde_json::Value = serde_json::from_str(&body).expect("health json");
    assert_eq!(h["healthy"], true);
    for r in ALL_ROLES {
        assert_eq!(h["roles"][r]["up"], true, "{r}: {body}");
    }
    // The API's TLS /healthz (port 8443 in production) reports the healthy process.
    assert_eq!(api_healthz(&api_bind).await, 200);
    // One heartbeat row per role, all for this instance.
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT role FROM heartbeats WHERE instance = 'run-test' ORDER BY 1")
            .fetch_all(&pool)
            .await
            .unwrap();
    let mut want: Vec<&str> = ALL_ROLES.to_vec();
    want.sort_unstable();
    assert_eq!(
        rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
        want,
        "heartbeat rows"
    );
    // A shutdown ends every role within the grace period.
    let t = Instant::now();
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("run stops within the grace period")
        .expect("task")
        .expect("clean stop");
    assert!(t.elapsed() < Duration::from_secs(26), "{:?}", t.elapsed());
    assert!(
        vc_obs::server::get(addr, "/metrics").await.is_err(),
        "the listener is closed after shutdown"
    );
}

#[tokio::test]
async fn run_keeps_the_other_roles_up_when_one_fails_and_reports_it() {
    let (tmp, pool, ctx, addr) = setup("run").await;
    // The processor cannot load its kind registry: it fails (and is retried); the others
    // keep running, and the API's TLS /healthz reports the process unhealthy.
    let mut c = (*ctx.cfg).clone();
    c.kinds.kinds_toml = tmp.path().join("missing-kinds.toml").display().to_string();
    let api_bind = c.api.bind.clone();
    let ctx = Ctx::new(c).with_pool(pool.clone());
    let opts = vcrawl::run::RunOptions {
        tls_dir: tls_dir(tmp.path()),
        backoff: Duration::from_millis(200),
        max_backoff: Duration::from_secs(1),
    };
    let (tx, rx) = tokio::sync::watch::channel(false);
    let task = tokio::spawn(vcrawl::run::run_until(
        ctx,
        vcrawl::run::parse_roles("scheduler,processor,api").unwrap(),
        opts,
        rx,
    ));
    wait_for_heartbeats(addr, &["scheduler"]).await;
    let start = Instant::now();
    let body = loop {
        let (status, body) = vc_obs::server::get(addr, "/healthz")
            .await
            .expect("/healthz");
        let h: serde_json::Value = serde_json::from_str(&body).expect("json");
        if status == 503 && h["roles"]["processor"]["up"] == false {
            break body;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{status} {body}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let h: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(h["healthy"], false);
    assert_eq!(h["roles"]["scheduler"]["up"], true, "{body}");
    assert_eq!(h["roles"]["api"]["up"], true, "{body}");
    let metrics = vc_obs::server::get(addr, "/metrics").await.unwrap().1;
    assert!(
        !metrics.contains("crawler_heartbeat{role=\"processor\"}"),
        "{metrics}"
    );
    assert_eq!(
        api_healthz(&api_bind).await,
        503,
        "TLS /healthz of an unhealthy process"
    );
    tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_secs(30), task)
        .await
        .expect("stops")
        .expect("task")
        .expect("clean stop");
}

/// `vcrawl run` as a real process: it serves every chosen role, and SIGTERM ends it with exit
/// code 0 well inside compose's 30 s `stop_grace_period`.
#[tokio::test]
async fn the_binary_stops_cleanly_on_sigterm() {
    let (pool, opts) = testkit::pg_with_url().await;
    let tmp = tempfile::tempdir().expect("tempdir");
    let (c, addr) = cfg(tmp.path(), &opts, "sigterm-test");
    let r = root();
    let p = |rel: &str| r.join(rel).display().to_string();
    // `VC_*_FILE` names secret files, so the place-data paths go through a TOML file.
    let toml = tmp.path().join("vcrawl.toml");
    std::fs::write(
        &toml,
        format!(
            "[extract]\nplaces_file = {:?}\ncountries_file = {:?}\nadmin1_file = {:?}\n",
            p("crates/vc-extract/data/places.tsv.gz"),
            p("crates/vc-extract/data/countries.tsv.gz"),
            p("crates/vc-extract/data/admin1.tsv.gz"),
        ),
    )
    .unwrap();
    let log = tmp.path().join("vcrawl.log");
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_vcrawl"));
    cmd.args([
        "run",
        "--roles",
        "scheduler,fetcher,processor,embedder,maintenance",
    ])
    .env_clear()
    .env("PATH", std::env::var("PATH").unwrap_or_default())
    .env("VC_DATABASE_URL", &c.database.url)
    .env("VC_DATABASE_PASSWORD_SECRET", "")
    .env("VC_SECRETS_BACKEND", "env")
    .env("VC_OBS_METRICS_ADDR", addr.to_string())
    .env("VC_OBS_HEARTBEAT_S", "1")
    .env("VC_OBS_LOG_FORMAT", "pretty")
    .env("VC_VCRAWL_INSTANCE", "sigterm-test")
    .env("VC_VCRAWL_DATA_DIR", &c.vcrawl.data_dir)
    .env("VC_STORE_BACKEND", "fs")
    .env("VC_STORE_FS_ROOT", &c.store.fs_root)
    .env("VC_STORE_SPOOL_DIR", &c.store.spool_dir)
    .env("VC_RENDER_ENABLED", "false")
    .env("VC_RENDER_CACHE_DIR", &c.render.cache_dir)
    .env("VC_SOURCES_DIR", &c.sources.dir)
    .env("VC_POLICY_DENYLIST_PATH", p("policy/denylist.yaml"))
    .env(
        "VC_POLICY_SUBRESOURCE_ALLOWLIST_PATH",
        p("policy/subresource_allowlist.yaml"),
    )
    .env("VC_KINDS_KINDS_TOML", p("kinds.toml"))
    .env("VC_KINDS_SCHEMAS_DIR", p("schemas"))
    .env("VC_KINDS_PROMPTS_DIR", p("prompts"))
    .env("VC_CONFIG", &toml)
    .stdout(std::fs::File::create(&log).expect("log file"))
    .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn vcrawl run");
    let roles = [
        "scheduler",
        "fetcher",
        "processor",
        "embedder",
        "maintenance",
    ];
    let up = tokio::time::timeout(Duration::from_secs(90), wait_for_heartbeats(addr, &roles)).await;
    if up.is_err() {
        let _ = child.kill();
        let out = child.wait_with_output().expect("output");
        panic!(
            "vcrawl run did not come up:\n{}\n{}",
            String::from_utf8_lossy(&out.stderr),
            std::fs::read_to_string(&log).unwrap_or_default()
        );
    }
    let (beats,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM heartbeats WHERE instance = 'sigterm-test'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(beats, 5, "one heartbeat row per role");
    let t = Instant::now();
    let kill = std::process::Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(kill.success());
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break s;
        }
        assert!(
            t.elapsed() < Duration::from_secs(25),
            "vcrawl run still running 25 s after SIGTERM (the host stops it with -t 25)"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        use std::io::Read;
        let _ = e.read_to_string(&mut err);
    }
    let out = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(status.success(), "exit {status:?}; stderr:\n{err}\n{out}");
    assert!(out.contains("vcrawl run: stopped"), "log:\n{out}");
}
