# Crate: vc-api

**Purpose.** The internal `/v1` read API for the Voyager backend. An axum router over the
`IndexReader` and `PlaceResolver` traits. The frozen contract is
`docs/Crawler-API.md`; a guide is on [Read API](Read-API).

Path: `crates/vc-api`. Dependencies: `axum`, `axum-server` (TLS), `tower-http`, `utoipa`
(OpenAPI), `argon2`, `sqlx`. Migration block `16xx` (no migration needed: `api_clients` and
`changes` are in `0010_api.sql`). Status: wired as the `api` role of [vcrawl](Crate-vcrawl);
search gets the vector leg only when the LLM is enabled.

## Modules

| Module | What it does |
|---|---|
| `handlers` | the endpoints: search, entities, entity by id (301 on merge), record versions, articles, observations, carbon factors, grid intensity and vehicle occupancy (`/v1/carbon/factors`, `/v1/carbon/grid`, `/v1/carbon/occupancy`, 2026-11-19), place facts (`/v1/facts`, 2026-11-20), places resolve, changes, sources, stats, openapi, healthz, readyz |
| `auth` | `Bearer <client_id>.<secret>`; argon2id hashes (`hash_token`, `verify_token`, `split_token`); `ClientStore` (`MemClientStore` for tests, Postgres in `pg`); verified tokens cached 60 s (`CACHE_TTL`) |
| `ratelimit` | per-client token bucket (600/min default); `429` with `Retry-After` 1–60 s |
| `shape` | licence shaping on every item: per-client licence classes, UsePolicy, `facts_only` text cut to 300 characters and full-text keys removed |
| `dto` | the response types (`ItemDto`, `EntityDto`, `ChangeDto`, …) with OpenAPI schemas |
| `catalog` | sources and stats (cached up to 60 s); the carbon reads (`FactorQuery`, `GridQuery`, `OccupancyQuery`; `MemCatalog` holds in-memory factors, grid points and occupancies for tests) |
| `error` | `{"error", "message"}` bodies and status codes |
| `tls` | `serve`: TLS on the API port, plus a separate localhost-only `/metrics` listener for the `vc_api_*` request counters (the api role's `crawler_*` metrics and heartbeat are served by vcrawl on `:9464`, like every role). `tls_from_secrets` loads `/run/secrets/api_tls_{cert,key}` (the pair `deploy.sh` installs only when key and certificate match) and falls back to Secret Manager when those files are missing or unusable. rustls refuses a mismatched pair, so a half-done TLS rotation does not crash-loop the api; nothing usable fails closed (no plain HTTP) |
| `metrics` | request metrics |
| `pg` | Postgres-backed client store and catalog, including the carbon reads (current `emission_factor` and `occupancy` versions filtered on `data` fields and the use policy in SQL, so paging is exact; `grid_ci:<CC>:…` observations by `LIKE` over validated parts) and the place facts (current
`place_fact` versions; a hook's place resolved in SQL through Wikidata sitelinks) |

## Config

`[api]`: bind address, metrics bind (`127.0.0.1:9100`), TLS secret names, rate per minute,
max page (100), excerpt length (300), request timeout (10 s).

## Tests

- `tests/api.rs`: `unauthenticated_401`, **`openapi_snapshot`** (insta, in
  `tests/snapshots/api__openapi.snap`), `changes_cursor_no_skips`, `facts_only_excerpt_capped`,
  `licence_and_provenance_on_every_item`, `redirected_entity_301`,
  `licence_classes_filtered_per_client`, `use_policy_restricts_served_fields`,
  `rate_limited_per_client_429`, `metrics_not_on_public_listener`,
  `places_resolve_and_limits`, `sources_stats_and_probes`,
  `carbon_factors_filter_by_category_country_region_year_source`,
  `carbon_factors_respect_the_client_licence_classes`, `carbon_grid_is_one_country_newest_first`,
  `carbon_occupancy_filters_by_mode_country_year_purpose_basis_source`,
  `carbon_occupancy_respects_the_client_licence_classes`, `carbon_occupancy_needs_a_token`,
  `facts_filter_by_place_country_and_type_and_resolve_hooks`,
  `facts_respect_the_client_licence_classes`, `facts_need_a_token`.
- `tests/pg.rs`: the Postgres client store, sources and stats; `pg_carbon_factors_and_grid`, `pg_carbon_occupancy`, `pg_facts_resolve_hooks_and_filter_by_place`.
- TLS: `mismatched_pair_is_refused`, `installed_pair_wins_over_a_half_rotated_secret_manager`
  (fixtures: two self-signed P-256 pairs in `tests/fixtures/tls/`).

## Compatibility rule

Additive only. Update the OpenAPI snapshot and `Crawler-API.md` in the same PR. A breaking
change needs `/v2` beside `/v1`.
