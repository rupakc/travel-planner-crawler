# Crate: vc-index

**Purpose.** `PgIndex`: the Postgres read model. It writes extracted records and serves
them back. Implements both halves of `vc_core::index`: `RecordSink` and `IndexReader`.

Path: `crates/vc-index`. Dependencies: `sqlx`, `pgvector`, `sha2`. Migration block `15xx`
(`1501_index_read_model.sql`, `1502_observation_kind.sql`; base tables in `0007`–`0010`).
Status: wired; the processor writes through it, the embedder and api roles read from it.

## RecordSink: writing

One transaction per capture:

- `records` and `record_versions`: a new version only when `data_hash` changes; an older
  capture never overwrites a newer one. An older capture also never re-activates a record
  a newer capture marked `stale`, and never inserts a new record into a page and kind that
  a newer capture was already judged on;
- `observations` for time-series kinds;
- entities, `entity_claims` and `entity_redirects`, using the v1 merge rule (same kind, same
  place, name trigram ≥ 0.8, distance ≤ 300 m) and only licence-compatible merges
  (`share_alike` stays separate);
- `changes` rows for the change feed;
- tombstones: remove records from the API and index, and are stored as standing exclusions
  (`tombstones`) that hold on re-ingest. A stricter UsePolicy applies without a new version.
- **Tombstone vs upsert ordering:** upserts take a shared transaction advisory lock
  (`vc-index:tombstones`), tombstones take it exclusively. An upsert either commits before
  the tombstone (which then sees its records) or starts after it (and its exclusion check
  sees the tombstone). Neither can leave an active record behind.
- **Author tombstones** store the name only as `sha256:<hex>`; the re-ingest check
  (`mentions_author`) matches hashed selectors (and older plain ones) and only where the
  name is an author field (`author`, `author.name`, `authors[]`). The scoped author scrub
  itself lives in vcrawl (`admin::author`); `data_hash` is public for it.

## IndexReader: reading

- **Hybrid search:** filters (exact place or radius with `vc_haversine_m`, kinds, licence
  classes, updated since), then reciprocal rank fusion of `ts_rank` and vector similarity,
  × quality × freshness (half-life 90 days). Cursor pages never overlap.
- Entities with redirects followed; per-attribute claims.
- Record versions, newest first.
- The `(xid, seq)` change feed, served only below the oldest open transaction. Observation
  tombstone rows carry the observation's `kind` (column from migration `1502`), so
  kind-filtered clients see them.
- Observation series.

## Embeddings

| Function | What it does |
|---|---|
| `ensure_embedding_index(model, dim)` | creates a per-model partial HNSW index (halfvec, `m` from config) |
| `pending_embeddings(...)` | current, UsePolicy-allowed versions without a vector, for the embedder role |
| `store_embedding(...)` | writes one vector; reads the record `FOR SHARE`, so it waits for an in-flight tombstone and never writes a vector for a tombstoned record |
| `with_query_embedder(e)` | plugs in a `QueryEmbedder` (wired by `vcrawl`, under the LLM budget) for the vector leg; without one, search is text-only |

Other: `merge_entities(from, into)` for operator merges.

## Config

`[index]`: HNSW `m` (16), `ef_construction` (64), `ef_search` (40), RRF constant (60),
freshness half-life (90 days), merge thresholds, stale factor (3 × cadence).

## Tests

`tests/index.rs`: `version_only_on_change`, `share_alike_not_merged_with_open`,
`entity_redirect_followed`, `observation_series_append`, `hybrid_ranks_exact_place_first`,
`tombstoned_never_returned`, `changes_ordered_by_xid`, `restricted_licence_rejected`,
`older_capture_never_overwrites_newer`, `use_policy_limits_serving`,
`facts_only_excerpt_and_licence_served`, `search_cursor_pages_without_overlap`,
`embeddings_feed_vector_rank`, `author_purge_and_gone_revival`,
`stricter_policy_applies_without_new_version`, `versions_page_and_operator_merge_rules`,
`source_purge_hides_observations`,
`tombstone_and_concurrent_upsert_never_leave_an_active_record`,
`older_capture_never_reactivates_a_record_a_newer_capture_staled`,
`older_capture_never_inserts_a_record_a_newer_capture_lacks`,
`observation_tombstone_change_carries_its_kind`,
`embedding_never_written_after_its_record_is_tombstoned`.

More: [Data Model](Data-Model), [Read API](Read-API).
