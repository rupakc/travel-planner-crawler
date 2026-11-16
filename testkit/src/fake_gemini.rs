//! **fake-gemini**: Vertex AI's REST wire format for Gemini, scripted.
//!
//! Routes (all under `/v1`, any project/location):
//!
//! * `POST …/publishers/google/models/{model}:generateContent` — online calls. Answers, in
//!   order: the next scripted reply ([`FakeGemini::push_text`], [`FakeGemini::push_json`],
//!   [`FakeGemini::push_error`], [`FakeGemini::push_spend_cap`]); else the first rule whose
//!   needle occurs in the request text ([`FakeGemini::add_rule`]); else `{}`. The response
//!   carries `candidates[0].content.parts[0].text`, `finishReason`, `usageMetadata`
//!   (prompt ≈ chars/4, candidates ≈ chars/4) and `modelVersion`. Errors use Google's shape
//!   `{"error": {"code", "message", "status"}}`.
//! * `POST …/publishers/google/models/{model}:predict` — text embeddings
//!   (`predictions[].embeddings.values`, deterministic unit vectors, `outputDimensionality`,
//!   default 768).
//! * `POST|GET …/batchPredictionJobs` — create / list (`filter=labels.k=v [AND …]`).
//! * `GET …/batchPredictionJobs/{id}` and `POST …/batchPredictionJobs/{id}:cancel`.
//! * `GET /health` (outside `/v1`) for container health checks.
//!
//! Batch jobs read their JSONL input from, and write `predictions.jsonl` to, buckets registered
//! with [`FakeGemini::with_bucket`] (`gs://<bucket>/<path>` → that store's `<path>`). Input lines
//! are `{"key", "request"}`; output lines are `{"key", "request", "response"}` or, for keys
//! marked with [`FakeGemini::fail_key`], `{"key", "request", "status": "<error>"}`. Jobs move
//! `PENDING → RUNNING → SUCCEEDED` on [`FakeGemini::advance`] (or on every GET in
//! [`BatchMode::AdvanceOnPoll`]); outputs are written when a job succeeds.
//!
//! Every call is recorded ([`FakeGemini::calls`]) with its JSON body and `Authorization`
//! header, so tests can assert labels and auth. Embedding vectors use the same function as
//! [`crate::FakeLlm::vector_for`] for one dimension.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::{Request, State};
use axum::response::Response;
use bytes::Bytes;
use chrono::Utc;
use http::{HeaderValue, Method, StatusCode, header};
use serde_json::{Value, json};
use vc_core::store::BlobStore;

use crate::server::FakeServer;

/// How batch jobs move between states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BatchMode {
    /// Only [`FakeGemini::advance`] / [`FakeGemini::complete`] move a job.
    #[default]
    Manual,
    /// Every GET of a job moves it one state (for E2E).
    AdvanceOnPoll,
}

/// A scripted online reply.
#[derive(Debug, Clone)]
pub enum Scripted {
    /// A candidate with this text.
    Text {
        /// The text.
        text: String,
        /// `finishReason` (default `STOP`).
        finish: String,
    },
    /// An HTTP error with Google's error body.
    Error {
        /// HTTP status.
        code: u16,
        /// `error.status` (e.g. `RESOURCE_EXHAUSTED`).
        status: String,
        /// `error.message`.
        message: String,
    },
}

/// One recorded call.
#[derive(Debug, Clone)]
pub struct GeminiCall {
    /// `generateContent`, `predict`, `batch.create`, `batch.get`, `batch.list`, `batch.cancel`.
    pub method: String,
    /// The model (online calls) or the job's model.
    pub model: String,
    /// The request path.
    pub path: String,
    /// The JSON body (`Null` for GETs).
    pub body: Value,
    /// The `Authorization` header.
    pub authorization: Option<String>,
}

impl GeminiCall {
    /// The request's `labels` object (`Null` if absent).
    pub fn labels(&self) -> Value {
        self.body.get("labels").cloned().unwrap_or(Value::Null)
    }
}

#[derive(Debug, Clone)]
struct Job {
    name: String,
    body: Value,
    state: &'static str,
    create_time: String,
    start_time: Option<String>,
    end_time: Option<String>,
    output_dir: Option<String>,
    error: Option<String>,
    succeeded: u64,
    failed: u64,
}

impl Job {
    fn to_json(&self) -> Value {
        let mut v = json!({
            "name": self.name,
            "displayName": self.body.get("displayName").cloned().unwrap_or(Value::Null),
            "model": self.body.get("model").cloned().unwrap_or(Value::Null),
            "inputConfig": self.body.get("inputConfig").cloned().unwrap_or(Value::Null),
            "outputConfig": self.body.get("outputConfig").cloned().unwrap_or(Value::Null),
            "labels": self.body.get("labels").cloned().unwrap_or_else(|| json!({})),
            "state": self.state,
            "createTime": self.create_time,
            "updateTime": self.end_time.clone().or_else(|| self.start_time.clone()).unwrap_or_else(|| self.create_time.clone()),
        });
        if let Some(t) = &self.start_time {
            v["startTime"] = json!(t);
        }
        if let Some(t) = &self.end_time {
            v["endTime"] = json!(t);
        }
        if let Some(d) = &self.output_dir {
            v["outputInfo"] = json!({ "gcsOutputDirectory": d });
        }
        if self.state == "JOB_STATE_SUCCEEDED" {
            v["completionStats"] = json!({
                "successfulCount": self.succeeded.to_string(),
                "failedCount": self.failed.to_string(),
                "incompleteCount": "0",
            });
        }
        if let Some(e) = &self.error {
            v["error"] = json!({ "code": 13, "message": e });
        }
        v
    }
}

#[derive(Debug, Default)]
struct Inner {
    script: VecDeque<Scripted>,
    rules: Vec<(String, String)>,
    calls: Vec<GeminiCall>,
    jobs: BTreeMap<String, Job>,
    fail_keys: HashSet<String>,
}

/// The fake Vertex AI server state.
pub struct FakeGemini {
    inner: Mutex<Inner>,
    buckets: HashMap<String, Arc<dyn BlobStore>>,
    mode: BatchMode,
    next_job: AtomicU64,
    default_dim: usize,
}

impl std::fmt::Debug for FakeGemini {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeGemini")
            .field("buckets", &self.buckets.keys().collect::<Vec<_>>())
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl Default for FakeGemini {
    fn default() -> Self {
        FakeGemini::new()
    }
}

impl FakeGemini {
    /// An empty fake (manual batch mode, no buckets, 768-dim embeddings).
    pub fn new() -> FakeGemini {
        FakeGemini {
            inner: Mutex::new(Inner::default()),
            buckets: HashMap::new(),
            mode: BatchMode::Manual,
            next_job: AtomicU64::new(1_000),
            default_dim: 768,
        }
    }

    /// Registers a bucket for batch input/output.
    pub fn with_bucket(mut self, name: &str, store: Arc<dyn BlobStore>) -> FakeGemini {
        self.buckets.insert(name.to_string(), store);
        self
    }

    /// Sets the batch mode.
    pub fn with_batch_mode(mut self, m: BatchMode) -> FakeGemini {
        self.mode = m;
        self
    }

    /// Sets the default embedding dimension.
    pub fn with_embedding_dim(mut self, d: usize) -> FakeGemini {
        self.default_dim = d;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("fake gemini lock")
    }

    /// Scripts a text reply.
    pub fn push_text(&self, text: impl Into<String>) {
        self.lock().script.push_back(Scripted::Text {
            text: text.into(),
            finish: "STOP".into(),
        });
    }

    /// Scripts a JSON reply (serialised into the text part).
    pub fn push_json(&self, v: Value) {
        self.push_text(v.to_string());
    }

    /// Scripts a reply cut off by `maxOutputTokens` (`finishReason: MAX_TOKENS`).
    pub fn push_truncated(&self, text: impl Into<String>) {
        self.lock().script.push_back(Scripted::Text {
            text: text.into(),
            finish: "MAX_TOKENS".into(),
        });
    }

    /// Scripts an error.
    pub fn push_error(&self, code: u16, status: &str, message: &str) {
        self.lock().script.push_back(Scripted::Error {
            code,
            status: status.into(),
            message: message.into(),
        });
    }

    /// Scripts the project spend cap being hit (403 naming billing).
    pub fn push_spend_cap(&self) {
        self.push_error(
            403,
            "PERMISSION_DENIED",
            "This API method requires billing to be enabled; the billing account budget cap was reached.",
        );
    }

    /// Answers any request whose text contains `needle` with `text` (after the script).
    pub fn add_rule(&self, needle: &str, text: impl Into<String>) {
        self.lock().rules.push((needle.to_string(), text.into()));
    }

    /// Makes batch lines with this key fail.
    pub fn fail_key(&self, key: &str) {
        self.lock().fail_keys.insert(key.to_string());
    }

    /// Every call so far.
    pub fn calls(&self) -> Vec<GeminiCall> {
        self.lock().calls.clone()
    }

    /// A job's JSON by name (`projects/…/batchPredictionJobs/<id>`) or id.
    pub fn job(&self, name: &str) -> Option<Value> {
        let g = self.lock();
        let id = name.rsplit('/').next().unwrap_or(name);
        g.jobs.get(id).map(Job::to_json)
    }

    /// Every job's JSON.
    pub fn jobs(&self) -> Vec<Value> {
        self.lock().jobs.values().map(Job::to_json).collect()
    }

    /// Moves a job one state forward (`PENDING → RUNNING → SUCCEEDED`). On success the outputs
    /// are written to the output bucket.
    pub async fn advance(&self, name: &str) {
        let id = name.rsplit('/').next().unwrap_or(name).to_string();
        let (job, rules, fail_keys) = {
            let mut g = self.lock();
            let Some(job) = g.jobs.get(&id).cloned() else {
                return;
            };
            if matches!(job.state, "JOB_STATE_PENDING" | "JOB_STATE_QUEUED") {
                let j = g.jobs.get_mut(&id).expect("job");
                j.state = "JOB_STATE_RUNNING";
                j.start_time = Some(Utc::now().to_rfc3339());
                return;
            }
            if job.state != "JOB_STATE_RUNNING" {
                return;
            }
            (job, g.rules.clone(), g.fail_keys.clone())
        };
        let result = self.run_job(&job, &rules, &fail_keys).await;
        let mut g = self.lock();
        let Some(j) = g.jobs.get_mut(&id) else {
            return;
        };
        if j.state != "JOB_STATE_RUNNING" {
            return; // cancelled or failed meanwhile
        }
        match result {
            Ok((dir, ok, failed)) => {
                j.state = "JOB_STATE_SUCCEEDED";
                j.output_dir = Some(dir);
                j.succeeded = ok;
                j.failed = failed;
            }
            Err(e) => {
                j.state = "JOB_STATE_FAILED";
                j.error = Some(e);
            }
        }
        j.end_time = Some(Utc::now().to_rfc3339());
    }

    /// Moves a job straight to its final state.
    pub async fn complete(&self, name: &str) {
        for _ in 0..2 {
            self.advance(name).await;
        }
    }

    /// Fails a (non-terminal) job with `message`.
    pub fn fail_job(&self, name: &str, message: &str) {
        let id = name.rsplit('/').next().unwrap_or(name).to_string();
        let mut g = self.lock();
        if let Some(j) = g.jobs.get_mut(&id)
            && !terminal(j.state)
        {
            j.state = "JOB_STATE_FAILED";
            j.error = Some(message.to_string());
            j.end_time = Some(Utc::now().to_rfc3339());
        }
    }

    fn bucket_path(&self, uri: &str) -> Option<(Arc<dyn BlobStore>, String)> {
        let rest = uri.strip_prefix("gs://")?;
        let (bucket, path) = rest.split_once('/')?;
        Some((self.buckets.get(bucket)?.clone(), path.to_string()))
    }

    fn input_uri(body: &Value) -> Option<String> {
        body.pointer("/inputConfig/gcsSource/uris/0")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Reads the input and writes `predictions.jsonl`.
    async fn run_job(
        &self,
        job: &Job,
        rules: &[(String, String)],
        fail_keys: &HashSet<String>,
    ) -> Result<(String, u64, u64), String> {
        let input = Self::input_uri(&job.body).ok_or("no input uri")?;
        let (in_store, in_path) = self.bucket_path(&input).ok_or("unknown input bucket")?;
        let prefix = job
            .body
            .pointer("/outputConfig/gcsDestination/outputUriPrefix")
            .and_then(Value::as_str)
            .ok_or("no output prefix")?
            .trim_end_matches('/')
            .to_string();
        let dir = format!(
            "{prefix}/prediction-model-{}",
            Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ")
        );
        let (out_store, out_dir) = self
            .bucket_path(&format!("{dir}/"))
            .ok_or("unknown output bucket")?;
        let bytes = in_store
            .get(&in_path, None)
            .await
            .map_err(|e| e.to_string())?;
        let mut lines = Vec::new();
        let (mut ok, mut failed) = (0, 0);
        for line in String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.trim().is_empty())
        {
            let v: Value =
                serde_json::from_str(line).map_err(|e| format!("bad input line: {e}"))?;
            let key = v.get("key").cloned().unwrap_or(Value::Null);
            let request = v.get("request").cloned().unwrap_or(Value::Null);
            let key_str = key.as_str().unwrap_or_default().to_string();
            if fail_keys.contains(&key_str) {
                failed += 1;
                lines.push(json!({"key": key, "request": request, "status": "13: Internal error encountered."}));
            } else {
                ok += 1;
                let text = rule_text(rules, &request);
                let response = candidate_response(&text, "STOP", &request, model_of(&job.body));
                lines.push(
                    json!({"key": key, "request": request, "response": response, "status": ""}),
                );
            }
        }
        let body = Bytes::from(
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        );
        let path = format!("{out_dir}predictions.jsonl");
        out_store
            .put_if_absent(&path, body.clone(), crc32c::crc32c(&body))
            .await
            .map_err(|e| e.to_string())?;
        Ok((dir, ok, failed))
    }

    /// The axum router.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new().fallback(handle).with_state(self)
    }

    /// Serves on `127.0.0.1:<random port>`.
    pub async fn start(self: Arc<Self>) -> FakeServer {
        FakeServer::spawn(self.router()).await
    }
}

fn terminal(state: &str) -> bool {
    matches!(
        state,
        "JOB_STATE_SUCCEEDED"
            | "JOB_STATE_FAILED"
            | "JOB_STATE_CANCELLED"
            | "JOB_STATE_PARTIALLY_SUCCEEDED"
    )
}

fn model_of(body: &Value) -> &str {
    body.get("model")
        .and_then(Value::as_str)
        .map(|m| m.rsplit('/').next().unwrap_or(m))
        .unwrap_or("unknown")
}

/// All text parts of a request (system instruction + contents).
fn request_text(req: &Value) -> String {
    let mut out = String::new();
    let mut collect = |parts: Option<&Value>| {
        for p in parts.and_then(Value::as_array).into_iter().flatten() {
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                out.push_str(t);
                out.push('\n');
            }
        }
    };
    collect(req.pointer("/systemInstruction/parts"));
    for c in req
        .get("contents")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        collect(c.get("parts"));
    }
    out
}

fn rule_text(rules: &[(String, String)], req: &Value) -> String {
    let text = request_text(req);
    rules
        .iter()
        .find(|(needle, _)| text.contains(needle.as_str()))
        .map(|(_, t)| t.clone())
        .unwrap_or_else(|| "{}".to_string())
}

fn tokens(s: &str) -> u64 {
    (s.chars().count() as u64).div_ceil(4).max(1)
}

fn candidate_response(text: &str, finish: &str, req: &Value, model: &str) -> Value {
    let prompt = tokens(&request_text(req));
    let cand = tokens(text);
    json!({
        "candidates": [{
            "content": {"role": "model", "parts": [{"text": text}]},
            "finishReason": finish,
            "avgLogprobs": -0.05
        }],
        "usageMetadata": {
            "promptTokenCount": prompt,
            "candidatesTokenCount": cand,
            "thoughtsTokenCount": 0,
            "totalTokenCount": prompt + cand,
            "trafficType": "ON_DEMAND"
        },
        "modelVersion": model,
        "createTime": Utc::now().to_rfc3339(),
        "responseId": ulid::Ulid::generate().to_string().to_lowercase()
    })
}

fn embedding(text: &str, dim: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(dim);
    let mut h = blake3::Hasher::new();
    h.update(text.as_bytes());
    let mut rd = h.finalize_xof();
    let mut buf = [0u8; 4];
    for _ in 0..dim {
        rd.fill(&mut buf);
        out.push((u32::from_le_bytes(buf) as f32 / u32::MAX as f32) * 2.0 - 1.0);
    }
    let norm = out
        .iter()
        .map(|x| x * x)
        .sum::<f32>()
        .sqrt()
        .max(f32::EPSILON);
    out.iter_mut().for_each(|x| *x /= norm);
    out
}

fn reply(status: StatusCode, v: Value) -> Response {
    let mut r = Response::new(Body::from(v.to_string()));
    *r.status_mut() = status;
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json; charset=UTF-8"),
    );
    r
}

fn google_error(code: u16, status: &str, message: &str) -> Response {
    reply(
        StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        json!({"error": {"code": code, "message": message, "status": status}}),
    )
}

/// `filter=labels.a=b AND labels.c="d"` → whether the job's labels match.
fn filter_matches(filter: &str, labels: &Value) -> bool {
    filter
        .split(" AND ")
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .all(|c| {
            let Some(rest) = c.strip_prefix("labels.") else {
                return true; // other filters (state, …) are not modelled
            };
            let Some((k, v)) = rest.split_once('=') else {
                return false;
            };
            let v = v.trim().trim_matches('"');
            labels.get(k.trim()).and_then(Value::as_str) == Some(v)
        })
}

fn query_param(q: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(q.as_bytes())
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.into_owned())
}

async fn handle(State(g): State<Arc<FakeGemini>>, req: Request) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().unwrap_or_default().to_string();
    let auth = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let raw = to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    let body: Value = if raw.is_empty() {
        Value::Null
    } else {
        match serde_json::from_slice(&raw) {
            Ok(v) => v,
            Err(e) => return google_error(400, "INVALID_ARGUMENT", &format!("invalid JSON: {e}")),
        }
    };
    if path == "/health" {
        return reply(StatusCode::OK, json!({"ok": true}));
    }
    let Some(rest) = path.strip_prefix("/v1/") else {
        return google_error(404, "NOT_FOUND", "unknown path");
    };
    let segs: Vec<&str> = rest.split('/').collect();
    let record = |m: &str, model: &str| {
        g.lock().calls.push(GeminiCall {
            method: m.to_string(),
            model: model.to_string(),
            path: path.clone(),
            body: body.clone(),
            authorization: auth.clone(),
        });
    };

    // projects/{p}/locations/{l}/publishers/google/models/{model}:{verb}
    if segs.len() == 8
        && segs[0] == "projects"
        && segs[2] == "locations"
        && segs[4] == "publishers"
        && segs[6] == "models"
        && method == Method::POST
    {
        let Some((model, verb)) = segs[7].split_once(':') else {
            return google_error(404, "NOT_FOUND", "missing method");
        };
        record(verb, model);
        return match verb {
            "generateContent" => {
                let next = g.lock().script.pop_front();
                match next {
                    Some(Scripted::Error {
                        code,
                        status,
                        message,
                    }) => google_error(code, &status, &message),
                    Some(Scripted::Text { text, finish }) => reply(
                        StatusCode::OK,
                        candidate_response(&text, &finish, &body, model),
                    ),
                    None => {
                        let text = rule_text(&g.lock().rules, &body);
                        reply(
                            StatusCode::OK,
                            candidate_response(&text, "STOP", &body, model),
                        )
                    }
                }
            }
            "predict" => {
                let dim = body
                    .pointer("/parameters/outputDimensionality")
                    .and_then(Value::as_u64)
                    .map(|d| d as usize)
                    .unwrap_or(g.default_dim);
                let preds: Vec<Value> = body
                    .get("instances")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|i| {
                        let t = i.get("content").and_then(Value::as_str).unwrap_or_default();
                        json!({"embeddings": {
                            "values": embedding(t, dim),
                            "statistics": {"token_count": tokens(t), "truncated": false}
                        }})
                    })
                    .collect();
                reply(StatusCode::OK, json!({"predictions": preds}))
            }
            _ => google_error(404, "NOT_FOUND", &format!("unknown method {verb}")),
        };
    }

    // projects/{p}/locations/{l}/batchPredictionJobs[/{id}[:cancel]]
    if segs.len() >= 5
        && segs[0] == "projects"
        && segs[2] == "locations"
        && segs[4] == "batchPredictionJobs"
    {
        let parent = format!("projects/{}/locations/{}", segs[1], segs[3]);
        if segs.len() == 5 {
            return match method {
                Method::POST => {
                    record("batch.create", model_of(&body));
                    let Some(input) = FakeGemini::input_uri(&body) else {
                        return google_error(
                            400,
                            "INVALID_ARGUMENT",
                            "inputConfig.gcsSource.uris is required",
                        );
                    };
                    let readable = match g.bucket_path(&input) {
                        Some((s, p)) => s.get(&p, None).await.is_ok(),
                        None => false,
                    };
                    if !readable {
                        return google_error(
                            400,
                            "INVALID_ARGUMENT",
                            &format!("cannot read {input}"),
                        );
                    }
                    let id = g.next_job.fetch_add(1, Ordering::SeqCst).to_string();
                    let job = Job {
                        name: format!("{parent}/batchPredictionJobs/{id}"),
                        body: body.clone(),
                        state: "JOB_STATE_PENDING",
                        create_time: Utc::now().to_rfc3339(),
                        start_time: None,
                        end_time: None,
                        output_dir: None,
                        error: None,
                        succeeded: 0,
                        failed: 0,
                    };
                    let v = job.to_json();
                    g.lock().jobs.insert(id, job);
                    reply(StatusCode::OK, v)
                }
                Method::GET => {
                    record("batch.list", "");
                    let filter = query_param(&query, "filter").unwrap_or_default();
                    let jobs: Vec<Value> = g
                        .lock()
                        .jobs
                        .values()
                        .filter(|j| j.name.starts_with(&parent))
                        .map(Job::to_json)
                        .filter(|j| filter_matches(&filter, &j["labels"]))
                        .collect();
                    reply(StatusCode::OK, json!({"batchPredictionJobs": jobs}))
                }
                _ => google_error(405, "INVALID_ARGUMENT", "method not allowed"),
            };
        }
        if segs.len() == 6 {
            let (id, verb) = match segs[5].split_once(':') {
                Some((i, v)) => (i, Some(v)),
                None => (segs[5], None),
            };
            let exists = g.lock().jobs.contains_key(id);
            if !exists {
                return google_error(
                    404,
                    "NOT_FOUND",
                    &format!("batch prediction job {id} not found"),
                );
            }
            return match (method, verb) {
                (Method::GET, None) => {
                    record("batch.get", "");
                    if g.mode == BatchMode::AdvanceOnPoll {
                        g.advance(id).await;
                    }
                    reply(StatusCode::OK, g.job(id).unwrap_or_default())
                }
                (Method::POST, Some("cancel")) => {
                    record("batch.cancel", "");
                    let mut inner = g.lock();
                    let j = inner.jobs.get_mut(id).expect("job exists");
                    if terminal(j.state) {
                        return google_error(
                            400,
                            "FAILED_PRECONDITION",
                            &format!("job {id} is already in a terminal state {}", j.state),
                        );
                    }
                    j.state = "JOB_STATE_CANCELLED";
                    j.end_time = Some(Utc::now().to_rfc3339());
                    reply(StatusCode::OK, json!({}))
                }
                _ => google_error(404, "NOT_FOUND", "unknown method"),
            };
        }
    }
    google_error(404, "NOT_FOUND", "unknown path")
}
