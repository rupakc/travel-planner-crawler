//! Talking to the stack: the fake web's admin endpoints, Postgres, the fake GCS bucket and
//! the API. Everything here gathers evidence; [`crate::checks`] judges it.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use testkit::fake_gcs::FakeGcsStore;
use testkit::oracle::PolicySnapshot;
use testkit::request_log::RequestLog;
use vc_core::store::BlobStore;

use crate::checks::{ApiEvidence, CHANGING_PAGES, ParkRow, VersionRow};

/// Settings, all from the environment (`e2e/compose.yaml` sets them).
#[derive(Debug, Clone)]
pub struct Env {
    /// `E2E_DATABASE_URL`.
    pub database_url: String,
    /// `E2E_FAKE_WEB` (admin base URL of the fake web).
    pub fake_web: String,
    /// `E2E_TRAP` (admin base URL of the private-IP trap).
    pub trap: String,
    /// `E2E_TRAP_METADATA` (admin base URL of the metadata-IP trap).
    pub trap_metadata: String,
    /// `E2E_GCS` (fake-gcs-server base URL).
    pub gcs: String,
    /// `E2E_BUCKETS` (comma-separated; the first is the raw bucket).
    pub buckets: Vec<String>,
    /// `E2E_API` (`https://api:8443`).
    pub api: String,
    /// `E2E_API_CA` (PEM path).
    pub api_ca: String,
    /// `E2E_API_TOKEN` (`<client_id>.<secret>`).
    pub api_token: String,
}

fn var(k: &str) -> Result<String> {
    std::env::var(k).with_context(|| format!("{k} is not set"))
}

impl Env {
    /// Reads the environment.
    pub fn from_env() -> Result<Env> {
        Ok(Env {
            database_url: var("E2E_DATABASE_URL")?,
            fake_web: var("E2E_FAKE_WEB")?.trim_end_matches('/').into(),
            trap: var("E2E_TRAP")?.trim_end_matches('/').into(),
            trap_metadata: var("E2E_TRAP_METADATA")?.trim_end_matches('/').into(),
            gcs: var("E2E_GCS")?.trim_end_matches('/').into(),
            buckets: var("E2E_BUCKETS")?
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            api: var("E2E_API")?.trim_end_matches('/').into(),
            api_ca: var("E2E_API_CA")?,
            api_token: var("E2E_API_TOKEN")?,
        })
    }

    /// The raw bucket.
    pub fn raw_bucket(&self) -> &str {
        self.buckets
            .first()
            .map(String::as_str)
            .unwrap_or("e2e-raw")
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .expect("reqwest client")
}

/// Polls `f` every second until it succeeds or `within` passes.
pub async fn wait_for<T, F, Fut>(what: &str, within: Duration, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let deadline = tokio::time::Instant::now() + within;
    loop {
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if tokio::time::Instant::now() >= deadline => {
                return Err(e.context(format!("{what}: gave up after {within:?}")));
            }
            Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
}

/// The fake web's (or the trap's) admin endpoints.
#[derive(Debug, Clone)]
pub struct FakeWebAdmin {
    base: String,
}

impl FakeWebAdmin {
    /// `base` = `http://fake-web`.
    pub fn new(base: &str) -> Self {
        FakeWebAdmin { base: base.into() }
    }

    /// `GET /__admin/health`.
    pub async fn health(&self) -> Result<()> {
        http()
            .get(format!("{}/__admin/health", self.base))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    /// The request log (`GET /__admin/log`, JSONL).
    pub async fn log(&self) -> Result<RequestLog> {
        let body = http()
            .get(format!("{}/__admin/log", self.base))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        RequestLog::from_jsonl(&body).context("parse request log")
    }

    /// What the crawler was served (`GET /__admin/snapshot`).
    pub async fn snapshot(&self) -> Result<PolicySnapshot> {
        Ok(http()
            .get(format!("{}/__admin/snapshot", self.base))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?)
    }

    /// Moves the scripted sites to cycle `n` (`POST /__admin/cycle`).
    pub async fn set_cycle(&self, n: usize) -> Result<()> {
        http()
            .post(format!("{}/__admin/cycle", self.base))
            .json(&serde_json::json!({ "cycle": n }))
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}

/// Connects to Postgres (waits for it).
pub async fn pg(url: &str) -> Result<PgPool> {
    wait_for("postgres", Duration::from_secs(90), || async {
        Ok(PgPoolOptions::new().max_connections(4).connect(url).await?)
    })
    .await
}

/// Whether a table exists (the schema may predate a role's migrations).
pub async fn table_exists(pool: &PgPool, name: &str) -> Result<bool> {
    let (b,): (bool,) = sqlx::query_as("SELECT to_regclass($1) IS NOT NULL")
        .bind(name)
        .fetch_one(pool)
        .await?;
    Ok(b)
}

/// Makes every due URL due now: the "time passes" between crawl cycles (recrawl cadences
/// are an hour at least). Leased URLs and parked keys are left alone; the frontier still
/// decides when each key may be requested.
pub async fn make_due_now(pool: &PgPool) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE url_state SET next_fetch_at = now() WHERE state = 'due' AND next_fetch_at > now()",
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Makes never-fetched due URLs due now: newly discovered links (sitemap, feed, page links)
/// are scheduled ahead in time, and a cycle lasts minutes, not hours. Fetched URLs keep their
/// recrawl time until the next cycle ([`make_due_now`]).
pub async fn make_new_due_now(pool: &PgPool) -> Result<u64> {
    let r = sqlx::query(
        "UPDATE url_state SET next_fetch_at = now()
         WHERE state = 'due' AND fetch_count = 0 AND next_fetch_at > now()",
    )
    .execute(pool)
    .await?;
    Ok(r.rows_affected())
}

/// Whether every changing page has a committed primary capture at or after `since` and the
/// processor has nothing pending: the cycle's new content is captured **and** processed.
pub async fn changing_pages_processed(
    pool: &PgPool,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<bool> {
    let urls: Vec<String> = CHANGING_PAGES.iter().map(|s| s.to_string()).collect();
    let (n,): (i64,) = sqlx::query_as(
        "SELECT count(DISTINCT url) FROM captures
         WHERE role = 'primary' AND url = ANY($1) AND captured_at >= $2",
    )
    .bind(&urls)
    .bind(since)
    .fetch_one(pool)
    .await?;
    Ok(n == i64::try_from(urls.len())? && backlog(pool).await? == 0)
}

/// `(url, captured_at, status, payload hash prefix, revisit, versions built)`.
type CaptureDiag = (
    String,
    chrono::DateTime<chrono::Utc>,
    Option<i16>,
    Option<String>,
    bool,
    i64,
);

/// Per changing page: its primary captures and the record versions built from them (URLs,
/// times, hashes and states only), for a `version_bump` failure.
pub async fn changing_pages_diagnostic(pool: &PgPool) -> Result<Vec<String>> {
    let urls: Vec<String> = CHANGING_PAGES.iter().map(|s| s.to_string()).collect();
    let rows: Vec<CaptureDiag> = sqlx::query_as(
        "SELECT c.url, c.captured_at, c.status, left(encode(c.payload_hash, 'hex'), 12),
                    c.revisit_of_id IS NOT NULL,
                    (SELECT count(*) FROM record_versions rv
                      WHERE rv.capture_id = c.capture_id AND rv.captured_at = c.captured_at)
             FROM captures c WHERE c.role = 'primary' AND c.url = ANY($1) ORDER BY 1, 2",
    )
    .bind(&urls)
    .fetch_all(pool)
    .await?;
    let mut out: Vec<String> = rows
        .into_iter()
        .map(|(u, at, st, h, rev, nv)| {
            format!(
                "capture {u} at {at} status {st:?} payload {h:?} revisit {rev} -> {nv} version(s)"
            )
        })
        .collect();
    let items: Vec<(String, String, i64, Option<String>)> = sqlx::query_as(
        "SELECT kind, state, count(*), left(max(last_error), 200) FROM work_items
         GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?;
    for (k, st, n, err) in items {
        out.push(format!("work_items {k} {st}: {n} (last error {err:?})"));
    }
    Ok(out)
}

/// Ready or leased work items (the processor's backlog).
pub async fn backlog(pool: &PgPool) -> Result<i64> {
    let (n,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM work_items WHERE state IN ('ready', 'leased')")
            .fetch_one(pool)
            .await?;
    Ok(n)
}

/// The hostile key's park row.
pub async fn park_row(pool: &PgPool, key: &str) -> Result<Option<ParkRow>> {
    let row: Option<(Option<chrono::DateTime<chrono::Utc>>, Option<String>)> =
        sqlx::query_as("SELECT parked_until, park_reason FROM politeness_keys WHERE key = $1")
            .bind(key)
            .fetch_optional(pool)
            .await?;
    Ok(row.map(|(parked_until, park_reason)| ParkRow {
        parked_until,
        park_reason,
    }))
}

/// Database time (the park check compares with it, not the runner's clock).
pub async fn db_now(pool: &PgPool) -> Result<chrono::DateTime<chrono::Utc>> {
    let (t,): (chrono::DateTime<chrono::Utc>,) =
        sqlx::query_as("SELECT now()").fetch_one(pool).await?;
    Ok(t)
}

/// Versions of the records built from the pages that change in cycle 1.
pub async fn changing_versions(pool: &PgPool) -> Result<Vec<VersionRow>> {
    let urls: Vec<String> = CHANGING_PAGES.iter().map(|s| s.to_string()).collect();
    let rows: Vec<(i64, String, i32, Vec<u8>, String)> = sqlx::query_as(
        "SELECT rv.record_id, c.url, rv.version, rv.data_hash, rv.capture_id
         FROM record_versions rv
         JOIN captures c ON c.capture_id = rv.capture_id AND c.captured_at = rv.captured_at
         WHERE c.url = ANY($1)
         ORDER BY 1, 3",
    )
    .bind(&urls)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(record_id, url, version, data_hash, capture_id)| VersionRow {
                record_id,
                url,
                version,
                data_hash,
                capture_id,
            },
        )
        .collect())
}

/// `(captures, segments, record_versions)` counts.
pub async fn raw_counts(pool: &PgPool) -> Result<(i64, i64, i64)> {
    let (c, s, v): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM captures), (SELECT count(*) FROM segments),
                (SELECT count(*) FROM record_versions)",
    )
    .fetch_one(pool)
    .await?;
    Ok((c, s, v))
}

/// The raw bucket as a [`BlobStore`].
pub fn raw_store(env: &Env) -> FakeGcsStore {
    FakeGcsStore::new(&env.gcs, env.raw_bucket())
}

/// The stored WARC records of every primary render capture of `host`, as raw bytes.
pub async fn render_captures(
    pool: &PgPool,
    store: &dyn BlobStore,
    host: &str,
) -> Result<Vec<(String, Vec<u8>)>> {
    let rows: Vec<(String, String, i64, i64)> = sqlx::query_as(
        "SELECT c.url, s.path, c.warc_offset, c.warc_length
         FROM captures c JOIN segments s ON s.segment_id = c.segment_id
         WHERE c.role = 'primary' AND c.fetch_mode = 'render' AND c.url LIKE $1
         ORDER BY c.captured_at",
    )
    .bind(format!("http://{host}/%"))
    .fetch_all(pool)
    .await?;
    let mut out = Vec::new();
    for (url, path, off, len) in rows {
        let recs =
            vc_store::segment::read_capture(store, &path, u64::try_from(off)?, u64::try_from(len)?)
                .await
                .map_err(|e| anyhow!("read capture {url} from {path}: {e}"))?;
        let mut bytes = Vec::new();
        for r in recs {
            bytes.extend_from_slice(&r.block);
        }
        out.push((url, bytes));
    }
    Ok(out)
}

/// Distinct fetcher instances that heartbeat.
pub async fn fetcher_instances(pool: &PgPool) -> Result<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT instance FROM heartbeats WHERE role = 'fetcher' ORDER BY 1",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().map(|r| r.0).collect())
}

/// Creates the buckets (idempotent: 409 = exists).
pub async fn create_buckets(env: &Env) -> Result<()> {
    for b in &env.buckets {
        let store = FakeGcsStore::new(&env.gcs, b);
        wait_for("fake-gcs bucket", Duration::from_secs(60), || async {
            store
                .create_bucket()
                .await
                .map_err(|e| anyhow!("create bucket {b}: {e}"))
        })
        .await?;
    }
    Ok(())
}

/// Registers the E2E API client (token `E2E_API_TOKEN`).
pub async fn put_api_client(pool: &PgPool, token: &str) -> Result<()> {
    let (client_id, _) = vc_api::auth::split_token(token)
        .ok_or_else(|| anyhow!("E2E_API_TOKEN must be <client_id>.<secret>"))?;
    let hash = vc_api::auth::hash_token(token).map_err(|e| anyhow!("hash token: {e}"))?;
    vc_api::pg::PgApiStore::new(pool.clone())
        .put_client(&vc_api::auth::StoredClient {
            client_id: client_id.to_string(),
            name: "e2e".into(),
            token_hash: hash,
            licence_classes: vec![
                vc_core::licence::LicenceClass::Open,
                vc_core::licence::LicenceClass::ShareAlike,
                vc_core::licence::LicenceClass::FactsOnly,
                vc_core::licence::LicenceClass::ApiTerms,
            ],
            rate_per_min: 600,
            enabled: true,
        })
        .await
        .map_err(|e| anyhow!("put api client: {e}"))
}

/// Queries the API three ways: with the E2E CA and the token, without the token, and with
/// a client that trusts only the system roots (must refuse our certificate).
pub async fn api_evidence(env: &Env) -> Result<ApiEvidence> {
    let ca_pem = std::fs::read(&env.api_ca).with_context(|| format!("read {}", env.api_ca))?;
    let ca = reqwest::Certificate::from_pem(&ca_pem).context("parse E2E CA")?;
    let trusted = reqwest::Client::builder()
        .add_root_certificate(ca)
        .https_only(true)
        .timeout(Duration::from_secs(20))
        .build()?;
    let url = format!("{}/v1/search?q=Kyoto&limit=20", env.api);

    let resp = wait_for("api", Duration::from_secs(60), || async {
        Ok(trusted.get(&url).bearer_auth(&env.api_token).send().await?)
    })
    .await?;
    let status = resp.status();
    let search: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        bail!("/v1/search answered {status}: {search}");
    }

    let unauthenticated_status = trusted.get(&url).send().await?.status().as_u16();

    let untrusted = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let untrusted_handshake_refused = match untrusted.get(&url).send().await {
        Ok(_) => false,
        Err(e) => e.is_connect() || e.is_request(),
    };
    Ok(ApiEvidence {
        search,
        unauthenticated_status,
        untrusted_handshake_refused,
    })
}
