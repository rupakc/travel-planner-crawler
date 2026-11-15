//! `fake_gemini`: Vertex AI online + batch wire format for E2E.
//!
//! ```text
//! fake_gemini [--listen 0.0.0.0:8080] [--gcs-url http://fake-gcs:4443] [--buckets a,b]
//!             [--rules rules.json] [--batch-mode auto|manual] [--embedding-dim 768]
//! ```
//!
//! Every flag also reads `FAKE_GEMINI_<FLAG>`. `--rules` is a JSON array of
//! `{"contains": "<needle>", "text": "<reply text>"}` (first match wins; default `{}`).
//! Batch jobs read/write the listed buckets on the GCS server at `--gcs-url`. In `auto` mode
//! (default) every GET of a job moves it one state.

use std::sync::Arc;

use testkit::fake_gcs::FakeGcsStore;
use testkit::fake_gemini::{BatchMode, FakeGemini};
use testkit::server::FakeServer;

fn arg(name: &str) -> Option<String> {
    let flag = format!("--{name}");
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        if a == flag {
            return it.next();
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v.to_string());
        }
    }
    std::env::var(format!(
        "FAKE_GEMINI_{}",
        name.replace('-', "_").to_ascii_uppercase()
    ))
    .ok()
}

#[tokio::main]
async fn main() {
    let listen = arg("listen").unwrap_or_else(|| "0.0.0.0:8080".into());
    let mode = match arg("batch-mode").as_deref() {
        None | Some("auto") => BatchMode::AdvanceOnPoll,
        Some("manual") => BatchMode::Manual,
        Some(o) => panic!("--batch-mode must be auto or manual, not {o}"),
    };
    let mut g = FakeGemini::new().with_batch_mode(mode);
    if let Some(d) = arg("embedding-dim") {
        g = g.with_embedding_dim(d.parse().expect("--embedding-dim must be a number"));
    }
    if let Some(gcs) = arg("gcs-url") {
        for b in arg("buckets")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|b| !b.is_empty())
        {
            let store = FakeGcsStore::new(&gcs, b);
            if let Err(e) = store.create_bucket().await {
                eprintln!("fake_gemini: cannot create bucket {b}: {e}");
            }
            g = g.with_bucket(b, Arc::new(store));
        }
    }
    if let Some(path) = arg("rules") {
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let rules: Vec<serde_json::Value> =
            serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"));
        for r in rules {
            let needle = r["contains"].as_str().expect("rule.contains is a string");
            let text = r["text"].as_str().expect("rule.text is a string");
            g.add_rule(needle, text);
        }
    }
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .unwrap_or_else(|e| panic!("bind {listen}: {e}"));
    let _server = FakeServer::spawn_on(listener, Arc::new(g).router());
    eprintln!("fake_gemini listening on {listen}");
    let _ = tokio::signal::ctrl_c().await;
}
