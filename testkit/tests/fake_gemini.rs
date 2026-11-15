//! fake-gemini speaks the Vertex AI wire format (online `generateContent`, `predict` for
//! embeddings, and `batchPredictionJobs`).

use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use testkit::MemBlobStore;
use testkit::fake_gemini::{BatchMode, FakeGemini};
use vc_core::store::BlobStore;

const BASE: &str = "/v1/projects/p/locations/global";

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn generate_content_returns_scripted_json_and_usage() {
    let g = Arc::new(FakeGemini::new());
    g.push_json(json!({"name": "Jazz night", "country": "Japan"}));
    let srv = g.clone().start().await;
    let url = format!(
        "{}{BASE}/publishers/google/models/gemini-3.1-flash-lite:generateContent",
        srv.base_url()
    );
    let body = json!({
        "systemInstruction": {"parts": [{"text": "Extract."}]},
        "contents": [{"role": "user", "parts": [{"text": "Jazz night in Tokyo, Japan"}]}],
        "generationConfig": {"maxOutputTokens": 400, "responseMimeType": "application/json"},
        "labels": {"app": "voyager-crawler", "purpose": "extraction"}
    });
    let r = client()
        .post(&url)
        .bearer_auth("t")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let v: Value = r.json().await.unwrap();
    let text = v["candidates"][0]["content"]["parts"][0]["text"]
        .as_str()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(text).unwrap(),
        json!({"name": "Jazz night", "country": "Japan"})
    );
    assert_eq!(v["candidates"][0]["finishReason"], "STOP");
    assert_eq!(v["candidates"][0]["content"]["role"], "model");
    let u = &v["usageMetadata"];
    assert!(u["promptTokenCount"].as_u64().unwrap() > 0);
    assert!(u["candidatesTokenCount"].as_u64().unwrap() > 0);
    assert_eq!(
        u["totalTokenCount"].as_u64().unwrap(),
        u["promptTokenCount"].as_u64().unwrap()
            + u["candidatesTokenCount"].as_u64().unwrap()
            + u["thoughtsTokenCount"].as_u64().unwrap()
    );
    assert_eq!(v["modelVersion"], "gemini-3.1-flash-lite");

    let calls = g.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].model, "gemini-3.1-flash-lite");
    assert_eq!(calls[0].method, "generateContent");
    assert_eq!(calls[0].labels()["purpose"], "extraction");
    assert_eq!(calls[0].authorization.as_deref(), Some("Bearer t"));
}

#[tokio::test]
async fn scripted_errors_use_the_google_error_shape() {
    let g = Arc::new(FakeGemini::new());
    g.push_error(429, "RESOURCE_EXHAUSTED", "Quota exceeded");
    g.push_spend_cap();
    let srv = g.clone().start().await;
    let url = format!(
        "{}{BASE}/publishers/google/models/m:generateContent",
        srv.base_url()
    );
    let body = json!({"contents": [{"role": "user", "parts": [{"text": "x"}]}]});
    let r = client().post(&url).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 429);
    let v: Value = r.json().await.unwrap();
    assert_eq!(v["error"]["status"], "RESOURCE_EXHAUSTED");
    assert_eq!(v["error"]["code"], 429);
    let r = client().post(&url).json(&body).send().await.unwrap();
    assert_eq!(r.status(), 403);
    let v: Value = r.json().await.unwrap();
    assert!(v["error"]["message"].as_str().unwrap().contains("billing"));
}

#[tokio::test]
async fn rules_answer_by_input_text_and_default_is_an_empty_object() {
    let g = Arc::new(FakeGemini::new());
    g.add_rule("visa", json!({"kind": "visa_rule"}).to_string());
    let srv = g.clone().start().await;
    let url = format!(
        "{}{BASE}/publishers/google/models/m:generateContent",
        srv.base_url()
    );
    let ask = |t: &str| json!({"contents": [{"role": "user", "parts": [{"text": t}]}]});
    let v: Value = client()
        .post(&url)
        .json(&ask("Japan visa rules"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        v["candidates"][0]["content"]["parts"][0]["text"],
        r#"{"kind":"visa_rule"}"#
    );
    let v: Value = client()
        .post(&url)
        .json(&ask("something else"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["candidates"][0]["content"]["parts"][0]["text"], "{}");
}

#[tokio::test]
async fn predict_returns_deterministic_unit_embeddings() {
    let g = Arc::new(FakeGemini::new());
    let srv = g.clone().start().await;
    let url = format!(
        "{}{BASE}/publishers/google/models/text-embedding-005:predict",
        srv.base_url()
    );
    let body = json!({
        "instances": [{"content": "a"}, {"content": "b"}, {"content": "a"}],
        "parameters": {"outputDimensionality": 16}
    });
    let v: Value = client()
        .post(&url)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let p = v["predictions"].as_array().unwrap();
    assert_eq!(p.len(), 3);
    let vals = |i: usize| -> Vec<f64> {
        p[i]["embeddings"]["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_f64().unwrap())
            .collect()
    };
    assert_eq!(vals(0).len(), 16);
    assert_eq!(vals(0), vals(2));
    assert_ne!(vals(0), vals(1));
    let norm: f64 = vals(1).iter().map(|x| x * x).sum::<f64>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "{norm}");
    assert!(
        p[0]["embeddings"]["statistics"]["token_count"]
            .as_u64()
            .is_some()
    );
}

#[tokio::test]
async fn fake_gemini_batch_lifecycle() {
    let bucket = Arc::new(MemBlobStore::new());
    let g = Arc::new(FakeGemini::new().with_bucket("vc-batch", bucket.clone()));
    g.add_rule("museum", json!({"kind": "attraction"}).to_string());
    g.fail_key("w-bad");
    let srv = g.clone().start().await;

    // Input JSONL, one request per line, keyed by work id.
    let line = |k: &str, text: &str| {
        json!({"key": k, "request": {"contents": [{"role": "user", "parts": [{"text": text}]}]}})
            .to_string()
    };
    let input = [
        line("w-1", "A museum in Kyoto"),
        line("w-2", "Something else"),
        line("w-bad", "Fails"),
    ]
    .join("\n");
    let b = Bytes::from(input);
    bucket
        .put_if_absent("in/job1.jsonl", b.clone(), crc32c::crc32c(&b))
        .await
        .unwrap();

    // Create.
    let jobs_url = format!("{}{BASE}/batchPredictionJobs", srv.base_url());
    let create = json!({
        "displayName": "01JBATCHROWID",
        "model": "publishers/google/models/gemini-3.1-flash-lite",
        "inputConfig": {"instancesFormat": "jsonl", "gcsSource": {"uris": ["gs://vc-batch/in/job1.jsonl"]}},
        "outputConfig": {"predictionsFormat": "jsonl", "gcsDestination": {"outputUriPrefix": "gs://vc-batch/out/job1"}},
        "labels": {"app": "voyager-crawler", "batch_id": "01JBATCHROWID", "purpose": "extraction"}
    });
    let r = client().post(&jobs_url).json(&create).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let job: Value = r.json().await.unwrap();
    let name = job["name"].as_str().unwrap().to_string();
    assert!(
        name.starts_with("projects/p/locations/global/batchPredictionJobs/"),
        "{name}"
    );
    assert_eq!(job["state"], "JOB_STATE_PENDING");
    assert_eq!(job["displayName"], "01JBATCHROWID");
    assert_eq!(job["labels"]["batch_id"], "01JBATCHROWID");
    assert!(job["createTime"].as_str().is_some());

    // List by label (reconcile adopts orphans this way); other labels don't match.
    let list: Value = client()
        .get(format!("{jobs_url}?filter=labels.app%3Dvoyager-crawler"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(list["batchPredictionJobs"].as_array().unwrap().len(), 1);
    let list: Value = client()
        .get(format!("{jobs_url}?filter=labels.app%3Dother"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        list["batchPredictionJobs"]
            .as_array()
            .is_none_or(|a| a.is_empty())
    );

    // Manual mode: the job only moves when told to.
    let get_url = format!("{}/v1/{name}", srv.base_url());
    let get = || async {
        client()
            .get(&get_url)
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    };
    assert_eq!(get().await["state"], "JOB_STATE_PENDING");
    g.advance(&name).await;
    assert_eq!(get().await["state"], "JOB_STATE_RUNNING");
    assert!(get().await["startTime"].as_str().is_some());
    g.advance(&name).await;
    let done = get().await;
    assert_eq!(done["state"], "JOB_STATE_SUCCEEDED");
    assert!(done["endTime"].as_str().is_some());
    assert_eq!(done["completionStats"]["successfulCount"], "2");
    assert_eq!(done["completionStats"]["failedCount"], "1");

    // Output: predictions.jsonl under outputInfo.gcsOutputDirectory, mapped by key.
    let dir = done["outputInfo"]["gcsOutputDirectory"].as_str().unwrap();
    let prefix = dir.strip_prefix("gs://vc-batch/").unwrap();
    assert!(prefix.starts_with("out/job1/"), "{prefix}");
    let out = bucket
        .get(&format!("{prefix}/predictions.jsonl"), None)
        .await
        .unwrap();
    let lines: Vec<Value> = std::str::from_utf8(&out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    let by_key = |k: &str| lines.iter().find(|l| l["key"] == k).unwrap().clone();
    assert_eq!(
        by_key("w-1")["response"]["candidates"][0]["content"]["parts"][0]["text"],
        r#"{"kind":"attraction"}"#
    );
    assert!(
        by_key("w-1")["response"]["usageMetadata"]["promptTokenCount"]
            .as_u64()
            .is_some()
    );
    assert_eq!(by_key("w-1")["request"]["contents"][0]["role"], "user");
    assert_eq!(
        by_key("w-2")["response"]["candidates"][0]["content"]["parts"][0]["text"],
        "{}"
    );
    assert!(by_key("w-bad").get("response").is_none());
    assert!(!by_key("w-bad")["status"].as_str().unwrap().is_empty());

    // Cancelling a finished job is a precondition failure; the job is unchanged.
    let r = client()
        .post(format!("{}/v1/{name}:cancel", srv.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(get().await["state"], "JOB_STATE_SUCCEEDED");
}

#[tokio::test]
async fn batch_cancel_fail_and_auto_advance() {
    let bucket = Arc::new(MemBlobStore::new());
    let g = Arc::new(
        FakeGemini::new()
            .with_bucket("b", bucket.clone())
            .with_batch_mode(BatchMode::AdvanceOnPoll),
    );
    let b = Bytes::from_static(b"{\"key\":\"k\",\"request\":{\"contents\":[]}}\n");
    bucket
        .put_if_absent("in.jsonl", b.clone(), crc32c::crc32c(&b))
        .await
        .unwrap();
    let srv = g.clone().start().await;
    let jobs_url = format!("{}{BASE}/batchPredictionJobs", srv.base_url());
    let create = |display: &str| {
        json!({
            "displayName": display,
            "model": "publishers/google/models/m",
            "inputConfig": {"instancesFormat": "jsonl", "gcsSource": {"uris": ["gs://b/in.jsonl"]}},
            "outputConfig": {"predictionsFormat": "jsonl", "gcsDestination": {"outputUriPrefix": "gs://b/out"}},
            "labels": {"app": "voyager-crawler"}
        })
    };
    let mk = |d: &'static str| {
        let jobs_url = jobs_url.clone();
        let body = create(d);
        async move {
            client()
                .post(&jobs_url)
                .json(&body)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["name"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };

    // Auto mode: every poll moves the job one state.
    let a = mk("a").await;
    let poll = |n: &str| {
        let u = format!("{}/v1/{n}", srv.base_url());
        async move {
            client()
                .get(&u)
                .send()
                .await
                .unwrap()
                .json::<Value>()
                .await
                .unwrap()["state"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };
    assert_eq!(poll(&a).await, "JOB_STATE_RUNNING");
    assert_eq!(poll(&a).await, "JOB_STATE_SUCCEEDED");
    assert_eq!(poll(&a).await, "JOB_STATE_SUCCEEDED");

    // Cancel a running job.
    let c = mk("c").await;
    let r = client()
        .post(format!("{}/v1/{c}:cancel", srv.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(g.job(&c).unwrap()["state"], "JOB_STATE_CANCELLED");

    // Scripted failure.
    let f = mk("f").await;
    g.fail_job(&f, "internal");
    let j = g.job(&f).unwrap();
    assert_eq!(j["state"], "JOB_STATE_FAILED");
    assert_eq!(j["error"]["message"], "internal");

    // Unknown job → 404; unreadable input → 400.
    let r = client()
        .get(format!("{}{BASE}/batchPredictionJobs/999", srv.base_url()))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    let mut bad = create("x");
    bad["inputConfig"]["gcsSource"]["uris"] = json!(["gs://nope/in.jsonl"]);
    let r = client().post(&jobs_url).json(&bad).send().await.unwrap();
    assert_eq!(r.status(), 400);
}
