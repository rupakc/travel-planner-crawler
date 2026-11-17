# Crate: vcrawl

**Purpose.** The single binary. It wires the library crates together into roles, and
provides the admin CLI, the migration runner and the container health check. It is the only
crate that depends on implementations rather than only on `vc-core` traits.

Path: `crates/vcrawl`. Dependencies: every `vc-*` crate, `clap`, `tokio`, `anyhow`, `sqlx`,
`fail`. Feature `failpoints` compiles the crash-injection points in (tests only). Migration
block `19xx` (also `1801`, `1904`–`1907`; see [Data Model](Data-Model)).

**Status:** every role is wired and tested, and the end-to-end suite runs them all together
in CI (see [Testing](Testing)). Not done yet: batch prediction (no role submits Vertex batch
jobs), the Parquet export (a stub that only lists due partitions) and the monthly full
restore drill on a Spot VM.

## Layout

| Path | Holds |
|---|---|
| `src/roles/<role>.rs` | one entry point per long-running role |
| `src/run.rs` | `vcrawl run`: several roles as tasks of one process (supervisor, shared pool, one `/metrics` + `/healthz`, graceful shutdown) |
| `src/data/` | the data path: `processor`, `peers` (held records), `stale`, `text_tier`, `embed`, `api`, `capture`, `sources` |
| `src/admin/` | one file per admin command group (`status`, `source`, `purge`, `author`, `rebuild`, `reextract`) |
| `src/agentic/` | `triage`, `scout`, `auditor`; see [Agentic Loops](Agentic-Loops) |
| `src/wiring/` | shared constructors: `config`, `db` (pools, including `ops_pool`), `fetch` (policy, permit client, renderer, third-party gate), `store` (blob store, spool, roller), `sources`, `llm` (startup probe), `obs`, `rawcap` (daily raw-bytes cap), `signals` |
| `src/ctx.rs` | `Ctx`: config, clock, instance name, metrics, progress tracker, the shared pool |
| `src/obs_jobs.rs` | the ops gauges every minute and the hourly LLM `settle_stale` (run by maintenance) |
| `src/migrate.rs`, `src/health.rs` | `vcrawl migrate`, `vcrawl health` |

Data-path roles share the `Ctx` pool. `admin`, `migrate` and a lone `maintenance` use
`wiring::db::ops_pool`, a small pool capped by `[database] max_connections`; under
`vcrawl run` maintenance uses the process's pool too.

## Roles

| Command | Job |
|---|---|
| `vcrawl run [--roles a,b,…]` | the production command: the chosen roles (default all six) as tasks of one process. One runtime; one Postgres pool of `run::pool_size` connections; one `/metrics` + `/healthz` on `obs.metrics_addr` (`/healthz` 503 while a role is down or stalled), one heartbeat per role. A failed role is restarted after a backoff (1 s doubling to 60 s), the others keep running. SIGTERM / Ctrl-C: fetch workers stop before their next request (no new pacing wait), claims complete with the rest `NotAttempted` (leases released), the spool is flushed; whatever still runs after `shutdown_grace_s` (20 s, inside the host's `docker compose stop -t 25`) is aborted, which is crash-safe. Under `run` the api's TLS `/healthz` on 8443 (the MIG autohealing check) reports the whole process: 503 while any role is down or stalled |
| `vcrawl scheduler` | every `scheduler_interval_s` (300 s): load and upsert `sources/*.yaml` (missing ones are disabled, never deleted); enqueue seeds, feeds, sitemaps and `adapter:<id>` rows; apply source limits (`min_delay_s` only raises a key's delay, `daily_budget` only lowers its budget); retire rows of disabled sources; make one page row due for origins whose robots.txt expired; export `due_urls` and `parked_keys_ratio`; prune old raw-byte counters and adapter cursors. Runs the LLM startup probe once when the LLM is on |
| `vcrawl fetcher` | the claim loop, `fetcher_concurrency` (16) workers. Per claim: robots.txt first for due origins (a `Crawl-delay` found there raises the permit delay at once); each leased URL by `http`, `render` or `adapter:<id>`; spool and fsync; `mark_fetched`; complete with a key signal. Cross-key redirects are enqueued, never followed; a challenge, 401 or 403 on a page parks the key; on robots.txt a 401/403 makes only that origin do-not-crawl (its URLs end `Disallowed`) and a challenge parks the key; 429/503 back the key off. A same-key redirect to an origin without cached robots.txt fetches it under the same permit. Also runs the segment roller (replay on start, then every `roller_tick_s`), the daily raw-bytes cap (shared through Postgres), resumable adapter cursors, and the sweeper that returns `fetched_pending` rows older than 6 h to `due` |
| `vcrawl processor` | `capture_ready` items to records: parse, final UsePolicy, in-scope links to the frontier (uncertain ones triaged by the LLM when it is on), extract, upsert; held `reference` records for high-stakes kinds (`held_records`); stale reconciliation; writes the extracted-text tier per committed segment; with `render.enabled`, re-queues `render: auto` pages flagged `needs_render` in render mode (once per content hash). A capture it cannot find or attribute is acked as skipped, logged at WARN and counted in `crawler_processor_skipped_total{reason}` (`missing`, `no_source`). Serves `/metrics` and writes a heartbeat |
| `vcrawl embedder` | embeds current, embeddable records under the embedding budget; idles when the LLM is off (still serving `/metrics` and writing a heartbeat) |
| `vcrawl api` | the `/v1` API over TLS; search gets the vector leg only with the LLM on, each query embedding reserved through the budget. Serves the crawler's `/metrics` on `obs.metrics_addr` and writes a heartbeat; vc-api's request counters stay on localhost |
| `vcrawl maintenance` | the scheduled jobs below |
| `vcrawl admin …` | the operator commands below; each prints one JSON document (ids and counts, never page content) |
| `vcrawl migrate [--contract \| --print-schema]` | apply expand migrations; `--contract` applies destructive ones; `--print-schema` prints `{"schema":N,"min_schema":N}` without a database |
| `vcrawl health` | exits 0 when Postgres answers and the container's `/healthz` (progress-based; every role under `vcrawl run`) answers 200 |

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
- **Migration gate:** an image's `migrations` label lists every expand migration it ships.
  `deploy.sh` refuses to run, or roll back to, an image when any of them is missing from
  `_sqlx_migrations` (owner blocks merge out of order, so 1102 can land after 1906 and the
  highest number proves nothing). Images without that label fall back to the min-schema
  check (`min_schema`, the newest migration, at most the database's highest).

## Deploy contract

From `deploy/compose.yaml` and `infrastructure/README.md`:

- `ENTRYPOINT ["vcrawl"]`; the compose command is `run` (every role in one container).
- Runs as uid 65532 on a read-only root filesystem.
- Settings are `VC_<SECTION>_<FIELD>` (vc-config); `crates/vc-config/tests/deploy_env.rs`
  fails on any other `VC_*` name in `deploy/compose.yaml`. The DB password arrives as a file
  (`VC_DATABASE_PASSWORD_FILE`); the API TLS pair is read from `/run/secrets` with a Secret
  Manager fallback; client tokens are argon2 hashes in `api_clients`.
- Image labels `io.voyager-crawler.schema`, `io.voyager-crawler.min-schema` and
  `io.voyager-crawler.migrations` come from `vcrawl migrate --print-schema` at build time.

## Config

`[vcrawl]`: instance, data directory, fetcher and processor concurrency, partitions ahead,
contract minimum days, scheduler interval, robots refresh batch, fetcher idle wait, roller
tick, recrawl bounds, health timeout, `vcrawl run`'s shutdown grace and pool size. Defaults:
[vc-config](Crate-vc-config).

## Tests

All in `crates/vcrawl/tests/`, on a real Postgres with testkit fakes:

| File | Covers |
|---|---|
| `crawl_path.rs`, `crawl_path_wiring.rs` | scheduler and fetcher end to end against `fake_web` (robots first, raw first, 304 as a revisit, `adapter_cursor_survives_a_restart`) |
| `fetcher_politeness.rs` | Crawl-delay learned mid-claim, adapter and robots 429/503, 5xx runs, challenge parks, the render page status ladder, same-key redirect robots, third-party key borrow |
| `failpoints.rs` | the failpoint suite: a crash at each raw-store step, then a restart; `no_lost_no_duplicate_no_orphan_304` |
| `processor.rs`, `data2_held.rs`, `data2_stale.rs`, `data2_text.rs`, `data2_triage.rs` | records, held records, stale records, the text tier, link triage |
| `render_requeue.rs` | `render: auto` pages flagged `needs_render` re-queued in render mode under normal politeness, once per content hash |
| `embedder.rs`, `api.rs` | embeddings and the API role |
| `maintenance.rs`, `maintenance_agentic.rs`, `obs_jobs.rs` | maintenance jobs, scout and auditor, ops gauges |
| `admin.rs`, `admin_purge_trace.rs`, `purge_author.rs`, `purge_hygiene.rs` | admin commands (`rebuild_captures_from_cdxj_equivalent`), purge traces, the scoped author purge, purge and expiry hygiene |
| `migrate.rs` | expand and contract rules; `migration_n_minus_1` replays the previous image's recorded queries against the new schema |
| `role_metrics.rs` | each role alone serves `/metrics` + `/healthz` and beats; `vcrawl run` starts every role with one listener and a heartbeat each, keeps the others up when one fails (and reports it), and the binary stops cleanly on SIGTERM |
| `run_shutdown.rs` | a shutdown mid-claim makes no further request, completes the claim (lease released, the rest `due`) and commits every fsynced spool frame, without waiting out the key's pacing |

See [Testing](Testing) for the E2E suite.
