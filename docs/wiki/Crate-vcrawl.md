# Crate: vcrawl

**Purpose.** The single binary. It wires the library crates together into roles, and
provides the admin CLI, the migration runner and the container health check. It is the only
crate that depends on implementations rather than only on `vc-core` traits.

Path: `crates/vcrawl`. Dependencies: every `vc-*` crate, `clap`, `tokio`, `anyhow`, `sqlx`,
`fail`. Feature `failpoints` compiles the crash-injection points in (tests only). Migration
block `19xx` (also `1801`, `1904`–`1906`; see [Data Model](Data-Model)).

**Status:** every role is wired and tested, and the end-to-end suite runs them all together
in CI (see [Testing](Testing)). Not done yet: batch prediction (no role submits Vertex batch
jobs), the Parquet export (a stub that only lists due partitions) and the monthly full
restore drill on a Spot VM.

## Layout

| Path | Holds |
|---|---|
| `src/roles/<role>.rs` | one entry point per long-running role |
| `src/data/` | the data path: `processor`, `peers` (held records), `stale`, `text_tier`, `embed`, `api`, `capture`, `sources` |
| `src/admin/` | one file per admin command group (`status`, `source`, `purge`, `author`, `rebuild`, `reextract`) |
| `src/agentic/` | `triage`, `scout`, `auditor`; see [Agentic Loops](Agentic-Loops) |
| `src/wiring/` | shared constructors: `config`, `db` (pools, including `ops_pool`), `fetch` (policy, permit client, renderer, third-party gate), `store` (blob store, spool, roller), `sources`, `llm` (startup probe), `obs`, `rawcap` (daily raw-bytes cap), `signals` |
| `src/ctx.rs` | `Ctx`: config, clock, instance name, metrics, progress tracker, the shared pool |
| `src/obs_jobs.rs` | the ops gauges every minute and the hourly LLM `settle_stale` (run by maintenance) |
| `src/migrate.rs`, `src/health.rs` | `vcrawl migrate`, `vcrawl health` |

Data-path roles share the `Ctx` pool. `admin`, `migrate` and `maintenance` use
`wiring::db::ops_pool`, a small pool capped by `[database] max_connections`.

## Roles

| Command | Job |
|---|---|
| `vcrawl scheduler` | every `scheduler_interval_s` (300 s): load and upsert `sources/*.yaml` (missing ones are disabled, never deleted); enqueue seeds, feeds, sitemaps and `adapter:<id>` rows; apply source limits (`min_delay_s` only raises a key's delay, `daily_budget` only lowers its budget); retire rows of disabled sources; make one page row due for origins whose robots.txt expired; export `due_urls` and `parked_keys_ratio`; prune old raw-byte counters and adapter cursors. Runs the LLM startup probe once when the LLM is on |
| `vcrawl fetcher` | the claim loop, `fetcher_concurrency` (16) workers. Per claim: robots.txt first for due origins (a `Crawl-delay` found there raises the permit delay at once); each leased URL by `http`, `render` or `adapter:<id>`; spool and fsync; `mark_fetched`; complete with a key signal. Cross-key redirects are enqueued, never followed; a challenge, 401 or 403 parks the key; 429/503 back the key off. A same-key redirect to an origin without cached robots.txt fetches it under the same permit. Also runs the segment roller (replay on start, then every `roller_tick_s`), the daily raw-bytes cap (shared through Postgres), resumable adapter cursors, and the sweeper that returns `fetched_pending` rows older than 6 h to `due` |
| `vcrawl processor` | `capture_ready` items to records: parse, final UsePolicy, in-scope links to the frontier (uncertain ones triaged by the LLM when it is on), extract, upsert; held `reference` records for high-stakes kinds (`held_records`); stale reconciliation; writes the extracted-text tier per committed segment. A capture it cannot find or attribute is acked as skipped, logged at WARN and counted in `crawler_processor_skipped_total{reason}` (`missing`, `no_source`). Serves `/metrics` and writes a heartbeat |
| `vcrawl embedder` | embeds current, embeddable records under the embedding budget; idles when the LLM is off |
| `vcrawl api` | the `/v1` API over TLS; search gets the vector leg only with the LLM on, each query embedding reserved through the budget |
| `vcrawl maintenance` | the scheduled jobs below |
| `vcrawl admin …` | the operator commands below; each prints one JSON document (ids and counts, never page content) |
| `vcrawl migrate [--contract \| --print-schema]` | apply expand migrations; `--contract` applies destructive ones; `--print-schema` prints `{"schema":N,"min_schema":N}` without a database |
| `vcrawl health` | exits 0 when Postgres answers and the role's own `/healthz` (progress-based) answers 200 |

### Maintenance jobs

Each job is idempotent, so a restart only repeats safe work.

| Job | Every | What |
|---|---|---|
| partitions | 6 h | keep `partitions_ahead_months` (2) monthly partitions created ahead |
| purge | 15 min | delete leftover `store_garbage` objects; advance every unfinished `purge_requests` row (index, raw, done) |
| ephemeral expiry | 1 h | first give every capture whose final use policy says `raw: ephemeral` (page-level meta robots `noarchive`, known only after parsing) and has no expiry `raw_expires_at = captured_at + store.ephemeral_days` (migration 1906 indexes those captures); then cut captures past `raw_expires_at` out of their segments |
| restore drill | 7 d | rebuild the newest 50 segments' rows from their CDXJ in memory and compare with the database; exports `restore_drill_ok` |
| digest | 1 h | write yesterday's `crawl_digest` row once |
| Parquet export | 24 h | stub: lists partitions older than 12 months |
| source scout | 7 d | proposals for under-covered (destination, kind) cells; LLM only |
| quality auditor | 7 d | audits sampled record versions using the text tier; LLM only |
| ops gauges | 1 min | `vc_obs::ops::export_ops` (backup ages, failed host units, deploy drift, TLS expiry, LLM spend, LLM forced off) |
| LLM settle | 1 h | `PgLedger::settle_stale` |

Chromium recycling is not a maintenance job: maintenance has no route to the `render`
network, and vc-render restarts its CDP session inside the fetcher.

### Admin commands

| Command | What |
|---|---|
| `status` | frontier, queue, budget and source health at a glance |
| `source validate \| list [--proposed] \| approve \| disable` | source specs and scout proposals |
| `purge --url \| --url-prefix \| --host \| --author \| --source \| --record` | tombstone, raw rewrite and attribution scrub; with `--reason`, `--requested-by`, `--index-only` |
| `rebuild-captures [--dry-run \| --verify]` | rebuild `segments` and `captures` rows from `.cdxj` objects (disaster recovery); skips garbage objects and honours tombstones |
| `reextract --kind` | queue re-extraction of every capture that produced records of a kind |

Purge details:

- A URL, prefix, host or source purge puts matching `url_state` rows into the terminal
  `tombstoned` state, so the URLs leave the crawl.
- An author purge (`admin::author`) is scoped: only records that name the person **as an
  author** are tombstoned and scrubbed, whole words only. Its raw scope is fixed at the
  index stage (`purge_requests.scope`) and covers every capture of the affected URLs, plus
  the dump chunks holding the author's posts. The name is stored only as `sha256:<hex>`.
- Known gap: URLs under a purged host or prefix that are discovered for the first time after
  the purge are still enqueued (their records stay excluded), and the next forum dump can
  bring an author's posts back in raw form until the purge is re-run.

### Migrations

- `vcrawl migrate` applies the expand migrations under sqlx's advisory lock. Migrations a
  newer image already applied are tolerated, so a rollback target can run it too. The image
  is recorded as seen in `heartbeats`.
- `--contract` applies `migrations/contract/*.sql` (table `_vc_contract_migrations`) only if
  this image was first seen at least `contract_min_days` (7) ago, no other image ran
  `migrate` since, and no rollback is listed in `<data_dir>/deploy/rollbacks`.
- **Min-schema gate:** an image's `min_schema` is the newest expand migration it ships.
  `deploy.sh` refuses to run, or roll back to, an image whose min-schema is above the
  database's schema.

## Deploy contract

From `deploy/compose.yaml` and `infrastructure/README.md`:

- `ENTRYPOINT ["vcrawl"]`; the role is the compose command.
- Runs as uid 65532 on a read-only root filesystem.
- Settings are `VC_<SECTION>_<FIELD>` (vc-config); `crates/vc-config/tests/deploy_env.rs`
  fails on any other `VC_*` name in `deploy/compose.yaml`. The DB password arrives as a file
  (`VC_DATABASE_PASSWORD_FILE`); the API TLS pair is read from `/run/secrets` with a Secret
  Manager fallback; client tokens are argon2 hashes in `api_clients`.
- Image labels `io.voyager-crawler.schema` and `io.voyager-crawler.min-schema` come from
  `vcrawl migrate --print-schema` at build time.

## Config

`[vcrawl]`: instance, data directory, fetcher and processor concurrency, partitions ahead,
contract minimum days, scheduler interval, robots refresh batch, fetcher idle wait, roller
tick, recrawl bounds, health timeout. Defaults: [vc-config](Crate-vc-config).

## Tests

All in `crates/vcrawl/tests/`, on a real Postgres with testkit fakes:

| File | Covers |
|---|---|
| `crawl_path.rs`, `crawl_path_wiring.rs` | scheduler and fetcher end to end against `fake_web` (robots first, raw first, 304 as a revisit, `adapter_cursor_survives_a_restart`) |
| `fetcher_politeness.rs` | Crawl-delay learned mid-claim, adapter and robots 429/503, 5xx runs, challenge parks, the render page status ladder, same-key redirect robots, third-party key borrow |
| `failpoints.rs` | the failpoint suite: a crash at each raw-store step, then a restart; `no_lost_no_duplicate_no_orphan_304` |
| `processor.rs`, `data2_held.rs`, `data2_stale.rs`, `data2_text.rs`, `data2_triage.rs` | records, held records, stale records, the text tier, link triage |
| `embedder.rs`, `api.rs` | embeddings and the API role |
| `maintenance.rs`, `maintenance_agentic.rs`, `obs_jobs.rs` | maintenance jobs, scout and auditor, ops gauges |
| `admin.rs`, `admin_purge_trace.rs`, `purge_author.rs`, `purge_hygiene.rs` | admin commands (`rebuild_captures_from_cdxj_equivalent`), purge traces, the scoped author purge, purge and expiry hygiene |
| `migrate.rs` | expand and contract rules; `migration_n_minus_1` replays the previous image's recorded queries against the new schema |

See [Testing](Testing) for the E2E suite.
