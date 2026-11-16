//! Tests for the agentic loops (fake Gemini = `testkit::llm::FakeLlm`, budget =
//! `testkit::llm::MemLedger`, Postgres = `testkit::pg`).

use std::sync::Arc;

use serde_json::{Value, json};
use sqlx::PgPool;
use testkit::clock::FakeClock;
use testkit::llm::{FakeLlm, MemLedger};
use vc_config::{LlmConfig, PolicyConfig};
use vc_core::llm::{LlmError, Purpose, Usd};
use vc_core::{CanonicalUrl, Clock};
use vc_policy::VoyagerPolicy;

use super::auditor::{AuditSettings, AuditText, Verdict, run_auditor};
use super::scout::{
    ProposalStatus, ScoutCell, normalize_host, render_proposals_md, run_scout, write_proposals_md,
};
use super::triage::{LinkCandidate, MemTriageCache, PgTriageCache, triage_links, url_pattern};
use super::{AgenticLlm, Called};

struct Rig {
    fake: Arc<FakeLlm>,
    ledger: Arc<MemLedger>,
    llm: AgenticLlm,
}

fn rig_with(enabled: bool, cap: Usd) -> Rig {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::default());
    let fake = Arc::new(FakeLlm::new());
    let ledger = Arc::new(MemLedger::new(clock, cap, cap));
    let config = LlmConfig {
        enabled,
        ..LlmConfig::default()
    };
    let llm = AgenticLlm::new(fake.clone(), ledger.clone(), config);
    Rig { fake, ledger, llm }
}

fn rig() -> Rig {
    rig_with(true, Usd::from_dollars(10.0))
}

fn link(u: &str) -> LinkCandidate {
    LinkCandidate {
        url: CanonicalUrl::parse(u).expect("url"),
        anchor: "anchor".into(),
        context: "context".into(),
    }
}

fn kinds() -> Vec<String> {
    vec!["event".into(), "attraction".into()]
}

fn policy() -> VoyagerPolicy {
    VoyagerPolicy::new(PolicyConfig {
        denylist_path: testkit::vc_policy_fixtures::denylist_path()
            .to_string_lossy()
            .into_owned(),
        subresource_allowlist_path: testkit::vc_policy_fixtures::subresource_allowlist_path()
            .to_string_lossy()
            .into_owned(),
        ..PolicyConfig::default()
    })
    .expect("policy")
}

fn cell() -> ScoutCell {
    ScoutCell {
        destination: "Kyoto, Japan".into(),
        kind: "event".into(),
    }
}

async fn count(pool: &PgPool, table: &str) -> i64 {
    let (n,): (i64,) = sqlx::query_as(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
        .fetch_one(pool)
        .await
        .expect("count");
    n
}

// ---------------------------------------------------------------- triage

#[test]
fn url_pattern_generalises_ids_numbers_and_slugs() {
    let p = |u: &str| url_pattern(&CanonicalUrl::parse(u).expect("url"));
    assert_eq!(
        p("https://www.ex.com/events/2026/11/jazz-night-at-the-park?page=2&lang=en"),
        "www.ex.com/events/{n}/{n}/{slug}?lang&page"
    );
    assert_eq!(
        p("https://ex.com/hotel/h12345.html"),
        "ex.com/hotel/{id}.html"
    );
    assert_eq!(
        p("https://ex.com/item/0f3a9c2b7d1e4f5a6b7c"),
        "ex.com/item/{id}"
    );
    assert_eq!(p("https://ex.com/about"), "ex.com/about");
    assert_eq!(p("https://ex.com/events/1"), p("https://ex.com/events/987"));
    assert_ne!(p("https://ex.com/events/1"), p("https://ex.com/news/1"));
}

#[tokio::test]
async fn triage_cached_per_pattern() {
    let r = rig();
    let cache = MemTriageCache::default();
    // Two patterns among four links.
    let links = vec![
        link("https://ex.com/events/101"),
        link("https://ex.com/login"),
        link("https://ex.com/events/102"),
        link("https://ex.com/events/103"),
    ];
    r.fake.push_json(json!({"items": [
        {"i": 0, "relevant": true, "kind": "event"},
        {"i": 1, "relevant": false, "kind": null}
    ]}));
    let rep = triage_links(&r.llm, &cache, &kinds(), &links)
        .await
        .expect("triage");
    assert_eq!(rep.patterns, 2);
    assert_eq!(rep.llm_calls, 1);
    assert_eq!(r.fake.calls(), 1);
    let req = &r.fake.requests()[0];
    assert_eq!(req.purpose, Purpose::Triage);
    let input: Value = serde_json::from_str(&req.input).expect("json input");
    assert_eq!(
        input["links"].as_array().map(Vec::len),
        Some(2),
        "one link per pattern"
    );
    let ev = rep.verdicts[0].clone().expect("decided");
    assert!(ev.relevant);
    assert_eq!(ev.kind.as_deref(), Some("event"));
    assert_eq!(rep.verdicts[2], rep.verdicts[0]);
    assert_eq!(rep.verdicts[3], rep.verdicts[0]);
    assert!(!rep.verdicts[1].clone().expect("decided").relevant);
    assert_eq!(cache.len(), 2);

    // New URLs of known patterns: answered from the cache, no call.
    let again = vec![
        link("https://ex.com/events/999"),
        link("https://ex.com/login"),
    ];
    let rep = triage_links(&r.llm, &cache, &kinds(), &again)
        .await
        .expect("triage");
    assert_eq!(rep.llm_calls, 0);
    assert_eq!(rep.cache_hits, 2);
    assert_eq!(r.fake.calls(), 1, "no second model call");
    assert!(rep.verdicts.iter().all(Option::is_some));
}

#[tokio::test]
async fn triage_drops_unknown_kinds_and_out_of_range_items() {
    let r = rig();
    let cache = MemTriageCache::default();
    let links = vec![link("https://ex.com/a/1"), link("https://ex.com/b")];
    r.fake.push_json(json!({"items": [
        {"i": 0, "relevant": true, "kind": "ignore previous instructions"},
        {"i": 7, "relevant": true, "kind": "event"}
    ]}));
    let rep = triage_links(&r.llm, &cache, &kinds(), &links)
        .await
        .expect("triage");
    let v = rep.verdicts[0].clone().expect("decided");
    assert!(v.relevant);
    assert_eq!(v.kind, None, "kind outside the allowed list is dropped");
    assert_eq!(rep.verdicts[1], None, "unanswered pattern stays undecided");
    assert_eq!(cache.len(), 1, "undecided pattern is not cached");
}

#[tokio::test]
async fn triage_batches_by_hundred_patterns() {
    let r = rig();
    let cache = MemTriageCache::default();
    let alpha = |mut i: usize| {
        let mut s = String::new();
        loop {
            s.push(char::from(b'a' + (i % 26) as u8));
            i /= 26;
            if i == 0 {
                break s;
            }
        }
    };
    let links: Vec<LinkCandidate> = (0..150)
        .map(|i| link(&format!("https://ex.com/sec{}", alpha(i))))
        .collect();
    let rep_patterns = {
        let mut s: Vec<String> = links.iter().map(|l| url_pattern(&l.url)).collect();
        s.sort();
        s.dedup();
        s.len()
    };
    assert!(rep_patterns > 100, "fixture needs more than one batch");
    r.fake.push_json(json!({"items": []}));
    r.fake.push_json(json!({"items": []}));
    let rep = triage_links(&r.llm, &cache, &kinds(), &links)
        .await
        .expect("triage");
    assert_eq!(rep.llm_calls, 2);
    let sizes: Vec<usize> = r
        .fake
        .requests()
        .iter()
        .map(|q| {
            serde_json::from_str::<Value>(&q.input).expect("json")["links"]
                .as_array()
                .map(Vec::len)
                .unwrap_or(0)
        })
        .collect();
    assert_eq!(sizes, vec![100, rep_patterns - 100]);
}

#[tokio::test]
async fn triage_pg_cache_survives_restart() {
    let pool = testkit::pg::pg().await;
    let r = rig();
    r.fake
        .push_json(json!({"items": [{"i": 0, "relevant": true, "kind": "attraction"}]}));
    let rep = triage_links(
        &r.llm,
        &PgTriageCache::new(pool.clone()),
        &kinds(),
        &[link("https://ex.com/sights/17")],
    )
    .await
    .expect("triage");
    assert_eq!(rep.llm_calls, 1);
    // A fresh cache instance on the same database: no call.
    let rep = triage_links(
        &r.llm,
        &PgTriageCache::new(pool.clone()),
        &kinds(),
        &[link("https://ex.com/sights/42")],
    )
    .await
    .expect("triage");
    assert_eq!(rep.llm_calls, 0);
    assert_eq!(rep.cache_hits, 1);
    assert_eq!(
        rep.verdicts[0].clone().and_then(|v| v.kind).as_deref(),
        Some("attraction")
    );
    assert_eq!(count(&pool, "llm_cache").await, 1);
}

// ---------------------------------------------------------------- budget / disabled

#[tokio::test]
async fn budget_exhausted_makes_no_call() {
    let r = rig_with(true, Usd::ZERO);
    let cache = MemTriageCache::default();
    let rep = triage_links(&r.llm, &cache, &kinds(), &[link("https://ex.com/events/1")])
        .await
        .expect("triage");
    assert!(rep.stopped);
    assert_eq!(rep.llm_calls, 0);
    assert_eq!(rep.verdicts, vec![None]);
    assert_eq!(r.fake.calls(), 0, "no call when the ledger refuses");
    assert_eq!(r.ledger.open_reservations(), 0);
    assert!(cache.is_empty());

    let pool = testkit::pg::pg().await;
    let rep = run_scout(&r.llm, &pool, &policy(), &[cell()])
        .await
        .expect("scout");
    assert!(rep.stopped);
    assert_eq!(r.fake.calls(), 0);
    assert_eq!(count(&pool, "source_candidates").await, 0);

    seed_record(&pool, "src-a", 1).await;
    let rep = run_auditor(&r.llm, &pool, &Texts, &AuditSettings::default())
        .await
        .expect("auditor");
    assert!(rep.stopped);
    assert_eq!(r.fake.calls(), 0);
    assert_eq!(count(&pool, "audits").await, 0);
}

#[tokio::test]
async fn llm_disabled_loops_are_noops() {
    let r = rig_with(false, Usd::from_dollars(10.0));
    let cache = MemTriageCache::default();
    let rep = triage_links(&r.llm, &cache, &kinds(), &[link("https://ex.com/events/1")])
        .await
        .expect("triage");
    assert!(rep.stopped);
    assert_eq!(rep.verdicts, vec![None]);

    let pool = testkit::pg::pg().await;
    seed_record(&pool, "src-a", 1).await;
    let rep = run_scout(&r.llm, &pool, &policy(), &[cell()])
        .await
        .expect("scout");
    assert!(rep.stopped && rep.proposals.is_empty());
    let rep = run_auditor(&r.llm, &pool, &Texts, &AuditSettings::default())
        .await
        .expect("auditor");
    assert!(rep.stopped && rep.audits.is_empty() && rep.sampled == 0);

    assert_eq!(r.fake.calls(), 0);
    let (reserved, spent) = r.ledger.totals("2026-01");
    assert_eq!(
        (reserved, spent),
        (Usd::ZERO, Usd::ZERO),
        "nothing reserved"
    );
    assert_eq!(count(&pool, "source_candidates").await, 0);
    assert_eq!(count(&pool, "audits").await, 0);
}

#[tokio::test]
async fn failed_call_settles_estimate_and_free_errors_release() {
    let r = rig();
    r.fake.push(Err(LlmError::Timeout));
    let req = vc_core::llm::LlmRequest {
        model: None,
        purpose: Purpose::Triage,
        system: "s".into(),
        input: "i".into(),
        response_schema: None,
        max_output_tokens: 10,
        thinking: Default::default(),
    };
    assert_eq!(r.llm.call(req.clone()).await.expect("call"), Called::Failed);
    let (_, spent) = r.ledger.totals("2026-01");
    assert!(spent > Usd::ZERO, "a call that may have been billed counts");

    r.fake.push(Err(LlmError::RateLimited));
    assert_eq!(r.llm.call(req.clone()).await.expect("call"), Called::Failed);
    assert_eq!(
        r.ledger.totals("2026-01").1,
        spent,
        "rate-limited call is free"
    );

    r.fake.push(Err(LlmError::BudgetExhausted));
    assert_eq!(r.llm.call(req).await.expect("call"), Called::Budget);
    assert_eq!(r.ledger.open_reservations(), 0);
}

// ---------------------------------------------------------------- scout

#[test]
fn normalize_host_accepts_bare_hosts_only() {
    assert_eq!(
        normalize_host("https://WWW.Kyoto.travel/en/events?x=1").as_deref(),
        Some("www.kyoto.travel")
    );
    assert_eq!(normalize_host("kyoto.jp.").as_deref(), Some("kyoto.jp"));
    for bad in [
        "",
        "localhost",
        "10.0.0.1",
        "ex.com:8080",
        "user@ex.com",
        "-x.com",
        "a..b",
    ] {
        assert_eq!(normalize_host(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn scout_never_crawls_before_approval() {
    let pool = testkit::pg::pg().await;
    let r = rig();
    r.fake.push_json(json!({"sites": [
        {"host": "https://kyoto.travel/en/", "name": "Kyoto City Official Travel Guide",
         "reason": "Official city events calendar"},
        {"host": "www.kyotoguide.com", "name": "Kyoto Guide", "reason": "Event listings"}
    ]}));
    let rep = run_scout(&r.llm, &pool, &policy(), &[cell()])
        .await
        .expect("scout");
    assert_eq!(r.fake.calls(), 1);
    assert_eq!(r.fake.requests()[0].purpose, Purpose::Scout);
    assert_eq!(rep.proposed().count(), 2);

    let rows: Vec<(String, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT host, status, decided_by, checks FROM source_candidates ORDER BY host",
    )
    .fetch_all(&pool)
    .await
    .expect("rows");
    assert_eq!(rows.len(), 2);
    for (_, status, decided_by, checks) in &rows {
        assert_eq!(status, "proposed", "waits for a human");
        assert_eq!(decided_by, &None);
        assert_eq!(checks["denylist"]["result"], "pass");
        assert_eq!(checks["robots"]["result"], "pending", "nothing fetched yet");
        assert_eq!(checks["terms"]["result"], "pending");
    }
    assert_eq!(rows[0].0, "kyoto.travel");

    // Nothing became a source, a URL, a claim or a work item.
    for table in [
        "sources",
        "urls",
        "url_state",
        "claims",
        "work_items",
        "origins",
    ] {
        assert_eq!(count(&pool, table).await, 0, "{table} must stay empty");
    }

    // The proposals file says so and lists both ids.
    let dir = std::env::temp_dir().join(format!("vc-proposals-{}", std::process::id()));
    let date = chrono::NaiveDate::from_ymd_opt(2026, 11, 16).expect("date");
    let path = write_proposals_md(&dir, date, &rep).expect("write");
    let md = std::fs::read_to_string(&path).expect("read");
    assert!(path.ends_with("2026-11-16.md"));
    assert!(md.contains("Nothing here is crawled"));
    assert!(md.contains("`kyoto.travel`") && md.contains("`www.kyotoguide.com`"));
    assert!(md.contains("vcrawl admin source approve"));
    let _ = std::fs::remove_dir_all(&dir);

    // A second run proposing the same host writes nothing new.
    r.fake
        .push_json(json!({"sites": [{"host": "kyoto.travel", "name": "again"}]}));
    let rep = run_scout(&r.llm, &pool, &policy(), &[cell()])
        .await
        .expect("scout");
    assert_eq!(rep.proposals[0].status, ProposalStatus::Known);
    assert_eq!(count(&pool, "source_candidates").await, 2);
    let input: Value = serde_json::from_str(&r.fake.requests()[1].input).expect("json");
    assert!(
        input["avoid"]
            .as_array()
            .expect("avoid")
            .iter()
            .any(|h| h == "kyoto.travel"),
        "known hosts are passed as avoid"
    );
}

#[tokio::test]
async fn scout_rejects_denylisted_brand_domain() {
    let pool = testkit::pg::pg().await;
    let r = rig();
    r.fake.push_json(json!({"sites": [
        {"host": "www.expedia.co.uk", "name": "Expedia UK"},
        {"host": "tripadvisor.de", "name": "Tripadvisor DE"},
        {"host": "m.booking.com", "name": "Booking mobile"},
        {"host": "www.japan.travel", "name": "JNTO", "reason": "National tourism board"}
    ]}));
    let rep = run_scout(&r.llm, &pool, &policy(), &[cell()])
        .await
        .expect("scout");
    let status = |h: &str| {
        rep.proposals
            .iter()
            .find(|p| p.host == h)
            .map(|p| p.status)
            .expect("listed")
    };
    assert_eq!(status("www.expedia.co.uk"), ProposalStatus::Denylisted);
    assert_eq!(status("tripadvisor.de"), ProposalStatus::Denylisted);
    assert_eq!(status("m.booking.com"), ProposalStatus::Denylisted);
    assert_eq!(status("www.japan.travel"), ProposalStatus::Proposed);

    let rows: Vec<(String, String, Option<String>, Value)> = sqlx::query_as(
        "SELECT host, status, decided_by, checks FROM source_candidates ORDER BY host",
    )
    .fetch_all(&pool)
    .await
    .expect("rows");
    let proposed: Vec<&str> = rows
        .iter()
        .filter(|r| r.1 == "proposed")
        .map(|r| r.0.as_str())
        .collect();
    assert_eq!(proposed, vec!["www.japan.travel"]);
    let expedia = rows
        .iter()
        .find(|r| r.0 == "www.expedia.co.uk")
        .expect("row");
    assert_eq!(expedia.1, "rejected");
    assert_eq!(expedia.2.as_deref(), Some("scout:denylist"));
    assert_eq!(expedia.3["denylist"]["result"], "denied");
    assert_eq!(expedia.3["denylist"]["rule"], "expedia.*");

    let md = render_proposals_md(
        chrono::NaiveDate::from_ymd_opt(2026, 11, 16).expect("d"),
        &rep,
    );
    let (proposed_part, refused_part) = md.split_once("## Refused").expect("sections");
    assert!(!proposed_part.contains("expedia"));
    assert!(refused_part.contains("`www.expedia.co.uk`"));
}

#[tokio::test]
async fn scout_skips_hosts_of_existing_sources_and_invalid_hosts() {
    let pool = testkit::pg::pg().await;
    seed_source(&pool, "kyoto-official", "https://www.kyoto.travel/en/").await;
    let r = rig();
    r.fake.push_json(json!({"sites": [
        {"host": "kyoto.travel", "name": "dup of a source"},
        {"host": "192.168.1.10", "name": "ip"},
        {"host": "not a host", "name": "junk"}
    ]}));
    let rep = run_scout(&r.llm, &pool, &policy(), &[cell()])
        .await
        .expect("scout");
    let st: Vec<ProposalStatus> = rep.proposals.iter().map(|p| p.status).collect();
    assert_eq!(
        st,
        vec![
            ProposalStatus::Known,
            ProposalStatus::Invalid,
            ProposalStatus::Invalid
        ]
    );
    assert_eq!(count(&pool, "source_candidates").await, 0);
}

// ---------------------------------------------------------------- auditor

struct Texts;

impl AuditText for Texts {
    async fn text(&self, capture_id: &str) -> vc_core::Result<Option<String>> {
        Ok((capture_id != "gone").then(|| format!("page text of {capture_id}")))
    }
}

async fn seed_source(pool: &PgPool, id: &str, seed: &str) {
    sqlx::query(
        "INSERT INTO sources (source_id, name, category, source_class, licence_class, licence,
                              method, spec, spec_hash)
         VALUES ($1, $1, 'events', 'official', 'facts_only', '{}', 'html', $2, 'h')
         ON CONFLICT DO NOTHING",
    )
    .bind(id)
    .bind(json!({"id": id, "seeds": [seed]}))
    .execute(pool)
    .await
    .expect("source");
}

/// A source with one active record (current version 1) captured as `cap-<n>`.
async fn seed_record(pool: &PgPool, source: &str, n: i32) -> i64 {
    seed_source(pool, source, &format!("https://{source}.example/")).await;
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO records (kind, source_id, url_key, record_key, licence_class, current_version)
         VALUES ('event', $1, $2, 'k', 'facts_only', 1) RETURNING record_id",
    )
    .bind(source)
    .bind(format!("url-{source}-{n}").into_bytes())
    .fetch_one(pool)
    .await
    .expect("record");
    sqlx::query(
        "INSERT INTO record_versions (record_id, version, capture_id, captured_at, data, data_hash,
                                      kind_schema, extractor_ver, method, licence_class, use_policy)
         VALUES ($1, 1, $2, now(), $3, '\\x00', 'event@1', 'x', 'llm', 'facts_only', '{}')",
    )
    .bind(id)
    .bind(format!("cap-{n}"))
    .bind(json!({"name": "Gion Matsuri", "start_date": "2026-07-01"}))
    .execute(pool)
    .await
    .expect("version");
    id
}

async fn quality(pool: &PgPool, record_id: i64) -> (f32, f32) {
    sqlx::query_as(
        "SELECT r.quality, rv.quality FROM records r JOIN record_versions rv USING (record_id)
         WHERE record_id = $1",
    )
    .bind(record_id)
    .fetch_one(pool)
    .await
    .expect("quality")
}

async fn source_score(pool: &PgPool, id: &str) -> f32 {
    let (q,): (f32,) = sqlx::query_as("SELECT quality_score FROM sources WHERE source_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("score");
    q
}

#[tokio::test]
async fn auditor_lowers_quality_score() {
    let pool = testkit::pg::pg().await;
    let rec = seed_record(&pool, "src-a", 1).await;
    assert_eq!(source_score(&pool, "src-a").await, 1.0);
    let r = rig();
    // The model's score disagrees with its verdict: it is clamped into the failure band.
    r.fake.push_json(json!({"verdict": "wrong", "score": 0.9,
                            "problems": ["start_date differs from the page"]}));
    let rep = run_auditor(&r.llm, &pool, &Texts, &AuditSettings::default())
        .await
        .expect("auditor");
    assert_eq!(r.fake.calls(), 1);
    let req = &r.fake.requests()[0];
    assert_eq!(req.purpose, Purpose::Auditor);
    let input: Value = serde_json::from_str(&req.input).expect("json");
    assert_eq!(input["page_text"], "page text of cap-1");
    assert_eq!(input["record"]["name"], "Gion Matsuri");

    assert_eq!(rep.audits.len(), 1);
    assert_eq!(rep.audits[0].verdict, Verdict::Wrong);
    assert!((rep.audits[0].score - 0.4).abs() < 1e-6);
    let (rq, vq) = quality(&pool, rec).await;
    assert!(
        rq < 0.5 && vq < 0.5,
        "record quality lowered from 0.5: {rq} {vq}"
    );
    let s = source_score(&pool, "src-a").await;
    assert!(s < 1.0, "source quality_score lowered: {s}");
    assert_eq!(rep.below_threshold, vec!["src-a".to_string()]);

    let (verdict, score, notes): (String, f32, Value) =
        sqlx::query_as("SELECT verdict, score, notes FROM audits WHERE record_id = $1")
            .bind(rec)
            .fetch_one(&pool)
            .await
            .expect("audit row");
    assert_eq!(verdict, "wrong");
    assert!((score - 0.4).abs() < 1e-6);
    assert_eq!(notes["problems"][0], "start_date differs from the page");
}

#[tokio::test]
async fn auditor_ok_keeps_record_quality_and_skips_missing_text() {
    let pool = testkit::pg::pg().await;
    let rec = seed_record(&pool, "src-b", 2).await;
    let gone = seed_record(&pool, "src-b", 3).await;
    sqlx::query("UPDATE record_versions SET capture_id = 'gone' WHERE record_id = $1")
        .bind(gone)
        .execute(&pool)
        .await
        .expect("update");
    let r = rig();
    r.fake
        .push_json(json!({"verdict": "ok", "score": 0.97, "problems": []}));
    let rep = run_auditor(&r.llm, &pool, &Texts, &AuditSettings::default())
        .await
        .expect("auditor");
    assert_eq!(rep.sampled, 2);
    assert_eq!(rep.no_text, 1);
    assert_eq!(r.fake.calls(), 1, "no call without page text");
    assert_eq!(
        quality(&pool, rec).await,
        (0.5, 0.5),
        "ok leaves record quality alone"
    );
    let s = source_score(&pool, "src-b").await;
    assert!((s - 0.97).abs() < 1e-6);
    assert!(rep.below_threshold.is_empty());
}
