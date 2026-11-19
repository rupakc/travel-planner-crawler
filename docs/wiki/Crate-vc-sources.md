# Crate: vc-sources

**Purpose.** Load and validate source files, seed them into the database and frontier, and
provide the built-in API and dump **adapters**.

Path: `crates/vc-sources`. Dependencies: `serde_norway` (YAML), `quick-xml`, `csv`, `bzip2`,
`flate2`, `sqlx`. Migration block `17xx` (`1701_adapter_cursors.sql`); the `sources` table
is in `0002_sources.sql`. Status: wired; the scheduler seeds sources every pass and the
fetcher runs adapters.

## Modules and key items

| Module | Key items | What it does |
|---|---|---|
| `spec` | `load_dir`, `read_dir`, `load_file`, `parse_spec`, `validate`, `ValidateOpts`, `Validation`, `deny_match`, `load_denylist` | parse `sources/<id>.yaml` into `SourceSpec` and check it: id equals file name, denylisted seeds are errors, `min_delay_s ≥ 5` (`MIN_DELAY_FLOOR_S`), `daily_budget ≤ 3000` (`DAILY_BUDGET_CEILING`), high-stakes kinds only from allowed classes, `restricted` must be disabled. The library half of `vcrawl admin source validate`. |
| `seed` | `seed_sources`, `seed_urls`, `seed_frontier`, `spec_hash`, `SeedReport` | upsert specs into `sources` (by spec hash), enqueue seed, sitemap, feed and `adapter:<id>` rows |
| `adapters` | `REGISTRY`, `ids()`, `get(id)`, `registry_with(cfg)`, `DumpSettings`, `ResumableAdapter`, `resumable()` | the built-in adapters; `ResumableAdapter::run_from(ctx, cursor)` lets an adapter continue where a capped run stopped |
| `cursors` | `PgAdapterCursors` | resume cursors in table `adapter_cursors` (`url_key`, `adapter`, `cursor` ≤ 2,048 chars) |
| `adapters::dump` | streaming dump runner | decompress, split into units (lines, CSV records, XML elements), filter on the fly, batch kept units into `resource` records, write a `dump_manifest` `metadata` record |

## How an adapter runs

- An adapter **never claims**. The fetcher (or maintenance, for dumps) claims the key, leases
  a URL row whose fetch mode is `adapter:<id>`, and calls `SourceAdapter::run` with the
  permit.
- Every request goes through `ctx.client` (lease check, global rate token, pacing).
- **Raw first:** each API response is spooled (fsync) as a WARC `response` before anything
  is derived from it.
- Fan-out (one row per country, per year) is done by enqueueing more `adapter:<id>` rows, so
  per-key politeness applies to each.
- API keys come from Secret Manager and are redacted before anything reaches the spool.
- **Status handling:** a 429 or 503 (from an API call or a dump download) is
  `FetchError::RateLimited` with the parsed `Retry-After` (seconds or HTTP date); other 5xx
  are `FetchError::ServerError`. The fetcher turns the first into a key back-off (the claim
  stops) and counts the second toward the key's server-error run. See
  [Politeness and Robots](Politeness-and-Robots).
- **Resume cursors:** `wikidata_sparql` (`<query>@<offset>`) and `ticketmaster_discovery`
  (the redacted next-page path, accepted only on the row's own origin and endpoint) are
  resumable. When a run stops at its page cap, the fetcher saves the cursor and the row is
  due again after 60 s (key politeness still applies); a complete run deletes it. The
  scheduler prunes cursors of rows that left the crawl or were not resumed for 30 days.
  Unknown cursors start over.
- Each adapter module also has a pure `parse` function, tested against recorded fixtures.

## The 12 adapters

`canada_advisories`, `ecb_fx`, `fcdo_content_api`, `gtfs_static`, `met_norway`,
`mobility_database_catalog`, `nager_holidays`, `ourairports_csv`, `se_travel_dump`,
`state_dept_advisories`, `ticketmaster_discovery`, `wikidata_sparql`, `wikivoyage_dump`.
What each one reads: [Sources and Licences](Sources-and-Licences#built-in-adapters-apis-and-dumps).
Later additions include the open-data readers (2026-11-18) and `factor_table` (2026-11-19,
CSV/XLSX emission-factor and grid-intensity tables; since 2026-11-19 also ODS, zipped CSV,
stacked tables found by an anchor cell, and several tables per file; also `occupancy` records,
two-row headers and filled-down group cells) and `ember_api` (2026-11-19, Ember API
carbon intensity per country, owner key in the `api_key` query parameter, redacted); see
[Carbon Footprint Data](Carbon-Footprint-Data).
Some adapters belong to sources that are disabled in `sources/*.yaml` (for example
`nager_holidays` and `mobility_database_catalog`, whose robots.txt disallows us or redirects
to a login page); a disabled source is never seeded. The list and reasons are on
[Sources and Licences](Sources-and-Licences).

## Config

`[sources]`: source directory, dump window (02:00–05:00 UTC), dump byte cap (2 GiB per
run), lease extension interval while streaming (60 s).

## Tests

- `tests/sources.rs`: `all_sources_yaml_load_and_validate`, `validation_rules`,
  `denylisted_seed_is_an_error`, `seed_frontier_enqueues_adapter_rows`,
  `seed_sources_upserts_into_postgres`.
- `tests/adapters.rs`: for every adapter, `<adapter>_parses_fixture` and
  `<adapter>_writes_raw_first`; plus `ecb_fx_garbage_is_spooled_then_rejected`,
  `met_norway_refuses_bad_coordinates_without_a_request`,
  `ticketmaster_key_never_reaches_the_spool`, `ticketmaster_page_cap_leaves_a_cursor`,
  `ticketmaster_without_key_makes_no_request`, `rate_limits_and_server_errors_are_typed`,
  `dump_rate_limit_and_server_error_are_typed`, `wikidata_sparql_resumes_from_its_cursor`,
  `ticketmaster_resumes_from_its_cursor`, and truncated-dump handling.

## Adding an adapter

Implement `SourceAdapter` in `src/adapters/<id>.rs`, add it to `REGISTRY`, record a wiremock
fixture, and test both "parses fixture" and "writes raw first".
