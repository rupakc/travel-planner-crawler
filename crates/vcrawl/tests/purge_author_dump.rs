//! Author erasure, second half of final review B3: once an author is purged, the next run of
//! the Stack Exchange dump adapter must not bring them back. The fetcher hands
//! `se_travel_dump` the purged authors' hashes (`tombstones`) when the run starts, so their
//! posts never reach raw, the text tier or the index.

#[path = "processor_support.rs"]
mod support;

mod crawl_path_support;

use std::sync::Arc;

use chrono::Utc;
use testkit::fake_web::{FakeWeb, Reply, Site};
use vc_core::index::{PurgeSelector, TombstoneReason};
use vc_core::store::BlobStore;
use vcrawl::admin::purge::{process_purge, request_purge};
use vcrawl::data::sources::SourceCache;
use vcrawl::data::text_tier::TextTier;
use vcrawl::roles::fetcher::{Fetcher, Pass};
use vcrawl::roles::scheduler::Scheduler;

const AUTHOR: &str = "Jordan Q";
const SITE_FILE: &str = "/download/item/item/travel.stackexchange.com.7z/Posts.xml";

fn posts() -> String {
    let row = |id: u32, kind: u32, parent: &str, user: u32, name: &str, body: &str| {
        format!(
            "  <row Id=\"{id}\" PostTypeId=\"{kind}\" {parent}CreationDate=\"2020-01-0{id}T10:00:00.000\" \
             Score=\"3\" Body=\"&lt;p&gt;{body}&lt;/p&gt;\" OwnerUserId=\"{user}\" \
             OwnerDisplayName=\"{name}\" Title=\"Petra at night\" Tags=\"|petra|\" \
             ContentLicense=\"CC BY-SA 4.0\" />\n"
        )
    };
    format!(
        "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<posts>\n{}{}{}</posts>\n",
        row(1, 1, "", 76, "Bob", "Is Petra by night worth it?"),
        row(2, 2, "ParentId=\"1\" ", 77, AUTHOR, "Yes, go at nine sharp"),
        row(3, 2, "ParentId=\"1\" ", 78, "Ann", "Only on Mondays"),
    )
}

/// Every stored blob (segments, CDXJ, text tier), decompressed when it is zstd.
async fn blobs_text(store: &testkit::MemBlobStore) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for p in store.paths() {
        let b = store.get(&p, None).await.expect("blob");
        let raw = vc_store::segment::decompress(&b).unwrap_or_else(|_| b.to_vec());
        out.push((p, String::from_utf8_lossy(&raw).into_owned()));
    }
    out
}

async fn traces(store: &testkit::MemBlobStore) -> Vec<String> {
    blobs_text(store)
        .await
        .into_iter()
        .filter(|(_, t)| {
            t.contains(AUTHOR) || t.contains("/users/77") || t.contains("go at nine sharp")
        })
        .map(|(p, _)| p)
        .collect()
}

async fn crawl_once(f: &Fetcher) {
    match f.pass().await.expect("pass") {
        Pass::Claimed(r) => assert!(!r.spooled.is_empty(), "{r:?}"),
        other => panic!("no claim: {other:?}"),
    }
    f.flush().await.expect("flush");
}

async fn process_all(r: &support::Rig, text: &TextTier) {
    while r.proc.tick(8).await.expect("process") > 0 {}
    let t = text.tick(10).await.expect("text tier");
    assert!(t.is_clean(), "{:?}", t.failed);
}

/// Purge an author, re-ingest a dump containing them: no trace in raw, the text tier or the
/// index; the other authors' posts are re-ingested.
#[tokio::test]
async fn purged_author_stays_out_of_a_reingested_dump() {
    let web = Arc::new(FakeWeb::with_sites(vec![
        Site::new("archive", "archive.test")
            .route("/robots.txt", Reply::text("User-agent: *\nAllow: /\n"))
            .route(SITE_FILE, Reply::new(200, "text/xml", posts())),
    ]));
    let srv = web.clone().start().await;
    let port = srv.addr().port();
    let pool = testkit::pg().await;
    let row = crawl_path_support::url("archive.test", port, "/details/item");
    let spec = crawl_path_support::spec(&format!(
        "id: se-test\nmethod: dump\nadapter: se_travel_dump\ncadence: quarterly\nseeds: [\"{row}\"]\n"
    ));
    vc_sources::seed::seed_sources(&pool, std::slice::from_ref(&spec), false)
        .await
        .expect("seed sources");
    let r = support::rig_on(pool.clone(), &[], support::RigOpts::default()).await;
    let text = TextTier::new(
        pool.clone(),
        r.blobs.clone() as Arc<dyn BlobStore>,
        vc_parse::registry(),
        vc_core::parse::Limits::default(),
        3,
    )
    .with_sources(SourceCache::with_ttl(
        pool.clone(),
        std::time::Duration::ZERO,
    ));
    let mut cfg = crawl_path_support::cfg();
    cfg.sources.dump_window_utc = (0, 0);
    let dir = tempfile::tempdir().expect("tmp");
    let f = crawl_path_support::fetcher(
        &cfg,
        &pool,
        &["archive.test"],
        vec![spec.clone()],
        dir.path(),
        r.blobs.clone(),
    );
    Scheduler::new(
        Arc::new(cfg.clone()),
        pool.clone(),
        f.parts().frontier.clone(),
    )
    .pass(std::slice::from_ref(&spec))
    .await
    .expect("seed");

    // First ingest: the author's post is in raw (and so would be found by a purge).
    crawl_once(&f).await;
    process_all(&r, &text).await;
    assert!(
        !traces(&r.blobs).await.is_empty(),
        "the first dump holds the author"
    );

    // Purge (index + raw stages).
    let id = request_purge(
        &pool,
        &PurgeSelector::Author(AUTHOR.into()),
        TombstoneReason::OptOut,
        "test",
    )
    .await
    .expect("request");
    let out = process_purge(&pool, r.blobs.as_ref(), id, Utc::now())
        .await
        .expect("purge");
    assert!(out.captures_removed >= 1, "{out:?}");
    assert_eq!(
        traces(&r.blobs).await,
        Vec::<String>::new(),
        "purge left raw"
    );

    // Re-ingest the same dump: the author is dropped before anything is spooled.
    crawl_path_support::fast_forward(&pool).await;
    crawl_once(&f).await;
    process_all(&r, &text).await;
    assert_eq!(
        traces(&r.blobs).await,
        Vec::<String>::new(),
        "the purged author came back with the dump (raw or text tier)"
    );
    let all = blobs_text(&r.blobs).await;
    let resources = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM captures WHERE fetch_mode = 'adapter:se_travel_dump'",
    )
    .fetch_one(&pool)
    .await
    .expect("captures");
    assert!(resources >= 2, "the dump was re-ingested: {resources}");
    assert!(
        all.iter()
            .any(|(_, t)| t.contains("Only on Mondays") && t.contains("Ann")),
        "the other posts were re-ingested"
    );

    // Index: no record, version, held record or cached LLM output names the author.
    let mentions = support::count(
        &pool,
        "SELECT (SELECT count(*) FROM record_versions
                 WHERE strpos(data::text || coalesce(attribution, ''), 'Jordan Q') > 0)
              + (SELECT count(*) FROM records
                 WHERE strpos(coalesce(licence::text, '') || coalesce(title, '')
                              || coalesce(search_text, ''), 'Jordan Q') > 0)
              + (SELECT count(*) FROM held_records WHERE strpos(to_jsonb(held_records)::text, 'Jordan Q') > 0)",
    )
    .await;
    assert_eq!(mentions, 0, "the author is in the index");
    drop(srv);
}
