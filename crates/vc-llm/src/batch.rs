//! `VertexBatchRunner`: the batch-prediction lifecycle (spec §17.3).
//!
//! 1. **prepare** writes `<batch_prefix><id>/input.jsonl` to the batch bucket, one line per item:
//!    `{"key": "<work_id>", "request": <generateContent body>}` (the request also carries the
//!    key as label `vc_key`, so results map even if Vertex drops unknown top-level fields), then
//!    inserts a `prepared` row holding the batch reservation.
//! 2. **submit** lists Vertex jobs labelled `batch_id=<id>` first and adopts one if it exists;
//!    otherwise creates the job (display name = row id, labels `{app, purpose, batch_id}`) and
//!    marks the row `submitted`.
//! 3. **reconcile** lists jobs labelled `app=voyager-crawler`, adopts any whose row lacks a job
//!    name (crash between create and row update), records finished jobs, and cancels jobs
//!    older than the timeout (72 h).
//! 4. **ingest** maps output lines by key; failed or absent keys are returned as `missing` for
//!    the caller to nack; settles the reservation with the actual (batch-price) usage.
//!
//! ```text
//! POST {base}/v1/projects/{p}/locations/{loc}/batchPredictionJobs
//! { "displayName": "<id>", "model": "publishers/google/models/<m>",
//!   "inputConfig":  {"instancesFormat": "jsonl", "gcsSource": {"uris": ["gs://…/input.jsonl"]}},
//!   "outputConfig": {"predictionsFormat": "jsonl",
//!                    "gcsDestination": {"outputUriPrefix": "gs://…/output"}},
//!   "labels": {"app": "voyager-crawler", "purpose": "extraction", "batch_id": "<id lowercase>"} }
//! GET  {base}/v1/{name}        GET {base}/v1/projects/{p}/locations/{loc}/batchPredictionJobs?filter=labels.app=voyager-crawler
//! POST {base}/v1/{name}:cancel
//! ```
//! Output lines: `{"status": "", "processed_time": …, "request": {…}, "response": {…}}`; a
//! failed line has a non-empty `status` and no usable response.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::PgPool;
use vc_config::LlmConfig;
use vc_core::llm::{
    BatchIngest, BatchItem, BatchRunner, BudgetLedger, LlmError, LlmResponse, ReconcileReport,
    Reservation, Usage, Usd,
};
use vc_core::store::BlobStore;
use vc_core::{BatchJobId, Clock, Error, ModelId, Result, WorkId};

use crate::client::{APP_LABEL, VertexClient, labels};
use crate::crc::crc32c;
use crate::generate::{generate_body, parse_response};
use crate::ledger::PgLedger;
use crate::pricing::Pricing;

/// The per-line label carrying the work id.
pub const KEY_LABEL: &str = "vc_key";

fn db(e: sqlx::Error) -> Error {
    Error::Db(e.to_string())
}

/// Vertex job states that mean "done, results available".
fn is_succeeded(s: &str) -> bool {
    matches!(s, "JOB_STATE_SUCCEEDED" | "JOB_STATE_PARTIALLY_SUCCEEDED")
}

/// Vertex job states that mean "done, failed".
fn is_failed(s: &str) -> bool {
    matches!(
        s,
        "JOB_STATE_FAILED" | "JOB_STATE_CANCELLED" | "JOB_STATE_EXPIRED"
    )
}

#[derive(Debug)]
struct Row {
    state: String,
    purpose: String,
    model: String,
    input_uri: String,
    output_uri: Option<String>,
    vertex_job_name: Option<String>,
    reservation_id: String,
    error: Option<String>,
}

type RowTuple = (
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
);

/// The Vertex batch-prediction runner. `blobs` is the blob store of `llm.batch_bucket`.
#[derive(Clone)]
pub struct VertexBatchRunner {
    /// Pool.
    pub pool: PgPool,
    /// Settings.
    pub config: LlmConfig,
    vertex: VertexClient,
    blobs: Arc<dyn BlobStore>,
    ledger: PgLedger,
    clock: Arc<dyn Clock>,
    pricing: Pricing,
}

impl std::fmt::Debug for VertexBatchRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VertexBatchRunner")
            .field("bucket", &self.config.batch_bucket)
            .finish_non_exhaustive()
    }
}

impl VertexBatchRunner {
    /// A runner writing batch I/O through `blobs` (bound to `config.batch_bucket`) and settling
    /// through `ledger`.
    pub fn new(
        pool: PgPool,
        config: LlmConfig,
        vertex: VertexClient,
        blobs: Arc<dyn BlobStore>,
        ledger: PgLedger,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let pricing = Pricing::from_config(&config);
        VertexBatchRunner {
            pool,
            config,
            vertex,
            blobs,
            ledger,
            clock,
            pricing,
        }
    }

    fn disabled(&self) -> Result<()> {
        if self.config.enabled {
            Ok(())
        } else {
            Err(LlmError::Disabled.into())
        }
    }

    /// The batch-price estimate of `items` (for `reserve_batch`).
    pub fn estimate(&self, items: &[BatchItem]) -> Usd {
        items.iter().fold(Usd::ZERO, |acc, i| {
            acc.saturating_add(self.pricing.estimate(&i.request, true))
        })
    }

    fn gs(&self, path: &str) -> String {
        format!("gs://{}/{}", self.config.batch_bucket, path)
    }

    fn path_of(&self, uri: &str) -> Result<String> {
        let prefix = format!("gs://{}/", self.config.batch_bucket);
        uri.strip_prefix(&prefix)
            .map(str::to_string)
            .ok_or_else(|| Error::Invalid(format!("not in the batch bucket: {uri}")))
    }

    fn jobs_url(&self) -> String {
        format!(
            "{}/batchPredictionJobs",
            self.vertex.location_url(&self.config.location)
        )
    }

    async fn load(&self, id: &BatchJobId) -> Result<Row> {
        let r: Option<RowTuple> = sqlx::query_as(
            "SELECT state, purpose, model, input_uri, output_uri, vertex_job_name,
                    reservation_id, error
             FROM batch_jobs WHERE id = $1",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await
        .map_err(db)?;
        let t = r.ok_or_else(|| Error::NotFound(format!("batch job {id}")))?;
        Ok(Row {
            state: t.0,
            purpose: t.1,
            model: t.2,
            input_uri: t.3,
            output_uri: t.4,
            vertex_job_name: t.5,
            reservation_id: t.6,
            error: t.7,
        })
    }

    /// Lists our Vertex jobs matching `filter` (all pages).
    async fn list_jobs(&self, filter: &str) -> Result<Vec<Value>> {
        let url = self.jobs_url();
        let mut out = Vec::new();
        let mut token = String::new();
        for _ in 0..1000 {
            let mut q = vec![("filter", filter), ("pageSize", "100")];
            if !token.is_empty() {
                q.push(("pageToken", token.as_str()));
            }
            let v = self.vertex.call(Method::GET, &url, &q, None).await?;
            if let Some(jobs) = v.get("batchPredictionJobs").and_then(Value::as_array) {
                out.extend(jobs.iter().cloned());
            }
            match v.get("nextPageToken").and_then(Value::as_str) {
                Some(t) if !t.is_empty() => token = t.to_string(),
                _ => break,
            }
        }
        Ok(out)
    }

    async fn input_keys(&self, row: &Row) -> Result<Vec<WorkId>> {
        let bytes = self.blobs.get(&self.path_of(&row.input_uri)?, None).await?;
        let mut keys = Vec::new();
        for line in bytes.split(|b| *b == b'\n') {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let v: Value = serde_json::from_slice(line)?;
            if let Some(k) = key_of(&v) {
                keys.push(k);
            }
        }
        Ok(keys)
    }

    async fn settle(&self, row: &Row, usage: Usage) -> Result<()> {
        let id = row
            .reservation_id
            .parse()
            .map_err(|_| Error::Invalid("bad reservation id".into()))?;
        if let Some(r) = self.ledger.reservation(id).await? {
            self.ledger.settle(r, usage).await?;
        }
        Ok(())
    }

    /// Reads every output line under the job's output directory.
    async fn output_lines(&self, row: &Row) -> Result<Vec<Value>> {
        let Some(dir) = &row.output_uri else {
            return Ok(Vec::new());
        };
        let prefix = self.path_of(dir.trim_end_matches('/'))?;
        let mut lines = Vec::new();
        for path in self.blobs.list(&format!("{prefix}/")).await? {
            if !path.ends_with(".jsonl") {
                continue;
            }
            let bytes = self.blobs.get(&path, None).await?;
            for line in bytes.split(|b| *b == b'\n') {
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                match serde_json::from_slice::<Value>(line) {
                    Ok(v) => lines.push(v),
                    Err(_) => tracing::warn!("unparseable batch output line skipped"),
                }
            }
        }
        Ok(lines)
    }
}

/// The work id of an input or output line: top-level `key`, else the request's `vc_key` label.
pub fn key_of(line: &Value) -> Option<WorkId> {
    let parse = |v: &Value| -> Option<i64> {
        v.as_i64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    line.get("key")
        .and_then(parse)
        .or_else(|| {
            line.pointer(&format!("/request/labels/{KEY_LABEL}"))
                .and_then(parse)
        })
        .map(WorkId)
}

#[async_trait]
impl BatchRunner for VertexBatchRunner {
    async fn prepare(&self, r: Reservation, items: Vec<BatchItem>) -> Result<BatchJobId> {
        self.disabled()?;
        if !r.batch {
            return Err(Error::Invalid("batch jobs need a batch reservation".into()));
        }
        if items.is_empty() {
            return Err(Error::Invalid("empty batch".into()));
        }
        let model_of = |i: &BatchItem| {
            i.request
                .model
                .clone()
                .unwrap_or_else(|| ModelId::new(&self.config.model))
        };
        let model = model_of(&items[0]);
        if items.iter().any(|i| model_of(i) != model) {
            return Err(Error::Invalid("one model per batch job".into()));
        }
        let id = BatchJobId::at(self.clock.now());
        let mut jsonl = Vec::new();
        for it in &items {
            let mut l = labels(it.request.purpose);
            l.insert(KEY_LABEL.into(), it.key.to_string());
            let line = json!({
                "key": it.key.to_string(),
                "request": generate_body(&it.request, model.as_str(), &l),
            });
            serde_json::to_writer(&mut jsonl, &line)?;
            jsonl.push(b'\n');
        }
        let path = format!("{}{}/input.jsonl", self.config.batch_prefix, id);
        let crc = crc32c(&jsonl);
        self.blobs.put_if_absent(&path, jsonl.into(), crc).await?;
        sqlx::query(
            "INSERT INTO batch_jobs (id, state, purpose, model, input_uri, reservation_id,
                                     reserved_usd, request_count, created_at)
             VALUES ($1, 'prepared', $2, $3, $4, $5, $6::bigint / 1000000.0, $7, $8)",
        )
        .bind(id.to_string())
        .bind(r.purpose.as_str())
        .bind(model.as_str())
        .bind(self.gs(&path))
        .bind(r.id.to_string())
        .bind(r.est.micros())
        .bind(items.len() as i32)
        .bind(self.clock.now())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        Ok(id)
    }

    async fn submit(&self, id: &BatchJobId) -> Result<()> {
        self.disabled()?;
        let row = self.load(id).await?;
        if row.state != "prepared" || row.vertex_job_name.is_some() {
            return Ok(());
        }
        let label = id.to_string().to_ascii_lowercase();
        // Never resubmit without listing first: a crash may have left a created job behind.
        let existing = self
            .list_jobs(&format!("labels.batch_id={label}"))
            .await?
            .into_iter()
            .find_map(|j| j.get("name").and_then(Value::as_str).map(str::to_string));
        let name = match existing {
            Some(n) => n,
            None => {
                let mut l: BTreeMap<String, String> = labels(
                    vc_core::llm::Purpose::ALL
                        .into_iter()
                        .find(|p| p.as_str() == row.purpose)
                        .unwrap_or(vc_core::llm::Purpose::Extraction),
                );
                l.insert("batch_id".into(), label.clone());
                let out_prefix =
                    format!("{}/output", row.input_uri.trim_end_matches("/input.jsonl"));
                let body = json!({
                    "displayName": id.to_string(),
                    "model": format!("publishers/google/models/{}", row.model),
                    "inputConfig": {
                        "instancesFormat": "jsonl",
                        "gcsSource": {"uris": [row.input_uri]},
                    },
                    "outputConfig": {
                        "predictionsFormat": "jsonl",
                        "gcsDestination": {"outputUriPrefix": out_prefix},
                    },
                    "labels": l,
                });
                let v = self
                    .vertex
                    .call(Method::POST, &self.jobs_url(), &[], Some(&body))
                    .await?;
                v.get("name")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .ok_or_else(|| LlmError::InvalidResponse("batch create: no name".into()))?
            }
        };
        sqlx::query(
            "UPDATE batch_jobs SET state = 'submitted', vertex_job_name = $2, submitted_at = $3
             WHERE id = $1 AND state = 'prepared' AND vertex_job_name IS NULL",
        )
        .bind(id.to_string())
        .bind(&name)
        .bind(self.clock.now())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        tracing::info!(batch_id = %id, "batch job submitted");
        Ok(())
    }

    async fn reconcile(&self, timeout: Duration) -> Result<ReconcileReport> {
        self.disabled()?;
        let now = self.clock.now();
        let mut report = ReconcileReport::default();
        let jobs = self.list_jobs(&format!("labels.app={APP_LABEL}")).await?;
        let mut by_name: HashMap<String, Value> = HashMap::new();
        for j in jobs {
            let Some(name) = j.get("name").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            if let Some(bid) = j.pointer("/labels/batch_id").and_then(Value::as_str) {
                let adopted = sqlx::query(
                    "UPDATE batch_jobs
                     SET vertex_job_name = $2, state = 'submitted',
                         submitted_at = COALESCE(submitted_at, $3)
                     WHERE upper(id) = upper($1) AND state = 'prepared'
                       AND vertex_job_name IS NULL",
                )
                .bind(bid)
                .bind(&name)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(db)?
                .rows_affected();
                if adopted > 0 {
                    tracing::warn!(batch_id = bid, "orphaned batch job adopted");
                    report.adopted += adopted as u32;
                }
            }
            by_name.insert(name, j);
        }

        let open: Vec<(String, String, Option<DateTime<Utc>>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT id, vertex_job_name, submitted_at, created_at FROM batch_jobs
             WHERE state = 'submitted' AND vertex_job_name IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db)?;
        let timeout = chrono::Duration::from_std(timeout).unwrap_or(chrono::Duration::MAX);
        for (id, name, submitted_at, created_at) in open {
            let job = match by_name.get(&name) {
                Some(j) => j.clone(),
                None => {
                    let url = self.vertex.resource_url(&self.config.location, &name);
                    self.vertex.call(Method::GET, &url, &[], None).await?
                }
            };
            let state = job.get("state").and_then(Value::as_str).unwrap_or("");
            let bid: BatchJobId = id.parse()?;
            if is_succeeded(state) {
                let out = job
                    .pointer("/outputInfo/gcsOutputDirectory")
                    .and_then(Value::as_str);
                sqlx::query(
                    "UPDATE batch_jobs SET state = 'succeeded', output_uri = $2, finished_at = $3
                     WHERE id = $1 AND state = 'submitted'",
                )
                .bind(&id)
                .bind(out)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(db)?;
                report.finished.push(bid);
            } else if is_failed(state) {
                let err = job
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or(state)
                    .chars()
                    .take(300)
                    .collect::<String>();
                let out = job
                    .pointer("/outputInfo/gcsOutputDirectory")
                    .and_then(Value::as_str);
                sqlx::query(
                    "UPDATE batch_jobs SET state = 'failed', error = $2, output_uri = $3,
                                          finished_at = $4
                     WHERE id = $1 AND state = 'submitted'",
                )
                .bind(&id)
                .bind(err)
                .bind(out)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(db)?;
                report.finished.push(bid);
            } else if now - submitted_at.unwrap_or(created_at) >= timeout {
                if state != "JOB_STATE_CANCELLING" {
                    let url = format!(
                        "{}:cancel",
                        self.vertex.resource_url(&self.config.location, &name)
                    );
                    self.vertex
                        .call(Method::POST, &url, &[], Some(&json!({})))
                        .await?;
                }
                let out = job
                    .pointer("/outputInfo/gcsOutputDirectory")
                    .and_then(Value::as_str);
                sqlx::query(
                    "UPDATE batch_jobs SET state = 'failed', output_uri = $3, finished_at = $4,
                            error = 'cancelled after ' || $2 || ' h timeout'
                     WHERE id = $1 AND state = 'submitted'",
                )
                .bind(&id)
                .bind(timeout.num_hours().to_string())
                .bind(out)
                .bind(now)
                .execute(&self.pool)
                .await
                .map_err(db)?;
                tracing::warn!(batch_id = %bid, "batch job cancelled after timeout");
                report.cancelled += 1;
                report.finished.push(bid);
            }
        }
        Ok(report)
    }

    async fn ingest(&self, id: &BatchJobId) -> Result<BatchIngest> {
        self.disabled()?;
        let row = self.load(id).await?;
        match row.state.as_str() {
            "prepared" | "submitted" => return Ok(BatchIngest::Pending),
            _ => {}
        }
        let keys = self.input_keys(&row).await?;
        let wanted: BTreeSet<WorkId> = keys.iter().copied().collect();
        let model = ModelId::new(&row.model);
        let mut usage = Usage::default();
        let mut results: Vec<(WorkId, LlmResponse)> = Vec::new();
        let mut seen: BTreeSet<WorkId> = BTreeSet::new();
        for line in self.output_lines(&row).await? {
            let Some(response) = line.get("response").filter(|r| r.is_object()) else {
                continue;
            };
            let want_json = line
                .pointer("/request/generationConfig/responseSchema")
                .is_some();
            let resp = parse_response(response, &model, want_json, &self.pricing, true);
            usage.input_tokens += resp.usage.input_tokens;
            usage.output_tokens += resp.usage.output_tokens;
            usage.thinking_tokens += resp.usage.thinking_tokens;
            usage.cached_input_tokens += resp.usage.cached_input_tokens;
            usage.cost = usage.cost.saturating_add(resp.usage.cost);
            let status_ok = line
                .get("status")
                .map(|s| s.is_null() || s.as_str().is_some_and(str::is_empty))
                .unwrap_or(true);
            let has_candidate = response.pointer("/candidates/0").is_some();
            let Some(key) = key_of(&line).filter(|k| wanted.contains(k)) else {
                continue;
            };
            if status_ok && has_candidate && seen.insert(key) {
                results.push((key, resp));
            }
        }
        self.settle(&row, usage.clone()).await?;
        tracing::info!(
            target: "llm_usage",
            purpose = row.purpose.as_str(),
            model = row.model.as_str(),
            batch_id = %id,
            input_tokens = usage.input_tokens,
            output_tokens = usage.output_tokens,
            thinking_tokens = usage.thinking_tokens,
            cost_micro_usd = usage.cost.micros(),
            "batch ingested"
        );
        if row.state == "failed" {
            sqlx::query(
                "UPDATE batch_jobs SET ingested_at = COALESCE(ingested_at, $2) WHERE id = $1",
            )
            .bind(id.to_string())
            .bind(self.clock.now())
            .execute(&self.pool)
            .await
            .map_err(db)?;
            return Ok(BatchIngest::Failed {
                error: row.error.unwrap_or_else(|| "batch job failed".into()),
                keys,
            });
        }
        sqlx::query(
            "UPDATE batch_jobs SET state = 'ingested', ingested_at = COALESCE(ingested_at, $2)
             WHERE id = $1 AND state IN ('succeeded', 'ingested')",
        )
        .bind(id.to_string())
        .bind(self.clock.now())
        .execute(&self.pool)
        .await
        .map_err(db)?;
        let missing = keys.into_iter().filter(|k| !seen.contains(k)).collect();
        results.sort_by_key(|(k, _)| *k);
        Ok(BatchIngest::Ingested { results, missing })
    }
}
