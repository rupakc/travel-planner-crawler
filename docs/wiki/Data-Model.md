# Data Model

All state lives in one Postgres 17 database with the `pgvector` extension. This page lists
the tables by area and explains the main ideas: records, versions, entities, observations
and kinds.

## Tables by area

| Area | Migration | Tables and functions |
|---|---|---|
| Helpers | `0001_extensions` | extensions, `vc_month_start` |
| Sources | `0002_sources` | `sources`, `source_candidates` (scout proposals) |
| Frontier | `0003_frontier` | `origins` (robots cache), `politeness_keys`, `claims`, `epoch_seq`, `rate_bucket`, `urls`, `url_state` |
| Raw | `0004_raw` | `segments`, `captures` |
| Queue and commit | `0005_queue` | `work_items`, `vc_enqueue`, `vc_commit_segment` |
| LLM | `0006_llm` | `llm_budget`, `llm_purpose_budget`, `llm_ledger`, `llm_cache`, `batch_jobs` |
| Records | `0007_records` | `extraction_runs`, `records`, `record_versions`, `observations` |
| Entities | `0008_entities` | `entities`, `entity_claims`, `entity_redirects` |
| Index | `0009_index` | `embeddings` |
| API | `0010_api` | `api_clients`, `changes` |
| Ops | `0011_ops` | `tombstones`, `purge_requests`, `audits`, `crawl_digest`, `heartbeats`, `vc_ensure_partitions` |
| Store | `1201_store_rewrite` | `vc_rewrite_segment` (purge rewrite) |
| Index read model | `1501_index_read_model` | `vc_haversine_m` and read-model indexes |

### Partitioning

| Table | Partitioned by | Why |
|---|---|---|
| `urls`, `url_state` | hash of `url_key`, 16 parts | millions of rows, spread evenly; aggressive per-part autovacuum |
| `captures`, `record_versions`, `observations` | month (range) | old months can be exported to Parquet and dropped |

`maintenance` keeps partitions created 2 months ahead (`vc_ensure_partitions`). There are no
foreign keys into partitioned tables; references carry the id and the month.

## The frontier tables

- `urls(url_key, url, key, source_id, discovered_from, first_seen)`: identity. Almost never
  updated. `url_key` is 16 bytes (blake3 of the canonical URL).
- `url_state(url_key, key, state, lease_epoch, priority, next_fetch_at, fetch_mode,
  last_capture_id, etag, last_modified, payload_hash, change_count, fetch_count, …)`:
  the moving parts. `state` is `due`, `leased`, `fetched_pending`, `gone`, `tombstoned` or
  `disabled`. `fetch_mode` is `http`, `render` or `adapter:<id>`. Claims use a partial index
  on `(key, next_fetch_at, priority) WHERE state = 'due'`.
- `politeness_keys`: `next_allowed_at`, `delay_ms`, EWMA latency, park level and reason,
  daily budget and use, shard, enabled.
- `claims(key, holder, epoch, deadline)`: at most one live claim per key.
- `origins`: robots.txt state (`unknown`, `ok`, `not_found`, `unauthorized`, `challenge`,
  `unavailable`), the first 500 KiB of the body, fetch and expiry times.

## Records and versions

A **record** is one fact-bearing item found at one URL: an event, an attraction, a visa rule.

- `records`: one row per `(url_key, kind, record_key)`. Holds the current version number,
  state, licence class, title, place ids, quality and the `conflict` flag.
- `record_versions`: the full history. A new version is written **only when `data_hash`
  changes**. Each version keeps full provenance:

| Provenance field | Meaning |
|---|---|
| `capture_id`, `captured_at` | which raw capture it came from |
| `kind_schema` | `kind@version` of `data` |
| `method` | `structured`, `adapter` or `llm` |
| `extractor_ver`, `model_id`, `prompt_hash`, `extraction_run_id` | how it was extracted |
| `licence_class`, `attribution` | what you may do with it |
| `use_policy` | the opt-out signals that applied |
| `valid_from`, `superseded_at` | when this version was current (`superseded_at` null = current) |

An older capture never overwrites a newer version. Excerpts are capped at 300 characters by
a database check.

### Record states

| State | Meaning |
|---|---|
| `active` | seen recently |
| `stale` | not seen for 3 × the source's cadence |
| `gone` | the page answered 404 or 410 twice |
| `tombstoned` | removed by opt-out, denylist, robots change or purge; never served again |

Tombstones are also stored as standing exclusions (`tombstones`), so a later re-crawl does
not bring the data back.

## Entities

An **entity** is the cross-source view of one real thing (for example "Tower of London"),
built from records of several sources.

- `entities`: canonical rows (`active` or `tombstoned`).
- `entity_claims(entity_id, attribute, value, record_id, version, confidence)`: which record
  said what, per attribute.
- `entity_redirects(old_id → new_id)`: when two entities merge, the old id redirects, so ids
  never break (the API answers `301`).

Merge rule in v1, kept simple on purpose: same kind, same place, normalised-name trigram
similarity ≥ 0.8 and distance ≤ 300 m. Only licence-compatible records merge:
`share_alike` data is never merged with other licences.

## Observations

Time series, not facts: FX rates, weather normals, advisory levels (later prices).
`observations(series_key, observed_at, value, capture_id, source_id)`. The `series_key`
comes from a template in `kinds.toml`, for example `fx:{base_currency}:{quote_currency}`
gives `fx:USD:EUR`.

## Kinds

A **kind** is a type of record. Each kind is defined by data, not code (see
[vc-kinds](Crate-vc-kinds)): a JSON Schema `schemas/<kind>.v1.json`, an extraction prompt
`prompts/<kind>.md`, and a policy entry in `kinds.toml`.

| Kind | Storage | Notes |
|---|---|---|
| `place`, `attraction`, `activity`, `walking_tour`, `event`, `hotel` | record | `hotel` holds descriptive facts only, no prices |
| `visa_rule`, `advisory` | record | **high-stakes**, official sources only |
| `emergency_numbers` | record | **high-stakes**, official, or two agreeing reference sources |
| `sim_offer`, `transport_option`, `tip`, `packing_hint` | record | |
| `article` | record | blogs and news: headline, ≤ 300-char excerpt, facts, link |
| `qa_thread` | record | Stack Exchange Travel only, with attribution |
| `airport`, `route` | record | |
| `fx_rate`, `weather_normal` | observation | time series |

Every v1 schema carries `source_url`, `as_of` and `evidence` (up to three verbatim quotes),
all verbatim-checked. Fields that name a city are paired with a country field.

## Embeddings and search

`embeddings(owner_type, owner_id, model_id, dim, vec halfvec)` holds vectors (768 dimensions
from `text-embedding-005`, stored as half-precision). Each model gets its own partial HNSW
index (`m = 16`). Only current versions that UsePolicy allows are embedded. Search combines
full-text rank and vector similarity; see [Read API](Read-API).

## Change feed

Every upsert, tombstone and redirect writes a `changes` row with `xid8 =
pg_current_xact_id()` and a sequence number. `/v1/changes` serves only rows from
transactions older than every still-open transaction, so a client never skips a change even
when transactions commit out of order. The cursor is `(xid, seq)`.

## Migration rules

- Every migration starts with `SET lock_timeout = '5s'` and is **expand-only**: the previous
  image must keep working on the new schema. A test (N-1) checks this.
- Destructive ("contract") migrations live in `migrations/contract/` and run only with
  `vcrawl migrate --contract`, after the current image has run for 7 days without a rollback.
- Number blocks per owner: P0 `0001–0099`, frontier `11xx`, store `12xx`, queue `13xx`,
  llm `14xx`, index `15xx`, api `16xx`, sources `17xx`, ops `18xx`.
- Indexes on partitioned tables are built per partition (`CONCURRENTLY`), then attached.

See also: [Storage and Raw Data](Storage-and-Raw-Data), [Processing and Extraction](Processing-and-Extraction).
