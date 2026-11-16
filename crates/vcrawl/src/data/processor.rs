//! The processor (spec §5.7): one committed capture → records.
//!
//! For each `capture_ready` item: load the committed capture and its stored body (a revisit
//! resolves to the body it repeats) → `vc-parse` with the media type fixed at fetch time → the
//! final use policy (fetch-time policy met with body signals; it can only restrict) → in-scope
//! links to the frontier → `vc-extract` (structured data first; the LLM only when it is
//! configured, the payload changed and the extraction budget reserves) → `RecordSink::upsert`.
//!
//! **Idempotent on redelivery** (the queue is at-least-once): the sink is idempotent on
//! (capture, extraction run, record key), links are enqueued `ON CONFLICT DO NOTHING`, and a
//! held record is keyed by (kind, source, record key).
//!
//! **Stale records:** after the upsert, this page's records that the capture no longer contains
//! (where its extraction could have produced them) are marked stale; stale ones it contains
//! again are re-activated ([`stale::reconcile`]).
//!
//! **High-stakes kinds** (`visa_rule`, `advisory`, `emergency_numbers`): `vc-extract` refuses
//! them from non-official classes before any model call. `emergency_numbers` from a `reference`
//! source is held until a second independent official/reference source agrees: held records
//! are fed back as peers ([`PendingPeers`], the shared `held_records` table) and promoted once
//! agreement is reached, by whichever processor instance sees the agreeing capture.

use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use tokio::task::JoinSet;
use vc_core::frontier::{Frontier, NewUrl};
use vc_core::index::{RecordSink, UpsertReport};
use vc_core::kinds::KindRegistry;
use vc_core::media::{FetchMode, MediaType};
use vc_core::parse::{HandlerRegistry, Limits, LinkKind, Parsed};
use vc_core::queue::{WorkItem, WorkKind, WorkQueue};
use vc_core::records::{ExtractedRecord, Provenance};
use vc_core::store::{BlobStore, CaptureRole};
use vc_core::usepolicy::UsePolicy;
use vc_core::{CaptureRef, Error, ErrorClass, KindId, Result};
use vc_extract::gate::{compare_with_peers, is_high_stakes, publisher_of};
use vc_extract::{ExtractInput, Extractor, Gate, PeerRecord, source_gate};

use crate::agentic::AgenticLlm;
use crate::agentic::triage::{LinkCandidate, PgTriageCache, triage_links};
use crate::data::capture::{self, CaptureReady, StoredCapture};
use crate::data::peers::{PeerScope, PendingPeers, index_peers};
use crate::data::sources::{LoadedSource, SourceCache};
use crate::data::stale::{self, Coverage};

/// Max links enqueued per capture.
pub const MAX_LINKS_PER_CAPTURE: usize = 1_000;

/// Why a capture produced nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The capture row is gone (purged) or never committed.
    Missing,
    /// Not the leased URL itself (sub-resource, robots, manifest).
    NotPrimary,
    /// Not a 2xx/304 response.
    Status,
    /// No source, or the source row is unknown.
    NoSource,
    /// The source is disabled.
    SourceDisabled,
    /// The source's licence class is never ingested.
    Restricted,
    /// No stored body (metadata, or the repeated capture is gone).
    NoBody,
    /// No content handler for the media type.
    NoHandler,
}

/// What processing one capture did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// Parsed and extracted.
    Processed {
        /// What the sink wrote for this capture.
        report: UpsertReport,
        /// Records held back awaiting agreement.
        held: usize,
        /// Previously held records promoted thanks to this capture.
        promoted: usize,
        /// New URLs enqueued.
        links: u64,
        /// Whether the payload changed since the URL's previous capture.
        payload_changed: bool,
        /// This page's records the capture no longer contains, marked stale.
        staled: u64,
        /// This page's stale records the capture contains again, re-activated.
        unstaled: u64,
    },
    /// Nothing to do.
    Skipped(SkipReason),
}

/// The processor's collaborators.
pub struct ProcessorDeps {
    /// Pool (captures, sources, peers).
    pub pool: PgPool,
    /// Raw store.
    pub blobs: Arc<dyn BlobStore>,
    /// Work queue (`capture_ready`).
    pub queue: Arc<dyn WorkQueue>,
    /// Record sink (`PgIndex`).
    pub sink: Arc<dyn RecordSink>,
    /// Kind registry (the same one the extractor uses).
    pub kinds: Arc<dyn KindRegistry>,
    /// Extraction pipeline (with LLM only when enabled).
    pub extractor: Arc<Extractor>,
    /// Frontier for discovered links (`None` = links are not followed).
    pub frontier: Option<Arc<dyn Frontier>>,
    /// Content handlers.
    pub handlers: HandlerRegistry,
    /// Parser limits.
    pub limits: Limits,
}

/// The processor.
pub struct Processor {
    d: ProcessorDeps,
    sources: SourceCache,
    pending: PendingPeers,
    triage: Option<(AgenticLlm, PgTriageCache)>,
    lease: Duration,
}

impl std::fmt::Debug for Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Processor").finish_non_exhaustive()
    }
}

fn kinds_union(a: &[KindId], b: &[KindId]) -> Vec<KindId> {
    let mut v: Vec<KindId> = a.iter().chain(b).cloned().collect();
    v.sort();
    v.dedup();
    v
}

impl Processor {
    /// A processor with a 10-minute item lease.
    pub fn new(d: ProcessorDeps) -> Self {
        let sources = SourceCache::new(d.pool.clone());
        let pending = PendingPeers::new(d.pool.clone());
        Processor {
            d,
            sources,
            pending,
            triage: None,
            lease: Duration::from_secs(600),
        }
    }

    /// Replaces the source cache (tests use a zero TTL).
    pub fn with_sources(mut self, s: SourceCache) -> Self {
        self.sources = s;
        self
    }

    /// Replaces the held-record set (tests use other limits).
    pub fn with_pending(mut self, p: PendingPeers) -> Self {
        self.pending = p;
        self
    }

    /// Enables link triage (spec §5.7): in-scope page links the heuristics leave uncertain go
    /// to [`triage_links`] (verdicts cached per URL pattern in `llm_cache`). Wired only when
    /// the LLM is enabled; a disabled LLM, a refused budget or a failure leaves links undecided,
    /// and undecided links keep the heuristic default (followed).
    pub fn with_triage(mut self, llm: AgenticLlm) -> Self {
        let cache = PgTriageCache::new(self.d.pool.clone());
        self.triage = Some((llm, cache));
        self
    }

    /// The held (not yet servable) records.
    pub fn pending(&self) -> &PendingPeers {
        &self.pending
    }

    /// Leases up to `n` `capture_ready` items, processes them concurrently and acks each one
    /// (or nacks it for a retry on a transient failure). Returns the number taken.
    pub async fn tick(self: &Arc<Self>, n: usize) -> Result<usize> {
        let items = self
            .d
            .queue
            .take(WorkKind::CaptureReady, n.max(1), self.lease)
            .await?;
        let taken = items.len();
        let mut set = JoinSet::new();
        for it in items {
            let me = self.clone();
            set.spawn(async move {
                let r = me.process_item(&it).await;
                (it, r)
            });
        }
        while let Some(joined) = set.join_next().await {
            let (it, r) = joined.map_err(|e| Error::Invalid(format!("processor task: {e}")))?;
            match r {
                Ok(o) => {
                    match &o {
                        Outcome::Skipped(r) => tracing::debug!(work_id = it.id.0,
                            reason = ?r, "capture skipped"),
                        Outcome::Processed { links, .. } => tracing::debug!(
                            work_id = it.id.0,
                            count = *links,
                            "capture processed (links enqueued)"
                        ),
                    }
                    self.d.queue.ack(it.id).await?
                }
                Err(e) => match e.class() {
                    ErrorClass::Permanent | ErrorClass::Policy => {
                        tracing::warn!(work_id = it.id.0, error_class = ?e.class(), error = %e,
                            "capture not processable; dropped");
                        self.d.queue.ack(it.id).await?;
                    }
                    class => {
                        tracing::warn!(work_id = it.id.0, error_class = ?class, error = %e,
                            "capture processing failed; retrying");
                        self.d.queue.nack(it.id, Duration::from_secs(60)).await?;
                    }
                },
            }
        }
        Ok(taken)
    }

    /// Processes one work item (its payload is `{capture_id, captured_at}`).
    pub async fn process_item(&self, it: &WorkItem) -> Result<Outcome> {
        let c = CaptureReady::parse(&it.payload)?;
        self.process(&c).await
    }

    /// Processes one committed capture.
    pub async fn process(&self, cref: &CaptureRef) -> Result<Outcome> {
        let pool = &self.d.pool;
        let Some(c) = capture::load(pool, cref).await? else {
            return Ok(Outcome::Skipped(SkipReason::Missing));
        };
        // The committed (microsecond) reference, not the payload's nanosecond one.
        let stored_ref = c.meta.capture_ref();
        let cref = &stored_ref;
        if c.meta.role != CaptureRole::Primary {
            return Ok(Outcome::Skipped(SkipReason::NotPrimary));
        }
        if let Some(s) = c.meta.status
            && !(200..300).contains(&s)
            && s != 304
        {
            return Ok(Outcome::Skipped(SkipReason::Status));
        }
        let Some(sid) = &c.meta.source_id else {
            return Ok(Outcome::Skipped(SkipReason::NoSource));
        };
        let Some(src) = self.sources.get(sid).await? else {
            return Ok(Outcome::Skipped(SkipReason::NoSource));
        };
        if !src.enabled {
            return Ok(Outcome::Skipped(SkipReason::SourceDisabled));
        }
        if !src.spec.licence.class.ingestible() {
            return Ok(Outcome::Skipped(SkipReason::Restricted));
        }
        let body = match capture::body(pool, &self.d.blobs, &c).await {
            // The segment was rewritten (purge) between loading the row and reading the
            // object: reload the row (new path and offsets) and read once more.
            Err(Error::NotFound(_)) => {
                let Some(c2) = capture::load(pool, cref).await? else {
                    return Ok(Outcome::Skipped(SkipReason::Missing));
                };
                match capture::body(pool, &self.d.blobs, &c2).await {
                    // Still missing: retry later rather than drop the capture.
                    Err(Error::NotFound(p)) => {
                        return Err(Error::Storage(format!(
                            "capture object missing after reload: {p}"
                        )));
                    }
                    r => r?,
                }
            }
            r => r?,
        };
        let Some(body) = body else {
            return Ok(Outcome::Skipped(SkipReason::NoBody));
        };
        let payload_changed = capture::payload_changed(pool, &c).await?;
        let Some(parsed) = vc_parse::parse(&self.d.handlers, &c.meta, &body, &self.d.limits)?
        else {
            return Ok(Outcome::Skipped(SkipReason::NoHandler));
        };
        let final_policy = c
            .meta
            .use_policy_fetch
            .meet(&UsePolicy::from_signals(&parsed.body_signals));
        capture::set_final_policy(pool, cref, &final_policy).await?;
        let links = self.enqueue_links(&src, &c, &parsed).await?;

        let prov = Provenance {
            source_id: src.spec.id.clone(),
            source_class: src.spec.source_class,
            licence: src.spec.licence.clone(),
            url_key: c.meta.url_key,
            url: c.meta.url.clone(),
            capture: c.meta.capture_ref(),
            use_policy: final_policy,
        };
        let high: Vec<KindId> = kinds_union(&src.spec.kinds, &src.spec.llm_kinds)
            .into_iter()
            .filter(|k| self.d.kinds.get(k).is_some_and(is_high_stakes))
            .collect();
        let peers = self.peers(&high, &prov).await?;
        let mut input = ExtractInput::new(&prov, &parsed);
        input.structured_kinds = &src.spec.kinds;
        input.llm_kinds = &src.spec.llm_kinds;
        input.payload_changed = payload_changed;
        input.peers = &peers;
        let mut out = self.d.extractor.extract(&input).await?;
        self.check_index_peers(&mut out, &high, &prov, &peers)
            .await?;

        let coverage = Coverage::of(&src.spec.kinds, &src.spec.llm_kinds, &out);
        let present: std::collections::HashSet<(String, String)> = out
            .records
            .iter()
            .chain(&out.pending)
            .filter(|r| r.observation.is_none())
            .map(|r| (r.kind.0.to_string(), r.record_key.0.clone()))
            .collect();
        let accepted_high: Vec<KindId> = out
            .records
            .iter()
            .chain(&out.pending)
            .filter(|r| high.contains(&r.kind))
            .map(|r| r.kind.clone())
            .collect();
        for r in &out.records {
            self.pending
                .release(&r.kind, &prov.source_id, &r.record_key)
                .await?;
        }
        let report = self.d.sink.upsert(out.records, &prov).await?;
        let held = out.pending.len();
        for r in out.pending {
            self.pending.hold(r, prov.clone()).await?;
        }
        let promoted = if accepted_high.is_empty() {
            0
        } else {
            self.promote(&kinds_union(&accepted_high, &[])).await?
        };
        let st = stale::reconcile(pool, &prov, &present, &coverage).await?;
        tracing::debug!(
            capture = %cref.capture_id,
            source = %prov.source_id.0,
            inserted = report.inserted,
            new_versions = report.new_versions,
            unchanged = report.unchanged,
            held,
            promoted,
            links,
            payload_changed,
            staled = st.staled,
            unstaled = st.unstaled,
            "capture processed"
        );
        Ok(Outcome::Processed {
            report,
            held,
            promoted,
            links,
            payload_changed,
            staled: st.staled,
            unstaled: st.unstaled,
        })
    }

    /// Held records from other sources, for the high-stakes kinds. Index peers depend on each
    /// record's place, so they are checked after extraction ([`Self::check_index_peers`]).
    async fn peers(&self, high: &[KindId], prov: &Provenance) -> Result<Vec<PeerRecord>> {
        if high.is_empty() {
            return Ok(Vec::new());
        }
        Ok(self
            .pending
            .of_kinds(high)
            .await?
            .iter()
            .filter(|h| h.provenance.source_id != prov.source_id)
            .map(|h| h.as_peer())
            .collect())
    }

    /// Checks each extracted high-stakes record against index peers of its own place (plus the
    /// held peers): flags conflicts on accepted records, and accepts held-back ones that now
    /// have enough independent agreeing publishers.
    async fn check_index_peers(
        &self,
        out: &mut vc_extract::ExtractOutput,
        high: &[KindId],
        prov: &Provenance,
        held: &[PeerRecord],
    ) -> Result<()> {
        if high.is_empty() {
            return Ok(());
        }
        let publisher = publisher_of(prov.url.host());
        let verdict = |def, rec: &ExtractedRecord, peers: &[PeerRecord]| {
            compare_with_peers(
                def,
                &prov.source_id,
                Some(&publisher),
                &rec.place_ids,
                &rec.data,
                peers,
            )
        };
        for rec in out.records.iter_mut() {
            let Some(def) = self
                .d
                .kinds
                .get(&rec.kind)
                .filter(|_| high.contains(&rec.kind))
            else {
                continue;
            };
            let mut peers = self.scoped_peers(&rec.kind, prov, rec).await?;
            peers.extend(held.iter().cloned());
            rec.conflict |= verdict(def, rec, &peers).conflict;
        }
        let mut still = Vec::new();
        for mut rec in std::mem::take(&mut out.pending) {
            let Some(def) = self.d.kinds.get(&rec.kind) else {
                still.push(rec);
                continue;
            };
            let Gate::NeedsAgreement(need) = source_gate(def, prov.source_class) else {
                still.push(rec);
                continue;
            };
            let mut peers = self.scoped_peers(&rec.kind, prov, &rec).await?;
            peers.extend(held.iter().cloned());
            let v = verdict(def, &rec, &peers);
            rec.conflict |= v.conflict;
            if v.agreeing_sources >= need {
                out.records.push(rec);
            } else {
                still.push(rec);
            }
        }
        out.pending = still;
        Ok(())
    }

    async fn scoped_peers(
        &self,
        kind: &KindId,
        prov: &Provenance,
        rec: &ExtractedRecord,
    ) -> Result<Vec<PeerRecord>> {
        index_peers(
            &self.d.pool,
            std::slice::from_ref(kind),
            &prov.source_id,
            &PeerScope::of(&rec.place_ids, &rec.data),
        )
        .await
    }

    /// Upserts held records of `kinds` that now have enough agreeing sources.
    async fn promote(&self, kinds: &[KindId]) -> Result<usize> {
        let mut promoted = 0;
        for kind in kinds {
            let Some(def) = self.d.kinds.get(kind) else {
                continue;
            };
            let held = self.pending.of_kinds(std::slice::from_ref(kind)).await?;
            for h in &held {
                let need = match source_gate(def, h.provenance.source_class) {
                    Gate::NeedsAgreement(n) => n,
                    Gate::Allowed => 1,
                    Gate::Denied => continue,
                };
                let mut peers = self.scoped_peers(kind, &h.provenance, &h.record).await?;
                peers.extend(
                    held.iter()
                        .filter(|o| o.provenance.source_id != h.provenance.source_id)
                        .map(|o| o.as_peer()),
                );
                let v = compare_with_peers(
                    def,
                    &h.provenance.source_id,
                    Some(&publisher_of(h.provenance.url.host())),
                    &h.record.place_ids,
                    &h.record.data,
                    &peers,
                );
                if v.agreeing_sources < need {
                    continue;
                }
                let mut rec: ExtractedRecord = h.record.clone();
                rec.conflict = v.conflict;
                self.d.sink.upsert(vec![rec], &h.provenance).await?;
                self.pending
                    .release(kind, &h.provenance.source_id, &h.record.record_key)
                    .await?;
                promoted += 1;
            }
        }
        Ok(promoted)
    }

    /// Batch indices of uncertain links that triage judged irrelevant (none without triage, with
    /// the LLM off, or on failure).
    async fn triage_drop(
        &self,
        src: &LoadedSource,
        parsed: &Parsed,
        uncertain: &[(usize, &vc_core::parse::Link)],
    ) -> std::collections::HashSet<usize> {
        let mut drop = std::collections::HashSet::new();
        let Some((llm, cache)) = &self.triage else {
            return drop;
        };
        if uncertain.is_empty() || !llm.enabled() {
            return drop;
        }
        let kinds: Vec<String> = kinds_union(&src.spec.kinds, &src.spec.llm_kinds)
            .iter()
            .map(|k| k.0.to_string())
            .collect();
        let context = parsed.title.clone().unwrap_or_default();
        let cands: Vec<LinkCandidate> = uncertain
            .iter()
            .map(|(_, l)| LinkCandidate {
                url: l.url.clone(),
                anchor: l.anchor.clone(),
                context: context.clone(),
            })
            .collect();
        match triage_links(llm, cache, &kinds, &cands).await {
            Ok(rep) => {
                for ((i, _), v) in uncertain.iter().zip(&rep.verdicts) {
                    if v.as_ref().is_some_and(|v| !v.relevant) {
                        drop.insert(*i);
                    }
                }
                tracing::debug!(
                    source = %src.spec.id.0,
                    links = cands.len(),
                    patterns = rep.patterns,
                    cache_hits = rep.cache_hits,
                    llm_calls = rep.llm_calls,
                    dropped = drop.len(),
                    "links triaged"
                );
            }
            Err(e) => {
                tracing::warn!(error_class = ?e.class(), error = %e,
                    "link triage failed; links keep the heuristic default");
            }
        }
        drop
    }

    /// Enqueues in-scope links (sitemap and feed entries keep the parent's depth; page links
    /// go one level deeper, up to the source's `max_depth`). `rel=nofollow`, canonical and
    /// alternate links are not followed.
    async fn enqueue_links(
        &self,
        src: &LoadedSource,
        c: &StoredCapture,
        parsed: &Parsed,
    ) -> Result<u64> {
        let Some(frontier) = &self.d.frontier else {
            return Ok(0);
        };
        if parsed.links.is_empty() || matches!(c.meta.fetch_mode, FetchMode::Adapter(_)) {
            return Ok(0);
        }
        let depth: Option<(i16,)> = sqlx::query_as("SELECT depth FROM urls WHERE url_key = $1")
            .bind(c.meta.url_key.as_bytes().as_slice())
            .fetch_optional(&self.d.pool)
            .await
            .map_err(|e| Error::Db(e.to_string()))?;
        let depth = depth.map(|d| d.0.max(0) as u8).unwrap_or(0);
        let listing = matches!(
            c.meta.media_type,
            MediaType::Sitemap | MediaType::SitemapText | MediaType::Rss | MediaType::Atom
        );
        let child = if listing {
            depth
        } else {
            depth.saturating_add(1)
        };
        let mut batch = Vec::new();
        let mut uncertain = Vec::new();
        for l in &parsed.links {
            if batch.len() >= MAX_LINKS_PER_CAPTURE {
                break;
            }
            if matches!(l.kind, LinkKind::Canonical | LinkKind::Alternate)
                || l.rel.iter().any(|r| r.eq_ignore_ascii_case("nofollow"))
                || l.url == c.meta.url
                || !src.in_scope(&l.url)
            {
                continue;
            }
            let follow_listing = matches!(l.kind, LinkKind::Sitemap | LinkKind::Feed);
            let d = if follow_listing { depth } else { child };
            if d > src.spec.max_depth {
                continue;
            }
            let mut n = NewUrl::new(l.url.clone());
            n.source_id = Some(src.spec.id.clone());
            n.discovered_from = Some(c.meta.url_key);
            n.depth = d;
            n.expect = match l.kind {
                LinkKind::Sitemap => Some(MediaType::Sitemap),
                _ => None,
            };
            // Certain: listing entries, and links an explicit include pattern matched. Page
            // links in scope only by the seed-host fallback are uncertain.
            if !listing && !follow_listing && src.include.is_empty() {
                uncertain.push((batch.len(), l));
            }
            batch.push(n);
        }
        let drop = self.triage_drop(src, parsed, &uncertain).await;
        if !drop.is_empty() {
            batch = batch
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !drop.contains(i))
                .map(|(_, n)| n)
                .collect();
        }
        if batch.is_empty() {
            return Ok(0);
        }
        frontier.enqueue(&batch).await
    }
}
