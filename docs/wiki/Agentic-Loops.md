# Agentic Loops

"Agentic" here means small jobs where Gemini helps the crawler decide what to do next. They
are useful but optional, so they come **after** the core crawler is live, and each has its
own slice of the LLM budget.

> **Status:** designed, not built (plan Task 17, phase P2b). The pieces they need already
> exist: the `triage` and `audit` work kinds in the queue, the `source_candidates` and
> `audits` tables, and the `triage`, `auditor` and `scout` budget shares. Nothing on this
> page runs today.

## Link triage

| | |
|---|---|
| Goal | Decide whether an uncertain link is worth crawling, and for which kind. |
| Input | Batches of 100 links: URL, anchor text and a little context. |
| When | Only for in-scope links that the cheap rules (include/exclude patterns, depth, sitemaps) leave uncertain. |
| Output | relevant or not, and a likely kind. |
| Cost control | Results are cached per URL pattern, so similar links are not asked about again. Budget share: 8 %. |
| Planned tests | `triage_cached_per_pattern` |

## Source scout (propose only)

| | |
|---|---|
| Goal | Find new sources for places and kinds with poor coverage. |
| When | Weekly, for under-covered (destination, kind) cells. |
| How | Gemini proposes sites. Automatic checks then run: robots.txt for `VoyagerBot`, the denylist including brand domains, a keyword scan of the site's terms page, feed and sitemap presence, licence hints. |
| Output | A **proposal** in `source_candidates` and `docs/proposals/<date>.md`. |
| Human step | Only `vcrawl admin source approve <id>` (a person) turns a proposal into a `sources/*.yaml` pull request. |
| Safety | Nothing is crawled before approval except robots.txt and the terms page, each under a normal permit. |
| Budget share | 4 % |
| Planned tests | `scout_never_crawls_before_approval`, `scout_rejects_denylisted_brand_domain` |

## Quality auditor

| | |
|---|---|
| Goal | Catch extraction mistakes. |
| When | Weekly. |
| How | Sample 100 record versions; Gemini checks each against the stored text of its capture. |
| Output | Rows in `audits` and a per-source quality score. The score feeds search ranking. An alert fires only when a source drops below a threshold. |
| Budget share | 7 % |
| Planned tests | `auditor_lowers_quality_score` |

## Rules they share

- Every call reserves through the ledger first, like extraction ([Gemini and Budgets](Gemini-and-Budgets)).
- Every request they cause to a website needs a permit, like any other crawl request.
- They never change sources on their own; people approve.

Code (planned): `crates/vcrawl/src/agentic/`, prompts in `prompts/agentic/`.
