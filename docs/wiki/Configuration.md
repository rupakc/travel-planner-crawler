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
env secret backend, or `VC_CONFIG`) are ignored. Unknown **fields** inside a known section
are an error.

> **Status:** `deploy/compose.yaml` also passes deploy-contract variables such as
> `VC_DATABASE_PASSWORD_FILE`, `VC_API_LISTEN`, `VC_API_*_FILE`, `VC_SPOOL_DIR`,
> `VC_GCS_RAW_BUCKET` and `VC_LOG_FORMAT`. These do not map one to one onto the section
> fields below; reconciling them is part of the `vcrawl` wiring.

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
monthly cap. `Config::validate()` still rejects all of them.

## Sections

The User-Agent is a constant in code, not a setting.

### `[database]`

| Field | Default | Meaning |
|---|---|---|
| `url` | `postgres://vcrawl@localhost:5432/vcrawl` | connection URL without the password |
| `password_secret` | `crawler-pg-password` | Secret Manager name of the password |
| `max_connections` | 10 | pool size per process |
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
| `daily_raw_bytes_cap` | 3 GiB | across all keys |

### `[render]`

| Field | Default | Meaning |
|---|---|---|
| `enabled` | false | |
| `cdp_url` | `http://chromium:9222` | headless-shell on the internal network |
| `max_tabs` | 2 | |
| `budget_s`, `max_subrequests`, `max_bytes` | 90, 60, 20 MiB | per render |
| `network_idle_ms`, `subresource_gap_ms` | 1000, 250 | |
| `subresource_cache_ttl_h`, `subresource_cache_max_bytes` | 24, 64 MiB | in-memory cache |
| `cache_dir` | `/var/lib/voyager-crawler/render-cache` | reserved |
| `restart_after_renders` | 200 | leak containment |

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
| `model` | `gemini-3.1-flash-lite` | `VC_LLM_MODEL` |
| `embedding_model`, `embedding_dim`, `embedding_location` | `text-embedding-005`, 768, `us-central1` | |
| `gcp_project`, `location` | `voyager-prod-ohzkl`, `global` | |
| `daily_usd`, `monthly_usd` | 1.50, 30.0 | caps ($30/month until batch prediction is wired or a commitment is bought) |
| `project_cap_usd` | none | `VC_PROJECT_CAP_USD`, required when enabled |
| `shares` | extraction 0.70, embedding 0.08, triage 0.08, auditor 0.07, scout 0.04, place 0.03 | |
| `max_output_tokens`, `max_input_tokens`, `max_chunks` | 400, 1200, 3 | |
| `chars_per_token` | 3.5 | for estimates |
| `breaker_h`, `stale_settle_h`, `batch_timeout_h` | 6, 1, 72 | |
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
| `tls_cert_secret`, `tls_key_secret` | `crawler-api-tls-cert`, `crawler-api-tls-key` |
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

| Field | Default |
|---|---|
| `instance` | hostname |
| `data_dir` | `/var/lib/voyager-crawler` |
| `fetcher_concurrency` | 16 keys in flight per process |
| `processor_concurrency` | 4 |
| `partitions_ahead_months` | 2 |
| `contract_min_days` | 7 |

Code: [vc-config](Crate-vc-config).
