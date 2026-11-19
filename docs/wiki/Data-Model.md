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
| Frontier | `1101_frontier_reschedule` | `url_state.refetch_at` + trigger: a forced refetch of a leased URL waits until the URL is `due` again |
| Frontier | `1102_url_state_pending_since` | `url_state.pending_since`: when a URL entered `fetched_pending`, for the sweeper |
| Store | `1201_store_rewrite` | `vc_rewrite_segment` (purge rewrite) |
| LLM | `1401_llm_breaker` | `llm_breaker` (one row): the shared, persisted spend breaker |
| Index read model | `1501_index_read_model` | read-model columns on `records` and `observations`, `entities.licence_spdx`, `vc_haversine_m` and indexes |
| Index | `1502_observation_kind` | `observations.kind`, so observation tombstones in the change feed carry their kind |
| Sources | `1701_adapter_cursors` | `adapter_cursors`: where an API adapter stopped, so the next run resumes |
| Ops | `1801_raw_bytes_daily` | `raw_bytes_daily`: decoded bytes spooled per UTC day, shared by every fetcher |
| vcrawl | `1901_held_records` | `held_records`: high-stakes reference records waiting for a second source |
| vcrawl | `1902_record_staleness` | `records.stale_since`, `stale_capture_id`, `stale_captured_at` |
| vcrawl | `1903_text_tier` | `text_tier_segments`: which segments have an extracted-text object |
| vcrawl | `1904_purge_garbage` | `store_garbage`; `vc_rewrite_segment` also releases validators of removed captures |
| vcrawl | `1905_purge_author_scope` | `purge_requests.scope`; author names stored only as `sha256:<hex>` |
| vcrawl | `1906_noarchive_expiry` | partial index over captures whose final use policy is `raw: ephemeral` and that have no `raw_expires_at` yet |
| vcrawl | `1907_render_requeue` | `url_state.render_requested_hash`: SHA-256 of the HTTP body that re-queued an `auto` URL for rendering (once per hash) |

### Tables added after go-live

| Table | Columns (main) | Purpose |
|---|---|---|
| `llm_breaker` | `id = 1`, `open_until`, `reason`, `updated_at` | Opened by a Vertex 403/429 that names billing or budget. Every role reads it before calling Vertex, so one trip pauses all roles and a restart keeps it. `open_until` only moves forward. See [Gemini and Budgets](Gemini-and-Budgets). |
| `adapter_cursors` | `url_key`, `adapter`, `cursor` (at most 2048 chars), `updated_at` | An adapter that stops at its page cap saves a position (query name and offset, or a redacted page path; never content or credentials). A complete run deletes the row. The scheduler prunes rows of URLs that left the crawl and rows not resumed for 30 days. |
| `raw_bytes_daily` | `day`, `bytes` | The daily cap on decoded bytes spooled (`fetch.daily_raw_bytes_cap`, default 3 GB), by database time. Replaces the old `raw_bytes:YYYY-MM-DD` rows in `rate_bucket`; their totals were carried over. |
| `held_records` | `kind`, `source_id`, `record_key`, `record`, `provenance`, `captured_at`, `held_at` | A high-stakes record (`emergency_numbers`) from a `reference` source, held until a second independent source agrees. Never served. Rows older than 7 days are ignored and deleted. |
| `text_tier_segments` | `segment_id`, `claimed_at`, `written_at`, `archive_rows`, `ephemeral_rows` | One row per segment whose `text/YYYY/MM/<segment>.jsonl.zst` object was written. `claimed_at` is a [lease](Glossary), so a crashed writer's segment is picked up again. See [Storage and Raw Data](Storage-and-Raw-Data). |
| `store_garbage` | `path`, `segment_id`, `reason` (`rewritten`, `abandoned`, `unreferenced`), `recorded_at` | Object paths a purge rewrite made obsolete. Written in the rewrite's own transaction, so purged bytes are deleted later even after a crash. `admin rebuild-captures` never brings a listed object back. |

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
  on `(key, next_fetch_at, priority) WHERE state = 'due'`. `pending_since` (1102) lets a
  sweeper return rows stuck in `fetched_pending` (for example a lost spool) to `due`.
  `refetch_at` (1101) holds a forced refetch until a running fetch ends.
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
| `stale` | a newer capture of its page no longer contains it; still served, never deleted. `stale_since` and `stale_capture_id` say which capture found it missing. A later capture that contains it re-activates it. Absence counts only when this capture's extraction could have produced the record (an unchanged page, a disabled LLM or a failed call never stales anything). |
| `gone` | the page answered 404 or 410 twice |
| `tombstoned` | removed by opt-out, denylist, robots change or purge; never served again |

Tombstones are also stored as standing exclusions (`tombstones`), so a later re-crawl does
not bring the data back. `records.tombstone_reason` is `opt_out`, `robots`, `denylist`,
`purge` or `gone`. Author purges store the author only as `sha256:<hex>` of the name, never
the name itself (1905).

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
| `public_holiday` | record | public, bank and school holidays per country/subdivision (added 2026-11-18) |
| `amenity` | record | public toilets, drinking fountains, free Wi-Fi spots from city registers; official only (added 2026-11-18) |
| `emission_factor` | record | one published GHG emission factor (kg CO2e per passenger-km, per room-night, per kg of food) as an official/reference table states it, with `region_code` for sub-national factors (eGRID subregion, US state; 2026-11-19); adapters `factor_table` and `eurostat_jsonstat`, official/reference only (added 2026-11-19); served by `/v1/carbon/factors` |
| `grid_carbon_intensity` | observation | gCO2e per kWh of one country's electricity per year, series `grid_ci:<ISO2>:<period>:<basis>`; adapters `factor_table` and `ember_api` (added 2026-11-19); served by `/v1/carbon/grid` and `/v1/observations` |
| `fx_rate`, `weather_normal` | observation | time series |
| `price_level` | observation | country price level per year and category, series `price_level:<ISO2>:<category>:<basis>` (`eu27_2020_100` Eurostat, `us_1` World Bank; added 2026-11-18) |

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
  image must keep working on the new schema. The N-1 test checks this (see [Testing](Testing)).
- Destructive ("contract") migrations live in `migrations/contract/` (empty today) and run
  only with `vcrawl migrate --contract`. It refuses unless the current image has run for
  `vcrawl.contract_min_days` (7) days with no other image and no rollback. They are tracked
  in `_vc_contract_migrations`, apart from the expand set.
- Indexes on partitioned tables are built per partition (`CONCURRENTLY`), then attached.
- No foreign keys into partitioned tables.

Number blocks per owner:

| Block | Owner |
|---|---|
| `0001–0099` | P0 (first schema) |
| `11xx` | frontier (`vc-frontier`) |
| `12xx` | store (`vc-store`) |
| `13xx` | queue (`vc-queue`) |
| `14xx` | llm (`vc-llm`) |
| `15xx` | index (`vc-index`) |
| `16xx` | api (`vc-api`) |
| `17xx` | sources (`vc-sources`) |
| `18xx` | ops |
| `19xx` | `vcrawl` (data path, purge) |

### Schema labels

`vcrawl migrate --print-schema` prints `{"schema":N,"min_schema":N}` and exits without
config or a database. `schema` is the highest expand migration in the image; `min_schema`
is the lowest schema the image can run on; today it equals `schema`, since the image's
queries may use any migration it ships. The deploy workflow stores them as the image
labels `io.voyager-crawler.schema` and `io.voyager-crawler.min-schema`, plus
`io.voyager-crawler.migrations`, every version the image ships. `deploy.sh` holds a deploy,
or refuses a rollback, when any of those is missing from `_sqlx_migrations` (blocks merge
out of order, so the highest number is not enough); `vcrawl migrate` also fails if one is
still missing after it ran. Without the list it falls back to `min-schema` ≤ the database's
highest applied migration. See [Operations and Deploy](Operations-and-Deploy).

See also: [Storage and Raw Data](Storage-and-Raw-Data), [Processing and Extraction](Processing-and-Extraction).
