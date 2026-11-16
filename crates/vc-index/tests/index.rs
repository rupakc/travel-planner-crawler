//! vc-index integration tests: real Postgres 17 + pgvector (testkit `pg()`).

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, TimeZone, Utc};
use serde_json::json;
use testkit::vc_index_fixtures::{licence, open, prov, prov_at, record, share_alike};
use vc_core::extract::GeoPoint;
use vc_core::index::{
    ChangeOp, EntityLookup, IndexReader, OwnerType, PurgeSelector, RecordSink, SearchQuery,
    TombstoneReason,
};
use vc_core::licence::LicenceClass;
use vc_core::records::{ExtractedRecord, ObservationPoint};
use vc_core::usepolicy::{IndexUse, UsePolicy, UseSignals};
use vc_core::{EntityId, Error, KindId, ModelId, RecordId, Result};
use vc_index::{PgIndex, QueryEmbedder};

const ALL_CLASSES: [LicenceClass; 4] = [
    LicenceClass::Open,
    LicenceClass::ShareAlike,
    LicenceClass::FactsOnly,
    LicenceClass::ApiTerms,
];

async fn index() -> PgIndex {
    PgIndex::new(testkit::pg().await)
}

fn query(q: Option<&str>) -> SearchQuery {
    SearchQuery {
        q: q.map(str::to_string),
        place_id: None,
        near: None,
        kinds: vec![],
        licence_classes: ALL_CLASSES.to_vec(),
        updated_since: None,
        limit: 20,
        cursor: None,
    }
}

fn poi(key: &str, name: &str, place: i64, lat: f64, lon: f64) -> ExtractedRecord {
    let mut r = record(
        "poi",
        key,
        json!({"name": name, "geo": {"lat": lat, "lon": lon}}),
    );
    r.place_ids = vec![place];
    r
}

async fn record_id(ix: &PgIndex, key: &str) -> RecordId {
    let id: i64 = sqlx::query_scalar("SELECT record_id FROM records WHERE record_key = $1")
        .bind(key)
        .fetch_one(&ix.pool)
        .await
        .unwrap();
    RecordId(id)
}

async fn entity_of(ix: &PgIndex, key: &str) -> EntityId {
    let id: String = sqlx::query_scalar(
        "SELECT DISTINCT c.entity_id FROM entity_claims c JOIN records r USING (record_id)
         WHERE r.record_key = $1",
    )
    .bind(key)
    .fetch_one(&ix.pool)
    .await
    .unwrap();
    id.parse().unwrap()
}

/// Waits until every committed `changes` row is below the cluster-wide xmin. Transaction ids
/// are cluster-wide, so open transactions of tests running in parallel (other databases on the
/// same server) hold the feed back, exactly as a slow writer would in production.
async fn settle(ix: &PgIndex) {
    for _ in 0..1500 {
        let done: bool = sqlx::query_scalar(
            "SELECT coalesce(max(xid) < pg_snapshot_xmin(pg_current_snapshot()), true) FROM changes",
        )
        .fetch_one(&ix.pool)
        .await
        .unwrap();
        if done {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("change feed did not settle");
}

// ---------------------------------------------------------------------------------------------
// Plan tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn version_only_on_change() {
    let ix = index().await;
    let p = prov("wv", open(), "https://example.org/a");
    let r = record("poi", "k1", json!({"name": "Old Bridge", "hours": "9-5"}));

    let rep = ix.upsert(vec![r.clone()], &p).await.unwrap();
    assert_eq!((rep.inserted, rep.new_versions, rep.unchanged), (1, 0, 0));

    // Same data again (another capture, keys in another order): no new version, no change row.
    settle(&ix).await;
    let changes_before = ix.changes(None, &[], 100).await.unwrap().changes.len();
    let same = record("poi", "k1", json!({"hours": "9-5", "name": "Old Bridge"}));
    let p2 = prov("wv", open(), "https://example.org/a");
    let rep = ix.upsert(vec![same], &p2).await.unwrap();
    assert_eq!((rep.inserted, rep.new_versions, rep.unchanged), (0, 0, 1));
    assert_eq!(rep.changes, 0);
    let id = record_id(&ix, "k1").await;
    let v = ix.record_versions(id, 10, None).await.unwrap();
    assert_eq!(v.items.len(), 1);
    settle(&ix).await;
    assert_eq!(
        ix.changes(None, &[], 100).await.unwrap().changes.len(),
        changes_before
    );

    // Changed data: version 2, version 1 superseded, newest first.
    let changed = record("poi", "k1", json!({"name": "Old Bridge", "hours": "8-6"}));
    let p3 = prov("wv", open(), "https://example.org/a");
    let rep = ix.upsert(vec![changed], &p3).await.unwrap();
    assert_eq!((rep.inserted, rep.new_versions, rep.unchanged), (0, 1, 0));
    let v = ix.record_versions(id, 10, None).await.unwrap();
    assert_eq!(v.items.len(), 2);
    assert_eq!(v.items[0].version, 2);
    assert!(v.items[0].superseded_at.is_none());
    assert_eq!(v.items[0].data["hours"], "8-6");
    assert_eq!(v.items[1].version, 1);
    assert!(v.items[1].superseded_at.is_some());
    assert_ne!(v.items[0].data_hash, v.items[1].data_hash);
    assert_eq!(v.items[0].schema, "poi@1");
}

#[tokio::test]
async fn share_alike_not_merged_with_open() {
    let ix = index().await;
    // Same kind, place, name and point: open + open merge; share_alike stays separate.
    ix.upsert(
        vec![poi("o1", "Blue Mosque", 745044, 41.0054, 28.9768)],
        &prov("src-a", open(), "https://a.example/blue"),
    )
    .await
    .unwrap();
    ix.upsert(
        vec![poi("o2", "Blue Mosque", 745044, 41.0055, 28.9769)],
        &prov(
            "src-b",
            licence(LicenceClass::Open, Some("CC0-1.0"), "B"),
            "https://b.example/blue",
        ),
    )
    .await
    .unwrap();
    ix.upsert(
        vec![poi("s1", "Blue Mosque", 745044, 41.0054, 28.9768)],
        &prov("wv", share_alike(), "https://wv.example/blue"),
    )
    .await
    .unwrap();

    let e_o1 = entity_of(&ix, "o1").await;
    let e_o2 = entity_of(&ix, "o2").await;
    let e_s1 = entity_of(&ix, "s1").await;
    assert_eq!(e_o1, e_o2, "open records merge");
    assert_ne!(e_o1, e_s1, "share_alike never merges with open");

    let EntityLookup::Found(sa) = ix.entity(&e_s1).await.unwrap() else {
        panic!("share-alike entity found")
    };
    assert_eq!(sa.item.licence.class, LicenceClass::ShareAlike);
    let s1 = record_id(&ix, "s1").await;
    assert!(!sa.claims.is_empty());
    assert!(sa.claims.iter().all(|c| c.record_id == s1));
    let EntityLookup::Found(op) = ix.entity(&e_o1).await.unwrap() else {
        panic!("open entity found")
    };
    assert_eq!(op.item.licence.class, LicenceClass::Open);
    assert_eq!(op.item.provenance.len(), 2);

    // CC BY-SA and ODbL are both share_alike but never merge with each other either.
    ix.upsert(
        vec![poi("s2", "Blue Mosque", 745044, 41.0054, 28.9768)],
        &prov(
            "osm",
            licence(LicenceClass::ShareAlike, Some("ODbL-1.0"), "OSM"),
            "https://osm.example/blue",
        ),
    )
    .await
    .unwrap();
    assert_ne!(entity_of(&ix, "s2").await, e_s1);
}

#[tokio::test]
async fn entity_redirect_followed() {
    let ix = index().await;
    // Two records 500 m apart: too far for one entity.
    ix.upsert(
        vec![poi("a", "Galata Tower", 745044, 41.0256, 28.9742)],
        &prov("src-a", open(), "https://a.example/galata"),
    )
    .await
    .unwrap();
    ix.upsert(
        vec![poi("b", "Galata Tower", 745044, 41.0301, 28.9742)],
        &prov("src-b", open(), "https://b.example/galata"),
    )
    .await
    .unwrap();
    let ea = entity_of(&ix, "a").await;
    let eb = entity_of(&ix, "b").await;
    assert_ne!(ea, eb);

    // A third record in the middle matches both: they merge into the older one (a).
    ix.upsert(
        vec![poi("c", "Galata Tower", 745044, 41.02785, 28.9742)],
        &prov("src-c", open(), "https://c.example/galata"),
    )
    .await
    .unwrap();
    assert_eq!(ix.entity(&eb).await.unwrap(), EntityLookup::Redirect(ea));
    let EntityLookup::Found(found) = ix.entity(&ea).await.unwrap() else {
        panic!("survivor found")
    };
    assert_eq!(found.item.provenance.len(), 3);
    assert_eq!(entity_of(&ix, "b").await, ea);

    // The merge is in the change feed.
    settle(&ix).await;
    let ch = ix.changes(None, &[], 100).await.unwrap();
    assert!(ch.changes.iter().any(|c| c.op == ChangeOp::Redirect
        && c.owner_type == OwnerType::Entity
        && c.owner_id == eb.to_string()));

    // Chains collapse: merging a into a new entity d redirects both a and b to d.
    ix.upsert(
        vec![poi("d", "Hagia Irene", 745044, 41.0100, 28.9800)],
        &prov("src-d", open(), "https://d.example/irene"),
    )
    .await
    .unwrap();
    let ed = entity_of(&ix, "d").await;
    ix.merge_entities(&ea, &ed).await.unwrap();
    assert_eq!(ix.entity(&ea).await.unwrap(), EntityLookup::Redirect(ed));
    assert_eq!(ix.entity(&eb).await.unwrap(), EntityLookup::Redirect(ed));
    assert_eq!(
        ix.entity(&EntityId::new()).await.unwrap(),
        EntityLookup::NotFound
    );
}

#[tokio::test]
async fn observation_series_append() {
    let ix = index().await;
    let t0 = Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0).unwrap();
    let obs = |key: &str, at: chrono::DateTime<Utc>, v: f64| {
        let mut r = record("fx_rate", key, json!({"rate": v}));
        r.observation = Some(ObservationPoint {
            series_key: "fx:EUR:USD".into(),
            observed_at: at,
            value: json!(v),
        });
        r
    };
    let p = prov("ecb", open(), "https://ecb.example/fx");
    let rep = ix
        .upsert(
            vec![obs("d1", t0, 1.10), obs("d2", t0 + Duration::days(1), 1.11)],
            &p,
        )
        .await
        .unwrap();
    assert_eq!(rep.observations, 2);
    assert_eq!(rep.inserted, 0, "observation kinds do not create records");

    // Replay of the same capture: idempotent. A new day appends.
    let rep = ix
        .upsert(
            vec![
                obs("d2", t0 + Duration::days(1), 1.11),
                obs("d3", t0 + Duration::days(2), 1.12),
            ],
            &prov("ecb", open(), "https://ecb.example/fx"),
        )
        .await
        .unwrap();
    assert_eq!(rep.observations, 1);

    let pts = ix
        .observations("fx:EUR:USD", t0, t0 + Duration::days(10), 100)
        .await
        .unwrap();
    assert_eq!(pts.len(), 3);
    assert!(pts.windows(2).all(|w| w[0].observed_at < w[1].observed_at));
    assert_eq!(pts[2].value, json!(1.12));
    assert_eq!(pts[0].licence.class, LicenceClass::Open);
    assert_eq!(pts[0].source_id.0, "ecb");

    // [from, to) and limit.
    let pts = ix
        .observations(
            "fx:EUR:USD",
            t0 + Duration::days(1),
            t0 + Duration::days(2),
            100,
        )
        .await
        .unwrap();
    assert_eq!(pts.len(), 1);
    let pts = ix
        .observations("fx:EUR:USD", t0, t0 + Duration::days(10), 2)
        .await
        .unwrap();
    assert_eq!(pts.len(), 2);

    settle(&ix).await;
    let ch = ix.changes(None, &[], 100).await.unwrap();
    assert!(
        ch.changes
            .iter()
            .any(|c| c.owner_type == OwnerType::Observation && c.owner_id == "fx:EUR:USD")
    );
}

#[tokio::test]
async fn hybrid_ranks_exact_place_first() {
    let ix = index().await;
    let istanbul = 745044;
    let mut exact = poi("exact", "Spice Bazaar market", istanbul, 41.0166, 28.9706);
    exact.quality = 0.4;
    // Nearby (Kadikoy, another place id), a better text match and higher quality.
    let mut near = record(
        "poi",
        "near",
        json!({"name": "Kadikoy market", "about": "market market food market",
               "geo": {"lat": 40.9900, "lon": 29.0270}}),
    );
    near.place_ids = vec![7_628_440];
    near.quality = 1.0;
    // Far away (Ankara): outside the radius.
    let far = poi("far", "Ankara market", 323786, 39.9334, 32.8597);
    // Same place, other kind.
    let mut ev = record("event", "ev", json!({"name": "market night"}));
    ev.place_ids = vec![istanbul];

    ix.upsert(
        vec![exact, near, far, ev],
        &prov("src", open(), "https://example.org/markets"),
    )
    .await
    .unwrap();

    let mut q = query(Some("market"));
    q.place_id = Some(istanbul);
    q.near = Some((
        GeoPoint {
            lat: 41.0082,
            lon: 28.9784,
        },
        20.0,
    ));
    q.kinds = vec![KindId::new("poi")];
    let page = ix.search(&q).await.unwrap();
    let titles: Vec<_> = page
        .items
        .iter()
        .map(|i| i.title.clone().unwrap())
        .collect();
    assert_eq!(titles, vec!["Spice Bazaar market", "Kadikoy market"]);
    assert!(page.items.iter().all(|i| i.score.is_some()));
    assert!(page.items.iter().all(|i| i.schema == "poi@1"));

    // Without a place the better text match + quality wins.
    let page = ix.search(&query(Some("market"))).await.unwrap();
    assert_eq!(page.items[0].title.as_deref(), Some("Kadikoy market"));
    assert_eq!(page.items.len(), 4);

    // Licence classes the client may not receive are filtered out; empty = none.
    let mut q = query(Some("market"));
    q.licence_classes = vec![LicenceClass::ShareAlike];
    assert!(ix.search(&q).await.unwrap().items.is_empty());
    q.licence_classes = vec![];
    assert!(ix.search(&q).await.unwrap().items.is_empty());
}

#[tokio::test]
async fn tombstoned_never_returned() {
    let ix = index().await;
    ix.upsert(
        vec![poi("t1", "Secret Garden", 1, 10.0, 10.0)],
        &prov("blog", open(), "https://optout.example/garden"),
    )
    .await
    .unwrap();
    ix.upsert(
        vec![poi("t2", "Public Park", 1, 20.0, 20.0)],
        &prov("blog", open(), "https://other.example/park"),
    )
    .await
    .unwrap();
    let id = record_id(&ix, "t1").await;
    let ent = entity_of(&ix, "t1").await;

    let n = ix
        .tombstone(
            &PurgeSelector::Host("optout.example".into()),
            TombstoneReason::OptOut,
        )
        .await
        .unwrap();
    assert_eq!(n, 1);

    let page = ix.search(&query(Some("garden"))).await.unwrap();
    assert!(page.items.is_empty());
    let page = ix.search(&query(None)).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title.as_deref(), Some("Public Park"));
    assert!(matches!(
        ix.record_versions(id, 10, None).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(ix.entity(&ent).await.unwrap(), EntityLookup::NotFound);
    settle(&ix).await;
    let ch = ix.changes(None, &[], 100).await.unwrap();
    assert!(ch.changes.iter().any(|c| c.op == ChangeOp::Tombstone
        && c.owner_type == OwnerType::Record
        && c.owner_id == id.0.to_string()));

    // A standing exclusion: a re-crawl of the opted-out host is not resurrected.
    let rep = ix
        .upsert(
            vec![poi("t1", "Secret Garden v2", 1, 10.0, 10.0)],
            &prov("blog", open(), "https://optout.example/garden"),
        )
        .await
        .unwrap();
    assert_eq!(rep.inserted + rep.new_versions, 0);
    assert!(
        ix.search(&query(Some("garden")))
            .await
            .unwrap()
            .items
            .is_empty()
    );

    // Tombstoning again is a no-op.
    let n = ix
        .tombstone(
            &PurgeSelector::Host("optout.example".into()),
            TombstoneReason::OptOut,
        )
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn changes_ordered_by_xid() {
    let ix = index().await;
    // A slow writer holds an open transaction that already has an xid and a change row.
    let mut slow = ix.pool.begin().await.unwrap();
    sqlx::query("INSERT INTO changes (owner_type, owner_id, op, kind) VALUES ('source', 'slow', 'upsert', NULL)")
        .execute(&mut *slow)
        .await
        .unwrap();

    // Concurrent writers commit after it started.
    let mut tasks = Vec::new();
    for w in 0..8 {
        let ix = ix.clone();
        tasks.push(tokio::spawn(async move {
            for i in 0..5 {
                let r = record(
                    "poi",
                    &format!("w{w}-{i}"),
                    json!({"name": format!("Place {w} {i}")}),
                );
                ix.upsert(
                    vec![r],
                    &prov("src", open(), &format!("https://w{w}.example/{i}")),
                )
                .await
                .unwrap();
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }

    // Nothing at or after the open transaction's xid is visible yet: no commit can be skipped.
    let early = ix.changes(None, &[], 1000).await.unwrap();
    assert!(
        early.changes.is_empty(),
        "rows newer than the oldest open xid are held back"
    );
    assert_eq!(early.next, None);

    slow.commit().await.unwrap();
    settle(&ix).await;

    // Page through with a small limit: strictly increasing (xid, seq), no gaps, no duplicates.
    let total: i64 = sqlx::query_scalar("SELECT count(*) FROM changes")
        .fetch_one(&ix.pool)
        .await
        .unwrap();
    let mut seen = Vec::new();
    let mut cur = None;
    loop {
        let page = ix.changes(cur, &[], 7).await.unwrap();
        if page.changes.is_empty() {
            assert_eq!(page.next, cur);
            break;
        }
        seen.extend(page.changes.iter().map(|c| c.cursor));
        cur = page.next;
    }
    assert_eq!(seen.len() as i64, total);
    assert!(seen.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(seen.iter().collect::<HashSet<_>>().len(), seen.len());
    // The slow writer's row is first: its xid is the oldest.
    let first = ix.changes(None, &[], 1).await.unwrap();
    assert_eq!(first.changes[0].owner_id, "slow");

    // Kind filter.
    let only = ix
        .changes(None, &[KindId::new("event")], 1000)
        .await
        .unwrap();
    assert!(only.changes.is_empty());
    let poi = ix.changes(None, &[KindId::new("poi")], 1000).await.unwrap();
    assert!(!poi.changes.is_empty());
    assert!(
        poi.changes
            .iter()
            .all(|c| c.kind == Some(KindId::new("poi")))
    );
}

// ---------------------------------------------------------------------------------------------
// Further edge cases
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn restricted_licence_rejected() {
    let ix = index().await;
    let p = prov(
        "nc",
        licence(LicenceClass::Restricted, None, "NC"),
        "https://nc.example/x",
    );
    let err = ix
        .upsert(vec![record("poi", "x", json!({"name": "X"}))], &p)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Policy(_)));
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM records")
        .fetch_one(&ix.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn older_capture_never_overwrites_newer() {
    let ix = index().await;
    let now = Utc::now();
    ix.upsert(
        vec![record("poi", "k", json!({"name": "New"}))],
        &prov_at("s", open(), "https://example.org/k", now),
    )
    .await
    .unwrap();
    let rep = ix
        .upsert(
            vec![record("poi", "k", json!({"name": "Old"}))],
            &prov_at(
                "s",
                open(),
                "https://example.org/k",
                now - Duration::hours(1),
            ),
        )
        .await
        .unwrap();
    assert_eq!((rep.new_versions, rep.unchanged), (0, 1));
    let id = record_id(&ix, "k").await;
    let v = ix.record_versions(id, 10, None).await.unwrap();
    assert_eq!(v.items.len(), 1);
    assert_eq!(v.items[0].data["name"], "New");
}

#[tokio::test]
async fn use_policy_limits_serving() {
    let ix = index().await;
    let mut noindex = record("poi", "ni", json!({"name": "Hidden Cafe"}));
    noindex.use_policy = UsePolicy::from_signals(&UseSignals {
        noindex: true,
        ..Default::default()
    });
    let mut ai = record(
        "poi",
        "ai",
        json!({"name": "Linked Cafe", "secret": "menu"}),
    );
    ai.use_policy = UsePolicy::from_signals(&UseSignals {
        tdm_reserved: true,
        ..Default::default()
    });
    assert_eq!(ai.use_policy.index, IndexUse::TitleAndLink);
    ix.upsert(
        vec![noindex, ai],
        &prov("s", open(), "https://example.org/cafes"),
    )
    .await
    .unwrap();
    let page = ix.search(&query(Some("cafe"))).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title.as_deref(), Some("Linked Cafe"));
    assert_eq!(page.items[0].data, json!({}));
    assert!(page.items[0].excerpt.is_none());
    // Only title and link are indexed: body text does not match.
    assert!(
        ix.search(&query(Some("menu")))
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn facts_only_excerpt_and_licence_served() {
    let ix = index().await;
    let mut r = record(
        "article",
        "a1",
        json!({"name": "Ten things to do in Lisbon"}),
    );
    r.excerpt = Some("x".repeat(300));
    let p = prov(
        "blog",
        licence(LicenceClass::FactsOnly, None, "A Blog"),
        "https://blog.example/lisbon",
    );
    ix.upsert(vec![r], &p).await.unwrap();
    let page = ix.search(&query(Some("lisbon"))).await.unwrap();
    let it = &page.items[0];
    assert_eq!(it.licence.class, LicenceClass::FactsOnly);
    assert_eq!(it.licence.attribution, "A Blog");
    assert!(it.excerpt.as_ref().unwrap().chars().count() <= 300);
    assert_eq!(it.provenance.len(), 1);
    assert_eq!(it.provenance[0].url, "https://blog.example/lisbon");
    assert_eq!(it.provenance[0].source_id.0, "blog");
}

#[tokio::test]
async fn search_cursor_pages_without_overlap() {
    let ix = index().await;
    let recs = (0..25)
        .map(|i| {
            record(
                "poi",
                &format!("p{i}"),
                json!({"name": format!("Museum {i}")}),
            )
        })
        .collect();
    ix.upsert(recs, &prov("s", open(), "https://example.org/museums"))
        .await
        .unwrap();
    let mut q = query(Some("museum"));
    q.limit = 10;
    let mut ids = Vec::new();
    loop {
        let page = ix.search(&q).await.unwrap();
        ids.extend(page.items.iter().map(|i| i.id.clone()));
        match page.next_cursor {
            Some(c) => q.cursor = Some(c),
            None => break,
        }
    }
    assert_eq!(ids.len(), 25);
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 25);
    q.cursor = Some("garbage".into());
    assert!(matches!(ix.search(&q).await, Err(Error::Invalid(_))));
}

struct FixedEmbedder {
    model: ModelId,
    vec: Vec<f32>,
}

#[async_trait]
impl QueryEmbedder for FixedEmbedder {
    fn model(&self) -> &ModelId {
        &self.model
    }
    fn dim(&self) -> usize {
        self.vec.len()
    }
    async fn embed_query(&self, _q: &str) -> Result<Option<Vec<f32>>> {
        Ok(Some(self.vec.clone()))
    }
}

fn unit(i: usize, dim: usize) -> Vec<f32> {
    let mut v = vec![0.0; dim];
    v[i] = 1.0;
    v
}

#[tokio::test]
async fn embeddings_feed_vector_rank() {
    let ix = index().await;
    let model = ModelId(Arc::from("test-embed-4"));
    assert!(ix.ensure_embedding_index(&model, 4).await.unwrap());
    assert!(!ix.ensure_embedding_index(&model, 4).await.unwrap());

    // Neither title matches the query text; only the vector can rank them.
    ix.upsert(
        vec![
            record("poi", "v1", json!({"name": "Alpha"})),
            record("poi", "v2", json!({"name": "Beta"})),
        ],
        &prov("s", open(), "https://example.org/v"),
    )
    .await
    .unwrap();
    let todo = ix.pending_embeddings(&model, 10).await.unwrap();
    assert_eq!(todo.len(), 2);
    for t in &todo {
        let v = if t.text.contains("Beta") {
            unit(1, 4)
        } else {
            unit(0, 4)
        };
        ix.store_embedding(t, &model, &v).await.unwrap();
    }
    assert!(ix.pending_embeddings(&model, 10).await.unwrap().is_empty());

    let ix = ix.with_query_embedder(Arc::new(FixedEmbedder {
        model: model.clone(),
        vec: unit(1, 4),
    }));
    let page = ix
        .search(&query(Some("something unrelated")))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert_eq!(page.items[0].title.as_deref(), Some("Beta"));

    // A new version makes the embedding stale again.
    ix.upsert(
        vec![record("poi", "v1", json!({"name": "Alpha", "x": 1}))],
        &prov("s", open(), "https://example.org/v"),
    )
    .await
    .unwrap();
    assert_eq!(ix.pending_embeddings(&model, 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn author_purge_and_gone_revival() {
    let ix = index().await;
    let qa = |key: &str, author: &str| {
        record(
            "qa_thread",
            key,
            json!({"name": format!("Visa question {key}"),
                   "answers": [{"author": author, "text": "answer"}]}),
        )
    };
    ix.upsert(
        vec![qa("q1", "alice"), qa("q2", "bob")],
        &prov("se", share_alike(), "https://travel.se.example/q"),
    )
    .await
    .unwrap();
    let n = ix
        .tombstone(
            &PurgeSelector::Author("alice".into()),
            TombstoneReason::Purge,
        )
        .await
        .unwrap();
    assert_eq!(n, 1);
    let page = ix.search(&query(Some("visa"))).await.unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].title.as_deref(), Some("Visa question q2"));
    // A new record by the same author is excluded on ingest.
    ix.upsert(
        vec![qa("q3", "alice")],
        &prov("se", share_alike(), "https://travel.se.example/q3"),
    )
    .await
    .unwrap();
    assert_eq!(
        ix.search(&query(Some("visa"))).await.unwrap().items.len(),
        1
    );

    // `gone` hides a record but a later sighting revives it (no standing exclusion).
    let q2 = record_id(&ix, "q2").await;
    let n = ix
        .tombstone(&PurgeSelector::Record(q2), TombstoneReason::Gone)
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert!(
        ix.search(&query(Some("visa")))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    ix.upsert(
        vec![qa("q2", "bob")],
        &prov("se", share_alike(), "https://travel.se.example/q"),
    )
    .await
    .unwrap();
    assert_eq!(
        ix.search(&query(Some("visa"))).await.unwrap().items.len(),
        1
    );
}

#[tokio::test]
async fn stricter_policy_applies_without_new_version() {
    let ix = index().await;
    let r = poi("p", "Clock Tower", 5, 1.0, 1.0);
    ix.upsert(
        vec![r.clone()],
        &prov("s", open(), "https://example.org/clock"),
    )
    .await
    .unwrap();
    let ent = entity_of(&ix, "p").await;
    assert!(matches!(
        ix.entity(&ent).await.unwrap(),
        EntityLookup::Found(_)
    ));

    // The site adds noindex: same data, no new version, but no longer served or linked.
    let mut p = prov("s", open(), "https://example.org/clock");
    p.use_policy = UsePolicy::from_signals(&UseSignals {
        noindex: true,
        ..Default::default()
    });
    let rep = ix.upsert(vec![r], &p).await.unwrap();
    assert_eq!((rep.unchanged, rep.new_versions), (1, 0));
    assert!(
        ix.search(&query(Some("clock")))
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(ix.entity(&ent).await.unwrap(), EntityLookup::NotFound);
    let id = record_id(&ix, "p").await;
    let v = ix.record_versions(id, 10, None).await.unwrap();
    assert_eq!(v.items.len(), 1);
    assert_eq!(v.items[0].use_policy.index, IndexUse::None);
}

#[tokio::test]
async fn versions_page_and_operator_merge_rules() {
    let ix = index().await;
    for i in 0..5 {
        ix.upsert(
            vec![record("poi", "k", json!({"name": "Fort", "rev": i}))],
            &prov("s", open(), "https://example.org/fort"),
        )
        .await
        .unwrap();
    }
    let id = record_id(&ix, "k").await;
    let p1 = ix.record_versions(id, 2, None).await.unwrap();
    assert_eq!(
        p1.items.iter().map(|v| v.version).collect::<Vec<_>>(),
        [5, 4]
    );
    let p2 = ix.record_versions(id, 2, p1.next_cursor).await.unwrap();
    assert_eq!(
        p2.items.iter().map(|v| v.version).collect::<Vec<_>>(),
        [3, 2]
    );
    let p3 = ix.record_versions(id, 2, p2.next_cursor).await.unwrap();
    assert_eq!(p3.items.iter().map(|v| v.version).collect::<Vec<_>>(), [1]);
    assert!(p3.next_cursor.is_none());
    assert!(matches!(
        ix.record_versions(RecordId(999_999), 2, None).await,
        Err(Error::NotFound(_))
    ));

    // Operators cannot merge across licences or kinds either.
    ix.upsert(
        vec![poi("o", "Fort", 1, 1.0, 1.0)],
        &prov("a", open(), "https://a.example/fort"),
    )
    .await
    .unwrap();
    ix.upsert(
        vec![poi("s", "Fort", 1, 1.0, 1.0)],
        &prov("b", share_alike(), "https://b.example/fort"),
    )
    .await
    .unwrap();
    let (eo, es) = (entity_of(&ix, "o").await, entity_of(&ix, "s").await);
    assert!(matches!(
        ix.merge_entities(&es, &eo).await,
        Err(Error::Policy(_))
    ));
    assert!(matches!(
        ix.merge_entities(&eo, &eo).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        ix.merge_entities(&EntityId::new(), &eo).await,
        Err(Error::NotFound(_))
    ));
}

#[tokio::test]
async fn source_purge_hides_observations() {
    let ix = index().await;
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let mut r = record("advisory_level", "a", json!({"level": 2}));
    r.observation = Some(ObservationPoint {
        series_key: "advisory:FR".into(),
        observed_at: t0,
        value: json!(2),
    });
    ix.upsert(vec![r], &prov("gov", open(), "https://gov.example/fr"))
        .await
        .unwrap();
    assert_eq!(
        ix.observations("advisory:FR", t0, t0 + Duration::days(1), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    ix.tombstone(
        &PurgeSelector::Source(vc_core::SourceId("gov".into())),
        TombstoneReason::OptOut,
    )
    .await
    .unwrap();
    assert!(
        ix.observations("advisory:FR", t0, t0 + Duration::days(1), 10)
            .await
            .unwrap()
            .is_empty()
    );
}

// ---------------------------------------------------------------------------------------------
// Final-review regressions (B11, B16, B17, B18)
// ---------------------------------------------------------------------------------------------

async fn state_of(ix: &PgIndex, key: &str) -> String {
    sqlx::query_scalar("SELECT state FROM records WHERE record_key = $1")
        .bind(key)
        .fetch_one(&ix.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn tombstone_and_concurrent_upsert_never_leave_an_active_record() {
    let ix = index().await;
    let url = "https://race.example/page";
    let p = prov("s", open(), url);
    // An uncommitted insert of the same record parks the upsert *after* it has read the
    // standing exclusions (none yet) and before it writes.
    let mut blocker = ix.pool.begin().await.unwrap();
    sqlx::query(
        "INSERT INTO records (kind, source_id, url_key, record_key, licence_class)
         VALUES ('poi', 's', $1, 'race', 'open')",
    )
    .bind(&p.url_key.as_bytes()[..])
    .execute(&mut *blocker)
    .await
    .unwrap();
    let up = {
        let ix = ix.clone();
        let p = p.clone();
        tokio::spawn(async move {
            ix.upsert(vec![record("poi", "race", json!({"name": "Race"}))], &p)
                .await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let ts = {
        let ix = ix.clone();
        let sel = PurgeSelector::Url(p.url_key);
        tokio::spawn(async move { ix.tombstone(&sel, TombstoneReason::OptOut).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    blocker.rollback().await.unwrap();
    up.await.unwrap().unwrap();
    ts.await.unwrap().unwrap();
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM records WHERE state = 'active'")
        .fetch_one(&ix.pool)
        .await
        .unwrap();
    assert_eq!(active, 0, "the opted-out URL has no active record");
}

#[tokio::test]
async fn older_capture_never_reactivates_a_record_a_newer_capture_staled() {
    let ix = index().await;
    let t = Utc::now() - Duration::hours(3);
    let url = "https://example.org/stale";
    ix.upsert(
        vec![record("poi", "s1", json!({"name": "Kiosk"}))],
        &prov_at("s", open(), url, t),
    )
    .await
    .unwrap();
    // A newer capture (t + 2h) no longer contains it: stale (as the processor marks it).
    sqlx::query(
        "UPDATE records SET state = 'stale', stale_since = now(), stale_capture_id = 'x',
                stale_captured_at = $1 WHERE record_key = 's1'",
    )
    .bind(t + Duration::hours(2))
    .execute(&ix.pool)
    .await
    .unwrap();
    // An out-of-order capture from t + 1h, with other data, arrives late.
    let rep = ix
        .upsert(
            vec![record(
                "poi",
                "s1",
                json!({"name": "Kiosk", "hours": "9-5"}),
            )],
            &prov_at("s", open(), url, t + Duration::hours(1)),
        )
        .await
        .unwrap();
    assert_eq!((rep.new_versions, rep.unchanged), (0, 1));
    assert_eq!(state_of(&ix, "s1").await, "stale");
    let id = record_id(&ix, "s1").await;
    assert_eq!(
        ix.record_versions(id, 10, None).await.unwrap().items.len(),
        1
    );

    // A capture newer than the stale one does re-activate it.
    ix.upsert(
        vec![record(
            "poi",
            "s1",
            json!({"name": "Kiosk", "hours": "8-6"}),
        )],
        &prov_at(
            "s",
            open(),
            url,
            t + Duration::hours(3) - Duration::minutes(1),
        ),
    )
    .await
    .unwrap();
    assert_eq!(state_of(&ix, "s1").await, "active");
}

#[tokio::test]
async fn older_capture_never_inserts_a_record_a_newer_capture_lacks() {
    let ix = index().await;
    let t = Utc::now() - Duration::hours(3);
    let url = "https://example.org/list";
    ix.upsert(
        vec![record("poi", "a", json!({"name": "A"}))],
        &prov_at("s", open(), url, t + Duration::hours(2)),
    )
    .await
    .unwrap();
    // An older capture of the same page carries B (gone by the newer capture).
    let rep = ix
        .upsert(
            vec![record("poi", "b", json!({"name": "B"}))],
            &prov_at("s", open(), url, t + Duration::hours(1)),
        )
        .await
        .unwrap();
    assert_eq!(rep.inserted, 0);
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM records WHERE record_key = 'b'")
        .fetch_one(&ix.pool)
        .await
        .unwrap();
    assert_eq!(n, 0);
    // A newer capture with B inserts it.
    let rep = ix
        .upsert(
            vec![record("poi", "b", json!({"name": "B"}))],
            &prov_at(
                "s",
                open(),
                url,
                t + Duration::hours(2) + Duration::minutes(1),
            ),
        )
        .await
        .unwrap();
    assert_eq!(rep.inserted, 1);
}

#[tokio::test]
async fn observation_tombstone_change_carries_its_kind() {
    let ix = index().await;
    let t0 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let mut r = record("advisory_level", "a", json!({"level": 2}));
    r.observation = Some(ObservationPoint {
        series_key: "advisory:FR".into(),
        observed_at: t0,
        value: json!(2),
    });
    ix.upsert(vec![r], &prov("gov", open(), "https://gov.example/fr"))
        .await
        .unwrap();
    ix.tombstone(
        &PurgeSelector::Source(vc_core::SourceId("gov".into())),
        TombstoneReason::OptOut,
    )
    .await
    .unwrap();
    settle(&ix).await;
    let ch = ix
        .changes(None, &[KindId::new("advisory_level")], 100)
        .await
        .unwrap();
    assert!(
        ch.changes.iter().any(|c| c.op == ChangeOp::Tombstone
            && c.owner_type == OwnerType::Observation
            && c.owner_id == "advisory:FR"),
        "kind-filtered clients see the observation tombstone"
    );
}

#[tokio::test]
async fn embedding_never_written_after_its_record_is_tombstoned() {
    let ix = index().await;
    let model = ModelId(Arc::from("test-embed-4"));
    ix.ensure_embedding_index(&model, 4).await.unwrap();
    ix.upsert(
        vec![record("poi", "e1", json!({"name": "Alpha"}))],
        &prov("s", open(), "https://example.org/e"),
    )
    .await
    .unwrap();
    let todo = ix.pending_embeddings(&model, 10).await.unwrap();
    assert_eq!(todo.len(), 1);
    // A tombstone in flight: the record row is updated and its embeddings deleted, not yet
    // committed, while the embedder stores the vector it computed.
    let id = todo[0].record_id.0;
    let mut ts = ix.pool.begin().await.unwrap();
    sqlx::query("UPDATE records SET state = 'tombstoned' WHERE record_id = $1")
        .bind(id)
        .execute(&mut *ts)
        .await
        .unwrap();
    sqlx::query("DELETE FROM embeddings WHERE owner_type = 'record' AND owner_id = $1")
        .bind(id.to_string())
        .execute(&mut *ts)
        .await
        .unwrap();
    let store = {
        let ix = ix.clone();
        let task = todo[0].clone();
        let model = model.clone();
        tokio::spawn(async move { ix.store_embedding(&task, &model, &unit(0, 4)).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    ts.commit().await.unwrap();
    store.await.unwrap().unwrap();
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM embeddings")
        .fetch_one(&ix.pool)
        .await
        .unwrap();
    assert_eq!(n, 0, "no embedding outlives its tombstoned record");
}
