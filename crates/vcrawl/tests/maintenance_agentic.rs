//! Role `maintenance`: the weekly agentic jobs (source scout, quality auditor) — scheduled,
//! fed by the under-covered cell query and the text tier, and silent with the LLM disabled.

mod admin_common;

use std::sync::Arc;

use admin_common::*;
use chrono::Duration;
use serde_json::json;
use testkit::llm::{FakeLlm, MemLedger};
use testkit::vc_index_fixtures::{licence, prov_at, record};
use testkit::{FakeClock, MemBlobStore};
use vc_config::{LlmConfig, ParseConfig, PolicyConfig};
use vc_core::index::RecordSink;
use vc_core::licence::LicenceClass;
use vc_core::llm::Usd;
use vc_core::media::MediaType;
use vc_core::{Clock, SegmentId};
use vcrawl::agentic::AgenticLlm;
use vcrawl::agentic::auditor::AuditText;
use vcrawl::roles::maintenance::{
    Agentic, CellSettings, Job, Maintenance, Schedule, StoreAuditText, under_covered_cells,
};

struct Rig {
    fake: Arc<FakeLlm>,
    ledger: Arc<MemLedger>,
    m: Maintenance,
    dir: tempfile::TempDir,
}

fn rig(pool: &sqlx::PgPool, store: Arc<MemBlobStore>, enabled: bool) -> Rig {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new(at(15, 0)));
    let fake = Arc::new(FakeLlm::new());
    let cap = Usd::from_dollars(10.0);
    let ledger = Arc::new(MemLedger::new(clock, cap, cap));
    let config = LlmConfig {
        enabled,
        ..LlmConfig::default()
    };
    let policy = vc_policy::VoyagerPolicy::new(PolicyConfig {
        denylist_path: testkit::vc_policy_fixtures::denylist_path()
            .to_string_lossy()
            .into_owned(),
        subresource_allowlist_path: testkit::vc_policy_fixtures::subresource_allowlist_path()
            .to_string_lossy()
            .into_owned(),
        ..PolicyConfig::default()
    })
    .expect("policy");
    let dir = tempfile::tempdir().expect("tmp");
    let agentic = Agentic {
        llm: AgenticLlm::new(fake.clone(), ledger.clone(), config),
        policy: Arc::new(policy),
        proposals_dir: dir.path().join("proposals"),
        text: Arc::new(StoreAuditText::new(
            pool.clone(),
            store.clone(),
            &ParseConfig::default(),
        )),
        cells: CellSettings {
            min_destination_records: 3,
            min_cell_records: 2,
            max_cells: 3,
            recent_days: 90,
        },
    };
    let m = Maintenance {
        pool: pool.clone(),
        store,
        metrics: None,
        partitions_ahead_months: 2,
        agentic: Some(agentic),
    };
    Rig {
        fake,
        ledger,
        m,
        dir,
    }
}

/// `n` live attraction records in Kyoto, Japan (one capture), plus `events` events.
async fn kyoto(pool: &sqlx::PgPool, n: usize, events: usize) {
    let ix = vc_index::PgIndex::new(pool.clone());
    let lic = licence(LicenceClass::Open, Some("CC0-1.0"), "Kyoto City");
    for (kind, count) in [("attraction", n), ("event", events)] {
        let recs = (0..count)
            .map(|i| {
                record(
                    kind,
                    &format!("{kind}-{i}"),
                    json!({"name": format!("{kind} {i}"), "city": "Kyoto", "country": "Japan"}),
                )
            })
            .collect();
        let p = prov_at(
            "kyoto-city",
            lic.clone(),
            &format!("https://kyoto.example/{kind}"),
            at(10, 1),
        );
        ix.upsert(recs, &p).await.expect("upsert");
    }
}

#[test]
fn agentic_jobs_are_scheduled_weekly() {
    let t = at(1, 0);
    let mut s = Schedule::default();
    assert!(s.due(t).contains(&Job::Scout) && s.due(t).contains(&Job::Auditor));
    for j in Job::ALL {
        s.done(j, t);
    }
    let day = s.due(t + Duration::days(1));
    assert!(!day.contains(&Job::Scout) && !day.contains(&Job::Auditor));
    let week = s.due(t + Duration::days(7));
    assert!(week.contains(&Job::Scout) && week.contains(&Job::Auditor));
}

#[tokio::test]
async fn under_covered_cells_skip_covered_and_recently_proposed() {
    let pool = testkit::pg().await;
    kyoto(&pool, 4, 2).await;
    let s = CellSettings {
        min_destination_records: 3,
        min_cell_records: 2,
        max_cells: 20,
        recent_days: 90,
    };
    let cells = under_covered_cells(&pool, &s).await.expect("cells");
    let kinds: Vec<&str> = cells.iter().map(|c| c.kind.as_str()).collect();
    assert!(cells.iter().all(|c| c.destination == "Kyoto, Japan"));
    assert!(
        !kinds.contains(&"attraction") && !kinds.contains(&"event"),
        "{kinds:?}"
    );
    assert!(
        kinds.contains(&"hotel") && kinds.contains(&"walking_tour"),
        "{kinds:?}"
    );

    sqlx::query(
        "INSERT INTO source_candidates (host, proposed_by, destination, kinds)
         VALUES ('hotels.kyoto.example', 'scout', 'Kyoto, Japan', '{hotel}')",
    )
    .execute(&pool)
    .await
    .expect("candidate");
    let again = under_covered_cells(&pool, &s).await.expect("cells");
    assert_eq!(again.len(), cells.len() - 1);
    assert!(again.iter().all(|c| c.kind != "hotel"));

    // A small destination is not a cell at all.
    let none = under_covered_cells(
        &pool,
        &CellSettings {
            min_destination_records: 50,
            ..s
        },
    )
    .await
    .expect("cells");
    assert!(none.is_empty());
}

#[tokio::test]
async fn llm_disabled_makes_no_model_call() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    kyoto(&pool, 4, 0).await;
    let r = rig(&pool, store, false);
    let mut sched = Schedule::default();
    let ran = r.m.tick(&mut sched, at(15, 0)).await;
    assert!(
        ran.contains(&Job::Scout) && ran.contains(&Job::Auditor),
        "{ran:?}"
    );
    assert_eq!(r.fake.calls(), 0, "no model call");
    assert_eq!(r.ledger.open_reservations(), 0, "no reservation");
    assert_eq!(
        count(&pool, "SELECT count(*) FROM source_candidates").await,
        0
    );
    assert_eq!(count(&pool, "SELECT count(*) FROM audits").await, 0);
    assert!(r.m.scout(at(15, 0)).await.expect("scout").is_none());
    assert!(r.m.audit().await.expect("audit").is_none());
    assert!(!r.dir.path().join("proposals").exists());
}

#[tokio::test]
async fn scout_job_proposes_for_under_covered_cells() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    kyoto(&pool, 4, 0).await;
    let r = rig(&pool, store, true);
    for i in 0..3 {
        r.fake.push_json(json!({"sites": [
            {"host": format!("https://www.kyoto-site-{i}.example/"), "name": "Site", "reason": "covers it"}
        ]}));
    }
    let rep = r.m.scout(at(15, 0)).await.expect("scout").expect("ran");
    assert_eq!(r.fake.calls(), 3, "one call per cell (max_cells = 3)");
    assert_eq!(rep.proposals.len(), 3);
    let dests: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT destination FROM source_candidates")
            .fetch_all(&pool)
            .await
            .expect("dest");
    assert_eq!(dests, vec!["Kyoto, Japan".to_string()]);
    let files: Vec<_> = std::fs::read_dir(r.dir.path().join("proposals"))
        .expect("proposals dir")
        .collect();
    assert_eq!(files.len(), 1);
    // The scout only proposes: nothing to crawl appeared.
    assert_eq!(count(&pool, "SELECT count(*) FROM sources").await, 0);
    assert_eq!(count(&pool, "SELECT count(*) FROM url_state").await, 0);
    assert_eq!(
        count(
            &pool,
            "SELECT count(*) FROM work_items WHERE kind <> 'capture_ready'"
        )
        .await,
        0
    );
}

#[tokio::test]
async fn audit_job_uses_the_text_tier() {
    let pool = testkit::pg().await;
    let store = Arc::new(MemBlobStore::new());
    let sid = SegmentId::at(at(10, 1));
    let tiered = meta("https://kyoto.example/a", at(10, 1), Some("kyoto-city"));
    let mut html = meta("https://kyoto.example/b", at(10, 1), Some("kyoto-city"));
    html.media_type = MediaType::Html;
    let (tc, hc) = (tiered.capture_ref(), html.capture_ref());
    commit_segment(
        &pool,
        &store,
        sid,
        false,
        &[
            entry(tiered, "raw"),
            entry(
                html,
                "<html><body><main><p>Gion Matsuri runs all July.</p></main></body></html>",
            ),
        ],
    )
    .await;
    let rows =
        vec![json!({"capture_id": tc.capture_id.to_string(), "text": "Kinkaku-ji opens at 9."})];
    let (path, _) = vc_store::text::put_text_tier(store.as_ref(), sid, false, &rows, 3)
        .await
        .expect("text tier");
    // The processor writes the object without a `text_ref`: found by the segment path.
    let _ = path;

    let text = StoreAuditText::new(pool.clone(), store.clone(), &ParseConfig::default());
    assert_eq!(
        text.text(&tc.capture_id.to_string()).await.expect("tier"),
        Some("Kinkaku-ji opens at 9.".to_string())
    );
    let parsed = text.text(&hc.capture_id.to_string()).await.expect("body");
    assert!(
        parsed
            .as_deref()
            .is_some_and(|t| t.contains("Gion Matsuri")),
        "{parsed:?}"
    );
    assert_eq!(
        text.text("01JD0000000000000000000000").await.expect("gone"),
        None
    );

    // The auditor job audits the record built from the tiered capture.
    let ix = vc_index::PgIndex::new(pool.clone());
    let mut p = prov_at(
        "kyoto-city",
        licence(LicenceClass::Open, None, "Kyoto City"),
        "https://kyoto.example/a",
        at(10, 1),
    );
    p.capture = tc;
    ix.upsert(
        vec![record(
            "attraction",
            "k",
            json!({"name": "Kinkaku-ji", "city": "Kyoto", "country": "Japan"}),
        )],
        &p,
    )
    .await
    .expect("upsert");
    let r = rig(&pool, store, true);
    r.fake
        .push_json(json!({"verdict": "ok", "score": 0.9, "problems": []}));
    let rep = r.m.audit().await.expect("audit").expect("ran");
    assert_eq!((rep.sampled, rep.llm_calls, rep.no_text), (1, 1, 0));
    assert_eq!(count(&pool, "SELECT count(*) FROM audits").await, 1);
    let input = &r.fake.requests()[0].input;
    assert!(
        input.contains("Kinkaku-ji opens at 9."),
        "the page text is sent"
    );
}
