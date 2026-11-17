# Crate: vc-config

**Purpose.** Load and validate the crawler's configuration: defaults, then a TOML file, then
`VC_*` environment variables (later wins). Every setting has a default. No secrets, only
secret **names**.

Path: `crates/vc-config`. Dependency: `figment`. Status: wired; every `vcrawl` role loads it
at startup.

## Key types and functions

| Item | What it does |
|---|---|
| `Config` | one struct with a field per section: `database`, `policy`, `frontier`, `fetch`, `render`, `store`, `parse`, `kinds`, `llm`, `extract`, `sources`, `queue`, `index`, `api`, `obs`, `secrets`, `vcrawl` |
| `Config::load(path)` | builds the figment (path, else `VC_CONFIG`), extracts and validates; an invalid `[llm]` section forces the LLM off instead of failing |
| `Config::force_llm_off_if_invalid()` | if `validate_llm` fails, sets `llm.enabled = false` and `llm.forced_off` to the reason (an alert gauge in vc-obs) |
| `Config::validate_llm()` | the `[llm]` rules alone |
| `Config::figment(path)` | the layered source, for tests |
| `Config::validate()` | the startup checks (see [Configuration](Configuration#startup-checks)) |
| `env_key_path(key)` | maps `LLM_DAILY_USD` to `llm.daily_usd`; `__` for deeper nesting; aliases such as `PROJECT_CAP_USD` to `llm.project_cap_usd`; unknown sections are ignored |
| `SECTIONS`, `ALIASES` | the known sections and aliases |
| `SECRET_FILE_VARS` | `VC_*_FILE` names that hold a secret file path, never a setting (today `VC_DATABASE_PASSWORD_FILE`) |
| `unmapped_env_keys(keys)` | the `VC_*` names no setting reads; `tests/deploy_env.rs` runs it on `deploy/compose.yaml` and fails on any |
| `ConfigError` | `Load` (parse) or `Invalid` (a failed check) |

Each section is one file in `src/sections/<name>.rs`, owned by the crate it configures. All
structs use `#[serde(default, deny_unknown_fields)]`, so a typo in a field name is an error,
not a silent default.

## Notable checks

- `[llm]`: caps `0 ≤ daily_usd ≤ monthly_usd`; purpose shares sum to 1; when enabled,
  `project_cap_usd > 0` and `monthly_usd ≤ 0.5 × project_cap_usd`. The monthly cap defaults
  to **$30**. A failure here does not stop startup: the LLM is forced off and
  `llm.forced_off` says why. Every other section's checks still refuse to start.
- Excerpts ≤ 300 characters; ≤ 5 URLs per claim; ≤ 5 redirects; 1–2 render tabs; delays
  within 1 hour.

## `[vcrawl]`

Binary-level settings for the roles:

| Key | Default | Meaning |
|---|---|---|
| `instance` | hostname | instance name in claim holders and heartbeats |
| `data_dir` | `/var/lib/voyager-crawler` | data directory on the data disk |
| `fetcher_concurrency` | 16 | fetcher workers (keys in flight) per process |
| `processor_concurrency` | 4 | processor workers |
| `partitions_ahead_months` | 2 | monthly partitions kept created ahead |
| `contract_min_days` | 7 | days the image must have run before `migrate --contract` |
| `scheduler_interval_s` | 300 | seconds between scheduler passes |
| `robots_refresh_batch` | 100 | origins per pass that get a robots.txt refresh |
| `fetcher_idle_ms` | 1000 | wait when no key is ready |
| `roller_tick_s` | 5 | seconds between segment-roller ticks |
| `recrawl_min_s` | 3600 | lower bound of the recrawl interval |
| `recrawl_default_s` | 604800 | recrawl interval without a source or with `on_demand` cadence |
| `health_timeout_s` | 5 | timeout of each `vcrawl health` check |

## Tests

`tests/deploy_env.rs`: every `VC_*` name in `deploy/compose.yaml` is a real setting.

Unit tests in `src/lib.rs`: `defaults_are_valid_and_llm_is_off`, `env_key_mapping`,
`toml_then_env_override`, `unknown_keys_are_rejected`,
`llm_enabled_requires_project_cap_and_half_rule`, `invalid_llm_config_forces_llm_off`.

All settings with defaults: [Configuration](Configuration).
