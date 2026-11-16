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

At startup, the client makes a one-token "probe" call and fails loudly if the model is not
available.

## Off by default, and capped against the project

- `VC_LLM_ENABLED=false` is the default. Then every call returns `Disabled` without touching
  the network.
- To turn it on, the owner first raises the project's Gemini spend cap in the console, then
  sets `VC_PROJECT_CAP_USD` to that amount.
- The crawler **refuses to start** if `llm.enabled` is true and the monthly cap is more than
  **50 %** of `VC_PROJECT_CAP_USD`, or if `VC_PROJECT_CAP_USD` is missing.
- Every Vertex request carries `labels: {app: voyager-crawler, purpose: <purpose>}`, so
  billing can tell crawler spend apart. A separate ₹9,000/month budget alert watches that
  label.

## Caps

| Cap | Default | Notes |
|---|---|---|
| Monthly (`VC_LLM_MONTHLY_USD`) | $40 | split into the purpose shares above |
| Daily (`VC_LLM_DAILY_USD`) | $1.50 | upper bound of a **dynamic** daily cap |
| Dynamic daily cap | `min(daily_usd, 1.5 × remaining month cap ÷ remaining days)` | spreads the month evenly |

Hitting the daily cap is normal and not an alert. An alert fires when monthly spend passes
80 % of the cap.

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
  (crashed caller) is settled as spent.
- **Batch jobs** reserve their whole estimate at submit time and are not auto-settled.
- A test runs 100 concurrent reservations against a nearly full cap and checks the cap is
  never exceeded on either row.

## Breaker and cache

- A Vertex 403 or 429 that names billing or budget opens a **6-hour breaker**: no calls
  until it closes. A plain rate limit is not treated as a billing error.
- `llm_cache(model, prompt_hash, input_hash) → output`: identical inputs are never paid
  twice. Answers cut off at the token limit are not cached.

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

> **Status:** the client, embedder, ledger, breaker, cache and batch runner are built and
> tested against a mock Vertex. The LLM stays off in production until the owner raises the
> project cap (go-live step).

Code: [vc-llm](Crate-vc-llm).
