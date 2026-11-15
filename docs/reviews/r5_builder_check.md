# R5 — Builder check: can P0 be written from the v2 design?

Reviewer: lead Rust engineer · 2026-11-15 · Input: design v2, research

Verdict: the politeness and storage contracts are strong, but the **seams between crates** are too loose for 15 agents to code against blind. Nothing here adds identity changes, browser retries or robots exemptions (anti-goal: no evasion).

## (a)+(b) Questions and resolutions

**Q1. Where do the traits live?** §5.1 puts `Frontier` in vc-frontier, `BlobStore` in vc-store, and so on. P1 crates would then wait on each other.
**R:** All cross-crate traits, and the types they mention, live in **`vc-core`** (modules `frontier`, `store`, `queue`, `llm`, `parse`, `sources`, `index`, `policy`). P1 crates depend only on `vc-core`, `vc-config` and dev-dep `testkit`. Only `vcrawl` (P2a) depends on implementations: a star graph.

**Q2. Who writes the WARC record, and what bytes?**
**R:** `vc-fetch` returns an `Exchange` (request head, response head, decoded body, timings, peer IP). `vc-store::warc` alone serialises records. Bodies are stored **content-decoded**, because we decode anyway to enforce the byte caps. `Content-Encoding` is dropped, `Content-Length` rewritten, the original encoding kept in `WARC-Voyager-Original-Encoding`, and the digest is sha256 of the decoded body. A 304 becomes a `revisit` record (profile `server-not-modified`). Each capturing role instance (fetcher; maintenance for dumps) is the only writer of its own `spool/<role>-<instance>/` and runs its own `SegmentRoller`.

**Q3. Who computes UsePolicy?** Signals come from robots (AI-bot block), headers (X-Robots-Tag, TDM, Content-Signal) and the body (meta robots, TDMRep link).
**R:** One pure `vc-policy` function, `use_policy(&UseSignals) -> UsePolicy`; `UsePolicy` is a meet-semilattice. The fetcher fills robots + header signals into the spool record and `captures.use_policy_fetch`. The processor adds body signals and writes `captures.use_policy_final` (mutable column; raw bytes stay immutable) and each `record_version`. Consumers read `use_policy_final`, or `use_policy_fetch` while it is NULL; the meet can only restrict: fails closed. `noarchive` sets `captures.raw_expires_at`, and maintenance purges those captures. `/.well-known/tdmrep.json` is an origin-level file fetched with robots.txt and cached in `origins`.

**Q4. Who fetches robots.txt?**
**R:** `vc-policy` is **pure** (no I/O). The fetcher fetches robots: expired origins come back in `Claim::robots_due`. The fetcher fetches it first under the same permit, spools it like any page (raw first) and commits the result to `origins`. Egress **never fetches robots**: a sub-request to an origin without fresh cached robots is denied (fail closed) and a refresh is enqueued. Allowlisted CDN hosts are seeded as keys so their robots stays fresh.

**Q5. How does the processor know the media type?**
**R:** A vc-core `MediaType` enum is fixed **at fetch time** from the discovery hint (`url_state.expect`, set by the enqueuer), then Content-Type, then a 512-byte sniff, and stored in `captures.media_type`. The processor dispatches through `HandlerRegistry::for_media(&MediaType)`. On a mismatch, the sniff wins and it is logged.

**Q6. How do adapters get permits?**
**R:** Adapters never claim. Adapter work is a URL row with `fetch_mode = 'adapter:<id>'`; the fetcher (maintenance for dumps) claims the key and calls `adapter.run(&mut AdapterCtx)` with the permit, a `PermitClient` (which paces pagination, Q8) and a `CaptureSink`. Long dumps call the epoch-fenced `Frontier::extend` between chunks; without it the lease expires and the stream aborts.

**Q7. How is a `ClaimPermit` unforgeable, and how is "DB now < deadline" checked without every crate touching Postgres?**
**R:** `ClaimPermit` has private fields and an `#[doc(hidden)] fn mint(..)`, and `clippy.toml` `disallowed-methods` allows `mint` only in vc-frontier and testkit. Liveness: `LeaseCheck::check` (sketch below), one indexed query on `claims` (`now()` vs deadline, epoch), called by `PermitClient` before **every** request.

**Q8. The global rate token: once per claim or once per request?** §5.3 takes one at claim time, but one claim covers up to 20 URLs plus render sub-requests.
**R:** **Per request.** `PermitClient::pace()` checks the lease, takes a DB token (`Frontier::take_rate_token`) and sleeps the key's delay since the last request end. Egress uses the same `pace()` (250 ms sub-resource delay).

**Q9. How does egress (a separate process) know the permit?**
**R:** Chromium's proxy credentials are `epoch:HMAC(epoch,key)`. Egress (on the `render`, `db` and outbound networks) looks the permit up by epoch and enforces scope + `pace()`. After the render, the fetcher collects the sub-request `Exchange`s from egress's control port (`GET /permits/{epoch}/exchanges`) and spools them with the DOM `resource` record.

**Q10. One commit transaction spans captures (store), url_state (frontier) and work_items (queue).**
**R:** The contract is **SQL**: migrations define `vc_commit_segment(jsonb)` and `vc_enqueue(kind, payload, dedupe_key)`; `SegmentCommitter::commit` is one call to the former. The `url_state` validators are updated only if `captured_at > url_state.last_captured_at`, a monotonic guard that is **not** epoch-gated, so orphan-epoch replays (§5.2) are safe. Epoch fencing applies to claim completion only.

**Q11. Records and the change feed.** Who writes records/versions/entities and `/v1/changes`?
**R:** `vc-extract` only returns `ExtractedRecord`s. `RecordSink` (vc-index) upserts records, versions (on `data_hash` change), entities/claims and a new `changes(seq bigserial, owner_type, owner_id, op, at)` table in one transaction.

**Q12. §5.7 needs "official source classes", but SourceSpec has no such field.**
**R:** Add `source_class: official | reference | editorial | community | commercial` to SourceSpec and `sources`; kind policy matches on it. Also fix §10.3's "3.1 Flash-Lite" to `gemini-3.5-flash-lite`.

**Q13. Partitioned tables and foreign keys.** `captures` and `record_versions` are range-partitioned by month, so plain FKs to them don't work.
**R:** No FKs into partitioned tables. References carry `(capture_id, captured_at)`; the ULID also encodes the month. The migrations create the current month + 2 future partitions + a DEFAULT partition, and maintenance creates the rest.

**Q14. testkit's role and timing.** If testkit is a P1 crate, the other 14 agents have no PG fixture or fakes on day 1.
**R:** P0 ships a **minimal testkit**: `pg()` (testcontainer, migrated), `FakeClock`, in-memory fakes of every vc-core trait, and the frozen **request-log schema** the politeness oracle reads. The P1 testkit agent adds fake-web, fake-gemini, fake-gcs and the oracles.

**Q15. Kinds without enums.**
**R:** `KindId(Arc<str>)`. `trait KindRegistry { fn get(&KindId) -> Option<&KindDef> }`, where `KindDef` = JSON schema, prompt, verbatim fields, allowed `source_class`es and `storage: Record | Observation`. Validation uses the `jsonschema` crate on `serde_json::Value`.

**Q16. Config ownership.**
**R:** One root struct, one file per crate section (`vc-config/src/sections/<crate>.rs`), written in P0 with defaults; agents edit only their own.

## (c) P0 deliverables

**Crates:** the 18 of §5.1 plus testkit (minimal, Q14). All compile; impls are `todo!()`.

**Shared types (vc-core):** ids (`UrlKey([u8;16])`, `OriginKey`, `PolitenessKey`, `CaptureId`/`SegmentId` (ULID), `Epoch`, `RecordKey`, `KindId`, `SourceId`), `CanonicalUrl`, `MediaType`, `FetchMode`, `UseSignals`/`UsePolicy`, `LicenceClass`/`SourceClass`/`Licence`, `Exchange`, `SpoolRecord`, `CaptureMeta`, `ExtractedRecord`, `Provenance`, `WorkItem`, `Reservation`, `ErrorClass`.

```rust
// vc-core::frontier
pub struct ClaimPermit { key: PolitenessKey, epoch: Epoch, deadline: DbTime,
                         scope: Arc<PermitScope>, delay: Duration }
pub struct Claim { pub permit: ClaimPermit, pub urls: Vec<LeasedUrl>, pub robots_due: Vec<OriginKey> }
#[async_trait] pub trait Frontier: LeaseCheck + Send + Sync {
    async fn claim(&self, shard: &[i16], max_urls: usize) -> Result<Option<Claim>>;
    async fn extend(&self, p: &ClaimPermit, by: Duration) -> Result<(), LeaseLost>;
    async fn take_rate_token(&self, p: &ClaimPermit) -> Result<(), LeaseLost>;
    async fn complete(&self, p: ClaimPermit, outcome: KeyOutcome) -> Result<(), LeaseLost>; // epoch-fenced
    async fn enqueue(&self, batch: &[NewUrl]) -> Result<u64>;
    async fn park(&self, key: &PolitenessKey, reason: ParkReason) -> Result<()>;
}
#[async_trait] pub trait LeaseCheck { async fn check(&self, p: &ClaimPermit) -> Result<(), LeaseLost>; }
// vc-core::policy (pure; impl in vc-policy)
pub trait Policy: Send + Sync {
    fn robots_verdict(&self, r: &RobotsEntry, url: &CanonicalUrl) -> RobotsVerdict;
    fn use_policy(&self, s: &UseSignals) -> UsePolicy;
    fn denied(&self, url: &CanonicalUrl) -> Option<DenyReason>;
    fn ssrf_check(&self, host: &str, ip: IpAddr, port: u16) -> Result<(), SsrfBlock>;
    fn is_challenge(&self, ex: &Exchange) -> bool;
}
// vc-core::fetch
#[async_trait] pub trait PermitClient: Send + Sync {
    async fn fetch(&self, p: &ClaimPermit, req: FetchRequest) -> Result<Exchange, FetchError>;
    async fn stream(&self, p: &ClaimPermit, req: FetchRequest) -> Result<LimitedBodyStream, FetchError>;
}
// vc-core::store
#[async_trait] pub trait BlobStore: Send + Sync {
    async fn put_if_absent(&self, path: &str, bytes: Bytes, crc32c: u32) -> Result<PutOutcome>;
    async fn get(&self, path: &str, range: Option<Range<u64>>) -> Result<Bytes>;
    async fn delete(&self, path: &str) -> Result<()>;
}
#[async_trait] pub trait CaptureSink { async fn append(&mut self, r: SpoolRecord) -> Result<CaptureId>; } // fsynced
#[async_trait] pub trait SegmentCommitter { async fn commit(&self, m: &SegmentManifest) -> Result<()>; }
// vc-core::parse
pub trait ContentHandler: Send + Sync {
    fn media(&self) -> &'static [MediaTypeMatch];
    fn handle(&self, meta: &CaptureMeta, body: &[u8], lim: &Limits) -> Result<Parsed>; // + body UseSignals
}
// vc-core::sources
#[async_trait] pub trait SourceAdapter: Send + Sync {
    fn id(&self) -> &'static str;
    async fn run(&self, ctx: &mut AdapterCtx<'_>) -> Result<AdapterReport>; // permit, client, sink, extend
}
// vc-core::queue / llm / index
#[async_trait] pub trait WorkQueue { async fn take(&self, kind: WorkKind, n: usize, lease: Duration) -> Result<Vec<WorkItem>>;
                                     async fn ack(&self, id: WorkId) -> Result<()>; async fn nack(&self, id: WorkId, retry_in: Duration) -> Result<()>; }
#[async_trait] pub trait BudgetLedger { async fn reserve(&self, purpose: Purpose, est: Usd) -> Result<Option<Reservation>>;
                                        async fn settle(&self, r: Reservation, actual: Usage) -> Result<()>; }
#[async_trait] pub trait LlmClient { async fn generate(&self, r: &Reservation, req: LlmRequest) -> Result<LlmResponse>; }
#[async_trait] pub trait Embedder { fn model(&self) -> &ModelId; async fn embed(&self, r: &Reservation, texts: &[String]) -> Result<Vec<Vec<f32>>>; }
#[async_trait] pub trait RecordSink { async fn upsert(&self, recs: Vec<ExtractedRecord>, prov: &Provenance) -> Result<UpsertReport>; }
```

**Migrations (`migrations/`, sqlx, P0 range 0001–0099):**
`0001_extensions` · `0002_sources` (sources, source_candidates) · `0003_frontier` (origins, politeness_keys, claims, epoch_seq, rate_bucket, urls ×16 hash parts, url_state ×16, fillfactor 70, partial claim index) · `0004_raw` (segments, captures by month incl. media_type, use_policy_fetch/final, raw_expires_at) · `0005_queue` (work_items, `vc_enqueue`, `vc_commit_segment`) · `0006_llm` (llm_budget, llm_ledger, batch_jobs) · `0007_records` (records, record_versions by month, extraction_runs, observations by month) · `0008_entities` (entities, entity_claims, entity_redirects) · `0009_index` (embeddings + per-model HNSW, FTS columns) · `0010_api` (api_clients, changes) · `0011_ops` (purge_requests, tombstones, audits, crawl_digest, heartbeats, `vc_ensure_partitions()`).

Also in P0: root `Cargo.toml` with every researched dependency pinned under `[workspace.dependencies]`, `Cargo.lock`, `deny.toml` (evasion-crate bans), `clippy.toml`, CI, `policy/*.yaml`, `sources/*.yaml`, the request-log schema and `docs/ownership.md` (path → owner).

## (d) Parallel-conflict risk and mitigation

Risk is **high** without rules. Hot files: `Cargo.toml`/lock, migrations, `vc-config`, `vc-core`, CI/deny, vcrawl wiring.

1. **One owner per path.** An agent writes only `crates/<crate>/**`, its config section and its migration block; `scripts/check-ownership.sh` fails a PR touching anything else.
2. **vc-core is frozen after P0.** Change requests go to the controller, who batches additive changes into one daily PR and rebases the worktrees.
3. **Dependencies.** Stub crates already list their deps (`workspace = true`), so the P0 lock is complete. New deps go through the controller (root `Cargo.toml`, lock, `deny.toml`). On a lock conflict: take main's lock, run `cargo metadata`, never `cargo update`.
4. **Migrations.** One owning crate per table; reserved version blocks (frontier 11xx, store 12xx, queue 13xx, llm 14xx, index 15xx, api 16xx, sources 17xx, ops 18xx). Editable until P3 (fresh DBs only); after the first prod deploy, append-only, timestamped, expand/contract with the N-1 test.
5. **Merge train.** The controller merges one PR at a time, testkit first, each with `cargo test --workspace` green.
6. **Integration is P2a's job, not P1's.** No P1 agent edits `vcrawl`; cross-crate behaviour is tested only through testkit's trait fakes.
7. **Anti-goal guard.** `deny.toml` bans plus a PR checklist: one UA, no header rotation, no proxy pools, challenges only "detect → park".
