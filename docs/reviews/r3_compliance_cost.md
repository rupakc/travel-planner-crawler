# R3: compliance, cost and operability review of the crawler design

## Blockers

**1. [blocker] The shared Vertex cap can switch off Voyager's AI.** Today's project Gemini cap is ₹1,800 (about $20). The crawler's own cap is $45, so on its own it can trip `ai_limit` for the app until the 1st. The §7 "label-filtered" crawler budget sees no Gemini spend unless every request and batch job sets `labels`, and the spec never says it does.
*Change:* Ship `VC_LLM_ENABLED=false`. Turn it on only after the runbook confirms the cap was raised to at least crawler cap + 2× the app's peak month. Add a required `VC_PROJECT_SPEND_CAP_USD`, and refuse LLM work unless the crawler's cap is ≤ 50% of it. Stop crawler LLM work at 80% of its own monthly cap. Reserve each batch on `max_output_tokens` plus thinking at submit time, with thinking set to minimal. Label every Vertex call `app=voyager-crawler`. Reopen the separate project (gcp.md §8), the only real isolation.

**2. [blocker] Opt-outs are mapped too narrowly, and nothing can be deleted.** `ai-input=no` and TDM reservations only skip extraction. The full text is still embedded, and §13 puts it into agent prompts, which is exactly what `ai-input` covers. Under DSM Art. 4, a reservation also removes the exception for the stored copy itself. The 365-day retention policy plus "never refetch" means removal and erasure requests can't be honoured.
*Change:* Add a signal → allowed-use matrix in `vc-policy`. A reservation or `ai-input=no` → metadata + link only: no embeddings, no grounding, raw kept as a hash only. Treat a robots.txt that blocks the major AI tokens but allows `*` as a reservation too. Drop the retention policy in v1 (versioning plus soft delete is enough). Add `vcrawl admin purge --host`, which rewrites segments or crypto-shreds a per-host key. Publish a 7-day opt-out SLA.

**3. [blocker] The dump sources don't fit the design.** The Wikidata JSON dump (about 140 GB), enwiki (about 25 GB) and Geofabrik extracts are all monthly. Against them stand G2's raw-first rule, a 10 MB body cap, a 100 GB disk shared with Postgres, and a locked bucket. Result: a full disk and a dead Postgres.
*Change:* Exempt public, versioned dumps from raw copying. Write a WARC `metadata` record (URL, upstream md5/sha1, date) and store only the filtered output. Process dumps as a stream and never spool the whole file. Drop the Wikidata dump in favour of the existing SPARQL seed. Fetch Geofabrik extracts only for the countries Voyager needs. Add a daily raw-bytes cap (5 GiB) that pauses fetching and alerts.

## Majors

**4. [major] The crawl volume is never stated.** GCS is priced at 5M pages/month, Gemini at 140k calls, and the limits allow about 20M (8 req/s). From `seeds.yaml` (50 feeds at 2–12 h, about 25 mostly weekly or monthly sitemap sources, the rest dumps/APIs), real HTML volume is about 50–200k pages/month. The spec's extraction parameters (3 × 1,200 tokens in, 300 out per record) cost 2–4× what gcp.md assumed, and triage, scout, auditor, place fallback, repairs and re-extraction after prompt changes aren't budgeted at all. The ~$100 *ceiling* holds because of the caps. The yield doesn't: expect about 40–70k extractions a month. Non-LLM run-rate is about $55–65.
*Change:* Add a per-category table (pages/day, LLM calls/day, GiB/day) and size the limits from it. Give each ledger `purpose` and re-extraction campaign its own sub-budget.

**5. [major] Chromium subrequests bypass the Gate.** They skip robots, the 1-in-flight rule, budgets and DNS pinning, and can reach `169.254.169.254`.
*Change:* Route every subrequest through `Gate` with CDP `Fetch.requestPaused`, and block link-local addresses on the container network. Better still, defer the renderer (item 11).

**6. [major] Licences are recorded, not enforced.** If merged `entities.attrs` include OSM data, the table becomes an ODbL Derivative Database that Voyager uses publicly, which triggers share-alike. Summaries adapted from Wikivoyage are CC BY-SA. Several seeds are non-commercial (Meteostat, WHO, OpenSky).
*Change:* Keep OSM attributes in their own table, linked only by id (a Collective Database), and be ready to publish it as ODbL. Give each licence a `served_as` value (full/excerpt/facts/link) and a `commercial_ok` flag, both enforced by the API. Return the union of attributions per entity. Deny NC sources by default.

**7. [major] Forum threads are personal data under GDPR (Art. 6/14/17, possibly Art. 9) and add little value.** *Change:* Cut `forum_thread` from v1. If it's kept: drop usernames, attribute by post URL, keep raw forum data 30 days, add an Art. 14 notice to BOT.md, and support purge.

**8. [major] The scout can crawl sites whose terms forbid it.** A keyword check on `/terms` misses `/legal`, PDFs, other languages and JS-rendered terms. Probation starts before any human review, and Gemini can suggest ccTLD or sister domains of denylisted brands.
*Change:* The scout only proposes. Nothing beyond robots.txt is fetched until `vcrawl admin source approve` opens a reviewed PR. Match the denylist by organisation (brand → domain set). Unknown licences default to link-only.

**9. [major] Migrations and rollback conflict.** `deploy.sh` migrates and then may roll back to an image that predates the schema. There's no major-upgrade procedure and no restore drill.
*Change:* Adopt the N-1 expand/contract rule, with `lock_timeout`, `CREATE INDEX CONCURRENTLY`, and no rollback across a migration. Write runbooks for pg 17→18 (`pg_upgrade` plus pinned pgvector), a quarterly restore drill, and "rebuild from raw".

**10. [major] Alerts on the app's shared channel will be broken and noisy.** The firewall blocks the uptime check's prober IPs, so `/healthz` alerts forever. The "> 80% of daily cap" alert fires on most days by design ($1.10 expected against a $1.20 threshold). Every parked host raises an alert.
*Change:* Use a metric-absence alert plus the VM uptime metric. Alert on the monthly forecast reaching 90% or the breaker opening. Send parked hosts, audits and the scout to a daily digest. Prefix alerts `[crawler]`. Serve the API on the internal IP only, and keep Postgres off the host network.

**11. [major] Overbuilt for v1.** Defer the renderer/Chromium (nearly every allowed seed is a feed, sitemap, dump or API; this also closes item 5), Gemini link triage (patterns are enough), the scout, the daily auditor (sample weekly by hand), the change-rate estimator (use fixed cadence plus conditional GET), cross-source entity resolution, and hybrid vector search (FTS plus place/kind until §13 shows a need). Ship only the kinds that have wired sources.

## Minors

12. **[minor] Memory.** 8 GB is enough with compose `mem_limit`s: Postgres 3 GB (`shared_buffers` 1.5 GB), Chromium 1.5 GB, processor 1 GB, the other roles 0.5 GB, Ops Agent 0.3 GB, plus 2 GB of swap.
13. **[minor] Buy the 1-year CUD (irrevocable) after 8 weeks of observed shape** and after the item 11 cuts.
14. **[minor] Daily snapshots of a churning frontier cost more than $1.50.** Use weekly snapshots.
15. **[minor] The free logging (50 GiB) and egress (200 GiB) tiers are shared with the app.** Log per batch, not per fetch, add a crawler egress governor, and keep per-host labels out of metrics.
16. **[minor] BOT.md.** Add a role mailbox, since a GitHub-only contact excludes some owners. Publish the static IP as JSON and state the opt-out SLA.
17. **[minor] Write a verified checklist for the owner's manual steps** (spend cap, INR budget, batch quota, CUD, secrets, CODEOWNERS). Pin the model and alert on model 404s, since 2.5 FL was retired in Oct 2026.
