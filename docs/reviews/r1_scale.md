# R1: scalability, extensibility and future-proofing review

Spec: `docs/superpowers/specs/2026-11-15-voyager-crawler-design.md`. Baseline: 5M fetches/month (~2/s), 10x = 50M/month with a frontier of 50M–500M URLs.

## Blockers

**1. [blocker] There is no sizing or retention plan for Postgres, and the claim that "10x is a connection-string change" (§4, §11) is wrong.**
The research (gcp.md §10) says 10x needs a partitioned or off-Postgres frontier. Rough year-1 numbers (100 GB disk, 8 GB RAM):
- `captures`: 60M rows, about 12 GB with indexes.
- `urls`: 50M rows, about 15 GB plus 10 GB of indexes, with bloat on top.
- `document_versions` jsonb.
- pgvector: 5M 768-d chunks is about 15 GB raw, and HNSW roughly doubles that.

HNSW that spills out of RAM degrades badly, and it shares the 8 GB with Chromium.

Change:
- (a) Add a sizing table (rows, bytes, index bytes) at 1x and 10x.
- (b) Partition `captures` and `document_versions` by month. Export old partitions to Parquet in GCS.
- (c) Use `halfvec` and keep vectors in their own table (see 8). Prefer an exact scan after a selective `place_id` filter over HNSW, and use HNSW only for unfiltered search.
- (d) Rewrite §11: partitioned frontier, optionally off Postgres; index and API on a separate database.

**2. [blocker] The document/entity model can't represent multi-record pages, time series or stable ids. Fixing it later means a rewrite.**
- `documents(doc_id, url_id, kind)` is one document per URL and kind, but §5.6 allows "multiple records per page" (listings, "top 20 things to do"). Add `record_key`: the JSON-LD `@id`, else a hash of the normalised name plus position. The unique key becomes `(url_id, kind, record_key)`.
- `fx_rate` and `weather_normal`, and later price history and reviews, are time series. Storing them as entity versions makes "history" a scan of jsonb diffs. Add `observations(entity_id, metric, observed_at, value numeric, unit, currency, doc_id, version)`, partitioned by month.
- Entities have no versions or per-attribute provenance, so conflicting sources get merged silently. Add `claims(entity_id, attr, value jsonb, doc_id, version, observed_at, confidence)` and derive `entities.attrs` from them.
- Entity resolution will merge and split entities, and consumers will cache ids. Add `entity_redirects(old_id → new_id)`; the API answers 301/`merged_into` and never a 404 for a merged id.

## Majors

**3. [major] The `urls` table is a vacuum and contention hot spot.**
Each fetch updates about 10 columns, including the indexed `next_fetch_at`, so no update is HOT and both the table and its indexes bloat. Discovery adds about 100 `INSERT … ON CONFLICT` per page. The `state` index has low cardinality and is useless, and `(host, next_fetch_at)` doesn't give "best-priority due URL".
Change:
- Split into `urls` (immutable identity: id, url, host, source, discovered_from) and a narrow `url_sched(url_id, host_id, state, priority, next_fetch_at, …)` with fillfactor 70 and per-table autovacuum.
- Hash-partition both by `host_id` (16–64 partitions) for per-partition vacuum.
- Add a partial index `(host_id, priority DESC, next_fetch_at) WHERE state='queued'`.
- Stage discovered links in an UNLOGGED `link_inbox` and merge it in batches every N seconds, after a per-process seen filter (a Bloom filter or LRU of xxh3 hashes).
- Fetchers lease URL batches per host; the lease owner enforces 1-in-flight in memory.

**4. [major] A 64-bit `url_id = xxh3(canon)` will collide at scale.**
Collision odds are about 7e-5 at 50M URLs and 0.7% at 500M; a collision silently merges two URLs. Change: use a `bigint` identity PK, a `UNIQUE` on a 128-bit `xxh3_128` (`bytea(16)`), and keep the canonical URL text.

**5. [major] The host-key model conflates robots, politeness and sharding.**
- robots.txt is per origin, rate per registrable domain, and "per IP" would serialise every site behind one CDN IP.
- `hash(host) % N` reshuffles all hosts when N changes.

Change:
- Use separate `origins` (robots) and `politeness_keys` (rate; registrable domain; IP grouping only for non-CDN ASNs).
- Store an explicit `politeness_keys.shard` column with leases; rebalancing is an UPDATE.

**6. [major] Re-extraction is unbounded in cost and has no plan.**
A corpus-wide prompt change costs about a month of LLM budget. Random Coldline range reads cost $0.02/GB plus ops: about $600 for 60M records, and Archive (§11) is worse.

Change:
- Persist the LLM input (trimmed main content) as content-addressed text in Standard storage (a WET-like layer).
- Add an `extraction_runs(run_id, kind, schema_version, prompt_hash, model, scope, budget_usd, status)` table. Gate each run on a canary set and backfill lazily (on recrawl, then within its own sub-budget).
- Any raw replay iterates by `(segment, offset)` and streams whole segments.

**7. [major] The traits are incomplete for a later scale-out.**
§11 relies on `trait CaptureQueue`, but §5.1 only defines `BlobStore`, `LlmClient` and `Clock`. Define these in `vc-core` now, each with Postgres and in-memory implementations and a conformance suite:
- `Frontier` (lease/ack/nack/extend, never raw SQL in the fetcher);
- `WorkQueue<T>` (captures, extraction, embedding, renders);
- `BlobStore` (put with crc32c, ranged get, streaming segment read, list);
- `LlmClient` (online and batch);
- `Embedder` (with a `model_id` and dimension);
- `PlaceResolver`;
- `SecretSource`.

**8. [major] Embeddings are tied to one model.**
A 768-d column for `text-embedding-005` means a model change forces a re-embed with downtime. Change: `embeddings(target_kind, target_id, model_id, dim, vec halfvec)`, one partial HNSW index per active `model_id`, and side-by-side backfill before cutover.

**9. [major] Kinds and sources are less extensible than G5 claims.**
Kinds are types in `vc-core`, so a new kind changes the core. PDFs (common for visa pages), images and other media have no extension point.
Change:
- Use a data-driven kind registry (`schemas/<kind>.vN.json` plus a `kinds` table; `kind` is a text FK), with a generic `Record { kind, schema_version, attrs }` and optional typed views.
- Add `trait ContentHandler` keyed by media type (html, feed, pdf, json-api, image metadata).

**10. [major] Schema evolution, API versioning and consumers.**
"Additive under `/v1`" doesn't say what happens when `place.v2` replaces `place.v1`, and one bearer token with an IP allowlist won't serve other consumers.
Change:
- Every item carries `schema: "place.v1"`. The API projects stored versions to the requested schema with up/downcasters.
- Add `GET /v1/changes?since=<seq>`, a monotonic `change_seq` over entity versions and tombstones, for sync and real-time feeds.
- Use per-client tokens with scopes and rate limits.
- Store `licence` as structured data (SPDX id plus `redistribute`, `commercial`, `share_alike`, `attribution_required`) for filtering per consumer purpose.

**11. [major] Postgres is a single point of failure with a 24 h RPO.**
A nightly `pg_dump` of tens of GB on 2 vCPUs is slow and loses up to a day of state. Change:
- Continuous WAL archiving to GCS (wal-g or pgBackRest) for point-in-time recovery.
- Add `vcrawl admin rebuild-captures` to rebuild `captures` from the GCS `.cdxj` files.

## Minors

12. **[minor] Migrations on every deploy against 50M-row tables.** Require expand/contract, `CREATE INDEX CONCURRENTLY` and `lock_timeout`, enforced by a CI lint (squawk).
13. **[minor] Segment naming `<role>-<host>-<seq>`.** If "host" is the crawled host, this produces many tiny objects. Name segments per writer instance.
14. **[minor] Roll latency and spool.** Add a priority lane with a 60 s roll for advisories and feeds. Bound the spool on the data disk and stop claiming work when it is full.
15. **[minor] Remaining gaps.** Reserve `llm_ledger` spend in local chunks, and track batch jobs in an idempotent `llm_batch_jobs` table. Add a GiST/PostGIS index for radius queries. Add global `payload_sha256` dedupe.
