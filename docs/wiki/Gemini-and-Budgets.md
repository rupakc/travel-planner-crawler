# Gemini and Budgets

The crawler uses Gemini on Vertex AI for a few jobs that need a language model. It shares
the GCP project with Voyager, so **its spend must never threaten Voyager's own AI**. Every
model call therefore goes through a race-free budget ledger, and the LLM is **off by
default**.

## What uses the model

| Purpose | Share of the monthly cap | What it does |
|---|---|---|
| `extraction` | 70 % | turn page text into records ([Processing and Extraction](Processing-and-Extraction)) |
| `embedding` | 8 % | vectors for search |
| `triage` | 8 % | link triage ([Agentic Loops](Agentic-Loops)) |
| `auditor` | 7 % | quality auditor |
| `scout` | 4 % | source scout |
| `place` | 3 % | choose between two close place candidates |

## Models and prices

| Setting | Default |
|---|---|
| Generation model | `gemini-3.1-flash-lite` (global endpoint). `gemini-3.5-flash-lite`, Voyager's model, is one setting away. |
| List price used for estimates | $0.25 per 1M input tokens, $1.50 per 1M output tokens (thinking included), $0.025 per 1M cached input |
| Batch prediction | half the list price |
| Embedding model | `text-embedding-005`, 768 dimensions, us-central1, $0.000025 per 1,000 characters |

At startup, the processor and the scheduler make a one-token **probe** call:

| Probe result | What happens |
|---|---|
| OK | the LLM is used |
| model not found (404) | fatal: the role stops, because the setting is wrong |
| anything else (timeout, 5xx, quota, token or database error, open breaker) | the LLM is **paused**, not the role: until the shared breaker's time, or for 10 minutes in that process only. The role keeps running, so a Vertex outage never causes a crash loop or watchdog reboots. |

## Off by default, and capped against the project

- `VC_LLM_ENABLED=false` is the default. Then every call returns `Disabled` without touching
  the network.
- To turn it on, the owner first raises the project's Gemini spend cap in the console, then
  sets `VC_PROJECT_CAP_USD` to that amount.
- The crawler **keeps the LLM off** if `llm.enabled` is true and the monthly cap is more
  than **50 %** of `VC_PROJECT_CAP_USD`, or if `VC_PROJECT_CAP_USD` is missing or 0, or if
  the `[llm]` section is otherwise invalid. It does not refuse to start: it sets
  `llm.forced_off` to the reason, exports the `crawler_llm_config_invalid` gauge, and an
  alert fires. Every role keeps working without the LLM. The compose default
  (`VC_PROJECT_CAP_USD=0`) is therefore safe.
- Every Vertex request carries `labels: {app: voyager-crawler, purpose: <purpose>}`, so
  billing can tell crawler spend apart. A separate ₹9,000/month budget alert watches that
  label.

## Caps

| Cap | Default | Notes |
|---|---|---|
| Monthly (`VC_LLM_MONTHLY_USD`) | $30 | split into the purpose shares above; $30 until batch prediction is wired or a commitment is bought |
| Daily (`VC_LLM_DAILY_USD`) | $1.50 | upper bound of a **dynamic** daily cap |
| Dynamic daily cap | `min(daily_usd, 1.5 × remaining month cap ÷ remaining days)` | spreads the month evenly |

Hitting the daily cap is normal and not an alert. An alert fires when monthly spend passes
80 % of the cap (`crawler_llm_month_spend_ratio`, exported every minute by the maintenance
role). It also fires if that gauge stops arriving for 30 minutes.

Why $30: it is the default until batch prediction is wired (half price) or the 1-year VM
commitment is bought. See [Cost](Cost). Keep `VC_PROJECT_CAP_USD` at least twice any new
cap.

## The race-free ledger

Many processes may want to call the model at the same time. A naive "check, then spend"
could overspend. Instead, every call **reserves** first, in one SQL statement against both
the day row and the month row (and the purpose row):

```sql
WITH d AS (UPDATE llm_budget SET reserved = reserved + $e
           WHERE period = $day AND spent + reserved + $e <= cap RETURNING 1),
     m AS (UPDATE llm_budget SET reserved = reserved + $e
           WHERE period = $month AND spent + reserved + $e <= cap
             AND EXISTS (SELECT 1 FROM d) RETURNING 1)
```

The reservation counts only if both updates returned a row; otherwise the transaction rolls
back and **no call is made**.

- **Estimate** = input tokens (characters ÷ 3.5, on the safe side) + `maxOutputTokens` +
  the thinking allowance, at list price.
- **Settle** with the real usage after the call. A reservation still open after 1 hour
  (a crashed caller) is settled as spent by `settle_stale`, which the maintenance role
  runs every hour.
- A call that fails **after** reaching Vertex still charges its estimate (the request may
  have been billed). A call refused before any request (disabled, budget exhausted, breaker
  open, rate limited) charges nothing. The place-disambiguation call follows the same rule.
- **Batch jobs** reserve their whole estimate at submit time and are not auto-settled.
- A test runs 100 concurrent reservations against a nearly full cap and checks the cap is
  never exceeded on either row.

## Breaker and cache

- A Vertex 403 or 429 that names billing or budget opens a **6-hour breaker**: no calls
  until it closes. A plain rate limit is not treated as a billing error.
- The breaker is **shared and persisted**: one row in the `llm_breaker` table (migration
  1401). Its `open_until` time only moves forward. Every process re-reads it at most every
  30 s before calling Vertex. So one role's trip pauses every role, and a restart does not
  forget it.
- `llm_cache(model, prompt_hash, input_hash) → output`: identical inputs are never paid
  twice. Answers cut off at the token limit are not cached.
- Page text is wrapped in markers that carry a **nonce** (a per-request tag that page text
  cannot fake). The nonce is a BLAKE3 hash of the page text, so a recapture of an unchanged
  page builds the identical request and hits the cache. Page text that imitates a marker is
  defanged first, so the nonce does not need to be secret.

## Batch prediction

Large re-extractions use Vertex batch prediction (half price). The lifecycle is crash-safe:

| Step | What happens |
|---|---|
| Prepare | Write the input JSONL to `gs://voyager-crawler-ops/llm-batch/<id>/`; insert a `batch_jobs` row in state `prepared`. Each line's key is the work id. |
| Submit | Create the Vertex job with the row id as display name and label; the row moves to `submitted`. |
| Reconcile | On start and every 10 minutes, list Vertex jobs labelled `app=voyager-crawler` and adopt any whose row lacks a job name. A `prepared` row is never resubmitted without listing first. |
| Ingest | Map results back by key. Missing or failed keys go back to the queue. |
| Timeout | After 72 hours: cancel, mark `failed`, return every item to the queue. |

## Logging

Logs carry token counts and cost only, never prompt or page text.

> **Status:** every LLM user is wired into the roles: extraction and link triage in the
> processor, embeddings in the embedder, and the weekly source scout and quality auditor in
> maintenance ([Agentic Loops](Agentic-Loops)). They are tested against a mock Vertex. The
> batch runner is built and tested but **not wired**: all calls use the online price. The
> LLM stays off in production until the owner raises the project cap (go-live step).

Code: [vc-llm](Crate-vc-llm).
