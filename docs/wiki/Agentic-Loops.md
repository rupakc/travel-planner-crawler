# Agentic Loops

"Agentic" here means small jobs where Gemini helps the crawler decide what to do next. They
are useful but optional. Each has its own slice of the LLM budget, and none of them can
change what is crawled without a person.

> **Status:** built and wired (plan Task 17). Link triage runs in the `processor`. The
> source scout and the quality auditor run **weekly** in the `maintenance` role. While
> `llm.enabled` is false (the default) every loop is a clean no-op: no query, no
> reservation, no model call, no writes. Code: `crates/vcrawl/src/agentic/`; prompts:
> `prompts/agentic/*.md` (built into the binary).

## Link triage

| | |
|---|---|
| Goal | Decide whether an uncertain link is worth crawling, and for which kind. |
| Where | The `processor`, once per capture, only when `llm.enabled` is true. |
| Which links | In-scope page links of a source that has no explicit `include` patterns (in scope only because they share the seed host). Sitemap and feed entries, links on listing pages, and links an include pattern matched are certain and skip triage. |
| Input | Batches of up to 100 URL patterns, with anchor text and the page title as context. |
| Output | Relevant or not, and a likely kind. Unknown kinds are dropped. |
| Effect | `relevant: false` keeps the link out of the [frontier](Glossary). Undecided links (LLM off, budget refused, failed call, bad answer) are followed as before. A triage error never fails the capture. |
| Cost control | Verdicts are cached per URL pattern in `llm_cache` (ids, numbers and slugs generalised), so a pattern is never paid for twice, even after a restart. Budget share: 8 %. |
| Tests | `triage_cached_per_pattern`, `triage_batches_by_hundred_patterns`, `triage_pg_cache_survives_restart`, `data2_triage.rs` |

## Source scout (propose only)

| | |
|---|---|
| Goal | Find new sources for places and kinds with poor coverage. |
| When | Weekly, in `maintenance`. |
| Which cells | Destinations (`city, country`) with at least 20 live records, crossed with the city-scoped kinds (activity, attraction, event, hotel, tip, transport_option, walking_tour) that have fewer than 5. Cells proposed in the last 90 days are skipped. Busiest first, 10 cells per run. |
| How | Gemini suggests up to 8 sites per cell. Suggestions on the denylist (including brand domains under any public suffix, such as `expedia.*`) are stored as `rejected` and never proposed again. Hosts of existing sources are skipped. Checks that need the network (robots.txt for `VoyagerBot`, a keyword scan of the terms page, feed and sitemap presence) are recorded as `pending`. |
| Output | Rows in `source_candidates` and `<data_dir>/proposals/<date>.md`. |
| Human step | Review with `vcrawl admin source list --proposed`. Only `vcrawl admin source approve <id>` (a person) turns a proposal into a `sources/*.yaml` pull request. |
| Safety | The scout holds no frontier, queue or fetch client. It never writes `sources`, `urls` or `work_items`, and never fetches anything. |
| Budget share | 4 % |
| Tests | `scout_never_crawls_before_approval`, `scout_rejects_denylisted_brand_domain`, `scout_job_proposes_for_under_covered_cells` |

## Quality auditor

| | |
|---|---|
| Goal | Catch extraction mistakes. |
| When | Weekly, in `maintenance`. |
| How | Sample 100 current record versions. Gemini checks each against the stored page text (at most 12,000 characters), read from the [text tier](Glossary); if the tier has no row, the stored raw body is parsed again. |
| Output | Rows in `audits`. A failure lowers the record's quality. Each source gets a `quality_score`: its mean audit score over the last 90 days. The score feeds search ranking. Sources below 0.6 are reported for alerting. |
| Budget share | 7 % |
| Tests | `auditor_lowers_quality_score`, `audit_job_uses_the_text_tier` |

## Rules they share

- Every call reserves through the ledger first, then settles, like extraction
  ([Gemini and Budgets](Gemini-and-Budgets)). A refused reservation, or the spend
  [breaker](Glossary), stops the loop for this run without a call.
- Text from crawled pages is always passed as untrusted JSON data in the user message. The
  model has no tools.
- Every request they cause to a website needs a permit, like any other crawl request.
- They never change sources on their own; people approve.
