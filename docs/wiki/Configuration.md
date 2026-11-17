# Configuration

All settings live in `vc-config`. **Every setting has a default**, so an empty file and no
environment variables give a working development setup. No secrets live in the
configuration, only the **names** of Secret Manager secrets.

## How settings are loaded

Later sources win:

1. built-in defaults;
2. a TOML file, named by `VC_CONFIG` (or passed by the binary);
3. environment variables `VC_<SECTION>_<FIELD>`.

| Example variable | Sets |
|---|---|
| `VC_LLM_ENABLED=true` | `llm.enabled` |
| `VC_LLM_DAILY_USD=1.0` | `llm.daily_usd` |
| `VC_DATABASE_URL=postgres://…` | `database.url` |
| `VC_FRONTIER_SHARDS=[0,1]` | `frontier.shards` |
| `VC_LLM_SHARES__SCOUT=0.05` | `llm.shares.scout` (deeper nesting uses `__`) |
| `VC_PROJECT_CAP_USD=200` | `llm.project_cap_usd` (alias) |

Names are case-insensitive. Variables whose section is unknown (such as `VC_SECRET_*` for the
env secret backend, or `VC_CONFIG`) are ignored, and so is `VC_DATABASE_PASSWORD_FILE` (a
secret file path, not a setting). Unknown **fields** inside a known section are an error.

## Variables set in production

`deploy/compose.yaml` passes only real settings. A test (`crates/vc-config/tests/deploy_env.rs`)
loads the config with exactly the compose environment and fails on any name that no setting
reads.

| Variable | Set for | Value | Meaning |
|---|---|---|---|
| `VC_DATABASE_URL` | every role | `postgres://vcrawl@postgres:5432/vcrawl` | database, without the password |
| `VC_DATABASE_PASSWORD_FILE` | every role | `/run/secrets/pg_password` | password file (tmpfs, written by `deploy.sh`) |
| `VC_STORE_RAW_BUCKET`, `VC_STORE_OPS_BUCKET` | every role | `voyager-crawler-raw`, `voyager-crawler-ops` | GCS buckets |
| `VC_LLM_GCP_PROJECT`, `VC_SECRETS_PROJECT` | every role | `voyager-prod-ohzkl` | project for Vertex and Secret Manager |
| `VC_OBS_LOG_FORMAT` | every role | `json` | log format |
| `VC_LLM_ENABLED` | every role | from `host.local.env`, default `false` | turn the LLM on |
| `VC_PROJECT_CAP_USD` | every role | from `host.local.env`, default `0` | the project's Gemini spend cap; 0 keeps the LLM off |
| `VC_LLM_MONTHLY_USD` | every role | from `host.local.env`, default `30` | `llm.monthly_usd`, the crawler's monthly LLM cap; at most 50 % of `VC_PROJECT_CAP_USD` |
| `VC_RENDER_CDP_URL` | vcrawl | `http://chromium:9222` | Chromium DevTools address |
| `VC_STORE_SPOOL_DIR` | vcrawl | `/spool` | spool mount |
| `VC_VCRAWL_INSTANCE` | vcrawl | `vcrawl` | stable name, so the fetcher's spool subfolder survives a container rebuild |
| `VC_VCRAWL_FETCHER_CONCURRENCY`, `VC_VCRAWL_PROCESSOR_CONCURRENCY` | vcrawl | `8`, `2` | sized for the 4 GB VM |
| `VC_OBS_METRICS_ADDR` | vcrawl | `192.168.250.2:9464` | `/metrics` only on the internal `metrics` network ([Security](Security)) |
| `VC_API_BIND` | vcrawl | `0.0.0.0:8443` inside the container | `api.bind`. The host's own `VC_API_BIND` (the VM's internal IP, from `host.env`) is a compose variable: the port is published only on that IP |

The one `vcrawl` container runs `vcrawl run` (every role in one process).

Owner overrides go in `/var/lib/voyager-crawler/deploy/host.local.env` on the VM.
`first-boot.sh` copies them into `host.env` on every boot. `VC_LLM_ENABLED`,
`VC_PROJECT_CAP_USD` and `VC_LLM_MONTHLY_USD` are forwarded from there to every role (the
shared role environment in `deploy/compose.yaml`, checked by `deploy_env.rs`); to change
another setting (for example `VC_LLM_DAILY_USD`), add it to that environment first.

Rendering stays off in production: compose does not set `VC_RENDER_ENABLED`, so
`render.enabled` keeps its default `false`.

## Startup checks

Loading fails (the role does not start) when:

- `frontier.max_urls_per_claim` is not 1–5, the global rate is not positive, or delays break
  `0 < min_delay_s ≤ max_delay_s ≤ 3600`, or `frontier.shards` is empty;
- `fetch.max_redirects > 5` or `render.max_tabs` is not 1–2;
- an excerpt length is above 300 characters;
- `queue.max_attempts` is 0.

An invalid `[llm]` section does **not** stop the role: the LLM is forced off instead
(`llm.forced_off` holds the reason, exported as `crawler_llm_config_invalid`, which alerts).
That covers LLM caps breaking `0 ≤ daily_usd ≤ monthly_usd`, purpose shares that do not
sum to 1, and `llm.enabled` with `VC_PROJECT_CAP_USD` missing, 0, or below twice the
monthly cap. `Config::validate()` still rejects all of them. Errors in any other section stay
fatal.

## Sections

The User-Agent is a constant in code, not a setting.

### `[database]`

| Field | Default | Meaning |
|---|---|---|
| `url` | `postgres://vcrawl@localhost:5432/vcrawl` | connection URL without the password |
| `password_secret` | `crawler-pg-password` | Secret Manager name of the password |
| `max_connections` | 10 | pool size of a role run alone (`vcrawl fetcher`, …); `vcrawl run` sizes its one pool from the roles instead (`vcrawl.run_pool_max`) |
| `statement_timeout_ms` | 30000 | per session |
| `lock_timeout_ms` | 5000 | per session |

### `[policy]`

| Field | Default | Meaning |
|---|---|---|
| `denylist_path` | `policy/denylist.yaml` | never-fetch list |
| `subresource_allowlist_path` | `policy/subresource_allowlist.yaml` | CDN hosts for renders |
| `robots_ttl_s` | 86400 | robots.txt cache (24 h) |
| `robots_max_bytes` | 512000 | parse the first 500 KiB |
| `robots_last_good_days` | 30 | use the last good robots after this many days of 5xx |
| `ai_bot_tokens` | GPTBot, ClaudeBot, CCBot, Google-Extended | blocking these while allowing `*` = AI reservation |
| `allowed_ports` | 80, 443 | |

### `[frontier]`

| Field | Default | Meaning |
|---|---|---|
| `shards` | `[0]` | shards this process claims from |
| `max_urls_per_claim` | 5 | at most 5 |
| `per_url_budget_s` | 240 | claim deadline = URLs × this |
| `global_rate_per_s`, `global_rate_burst` | 4.0, 4.0 | requests per second across all processes |
| `min_delay_s`, `max_delay_s` | 5, 3600 | per-key delay bounds |
| `latency_factor`, `ewma_alpha` | 10.0, 0.3 | delay ≥ 10 × average latency |
| `daily_budget` | 3000 | requests per key per day |
| `park_initial_h`, `park_max_days` | 24, 30 | park ladder |
| `server_error_threshold`, `server_error_park_h` | 5, 1 | 5 server errors in a row: park 1 h |
| `rate_limit_backoff_max_h` | 24 | 429/503 back-off cap |
| `enqueue_batch` | 1000 | rows per insert |
| `holder` | hostname + pid | written to `claims.holder` |

### `[fetch]`

| Field | Default | Meaning |
|---|---|---|
| `request_timeout_s`, `connect_timeout_s` | 30, 10 | always clamped to the lease deadline |
| `max_redirects` | 5 | |
| `html_max_bytes`, `other_max_bytes` | 5 MiB, 10 MiB | decoded byte caps |
| `max_nested_gzip` | 2 | |
| `allowed_content_types` | HTML, XHTML, RSS, Atom, RDF, XML, JSON, JSON-LD, CSV, plain text, gzip, bzip2 | |
| `subresource_content_types` | CSS and JavaScript types | render sub-requests only |
| `dump_content_types` | `application/zip`, `application/x-zip-compressed`, `application/octet-stream` | dump downloads only (GTFS zips; the adapter checks the zip magic) |
| `daily_raw_bytes_cap` | 3 GiB | raw bytes stored per UTC day across all keys; counted in Postgres (`raw_bytes_daily`, migration 1801), so it holds across processes and restarts |

### `[render]`

| Field | Default | Meaning |
|---|---|---|
| `enabled` | false | |
| `cdp_url` | `http://chromium:9222` | headless-shell on the internal network |
| `max_tabs` | 1 | concurrent renders (1–2); one on the 4 GB VM |
| `budget_s`, `max_subrequests`, `max_bytes` | 90, 60, 20 MiB | per render |
| `network_idle_ms` | 1000 | no paused request for this long = network idle |
| `subresource_gap_ms` | 250 | the **sub-resource gap**: minimum time between two sub-requests (scripts, stylesheets) of one render to the same host. Applied inside the permit client's `admit`, never longer than the key's page delay; the next page still waits the full delay |
| `subresource_cache_ttl_h`, `subresource_cache_max_bytes` | 24, 64 MiB | in-memory cache |
| `cache_dir` | `/var/lib/voyager-crawler/render-cache` | reserved |
| `restart_after_renders` | 50 | leak containment: a new CDP session after this many renders (each render has its own browser context, disposed afterwards) |

### `[store]`

| Field | Default | Meaning |
|---|---|---|
| `spool_dir` | `/var/lib/voyager-crawler/spool` | one subfolder per role instance |
| `segment_max_bytes`, `segment_max_age_s` | 128 MiB, 600 | when a segment rolls |
| `backend` | `gcs` | or `fs` (tests, development) |
| `raw_bucket`, `ops_bucket` | `voyager-crawler-raw`, `voyager-crawler-ops` | |
| `fs_root` | `/var/lib/voyager-crawler/blobs` | for `fs` |
| `spool_backpressure_bytes`, `upload_failure_pause_s` | 2 GiB, 900 | pause fetchers |
| `zstd_level` | 9 | |
| `ephemeral_days` | 30 | `noarchive` / TDM retention |
| `gcs_endpoint`, `gcs_anonymous` | empty, false | for fake-gcs-server in tests |

### `[parse]`

| Field | Default |
|---|---|
| `max_dom_depth`, `max_nodes` | 256, 200000 |
| `max_text_bytes`, `max_links`, `max_sitemap_entries` | 1 MiB, 5000, 50000 |
| `cpu_time_ms` | 5000 |
| `render_min_text_chars` | 200 (below this, `auto` sources render) |

### `[kinds]`

`kinds_toml = "kinds.toml"`, `schemas_dir = "schemas"`, `prompts_dir = "prompts"`.

### `[llm]`

| Field | Default | Meaning |
|---|---|---|
| `enabled` | **false** | `VC_LLM_ENABLED` |
| `forced_off` | none | not a setting: the reason the LLM was forced off at load (see Startup checks) |
| `model` | `gemini-3.1-flash-lite` | `VC_LLM_MODEL` |
| `embedding_model`, `embedding_dim`, `embedding_location` | `text-embedding-005`, 768, `us-central1` | |
| `gcp_project`, `location` | `voyager-prod-ohzkl`, `global` | |
| `daily_usd`, `monthly_usd` | 1.50, 30.0 | caps ($30/month until batch prediction is wired or a commitment is bought) |
| `project_cap_usd` | none | `VC_PROJECT_CAP_USD`, required when enabled |
| `shares` | extraction 0.70, embedding 0.08, triage 0.08, auditor 0.07, scout 0.04, place 0.03 | |
| `max_output_tokens`, `max_input_tokens`, `max_chunks` | 400, 1200, 3 | |
| `chars_per_token` | 3.5 | for estimates |
| `breaker_h`, `stale_settle_h`, `batch_timeout_h` | 6, 1, 72 | breaker hours; open reservations older than this are settled as spent (hourly sweep); batch job timeout |
| `request_timeout_s` | 60 | |
| `input_usd_per_m`, `output_usd_per_m`, `cached_input_usd_per_m` | 0.25, 1.50, 0.025 | list prices |
| `batch_discount`, `embedding_usd_per_1k_chars` | 0.5, 0.000025 | |
| `batch_bucket`, `batch_prefix` | `voyager-crawler-ops`, `llm-batch/` | |
| `api_endpoint` | derived from location | tests point it at a mock |

### `[extract]`

| Field | Default | Meaning |
|---|---|---|
| `places_file`, `countries_file`, `admin1_file` | `data/*.tsv.gz` | GeoNames export |
| `repair_attempts` | 1 | |
| `excerpt_chars` | 300 | at most 300 |
| `min_quality` | 0.3 | |
| `canary_records` | 200 | |
| `max_records_per_call` | 20 | |
| `est_input_usd_per_mtok`, `est_output_usd_per_mtok` | 0.25, 1.50 | |
| `place_min_score`, `place_ambiguity_margin` | 0.5, 0.15 | |

### `[sources]`

| Field | Default |
|---|---|
| `dir` | `sources` |
| `dump_window_utc` | 02:00–05:00 |
| `dump_max_bytes` | 2 GiB per adapter run |
| `gtfs_max_bytes` | 600 MiB per GTFS feed (`gtfs_static`): larger feeds are skipped with a report |
| `dump_extend_every_s` | 60 (extend the lease while streaming) |

### `[queue]`

| Field | Default |
|---|---|
| `lease_s` | 300 |
| `max_attempts` | 8 (then dead-letter) |
| `backoff_base_s`, `backoff_max_s` | 30, 21600 |
| `retain_done_days` | 7 |

### `[index]`

| Field | Default |
|---|---|
| `hnsw_m`, `hnsw_ef_construction`, `hnsw_ef_search` | 16, 64, 40 |
| `rrf_k` | 60 |
| `freshness_half_life_days` | 90 |
| `merge_min_trigram`, `merge_max_distance_m` | 0.8, 300 |
| `stale_cadence_factor` | 3 |

### `[api]`

| Field | Default |
|---|---|
| `bind` | `0.0.0.0:8443` (internal IP only in production) |
| `metrics_bind` | `127.0.0.1:9100` |
| `tls_cert_secret`, `tls_key_secret` | `crawler-api-tls-cert`, `crawler-api-tls-key` (used only when the files `/run/secrets/api_tls_cert` and `api_tls_key` are missing or unusable) |
| `rate_per_min` | 600 per client |
| `max_page` | 100 |
| `excerpt_chars` | 300 |
| `request_timeout_s` | 10 |

### `[obs]`

| Field | Default |
|---|---|
| `log_format` | `json` (or `pretty`) |
| `log_level` | `info` |
| `heartbeat_s` | 60 |
| `progress_window_s` | 600 (health fails without progress for this long while work is due) |
| `metrics_addr` | `0.0.0.0:9464` (`/metrics` and `/healthz`) |

### `[secrets]`

| Field | Default |
|---|---|
| `backend` | `secret_manager` (or `env`) |
| `project` | `voyager-prod-ohzkl` |
| `env_prefix` | `VC_SECRET_` (`crawler-pg-password` → `VC_SECRET_CRAWLER_PG_PASSWORD`) |
| `endpoint` | `https://secretmanager.googleapis.com` |
| `timeout_s` | 10 |

### `[vcrawl]`

Settings of the `vcrawl` binary itself (its roles and admin commands).

| Field | Default | Meaning |
|---|---|---|
| `instance` | hostname | instance name; part of the role's spool subfolder name |
| `data_dir` | `/var/lib/voyager-crawler` | data disk |
| `fetcher_concurrency` | 16 | keys in flight per fetcher process |
| `processor_concurrency` | 4 | |
| `partitions_ahead_months` | 2 | monthly partitions created ahead |
| `contract_min_days` | 7 | `migrate --contract` needs the current image to have run this long |
| `scheduler_interval_s` | 300 | scheduler pass: sources to seeds, source limits, robots refresh |
| `robots_refresh_batch` | 100 | origins refreshed per scheduler pass |
| `fetcher_idle_ms` | 1000 | fetcher wait when no key is ready |
| `roller_tick_s` | 5 | segment roller: seal due files, upload, commit |
| `recrawl_min_s` | 3600 | shortest recrawl interval (the source cadence is the longest) |
| `recrawl_default_s` | 604800 (7 days) | recrawl interval without a source or with `on_demand` cadence |
| `health_timeout_s` | 5 | per-dependency timeout of `vcrawl health` |
| `shutdown_grace_s` | 20 | `vcrawl run`: time a SIGTERM leaves the roles to finish (fetches in flight end, the spool is flushed, leases are released) before the rest is aborted; inside the host's `docker compose stop -t 25` (shutdown.sh) and compose's 30 s `stop_grace_period` |
| `run_pool_max` | 0 | `vcrawl run`: size of the one shared pool; 0 = the roles' sum (scheduler 2, fetcher 2 + concurrency/2, processor 1 + concurrency, embedder 1, api 4, maintenance 3: 19 in production) |
| `source_allowlist` | unset | go-live ramp: a file of source ids (`#` comments; relative to `sources.dir`). The scheduler seeds only those, the fetcher fetches nothing for other sources' URLs (released for an hour). A fresh VM sets `VC_VCRAWL_SOURCE_ALLOWLIST=/app/sources/ramp-1.txt` once in `host.local.env`; unreadable = fail closed |

Code: [vc-config](Crate-vc-config).
