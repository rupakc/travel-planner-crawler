//! Fixtures for `vc-api` tests (owned by the vc-api task; add items here, never edit lib.rs).
//!
//! * [`MemIndex`]: an in-memory [`IndexReader`] with a change feed that follows the real
//!   `xid8` visibility rule (only changes whose transaction id is below the oldest still-open
//!   transaction are served), so cursor paging can be tested without vc-index.
//! * [`MemPlaces`]: a fixed [`PlaceResolver`].
//! * [`item`] / [`licence`] / [`provenance`]: builders for served items.

use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use vc_core::error::Result;
use vc_core::extract::{PlaceMatch, PlaceQuery, PlaceResolver};
use vc_core::ids::{EntityId, KindId, RecordId, SourceId};
use vc_core::index::{
    ChangeCursor, ChangeOp, ChangeView, ChangesPage, EntityLookup, EntityView, IndexReader, Item,
    ObservationView, OwnerType, Page, ProvenanceView, RecordVersionView, SearchQuery,
};
use vc_core::licence::{Licence, LicenceClass};
use vc_core::records::{ExtractionMethod, ExtractorInfo};
use vc_core::usepolicy::UsePolicy;

/// A fixed timestamp (2026-11-01T00:00:00Z) for deterministic fixtures.
pub fn t0() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0)
        .single()
        .expect("valid date")
}

/// A licence of the given class.
pub fn licence(class: LicenceClass) -> Licence {
    Licence {
        class,
        spdx: match class {
            LicenceClass::Open => Some("CC-BY-4.0".into()),
            LicenceClass::ShareAlike => Some("CC-BY-SA-4.0".into()),
            _ => None,
        },
        attribution: format!("Example {} source", class.as_str()),
        url: Some("https://example.org/licence".parse().expect("url")),
    }
}

/// One provenance entry.
pub fn provenance(source: &str, url: &str) -> ProvenanceView {
    ProvenanceView {
        source_id: SourceId(source.into()),
        url: url.into(),
        captured_at: t0(),
        extractor: ExtractorInfo {
            version: "test-1".into(),
            method: ExtractionMethod::Structured,
            model: None,
            prompt_hash: None,
            run_id: None,
        },
    }
}

/// A served item with sensible defaults (everything allowed).
pub fn item(id: &str, kind: &str, class: LicenceClass) -> Item {
    Item {
        id: id.into(),
        kind: KindId::new(kind),
        schema: format!("{kind}@1"),
        title: Some(format!("Title of {id}")),
        excerpt: Some(format!("Excerpt of {id}")),
        data: serde_json::json!({ "name": format!("Name of {id}") }),
        place_ids: vec![2_643_743],
        licence: licence(class),
        provenance: vec![provenance(
            "example-src",
            &format!("https://example.org/{id}"),
        )],
        conflict: false,
        use_policy: UsePolicy::ALLOW_ALL,
        updated_at: t0(),
        score: None,
    }
}

/// One change of the in-memory feed.
#[derive(Debug, Clone)]
struct MemChange {
    xid: u64,
    seq: i64,
    kind: Option<KindId>,
    owner_id: String,
}

#[derive(Default)]
struct Feed {
    next_seq: i64,
    next_xid: u64,
    open: Vec<u64>,
    rows: Vec<MemChange>,
}

/// An in-memory [`IndexReader`].
pub struct MemIndex {
    items: Mutex<Vec<Item>>,
    entities: Mutex<BTreeMap<String, EntityLookup>>,
    versions: Mutex<BTreeMap<i64, Vec<RecordVersionView>>>,
    observations: Mutex<Vec<ObservationView>>,
    feed: Mutex<Feed>,
    /// The last search query seen (for assertions).
    pub last_query: Mutex<Option<SearchQuery>>,
}

impl Default for MemIndex {
    fn default() -> Self {
        MemIndex {
            items: Mutex::new(Vec::new()),
            entities: Mutex::new(BTreeMap::new()),
            versions: Mutex::new(BTreeMap::new()),
            observations: Mutex::new(Vec::new()),
            feed: Mutex::new(Feed {
                next_seq: 1,
                next_xid: 1000,
                ..Feed::default()
            }),
            last_query: Mutex::new(None),
        }
    }
}

impl MemIndex {
    /// An empty index.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a searchable item.
    pub fn add_item(&self, it: Item) {
        self.items.lock().expect("lock").push(it);
    }

    /// Adds an entity.
    pub fn add_entity(&self, id: EntityId, view: EntityView) {
        self.entities
            .lock()
            .expect("lock")
            .insert(id.to_string(), EntityLookup::Found(Box::new(view)));
    }

    /// Records that `old` was merged into `new`.
    pub fn add_redirect(&self, old: EntityId, new: EntityId) {
        self.entities
            .lock()
            .expect("lock")
            .insert(old.to_string(), EntityLookup::Redirect(new));
    }

    /// Adds record versions (newest first).
    pub fn add_versions(&self, id: RecordId, v: Vec<RecordVersionView>) {
        self.versions.lock().expect("lock").insert(id.0, v);
    }

    /// Adds an observation.
    pub fn add_observation(&self, o: ObservationView) {
        self.observations.lock().expect("lock").push(o);
    }

    /// Opens a transaction; returns its xid.
    pub fn begin(&self) -> u64 {
        let mut f = self.feed.lock().expect("lock");
        let x = f.next_xid;
        f.next_xid += 1;
        f.open.push(x);
        x
    }

    /// Writes a change row inside transaction `xid` (seq is taken at write time, like
    /// `bigserial`, so seq order and commit order can disagree).
    pub fn write_change(&self, xid: u64, owner_id: &str, kind: &str) {
        let mut f = self.feed.lock().expect("lock");
        let seq = f.next_seq;
        f.next_seq += 1;
        f.rows.push(MemChange {
            xid,
            seq,
            kind: Some(KindId::new(kind)),
            owner_id: owner_id.into(),
        });
    }

    /// Commits transaction `xid`.
    pub fn commit(&self, xid: u64) {
        self.feed.lock().expect("lock").open.retain(|x| *x != xid);
    }

    /// Writes one change in its own committed transaction.
    pub fn change(&self, owner_id: &str, kind: &str) {
        let x = self.begin();
        self.write_change(x, owner_id, kind);
        self.commit(x);
    }
}

#[async_trait]
impl IndexReader for MemIndex {
    async fn search(&self, q: &SearchQuery) -> Result<Page<Item>> {
        *self.last_query.lock().expect("lock") = Some(q.clone());
        let all = self.items.lock().expect("lock").clone();
        let start: usize = q
            .cursor
            .as_deref()
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let filtered: Vec<Item> = all
            .into_iter()
            .filter(|i| q.kinds.is_empty() || q.kinds.contains(&i.kind))
            .filter(|i| q.place_id.is_none_or(|p| i.place_ids.contains(&p)))
            .collect();
        let limit = q.limit.max(1) as usize;
        let items: Vec<Item> = filtered.iter().skip(start).take(limit).cloned().collect();
        let next =
            (start + items.len() < filtered.len()).then(|| (start + items.len()).to_string());
        Ok(Page {
            items,
            next_cursor: next,
        })
    }

    async fn entity(&self, id: &EntityId) -> Result<EntityLookup> {
        Ok(self
            .entities
            .lock()
            .expect("lock")
            .get(&id.to_string())
            .cloned()
            .unwrap_or(EntityLookup::NotFound))
    }

    async fn record_versions(
        &self,
        id: RecordId,
        limit: u32,
        _cursor: Option<String>,
    ) -> Result<Page<RecordVersionView>> {
        let v = self
            .versions
            .lock()
            .expect("lock")
            .get(&id.0)
            .cloned()
            .unwrap_or_default();
        Ok(Page {
            items: v.into_iter().take(limit as usize).collect(),
            next_cursor: None,
        })
    }

    async fn changes(
        &self,
        since: Option<ChangeCursor>,
        kinds: &[KindId],
        limit: u32,
    ) -> Result<ChangesPage> {
        let f = self.feed.lock().expect("lock");
        // pg_snapshot_xmin(pg_current_snapshot()): the oldest open xid, else the next xid.
        let xmin = f.open.iter().copied().min().unwrap_or(f.next_xid);
        let mut rows: Vec<&MemChange> = f
            .rows
            .iter()
            .filter(|r| r.xid < xmin)
            .filter(|r| {
                since.is_none_or(|s| {
                    ChangeCursor {
                        xid: r.xid,
                        seq: r.seq,
                    } > s
                })
            })
            .filter(|r| kinds.is_empty() || r.kind.as_ref().is_some_and(|k| kinds.contains(k)))
            .collect();
        rows.sort_by_key(|r| (r.xid, r.seq));
        rows.truncate(limit as usize);
        let changes: Vec<ChangeView> = rows
            .into_iter()
            .map(|r| ChangeView {
                cursor: ChangeCursor {
                    xid: r.xid,
                    seq: r.seq,
                },
                owner_type: OwnerType::Record,
                owner_id: r.owner_id.clone(),
                op: ChangeOp::Upsert,
                kind: r.kind.clone(),
                at: t0(),
            })
            .collect();
        let next = changes.last().map(|c| c.cursor).or(since);
        Ok(ChangesPage { changes, next })
    }

    async fn observations(
        &self,
        series: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ObservationView>> {
        Ok(self
            .observations
            .lock()
            .expect("lock")
            .iter()
            .filter(|o| o.series_key == series && o.observed_at >= from && o.observed_at < to)
            .take(limit as usize)
            .cloned()
            .collect())
    }
}

/// A [`PlaceResolver`] that answers every query with the same candidates.
#[derive(Default)]
pub struct MemPlaces {
    /// The candidates.
    pub matches: Vec<PlaceMatch>,
}

#[async_trait]
impl PlaceResolver for MemPlaces {
    async fn resolve(&self, q: &PlaceQuery) -> Result<Vec<PlaceMatch>> {
        Ok(self
            .matches
            .iter()
            .filter(|m| {
                q.country
                    .as_ref()
                    .is_none_or(|c| c.eq_ignore_ascii_case(&m.country))
            })
            .cloned()
            .collect())
    }
}
