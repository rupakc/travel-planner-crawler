//! Behaviour of the in-memory fakes that P1 crates test against.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use testkit::{
    FakeClock, FakeLlm, MemBlobStore, MemFrontier, MemLedger, MemQueue, RequestLog,
    RequestLogEntry, UrlState,
};
use vc_core::frontier::{Frontier, KeyOutcome, LeaseCheck, NewUrl, UrlOutcome, UrlResult};
use vc_core::llm::{
    BudgetLedger, LlmClient, LlmError, LlmRequest, Purpose, ThinkingLevel, Usage, Usd,
};
use vc_core::queue::{WorkKind, WorkQueue};
use vc_core::store::{BlobStore, PutOutcome};
use vc_core::{CanonicalUrl, CaptureId, CaptureRef, Clock, Error};

fn clock() -> (FakeClock, Arc<dyn Clock>) {
    let c = FakeClock::default();
    (c.clone(), Arc::new(c))
}

#[tokio::test]
async fn mem_frontier_fences_epochs_and_pending_urls() {
    let (fc, clk) = clock();
    let f = MemFrontier::new(clk.clone());
    let u = CanonicalUrl::parse("https://a.example.com/x").unwrap();
    assert_eq!(
        f.enqueue(&[NewUrl::new(u.clone()), NewUrl::new(u.clone())])
            .await
            .unwrap(),
        1
    );
    let c1 = f.claim(&[0], 5).await.unwrap().expect("claim");
    assert_eq!(c1.urls.len(), 1);
    assert_eq!(c1.robots_due.len(), 1);
    assert!(f.claim(&[0], 5).await.unwrap().is_none(), "key is claimed");
    f.check(&c1.permit).await.unwrap();
    f.take_rate_token(&c1.permit).await.unwrap();

    // Lease expires; a new claim supersedes the old epoch.
    fc.advance(Duration::from_secs(241));
    assert!(f.check(&c1.permit).await.is_err());
    let c2 = f
        .claim(&[0], 5)
        .await
        .unwrap()
        .expect("re-claim after expiry");
    assert!(c2.permit.epoch() > c1.permit.epoch());
    assert!(
        f.complete(c1.permit, KeyOutcome::normal()).await.is_err(),
        "stale epoch rejected"
    );

    // fetched_pending is never re-leased; the commit of its epoch returns it to due.
    f.mark_fetched(&c2.permit, &u.key(), clk.now())
        .await
        .unwrap();
    let epoch = c2.permit.epoch();
    let mut o = KeyOutcome::normal();
    o.urls.push(UrlOutcome {
        url_key: u.key(),
        result: UrlResult::Fetched,
    });
    f.complete(c2.permit, o).await.unwrap();
    assert_eq!(f.url_state(&u.key()), Some(UrlState::FetchedPending));
    fc.advance(Duration::from_secs(10));
    assert!(f.claim(&[0], 5).await.unwrap().is_none());
    let cap = CaptureRef {
        capture_id: CaptureId::new(),
        captured_at: clk.now(),
    };
    f.commit_capture(&u.key(), epoch, cap, Default::default(), None);
    assert_eq!(f.url_state(&u.key()), Some(UrlState::Due));
}

#[tokio::test]
async fn mem_frontier_extend_moves_permit_deadline() {
    let (_fc, clk) = clock();
    let f = MemFrontier::new(clk);
    f.enqueue(&[NewUrl::new(
        CanonicalUrl::parse("https://b.example/").unwrap(),
    )])
    .await
    .unwrap();
    let c = f.claim(&[0], 1).await.unwrap().unwrap();
    let before = c.permit.deadline();
    f.extend(&c.permit, Duration::from_secs(60)).await.unwrap();
    assert_eq!((c.permit.deadline().0 - before.0).num_seconds(), 60);
    assert!(f.claim(&[0], 6).await.is_err(), "max 5 urls per claim");
}

#[tokio::test]
async fn mem_frontier_reschedule() {
    let (fc, clk) = clock();
    let f = MemFrontier::new(clk.clone()).with_delay(Duration::ZERO);
    let u = CanonicalUrl::parse("https://r.example.com/x").unwrap();
    let k = u.key();
    f.enqueue(&[NewUrl::new(u.clone())]).await.unwrap();
    assert!(
        !f.reschedule(
            &CanonicalUrl::parse("https://nope.example/").unwrap().key(),
            clk.now()
        )
        .await
        .unwrap(),
        "unknown url"
    );

    // Leased: undisturbed until it returns to due, then due no later than `at`.
    let c = f.claim(&[0], 5).await.unwrap().unwrap();
    let at = clk.now() + chrono::Duration::minutes(5);
    assert!(f.reschedule(&k, at).await.unwrap());
    assert_eq!(f.url_state(&k), Some(UrlState::Leased));
    f.check(&c.permit).await.unwrap();
    let mut o = KeyOutcome::normal();
    o.urls.push(UrlOutcome {
        url_key: k,
        result: UrlResult::Retry {
            at: clk.now() + chrono::Duration::days(1),
        },
    });
    f.complete(c.permit, o).await.unwrap();
    assert_eq!(f.url_state(&k), Some(UrlState::Due));
    assert_eq!(f.next_fetch_at(&k), Some(at));

    // Due: moves to `at` (earlier), and the key's own delay still applies.
    let now_at = clk.now();
    assert!(f.reschedule(&k, now_at).await.unwrap());
    assert_eq!(f.next_fetch_at(&k), Some(now_at));
    let c = f.claim(&[0], 5).await.unwrap().expect("due now");

    // fetched_pending: applied at the commit of its epoch.
    let day = clk.now() + chrono::Duration::days(1);
    f.mark_fetched(&c.permit, &k, day).await.unwrap();
    let epoch = c.permit.epoch();
    let at2 = clk.now() + chrono::Duration::minutes(1);
    assert!(f.reschedule(&k, at2).await.unwrap());
    let mut o = KeyOutcome::normal();
    o.urls.push(UrlOutcome {
        url_key: k,
        result: UrlResult::Fetched,
    });
    f.complete(c.permit, o).await.unwrap();
    assert_eq!(f.url_state(&k), Some(UrlState::FetchedPending));
    fc.advance(Duration::from_secs(1));
    let cap = CaptureRef {
        capture_id: CaptureId::new(),
        captured_at: clk.now(),
    };
    f.commit_capture(&k, epoch, cap, Default::default(), None);
    assert_eq!(f.url_state(&k), Some(UrlState::Due));
    assert_eq!(f.next_fetch_at(&k), Some(at2));

    // Terminal states are not revived.
    fc.advance(Duration::from_secs(120));
    let c = f.claim(&[0], 5).await.unwrap().expect("due");
    let mut o = KeyOutcome::normal();
    o.urls.push(UrlOutcome {
        url_key: k,
        result: UrlResult::Disallowed,
    });
    f.complete(c.permit, o).await.unwrap();
    assert!(!f.reschedule(&k, clk.now()).await.unwrap());
    assert_eq!(f.url_state(&k), Some(UrlState::Disabled));
}

#[tokio::test]
async fn mem_blob_store_is_create_only() {
    let s = MemBlobStore::new();
    let b = Bytes::from_static(b"warc");
    let crc = crc32c::crc32c(&b);
    assert_eq!(
        s.put_if_absent("segments/a", b.clone(), crc).await.unwrap(),
        PutOutcome::Created
    );
    assert_eq!(
        s.put_if_absent("segments/a", b.clone(), crc).await.unwrap(),
        PutOutcome::AlreadyIdentical
    );
    let other = Bytes::from_static(b"WARC");
    assert!(matches!(
        s.put_if_absent("segments/a", other.clone(), crc32c::crc32c(&other))
            .await,
        Err(Error::Conflict(_))
    ));
    assert!(
        s.put_if_absent("segments/b", b.clone(), crc + 1)
            .await
            .is_err(),
        "crc mismatch"
    );
    assert_eq!(&s.get("segments/a", Some(1..3)).await.unwrap()[..], b"ar");
    assert_eq!(s.list("segments/").await.unwrap(), vec!["segments/a"]);
}

#[tokio::test]
async fn mem_queue_dedupes_redelivers_and_dead_letters() {
    let (fc, clk) = clock();
    let q = MemQueue::new(clk);
    let id = q
        .enqueue(WorkKind::Extract, serde_json::json!({}), Some("k"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        q.enqueue(WorkKind::Extract, serde_json::json!({}), Some("k"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        q.take(WorkKind::Extract, 10, Duration::from_secs(5))
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        q.take(WorkKind::Extract, 10, Duration::from_secs(5))
            .await
            .unwrap()
            .is_empty()
    );
    fc.advance(Duration::from_secs(6));
    assert_eq!(
        q.take(WorkKind::Extract, 10, Duration::from_secs(5))
            .await
            .unwrap()
            .len(),
        1,
        "lease expired"
    );
    for _ in 0..8 {
        q.nack(id, Duration::ZERO).await.unwrap();
        q.take(WorkKind::Extract, 1, Duration::from_secs(5))
            .await
            .unwrap();
    }
    assert_eq!(q.state(id), Some(testkit::queue::ItemState::Dead));
}

#[tokio::test]
async fn mem_ledger_never_exceeds_caps_under_concurrency() {
    let (_fc, clk) = clock();
    let l = Arc::new(MemLedger::new(
        clk,
        Usd::from_dollars(1.0),
        Usd::from_dollars(40.0),
    ));
    let tasks: Vec<_> = (0..100)
        .map(|_| {
            let l = l.clone();
            tokio::spawn(async move {
                l.reserve(Purpose::Extraction, Usd::from_dollars(0.03))
                    .await
            })
        })
        .collect();
    let mut ok = Vec::new();
    for t in tasks {
        if let Some(r) = t.await.unwrap().unwrap() {
            ok.push(r);
        }
    }
    assert_eq!(ok.len(), 33);
    let r = ok.pop().unwrap();
    l.settle(
        r.clone(),
        Usage {
            cost: Usd::from_dollars(0.01),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    l.settle(r, Usage::default()).await.unwrap(); // idempotent
    let (reserved, spent) = l.totals(&ok[0].day.format("%Y-%m-%d").to_string());
    assert_eq!(
        (reserved, spent),
        (Usd::from_dollars(0.96), Usd::from_dollars(0.01))
    );
}

#[tokio::test]
async fn fake_llm_scripts_and_disables() {
    let (_fc, clk) = clock();
    let ledger = MemLedger::new(clk, Usd::from_dollars(1.0), Usd::from_dollars(1.0));
    let r = ledger
        .reserve(Purpose::Extraction, Usd::ZERO)
        .await
        .unwrap()
        .unwrap();
    let llm = FakeLlm::new();
    let req = LlmRequest {
        model: None,
        purpose: Purpose::Extraction,
        system: "s".into(),
        input: "i".into(),
        response_schema: None,
        max_output_tokens: 400,
        thinking: ThinkingLevel::Off,
    };
    llm.push_json(serde_json::json!({"a": 1}));
    assert_eq!(
        llm.generate(&r, req.clone()).await.unwrap().json,
        Some(serde_json::json!({"a": 1}))
    );
    llm.set_enabled(false);
    assert!(matches!(
        llm.generate(&r, req).await,
        Err(Error::Llm(LlmError::Disabled))
    ));
    assert_eq!(llm.calls(), 1);
}

#[test]
fn request_log_roundtrips_jsonl() {
    let log = RequestLog::new();
    let t = chrono::Utc::now();
    log.record(RequestLogEntry {
        key: "example.com".into(),
        url: "https://example.com/".into(),
        started_at: t,
        ended_at: t,
        status: 200,
        ua: vc_core::USER_AGENT.into(),
    });
    let back = RequestLog::from_jsonl(&log.to_jsonl()).unwrap();
    assert_eq!(back.entries(), log.entries());
    assert_eq!(back.by_key()["example.com"].len(), 1);
}

/// Like `PgFrontier`, the fake never enqueues a URL a standing exclusion names (host,
/// url_prefix, url, source); authors never select URLs.
#[tokio::test]
async fn mem_frontier_enqueue_skips_tombstoned_urls() {
    use vc_core::index::PurgeSelector;
    let (_fc, clk) = clock();
    let f = MemFrontier::new(clk);
    let nu = |s: &str| NewUrl::new(CanonicalUrl::parse(s).unwrap());
    let exact = nu("https://exact.example/gone");
    f.tombstone(PurgeSelector::Host("purged.example".into()));
    f.tombstone(PurgeSelector::UrlPrefix(
        "https://mixed.example/private/".into(),
    ));
    f.tombstone(PurgeSelector::Url(exact.url.key()));
    f.tombstone(PurgeSelector::Source(vc_core::SourceId("gone-src".into())));
    f.tombstone(PurgeSelector::Author("sha256:00".into()));
    let mut from_src = nu("https://ok.example/s");
    from_src.source_id = Some(vc_core::SourceId("gone-src".into()));
    let keep = [
        nu("https://sub.purged.example/a"),
        nu("https://mixed.example/public/b"),
        nu("https://ok.example/c"),
    ];
    let drop = [
        nu("https://purged.example/new"),
        nu("https://mixed.example/private/x"),
        exact,
        from_src,
    ];
    let batch: Vec<NewUrl> = keep.iter().chain(drop.iter()).cloned().collect();
    assert_eq!(f.enqueue(&batch).await.unwrap(), keep.len() as u64);
    for k in &keep {
        assert_eq!(f.url_state(&k.url.key()), Some(UrlState::Due), "{}", k.url);
    }
    for d in &drop {
        assert_eq!(f.url_state(&d.url.key()), None, "{}", d.url);
    }
}
