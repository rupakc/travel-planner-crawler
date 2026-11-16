# Crate: vc-config

**Purpose.** Load and validate the crawler's configuration: defaults, then a TOML file, then
`VC_*` environment variables (later wins). Every setting has a default. No secrets, only
secret **names**.

Path: `crates/vc-config`. Dependency: `figment`.

## Key types and functions

| Item | What it does |
|---|---|
| `Config` | one struct with a field per section: `database`, `policy`, `frontier`, `fetch`, `render`, `store`, `parse`, `kinds`, `llm`, `extract`, `sources`, `queue`, `index`, `api`, `obs`, `secrets`, `vcrawl` |
| `Config::load(path)` | builds the figment (path, else `VC_CONFIG`), extracts and validates |
| `Config::figment(path)` | the layered source, for tests |
| `Config::validate()` | the startup checks (see [Configuration](Configuration#startup-checks)) |
| `env_key_path(key)` | maps `LLM_DAILY_USD` to `llm.daily_usd`; `__` for deeper nesting; aliases such as `PROJECT_CAP_USD` to `llm.project_cap_usd`; unknown sections are ignored |
| `SECTIONS`, `ALIASES` | the known sections and aliases |
| `ConfigError` | `Load` (parse) or `Invalid` (a failed check) |

Each section is one file in `src/sections/<name>.rs`, owned by the crate it configures. All
structs use `#[serde(default, deny_unknown_fields)]`, so a typo in a field name is an error,
not a silent default.

## Notable checks

- `llm.enabled = true` needs `llm.project_cap_usd`, and `monthly_usd ≤ 0.5 × project_cap_usd`.
- LLM purpose shares must sum to 1.
- Excerpts ≤ 300 characters; ≤ 5 URLs per claim; ≤ 5 redirects; 1–2 render tabs; delays
  within 1 hour.

## Tests

Unit tests in `src/lib.rs`: `defaults_are_valid_and_llm_is_off`, `env_key_mapping`,
`toml_then_env_override`, `unknown_keys_are_rejected`,
`llm_enabled_requires_project_cap_and_half_rule`.

All settings with defaults: [Configuration](Configuration).
