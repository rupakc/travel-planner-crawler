# Crate: vc-index

**Purpose.** `PgIndex`: the Postgres read model. It writes extracted records and serves
them back. Implements both halves of `vc_core::index`: `RecordSink` and `IndexReader`.

Path: `crates/vc-index`. Dependencies: `sqlx`, `pgvector`. Migration block `15xx`
(`1501_index_read_model.sql`; base tables in `0007`–`0010`).

## RecordSink: writing

One transaction per capture:

- `records` and `record_versions`: a new version only when `data_hash` changes; an older
  capture never overwrites a newer one;
- `observations` for time-series kinds;
- entities, `entity_claims` and `entity_redirects`, using the v1 merge rule (same kind, same
  place, name trigram ≥ 0.8, distance ≤ 300 m) and only licence-compatible merges
  (`share_alike` stays separate);
- `changes` rows for the change feed;
- tombstones: remove records from the API and index, and are stored as standing exclusions
  (`tombstones`) that hold on re-ingest. A stricter UsePolicy applies without a new version.

## IndexReader: reading

- **Hybrid search:** filters (exact place or radius with `vc_haversine_m`, kinds, licence
  classes, updated since), then reciprocal rank fusion of `ts_rank` and vector similarity,
  × quality × freshness (half-life 90 days). Cursor pages never overlap.
- Entities with redirects followed; per-attribute claims.
- Record versions, newest first.
- The `(xid, seq)` change feed, served only below the oldest open transaction.
- Observation series.

## Embeddings

| Function | What it does |
|---|---|
| `ensure_embedding_index(model, dim)` | creates a per-model partial HNSW index (halfvec, `m` from config) |
| `pending_embeddings(...)` | current, UsePolicy-allowed versions without a vector, for the embedder role |
| `store_embedding(...)` | writes one vector |
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
`source_purge_hides_observations`.

More: [Data Model](Data-Model), [Read API](Read-API).
