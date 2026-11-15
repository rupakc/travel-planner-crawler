//! VertexBatchRunner against a fake Vertex (wiremock), MemBlobStore and a real Postgres.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::{Value, json};
use testkit::{FakeClock, MemBlobStore};
use vc_core::llm::{BatchIngest, BatchItem, BatchRunner, BudgetLedger, Purpose, Usd};
use vc_core::store::BlobStore;
use vc_core::{BatchJobId, WorkId};
use vc_llm::{PgLedger, VertexBatchRunner, crc32c};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const H72: Duration = Duration::from_secs(72 * 3600);

struct Env {
    server: MockServer,
    pool: sqlx::PgPool,
    clock: Arc<FakeClock>,
    blobs: Arc<MemBlobStore>,
    ledger: PgLedger,
    runner: VertexBatchRunner,
}

async fn env() -> Env {
    let server = MockServer::start().await;
    let pool = testkit::pg().await;
    let clock = clock_at(2026, 11, 15);
    let config = cfg(&server.uri());
    let ledger = PgLedger::new(pool.clone(), config.clone(), dyn_clock(&clock));
    let blobs = Arc::new(MemBlobStore::new());
    let runner = VertexBatchRunner::new(
        pool.clone(),
        config.clone(),
        vertex(&config, &clock),
        blobs.clone(),
        ledger.clone(),
        dyn_clock(&clock),
    );
    Env {
        server,
        pool,
        clock,
        blobs,
        ledger,
        runner,
    }
}

impl Env {
    async fn prepare(&self, keys: &[i64]) -> BatchJobId {
        let items: Vec<BatchItem> = keys
            .iter()
            .map(|k| BatchItem {
                key: WorkId(*k),
                request: request(&format!("page {k}")),
            })
            .collect();
        let r = self
            .ledger
            .reserve_batch(Purpose::Extraction, self.runner.estimate(&items))
            .await
            .unwrap()
            .expect("batch reservation fits");
        self.runner.prepare(r, items).await.unwrap()
    }

    async fn row(&self, id: &BatchJobId) -> (String, Option<String>) {
        sqlx::query_as("SELECT state, vertex_job_name FROM batch_jobs WHERE id = $1")
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await
            .unwrap()
    }

    async fn mount_list(&self, filter: &str, jobs: Vec<Value>) {
        Mock::given(method("GET"))
            .and(path(JOBS_PATH))
            .and(query_param("filter", filter))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"batchPredictionJobs": jobs})),
            )
            .mount(&self.server)
            .await;
    }

    async fn mount_create(&self, times: u64) {
        Mock::given(method("POST"))
            .and(path(JOBS_PATH))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"name": JOB_NAME, "state": "JOB_STATE_PENDING"})),
            )
            .expect(times)
            .mount(&self.server)
            .await;
    }

    async fn ledger_status(&self) -> Vec<String> {
        sqlx::query_as::<_, (String,)>("SELECT status FROM llm_ledger ORDER BY created_at")
            .fetch_all(&self.pool)
            .await
            .unwrap()
            .into_iter()
            .map(|r| r.0)
            .collect()
    }
}

fn lower(id: &BatchJobId) -> String {
    id.to_string().to_ascii_lowercase()
}

/// submit creates the job with display name = row id, model path, JSONL GCS input/output and
/// labels; the row becomes `submitted`. A second submit is a no-op.
#[tokio::test]
async fn submit_creates_job_once() {
    let e = env().await;
    let id = e.prepare(&[1, 2]).await;
    assert_eq!(e.row(&id).await, ("prepared".into(), None));
    e.mount_list(&format!("labels.batch_id={}", lower(&id)), vec![])
        .await;
    e.mount_create(1).await;
    e.runner.submit(&id).await.unwrap();
    e.runner.submit(&id).await.unwrap();
    assert_eq!(
        e.row(&id).await,
        ("submitted".into(), Some(JOB_NAME.into()))
    );

    let reqs = e.server.received_requests().await.unwrap();
    let create = reqs.iter().find(|r| r.method.as_str() == "POST").unwrap();
    let body: Value = create.body_json().unwrap();
    let base = format!("gs://voyager-crawler-ops/llm-batch/{id}");
    assert_eq!(
        body,
        json!({
            "displayName": id.to_string(),
            "model": "publishers/google/models/gemini-3.1-flash-lite",
            "inputConfig": {"instancesFormat": "jsonl",
                            "gcsSource": {"uris": [format!("{base}/input.jsonl")]}},
            "outputConfig": {"predictionsFormat": "jsonl",
                             "gcsDestination": {"outputUriPrefix": format!("{base}/output")}},
            "labels": {"app": "voyager-crawler", "purpose": "extraction", "batch_id": lower(&id)}
        })
    );
    assert!(e.blobs.contains(&format!("llm-batch/{id}/input.jsonl")));
}

/// A crash between the Vertex create and the row update leaves a `prepared` row with no job
/// name: reconcile adopts the job by its `batch_id` label, and nothing is ever resubmitted.
#[tokio::test]
async fn batch_crash_after_submit_reconciled_no_resubmit() {
    let e = env().await;
    let id = e.prepare(&[1, 2, 3]).await;
    let other = e.prepare(&[4]).await;
    e.mount_create(0).await; // never create
    e.mount_list(
        "labels.app=voyager-crawler",
        vec![job("JOB_STATE_RUNNING", &lower(&id), None)],
    )
    .await;

    let rep = e.runner.reconcile(H72).await.unwrap();
    assert_eq!(rep.adopted, 1);
    assert_eq!(rep.cancelled, 0);
    assert!(rep.finished.is_empty());
    assert_eq!(
        e.row(&id).await,
        ("submitted".into(), Some(JOB_NAME.into()))
    );
    // A second reconcile adopts nothing new; the other row is untouched.
    assert_eq!(e.runner.reconcile(H72).await.unwrap().adopted, 0);
    assert_eq!(e.row(&other).await, ("prepared".into(), None));
    // submit after adoption: the row is no longer prepared, so no create.
    e.runner.submit(&id).await.unwrap();

    // The same crash, recovered by submit itself: it lists by batch_id before creating.
    let other_name = "projects/123/locations/global/batchPredictionJobs/988";
    let mut other_job = job("JOB_STATE_PENDING", &lower(&other), None);
    other_job["name"] = json!(other_name);
    e.mount_list(
        &format!("labels.batch_id={}", lower(&other)),
        vec![other_job],
    )
    .await;
    e.runner.submit(&other).await.unwrap();
    assert_eq!(
        e.row(&other).await,
        ("submitted".into(), Some(other_name.into()))
    );
    e.server.verify().await;
}

/// Results are mapped by key (top-level `key`, else the `vc_key` request label); failed lines,
/// absent keys and unknown keys never become results; missing keys come back for nack. The
/// reservation is settled at batch price and the row ends `ingested`.
#[tokio::test]
async fn batch_results_mapped_by_key_missing_nacked() {
    let e = env().await;
    let id = e.prepare(&[1, 2, 3, 4]).await;
    e.mount_list(&format!("labels.batch_id={}", lower(&id)), vec![])
        .await;
    e.mount_create(1).await;
    e.runner.submit(&id).await.unwrap();
    assert_eq!(e.runner.ingest(&id).await.unwrap(), BatchIngest::Pending);

    let out_dir = format!(
        "gs://voyager-crawler-ops/llm-batch/{id}/output/prediction-model-2026-11-15T13:00:00.000000Z"
    );
    e.mount_list(
        "labels.app=voyager-crawler",
        vec![job("JOB_STATE_SUCCEEDED", &lower(&id), Some(&out_dir))],
    )
    .await;
    let rep = e.runner.reconcile(H72).await.unwrap();
    assert_eq!(rep.finished, vec![id]);
    assert_eq!(e.row(&id).await.0, "succeeded");

    let ok = |name: &str| gen_response(&json!({"name": name}));
    let lines = [
        // key 2 has no top-level key: mapped through request.labels.vc_key.
        json!({"status": "", "processed_time": "2026-11-15T12:30:00Z",
               "request": {"labels": {"app": "voyager-crawler", "vc_key": "2"}}, "response": ok("two")}),
        json!({"key": "1", "status": "", "request": {"generationConfig": {"responseSchema": {}}},
               "response": ok("one")}),
        // key 3 failed.
        json!({"key": "3", "status": "Bad Request: {\"error\": {\"code\": 400}}", "request": {},
               "response": {}}),
        // A key that was never in the input is ignored.
        json!({"key": "99", "status": "", "request": {}, "response": ok("ghost")}),
        // A duplicate of key 1 (resumed job) does not produce a second result.
        json!({"key": "1", "status": "", "request": {}, "response": ok("again")}),
    ];
    let mut jsonl = Vec::new();
    for l in &lines {
        serde_json::to_writer(&mut jsonl, l).unwrap();
        jsonl.push(b'\n');
    }
    let out_path = format!(
        "{}/predictions.jsonl",
        out_dir.trim_start_matches("gs://voyager-crawler-ops/")
    );
    let crc = crc32c(&jsonl);
    e.blobs
        .put_if_absent(&out_path, jsonl.into(), crc)
        .await
        .unwrap();

    let BatchIngest::Ingested { results, missing } = e.runner.ingest(&id).await.unwrap() else {
        panic!("expected Ingested");
    };
    let keys: Vec<i64> = results.iter().map(|(k, _)| k.0).collect();
    assert_eq!(keys, [1, 2]);
    assert_eq!(results[0].1.json, Some(json!({"name": "one"})));
    assert_eq!(results[1].1.text, json!({"name": "two"}).to_string());
    assert_eq!(missing, vec![WorkId(3), WorkId(4)]);
    assert_eq!(e.row(&id).await.0, "ingested");

    // Settled at batch price: every output line with usage (5 × (120 × 0.125 + 40 × 0.75)).
    assert_eq!(e.ledger_status().await, ["settled"]);
    let m = e.ledger.row("2026-11").await.unwrap().unwrap();
    assert_eq!(m.reserved, Usd::ZERO);
    assert_eq!(m.spent, Usd(4 * 45));
    // Re-ingest (e.g. after a crash before the caller acked) returns the same mapping and
    // does not settle twice.
    let again = e.runner.ingest(&id).await.unwrap();
    assert!(matches!(again, BatchIngest::Ingested { ref results, .. } if results.len() == 2));
    assert_eq!(
        e.ledger.row("2026-11").await.unwrap().unwrap().spent,
        Usd(4 * 45)
    );
}

/// After 72 h a running job is cancelled, the row is `failed`, ingest returns every key for
/// nack, and the reservation (nothing billed) is released.
#[tokio::test]
async fn batch_72h_cancel() {
    let e = env().await;
    let id = e.prepare(&[7, 8]).await;
    e.mount_list(&format!("labels.batch_id={}", lower(&id)), vec![])
        .await;
    e.mount_create(1).await;
    e.runner.submit(&id).await.unwrap();
    e.mount_list(
        "labels.app=voyager-crawler",
        vec![job("JOB_STATE_RUNNING", &lower(&id), None)],
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("/v1/{JOB_NAME}:cancel")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&e.server)
        .await;

    e.clock.advance(Duration::from_secs(71 * 3600));
    let rep = e.runner.reconcile(H72).await.unwrap();
    assert_eq!(
        (rep.cancelled, rep.finished.len()),
        (0, 0),
        "71 h: still running"
    );

    e.clock.advance(Duration::from_secs(3600 + 60));
    let rep = e.runner.reconcile(H72).await.unwrap();
    assert_eq!(rep.cancelled, 1);
    assert_eq!(rep.finished, vec![id]);
    assert_eq!(e.row(&id).await.0, "failed");
    // Not cancelled twice.
    assert_eq!(e.runner.reconcile(H72).await.unwrap().cancelled, 0);

    match e.runner.ingest(&id).await.unwrap() {
        BatchIngest::Failed { error, keys } => {
            assert!(error.contains("72 h"), "{error}");
            assert_eq!(keys, vec![WorkId(7), WorkId(8)]);
        }
        other => panic!("expected Failed, got {other:?}"),
    }
    assert_eq!(e.ledger_status().await, ["released"]);
    let m = e.ledger.row("2026-11").await.unwrap().unwrap();
    assert_eq!((m.reserved, m.spent), (Usd::ZERO, Usd::ZERO));
    e.server.verify().await;
}

/// A job Vertex reports as failed is recorded with its error and all keys are nacked.
#[tokio::test]
async fn vertex_failed_job_nacks_all() {
    let e = env().await;
    let id = e.prepare(&[5]).await;
    e.mount_list(&format!("labels.batch_id={}", lower(&id)), vec![])
        .await;
    e.mount_create(1).await;
    e.runner.submit(&id).await.unwrap();
    let mut j = job("JOB_STATE_FAILED", &lower(&id), None);
    j["error"] = json!({"code": 3, "message": "invalid input file"});
    e.mount_list("labels.app=voyager-crawler", vec![j]).await;
    assert_eq!(e.runner.reconcile(H72).await.unwrap().finished, vec![id]);
    assert_eq!(
        e.runner.ingest(&id).await.unwrap(),
        BatchIngest::Failed {
            error: "invalid input file".into(),
            keys: vec![WorkId(5)]
        }
    );
}

/// prepare refuses non-batch reservations, empty batches and mixed models.
#[tokio::test]
async fn prepare_validates() {
    let e = env().await;
    let r = e
        .ledger
        .reserve(Purpose::Extraction, Usd(10))
        .await
        .unwrap()
        .unwrap();
    let item = BatchItem {
        key: WorkId(1),
        request: request("x"),
    };
    assert!(e.runner.prepare(r, vec![item.clone()]).await.is_err());
    let rb = e
        .ledger
        .reserve_batch(Purpose::Extraction, Usd(10))
        .await
        .unwrap()
        .unwrap();
    assert!(e.runner.prepare(rb.clone(), vec![]).await.is_err());
    let mut other = item.clone();
    other.request.model = Some(vc_core::ModelId::new("gemini-3.5-flash-lite"));
    assert!(e.runner.prepare(rb, vec![item, other]).await.is_err());
    assert!(e.blobs.paths().is_empty());
}
