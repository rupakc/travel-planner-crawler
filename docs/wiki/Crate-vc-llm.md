# Crate: vc-llm

**Purpose.** Gemini on Vertex AI for the crawler, always under the race-free budget. With
`llm.enabled = false` (the default) every call returns `LlmError::Disabled` without touching
the network.

Path: `crates/vc-llm`. Dependencies: `reqwest`, `google-cloud-auth` (ADC), `sqlx`. Migration
block `14xx` (tables are in `0006_llm.sql`; `1401_llm_breaker.sql` adds the shared breaker).

Status: wired into the processor, embedder, api, scheduler (startup probe) and maintenance
(agentic loops, hourly `settle_stale`) roles, all behind `VC_LLM_ENABLED` (off by default).
`VertexBatchRunner` is built and tested but no role submits batch jobs yet.

## Key items

| Item | What it does |
|---|---|
| `VertexLlm` | REST `generateContent` on the global endpoint with `responseSchema`, `maxOutputTokens`, `thinkingConfig`, temperature 0.1 and `labels {app: voyager-crawler, purpose}`; optional cache; `probe()` makes a one-token call at startup and returns a `ProbeOutcome` |
| `VertexEmbedder` | `text-embedding-005` (768 dimensions) via `predict`, up to 5 texts per request |
| `PgLedger` | implements `BudgetLedger`: the day + month (+ purpose) reservation in one statement, the dynamic daily cap, settle, and `settle_stale` (non-batch reservations older than `stale_settle_h`, default 1 h, settled as spent; maintenance runs it hourly) |
| `VertexBatchRunner` | batch prediction with JSONL in GCS: prepare, submit, reconcile, ingest, 72 h cancel |
| `VertexClient` | ADC tokens, error mapping, the 6 h `Breaker` on billing/budget errors; `with_shared_breaker(pool)` makes the breaker shared (every role uses it) |
| `Breaker` | `trip_shared` writes `llm_breaker.open_until` (one row, only moves forward); `sync` re-reads it at most every 30 s before a call, so one role's trip pauses every role and survives restarts; `pause` is a local, unpersisted pause |
| `LlmCache`, `CacheKey` | `llm_cache(model, prompt_hash, input_hash)` |
| `pricing` | list prices, `thinking_allowance`, `remaining_days` |
| `schema::to_vertex_schema` | converts a kind schema to the subset Vertex accepts |
| `labels(purpose)`, `APP_LABEL` | billing labels on every request |
| `is_billing_error(status, body)` | tells a billing 403/429 from a plain rate limit |

## Rules

- Estimate before calling: input characters ÷ 3.5 + `maxOutputTokens` + thinking allowance,
  at list price; the call happens only if the reservation succeeds.
- Settle with real usage; a non-batch reservation open for 1 hour is settled as spent.
- A billing or budget error opens a 6-hour breaker, shared through Postgres; a 404 is "model
  unavailable".
- Startup probe: only `ModelUnavailable` (404) is fatal. Any other failure (timeout, 5xx,
  quota, token or database error, open breaker) returns `ProbeOutcome::Paused { until }`: the
  breaker's time, or a local 10-minute pause. Roles start with the LLM paused instead of
  crash-looping.
- Answers cut at the token limit are not cached.
- Logs carry token counts and cost only, never prompt or page text.

## Config

`[llm]`: enabled, model, embedding model and location, project and location, caps, project
cap, purpose shares, token limits, chars per token, breaker and settle times, batch timeout,
prices, batch bucket and prefix, endpoint override. See [Configuration](Configuration).

## Tests

- `tests/ledger.rs` (Postgres): **`budget_race_never_exceeds_cap`** (100 concurrent
  reservations), `dynamic_daily_cap`, `purpose_share_enforced`,
  `stale_reservation_settled_as_spent`, `batch_reservation_not_auto_settled`,
  `settle_zero_releases_and_is_idempotent`.
- `tests/batch.rs` (wiremock + Postgres): `submit_creates_job_once`,
  `batch_crash_after_submit_reconciled_no_resubmit`, `batch_results_mapped_by_key_missing_nacked`,
  `batch_72h_cancel`, `vertex_failed_job_nacks_all`.
- `tests/vertex.rs` (wiremock): request and response shape, `billing_403_opens_breaker`,
  `rate_limit_is_not_billing_and_404_is_model_unavailable`, `probe_one_token_call`,
  `embed_request_shape_and_settle`, `labels_on_every_request`,
  `disabled_never_calls_network`, `cache_hit_no_call`, `max_tokens_answer_not_cached`,
  `breaker_is_shared_across_roles_and_restarts`,
  `probe_failure_other_than_missing_model_pauses`.
- `tests/schemas.rs`: `every_kind_schema_converts_to_vertex_subset`.

More: [Gemini and Budgets](Gemini-and-Budgets).
