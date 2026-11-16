//! Link triage in the processor (spec §5.7): uncertain in-scope page links go to the triage
//! model; its verdict decides whether a link is enqueued, and verdicts are cached per URL
//! pattern.

#[path = "processor_support.rs"]
mod support;

use serde_json::json;
use support::*;

async fn enqueued(r: &Rig) -> Vec<String> {
    sqlx::query_scalar("SELECT url FROM urls ORDER BY url")
        .fetch_all(&r.pool)
        .await
        .unwrap()
}

async fn page(r: &Rig, path: &str, file: &str, minutes: i64) {
    let url = format!("https://events.example.org{path}");
    let c = commit(
        &r.pool,
        &r.blobs,
        Cap::html(&url, "events-test", &fixture(file), at(minutes)),
    )
    .await;
    r.proc.process(&c).await.unwrap();
}

#[tokio::test]
async fn triage_verdict_decides_enqueue_and_pattern_is_cached() {
    let r = rig(
        &["events-test"],
        RigOpts {
            frontier: true,
            triage: Some(true),
            ..RigOpts::default()
        },
    )
    .await;
    // Patterns in link order: events.example.org/e/{n}, events.example.org/shop/basket.
    r.llm.push_json(json!({"items": [
        {"i": 0, "relevant": true, "kind": "event"},
        {"i": 1, "relevant": false, "kind": null}
    ]}));
    page(&r, "/whats-on", "triage_a.html", 0).await;
    assert_eq!(r.llm.calls(), 1, "one batched triage call");
    let req = &r.llm.requests()[0];
    assert_eq!(req.purpose, vc_core::llm::Purpose::Triage);
    assert!(req.input.contains("/e/101") && req.input.contains("/shop/basket"));
    assert_eq!(
        enqueued(&r).await,
        vec!["https://events.example.org/e/101"],
        "the irrelevant link is not enqueued"
    );

    // A second page links a URL of the same pattern: answered from the cache, no model call.
    page(&r, "/more", "triage_b.html", 5).await;
    assert_eq!(r.llm.calls(), 1, "cached per URL pattern");
    assert_eq!(
        enqueued(&r).await,
        vec![
            "https://events.example.org/e/101",
            "https://events.example.org/e/202"
        ]
    );
    assert_eq!(r.ledger.open_reservations(), 0);
}

#[tokio::test]
async fn triage_with_llm_disabled_makes_no_call_and_keeps_links() {
    let r = rig(
        &["events-test"],
        RigOpts {
            frontier: true,
            triage: Some(false),
            ..RigOpts::default()
        },
    )
    .await;
    page(&r, "/whats-on", "triage_a.html", 0).await;
    assert_eq!(r.llm.calls(), 0);
    assert_eq!(
        enqueued(&r).await,
        vec![
            "https://events.example.org/e/101",
            "https://events.example.org/shop/basket"
        ],
        "undecided links keep the heuristic default"
    );
}

#[tokio::test]
async fn failed_triage_call_keeps_links() {
    let r = rig(
        &["events-test"],
        RigOpts {
            frontier: true,
            triage: Some(true),
            ..RigOpts::default()
        },
    )
    .await;
    // An answer that is not the schema: no verdicts, nothing dropped, nothing cached.
    r.llm.push_json(json!({"unexpected": true}));
    page(&r, "/whats-on", "triage_a.html", 0).await;
    assert_eq!(r.llm.calls(), 1);
    assert_eq!(enqueued(&r).await.len(), 2);
}
