# Voyager Crawler — Design (v1)

Status: draft for review · 2026-11-15 · Owner: rupakc · Research: `docs/research/{rust_stack,sources,gcp}.md`, `docs/research/seeds.yaml`

## 1. Intent

Voyager (travel-planner: web, Android, iOS) currently gets its data from the model's knowledge, a few live API calls (SerpAPI, Serper, DuckDuckGo) and three ad-hoc fetchers. The owner wants **sovereignty over travel data**: a 24x7 crawler that collects, versions, curates and indexes the travel data behind every agent, plus travel blogs, forums and news, and serves it to Voyager's agents through a read API. Data quality decides the product's quality, so this is its foundation.

## 2. Goals, non-goals, anti-goals

**Goals**
- G1. Crawl and ingest, continuously and politely, the sources behind all 22 agents' domains plus blogs, forums and news (the 117 allowed seeds in `seeds.yaml`, growing over time).
- G2. Store every raw response in GCP **before** any processing. Raw data is immutable, content-addressed, versioned and replayable.
- G3. Curate: dedupe, detect language, score quality, resolve each item to a place (GeoNames id / country / IATA city), and version every extracted entity with full provenance (source URL, capture id, extractor + schema + model version, licence, attribution).
- G4. Index: Postgres full-text + pgvector embeddings, queryable by place, kind and text, and served by a read API to the Voyager backend.
- G5. Adding a new domain is a YAML file (HTML/feed/sitemap sources) or one small Rust adapter (API/dump sources), plus a test fixture. Changing the core is not required.
- G6. Agentic where it pays: Gemini extracts typed records from unstructured pages, triages discovered links, proposes new sources (quarantined until the policy checks pass), and audits extraction quality. Always under a hard token budget.
- G7. Runs 24x7 on GCP in `voyager-prod-ohzkl` for about $100/month, self-healing, observable, with alerts.
- G8. Thoroughly tested: unit, property, integration (real Postgres, fake web, fake Gemini), end-to-end (docker compose "mini internet"), and a live smoke test in production.

**Non-goals**
- Live flight and hotel prices (they change by the minute and every source forbids scraping; they stay on SerpAPI/APIs in the main app).
- Training models on crawled content.
- Republishing full articles: blogs and news are stored for reference, retrieval and summarisation, served as headline + excerpt + link.
- Rewriting Voyager's agents. Integration is a separate, feature-flagged phase in the main repo (§13).

**Anti-goals (failure even if it "works")**
- Bot-detection evasion of any kind: stealth plugins, fingerprint spoofing, UA rotation, residential proxies, CAPTCHA solving, TLS impersonation. A block means **no**.
- Ignoring robots.txt, `Crawl-delay`, `X-Robots-Tag`, TDM reservations or `Content-Signal` lines.
- Crawling a site on the do-not-crawl list (Booking, Expedia, Tripadvisor, Skyscanner, Google Flights/Travel, Airbnb, Reddit, Kayak, GetYourGuide, Viator, FlyerTalk and the others in `seeds.yaml` with `allowed: false`).
- Any crawler spend that can switch off Voyager's own AI (shared Vertex spend cap): §10.4.
- Data without provenance or licence.

## 3. Constraints

- GCP project `voyager-prod-ohzkl`, us-east1, existing billing account (billed in INR). Owner decision 2026-11-15: same project, with the crawler's own hard Gemini cap in code, the project spend cap raised by the owner, and a separate crawler budget alert.
- Budget about $100/month (§10).
- Gemini only (Vertex AI, ADC, no API keys). Rust core. Headless Chromium where needed.
- Separate public repo `rupakc/travel-planner-crawler`, MIT. CI on GitHub Actions; CI never gets GCP credentials (same posture as the main repo: signed images in GHCR, pulled by the VM).
- The main app's two-way compatibility rule applies to the read API: additive changes only, versioned under `/v1`.

## 4. Approaches considered

| | A. Own Rust core, Postgres frontier, single VM (chosen) | B. Build on `spider` (Rust crawler framework) | C. Managed GCP pipeline (Cloud Run jobs + Pub/Sub + Cloud SQL + Dataflow) |
|---|---|---|---|
| Politeness control | Full: per-host state machine in our frontier | Partial: built around speed; default features include UA generation, fingerprinting, stealth | Full but spread over services |
| Ethics | Clean | `spider_fingerprint` is a non-optional dependency; we'd fight the defaults | Clean |
| Cost at our size | ~$97–100 | similar | $150–250+ (worker pools, NAT, Cloud SQL dedicated) |
| Scale-up path | Shard by host hash across N fetcher VMs; same code | Same | Native |
| Complexity | Medium, all ours, testable | Low to start, high to tame | High ops surface |

**Chosen: A.** B ships evasion as default features, which conflicts with our anti-goals. C costs 2x the budget for scale we don't need yet. A's design keeps C's scale-up path open: roles are separate processes that share only Postgres and GCS, so they can later move to separate VMs or Cloud Run without code changes.

**Raw storage** (decided): WARC/1.1 records, zstd, rolled into ~128 MB segment objects in GCS (`raw/` Standard → Coldline after 30 days), plus a Postgres `captures` row pointing at (segment, offset, length). One object per page was rejected: $25/month in writes and $100/month in Coldline transitions at 5M pages.

**Database** (decided): self-hosted Postgres 17 + pgvector on the VM's persistent SSD, with daily snapshots and a nightly `pg_dump` to GCS. Cloud SQL shared-core is too small for a frontier, and a usable Cloud SQL tier costs $38–62. Migrating to Cloud SQL is a connection-string change at 10x (§11).

**Infra as code** (decided): an idempotent `gcloud` bootstrap script with `--dry-run` and tests, same as the main repo. Terraform isn't installed or used anywhere in this project, and one toolchain is better than two.

## 5. Architecture

```
            seeds.yaml + sources/*.yaml + Rust adapters (API/dump)
                              │
   ┌──────────────┐   ┌───────▼────────┐    ┌────────────┐
   │  scheduler   │──▶│  frontier (PG) │◀───│ discoverer │◀── links from parsed pages
   │ sitemaps,    │   │ urls, hosts,   │    │ (heuristic │    (Gemini triage for
   │ feeds, recrawl│  │ robots cache   │    │ + Gemini)  │     uncertain batches)
   └──────────────┘   └───────┬────────┘    └────────────┘
                              │ claim host (SKIP LOCKED, ≤1 in flight/host)
                      ┌───────▼────────┐  needs JS?  ┌──────────────────┐
                      │   fetcher      │────────────▶│ renderer         │
                      │ reqwest, robots│             │ chromiumoxide +  │
                      │ politeness,SSRF│◀────────────│ headless-shell   │
                      └───────┬────────┘             └──────────────────┘
                              │ WARC record (request+response, render snapshot)
                      ┌───────▼────────┐
                      │  raw store     │ GCS segments + captures row (PG)   ← G2: stored BEFORE processing
                      └───────┬────────┘
                              │ capture_ready (PG queue)
                      ┌───────▼────────┐
                      │  processor     │ decode → main content → JSON-LD/microdata/feeds
                      │                │ → lang, simhash dedupe, quality, place resolution
                      │                │ → Gemini extraction (only changed + needed, batched)
                      └───────┬────────┘
                              │ entity versions
                      ┌───────▼────────┐        ┌──────────────┐
                      │ index (PG FTS  │◀───────│ embedder     │ (Vertex embeddings, batch)
                      │ + pgvector)    │        └──────────────┘
                      └───────┬────────┘
                      ┌───────▼────────┐
                      │ read API (axum)│──▶ Voyager backend (private, bearer token, allowlisted IP)
                      └────────────────┘
```

One binary, `vcrawl`, with roles: `scheduler`, `fetcher`, `renderer-pool` (the fetcher drives the Chromium sidecar directly), `processor`, `embedder`, `api`, `admin` (CLI), `migrate`. In production each role is a compose service, so roles scale independently.

### 5.1 Rust workspace

```
crates/
  vc-core        types: CanonUrl, HostKey, SourceSpec, Capture, Entity kinds, errors, Clock trait
  vc-config      figment config (file + env), validation; every setting has a default
  vc-policy      robots (texting_robots + RFC 9309 fetch rules), politeness state machine,
                 opt-out signals (X-Robots-Tag, meta robots, TDMRep, Content-Signal), denylist
  vc-frontier    Postgres frontier: enqueue/dedupe, host claim, schedule, recrawl estimator
  vc-fetch       HTTP fetch (reqwest/rustls), conditional GET, limits, SSRF guard, redirects
  vc-render      chromiumoxide client: network-idle, auto-scroll, allowlisted "expand" clicks,
                 resource blocking (images/fonts/trackers), DOM + text snapshot
  vc-store       WARC writer/reader, segment roller, GCS + local-FS backends (trait BlobStore)
  vc-parse       HTML (scraper/lol_html), main content (dom_smoothie), JSON-LD/microdata,
                 sitemap (quick-xml), feeds (feed-rs), lang (whatlang), simhash, links
  vc-llm         Gemini on Vertex: online generateContent + batch jobs, responseSchema, token
                 ledger + hard daily cap, retries; trait LlmClient with a fake for tests
  vc-extract     per-kind extraction schemas (JSON Schema, versioned) + prompts; structured-data-
                 first, LLM second; validation; place resolution
  vc-sources     SourceSpec loader (sources/*.yaml), adapter trait + built-in adapters
                 (wikivoyage dump, wikidata sparql, ecb fx, fcdo api, state-dept api, ...)
  vc-index       entity/version tables, FTS, pgvector, hybrid query
  vc-api         axum read API (/v1), auth, OpenAPI
  vc-obs         tracing JSON logs, metrics (Prometheus text), health
  vcrawl         binary: roles + admin CLI
testkit/         fake-web server (scriptable sites: robots, 429, redirects, sitemaps, SPA pages),
                 fake Gemini (Vertex wire format), Postgres testcontainer helpers, fixtures
sources/         one YAML per source (generated first from docs/research/seeds.yaml)
schemas/         extraction JSON Schemas per entity kind, versioned (place.v1.json, ...)
deploy/          compose.yaml, VM host kit (deploy.sh pull-verify-swap, systemd), bootstrap.sh
.claude/         agents, skills, settings; CLAUDE.md; docs/
```

### 5.2 Politeness and compliance (the core contract)

Every outbound request goes through `vc-policy::Gate`. No code path can bypass it: `vc-fetch` and `vc-render` take a `GatePermit` that only `Gate` can construct.

- **Identity.** `User-Agent: VoyagerBot/<ver> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)`. One fixed UA for both HTTP and Chromium (the render UA is overridden to the same string). `BOT.md` explains the bot and how to opt out, and gives a contact (a GitHub issue, so no personal email is exposed).
- **robots.txt (RFC 9309).**
  - Fetch per origin and cache for 24 h; parse the first 500 KiB.
  - 2xx → parse. Other 4xx → allow all. 5xx or network error → disallow all for that origin, retry with backoff, and after 30 days of failures keep the last good copy.
  - **401/403 on robots.txt → treat the whole host as do-not-crawl** (stricter than the RFC, by policy).
  - Rules match our product token `VoyagerBot`, falling back to `*`. Every redirect hop gets its own robots check.
- **Rate.**
  - At most 1 in-flight request per host (registrable domain via the PSL; also per IP for shared hosting).
  - Delay = max(robots Crawl-delay, source `min_delay` (default 5 s), 10 × last response time, adaptive penalty), capped at 1 h.
  - Per-host daily request budget (default 5,000; sources can only lower it).
  - Global fetch rate cap (default 8 req/s across all hosts).
- **Back-off.**
  - 429/503: honour `Retry-After`, else exponential backoff (×2, max 24 h).
  - 403, 401 or a challenge page (detected by status + markers such as Cloudflare/Akamai/DataDome interstitials, `cf-mitigated`, captcha forms) → **park the host** for 24 h, doubling up to 30 days, and alert. Never retry with a browser or a different identity.
  - 5 consecutive 5xx → park 1 h.
- **Opt-out signals** (stored on the capture, enforced downstream):
  - `noindex` → don't index.
  - `noarchive` → keep the raw copy for change detection only and never serve it.
  - TDM reservation (`tdm-reservation: 1`, TDMRep) or `Content-Signal: ai-input=no` → skip LLM extraction (structured data still allowed).
  - `ai-train=no` → never used for training (we don't train anyway; recorded).
- **Denylist.** A `policy/denylist.yaml` (from the research) is checked at enqueue and at fetch. It holds hosts plus a reason, and changing it requires review (a CODEOWNERS entry).
- **Safety.** An SSRF guard (no private, loopback or link-local IPs; DNS resolved once and pinned). Max body 10 MB (5 MB HTML). Content-type allowlist. Redirect limit 5. Decompression-bomb guard. Timeouts 30 s (render 45 s).
- **Content scope.** Blogs and news: the API serves only title, excerpt (≤ 300 chars), link and our own derived facts. The full text is used only for extraction and embeddings.

### 5.3 Frontier and scheduling

Postgres tables (simplified):
- `hosts(host PK, ip, next_fetch_at, crawl_delay_ms, robots_state, robots_fetched_at, parked_until, park_reason, consecutive_errors, daily_budget, used_today, ewma_latency_ms)`
- `urls(url_id PK = xxh3(canon), url, host, source_id, kind_hint, priority, state, depth, discovered_from, next_fetch_at, last_capture_id, etag, last_modified, content_hash, change_count, fetch_count, est_change_interval)`, with indexes on `(host, next_fetch_at)` and on `state`.
- A **claim** is a single transaction: pick a ready host with `next_fetch_at ≤ now()`, not parked and under budget, `FOR UPDATE SKIP LOCKED`; take its best-priority due URL; set the host's `next_fetch_at = now + lease`. After the fetch, set the real next time.
- **Recrawl:** a per-URL change-interval estimate (Cho & Garcia-Molina estimator over observed changes, bounded by the source cadence's min/max). Conditional GET makes unchanged pages cheap (a 304 still writes a capture metadata row, but no WARC body).
- **Discovery:** sitemaps (index + gz), feeds, and in-scope links. Each source defines `include`/`exclude` URL patterns and a max depth. Out-of-scope links are dropped. In-scope links with low heuristic relevance go to Gemini triage in batches of 100 URLs + anchor text (cheap tokens).
- **Priorities:** freshness need (feeds/news > advisories > events > places) × source weight × depth decay.

### 5.4 Rendering

`render: never | auto | always` per source. In `auto` mode the processor flags a capture as needing a render when there's little text (< 500 chars of main content), when SPA markers are present (an empty `#root`/`#__next`, `noscript` "enable JavaScript"), or when the source's expected selectors are missing. The URL is then re-queued with `render=true`; robots and politeness still apply, and Chromium's sub-requests are limited to the same registrable domain plus allowlisted CDNs. The render saves the final DOM HTML as a WARC `resource` record (the original HTTP exchange is also kept). One Chromium pool (`chromedp/headless-shell` pinned) with max 2 tabs. The image stays pinned, and dependabot proposes updates.

### 5.5 Raw store and versioning

- **WARC/1.1** `request` + `response` (+ `resource` for render, + `metadata` with policy signals), zstd per record, appended into a segment file. Segments close at 128 MB or 10 minutes and are uploaded to `gs://voyager-crawler-raw/segments/YYYY/MM/DD/<role>-<host>-<seq>.warc.zst` with a `.cdxj` index. A segment counts as stored only after GCS confirms the upload and the crc32c matches. Only then are the `captures` rows (segment, offset, length, sha256 of the payload) committed and the processor allowed to see them. G2 is enforced by this ordering.
- **Local spool** on disk before upload; replayed on restart, so a crash loses nothing that was fetched and acknowledged.
- **Bucket:** object versioning on, soft delete 7 days, Standard → Coldline after 30 days, no deletes for 365 days (retention policy, not locked).
- **Dedupe:** if a payload's sha256 equals the URL's previous capture, the capture row points at the previous record (a WARC `revisit` record), so no duplicate body is stored.
- **Entity versioning:**
  - `documents(doc_id, url_id, kind, current_version)`; `document_versions(doc_id, version, capture_id, content_hash, extracted jsonb, schema_id, schema_version, extractor_version, model_id, prompt_hash, quality, lang, place_ids, licence, attribution, created_at, superseded_at)`.
  - A new version is written only when the extracted payload changes (canonical-JSON hash).
  - Re-extraction after a schema or prompt change creates new versions linked to the same capture. Raw data is never refetched for that.
- **Entities** (canonical, cross-source): `entities(entity_id, kind, name, place_id, geo, attrs jsonb, confidence, first_seen, last_seen, status)` with `entity_sources(entity_id, doc_id, version)`. Entity resolution: same kind + same place + name similarity (trigram + normalised) + geo distance.
- **Curation states:** `active`, `stale` (not seen within 3× its cadence), `gone` (404/410 twice), `tombstoned` (opt-out or denylist; the content is no longer served).

### 5.6 Processing and agentic extraction

Order, cheapest first:
1. Decode and run main-content extraction.
2. JSON-LD/microdata (schema.org Event, Place, TouristAttraction, LodgingBusiness, Article, FAQ) → typed records with no LLM call.
3. Source-specific parsers (feeds, dumps, APIs) → typed records.
4. **Gemini extraction**, only if (a) the page's content hash changed, (b) the source spec asks for an LLM kind, (c) the opt-out signals allow it, and (d) the token ledger has room. The input is trimmed main content (≤ 1,200 tokens, chunked with overlap for long pages; at most 3 chunks). The output follows the kind's `responseSchema` (≤ 300 output tokens per record, multiple records per page allowed).
5. Validate against the JSON Schema; reject or repair once; record the confidence.
6. Place resolution: Voyager's place data (`airports.db`'s GeoNames subset, shipped as a data file) plus Gemini fallback for ambiguous names; country always paired with city.

**Entity kinds v1:** `place`, `attraction`, `activity`, `walking_tour`, `event`, `hotel` (descriptive facts only, no prices), `visa_rule`, `advisory`, `sim_offer`, `transport_option`, `emergency_numbers`, `fx_rate`, `weather_normal`, `tip`, `packing_hint`, `article` (blog/news), `forum_thread`, `airport`, `route`.

**Agentic loops, all budgeted:**
- **Link triage:** batches of URL + anchor + context → relevant y/n + kind. Cached per URL pattern.
- **Source scout:** weekly, per under-covered (destination, kind) cell in the index, Gemini proposes candidate sites. Candidates go to `source_candidates` and pass automated checks (robots allows VoyagerBot, not denylisted, no ToS keyword hits on /terms, feed or sitemap present). They are then crawled on probation (max 50 pages, low rate) and promoted automatically only if extraction yield and quality pass, otherwise rejected. Every decision is logged and reviewable in the admin CLI.
- **Quality auditor:** daily, samples 50 fresh entity versions; Gemini checks them against the source text; failures lower the source's quality score and open a GitHub-issue-style row in `audits`.

**Batch vs online:** extraction jobs collect into Vertex **batch prediction** JSONL files in GCS (50% off), submitted every 2 h or at 5,000 requests. Online calls are only for priority items (advisories, feeds), within an online sub-budget.

### 5.7 Token budget (hard cap)

`llm_ledger(day, model, purpose, input_tokens, output_tokens, est_usd)`. Each call reserves its estimated cost up front, and the reservation is reconciled with actual usage. Over `LLM_DAILY_USD_CAP` (default 1.50) → LLM steps are skipped and items wait until the next day, with priority preserved. A separate `LLM_MONTHLY_USD_CAP` (default 45) is also enforced. Spend-cap errors from Vertex (403/429 naming billing) → a 6 h circuit breaker. The ledger is exported as a metric and an alert.

### 5.8 Index and read API

- **FTS:** `tsvector` on name + summary + attrs text, per-language config (simple fallback).
- **Vectors:** pgvector HNSW on entity summary and article chunk embeddings (Vertex `text-embedding-005`, 768-d, batch).
- **Hybrid query:** filter by place (exact place_id, then radius), kind and freshness; rank by reciprocal rank fusion of BM25-ish `ts_rank` and vector similarity, times quality and freshness.
- **API (`/v1`, JSON, OpenAPI at `/v1/openapi.json`):**
  - `GET /v1/entities?place=&kind=&q=&limit=&cursor=`
  - `GET /v1/entities/{id}` (with sources and provenance)
  - `GET /v1/entities/{id}/versions`
  - `GET /v1/search?q=&place=&kind=` (hybrid)
  - `GET /v1/articles?place=&q=` (excerpts only)
  - `GET /v1/places/resolve?name=&country=`
  - `GET /v1/sources`, `GET /v1/stats`, `GET /healthz`, `GET /readyz`, `GET /metrics` (internal)
- Every item carries `provenance {source_id, url, captured_at, licence, attribution}`.
- Auth: a bearer token (Secret Manager), plus a firewall that only allows the app VM's IP. Rate limited.

### 5.9 Extending to a new domain

1. Add `sources/<id>.yaml`: seeds, sitemaps, feeds, include/exclude patterns, kinds, render mode, cadence, licence, attribution, optional lower rate. `vcrawl admin source validate` checks robots and the denylist and dry-runs 3 URLs.
2. For API or dump sources: implement `trait SourceAdapter { fn plan(); async fn run(ctx) -> Stream<Record> }` in `vc-sources/adapters/`. Raw API responses are also written as WARC records (G2 applies to APIs too).
3. For a new entity kind: add `schemas/<kind>.v1.json` and a prompt in `vc-extract/prompts/`. The registry test fails if a kind lacks a schema, prompt or fixture.
4. Add fixtures under `testkit/fixtures/<id>/` and a golden test.

## 6. Configuration and roles in production

docker compose on one VM:
- `postgres` (17 + pgvector, data on the persistent SSD)
- `migrate` (one-shot)
- `scheduler`
- `fetcher` ×1 (async, many hosts concurrently; bounded by the global rate)
- `chromium` (headless-shell)
- `processor`
- `embedder`
- `api`
- Ops Agent on the host for logs and metrics

All settings come from env with defaults (`VC_*`), and secrets come from Secret Manager at start (the bearer token; no other secrets, since Vertex and GCS use the VM service account's ADC).

## 7. GCP footprint (all in `voyager-prod-ohzkl`, us-east1)

- **VM** `voyager-crawler`: e2-standard-2, Debian 12 + Docker, 20 GB boot disk + 100 GB pd-balanced data disk, service account `crawler-runtime`, Standard-tier static IP, Shielded VM, OS Login. On-demand for the first 4 weeks; then a 1-year commitment (owner decision, flagged in the runbook).
- **GCS:** `voyager-crawler-raw` (segments; versioning, lifecycle, retention) and `voyager-crawler-ops` (pg_dump backups 30 days, batch-prediction I/O 7 days).
- **IAM:**
  - `crawler-runtime`: storage.objectAdmin on those two buckets only; a custom role with Vertex predict + batchPredictionJobs create/get; logging.logWriter; monitoring.metricWriter; secretAccessor on the crawler's secrets.
  - The app VM's service account gets nothing new; it reaches the API over the network with the token.
- **Network:** firewall allows TCP 8443 to the crawler only from the app VM's IP (and IAP SSH). Egress is unrestricted outbound for crawling. No NAT.
- **Monitoring:** uptime check on `/healthz`; alert policies for crawler down, LLM ledger > 80% of the daily cap, parked-host spikes, a 0 fetch rate for 30 min, disk > 80%, and backups not uploaded in 26 h. Notifications go to the existing channel.
- **Budget:** a separate budget filtered to the crawler's labels/services, ₹8,800/month with 50/90/100% alerts. All resources carry the label `app=voyager-crawler`. The owner raises the project's Gemini spend cap (console only; runbook step).
- **Snapshots:** a daily snapshot schedule on the data disk, kept 7 days.

## 8. Deploy

- **CI (GitHub Actions):** fmt, clippy `-D warnings`, cargo test (unit + integration with Postgres service + fake web + fake Gemini), cargo-deny (licences/advisories), E2E (compose "mini internet"), docker build, Trivy, shellcheck, bootstrap dry-run tests.
- **On `main`:** build `ghcr.io/rupakc/voyager-crawler:<sha>`, cosign keyless sign, move `:prod`.
- **VM:** a systemd timer runs `deploy.sh` every 2 minutes:
  - resolves `:prod` to a digest and verifies the cosign signature (pinned identity: this repo's workflow);
  - pulls the image and runs `migrate`;
  - does a rolling `compose up -d` per role, waits for health, and rolls back to the previous digest on failure.
- Same pattern as the main app; no GCP credentials in CI.

## 9. Observability

- JSON logs via tracing (`role`, `host`, `url_id`, `source_id`, `capture_id`); no page content in logs.
- `/metrics`: fetches by status/host class, bytes, parked hosts, robots denials, queue depth by state, segment uploads, extraction yields, LLM tokens/USD, API latency.
- `vcrawl admin status` prints a dashboard. A `crawl_stats_daily` table drives `/v1/stats`.

## 10. Cost (USD/month, steady state)

| Item | Cost |
|---|---|
| e2-standard-2 (on-demand $48.92 for 4 weeks, then 1-yr commitment) | 30.82 |
| Disks (20 GB boot + 100 GB balanced) + snapshots | 13.50 |
| Static IP | 3.65 |
| GCS raw (grows; month 12) + ops bucket | ~10 |
| Gemini extraction (3.1 Flash-Lite batch, ~140k calls) + triage | ≤ 33 (hard cap 45) |
| Embeddings (text-embedding-005 batch) | ~5 |
| Egress, ops | ~3 |
| **Total** | **~$97–100** (≈ $115 in the first month on-demand) |

**10.4 Protection for Voyager's AI.** The in-code daily/monthly LLM caps are the primary guard. The project's Gemini spend cap is raised by the owner so that the crawler's capped maximum (45) plus Voyager's own usage stays under it, plus a dedicated crawler budget alert. `LLM_MODEL` defaults to `gemini-3.1-flash-lite` for cost; `gemini-3.5-flash-lite` is one setting away.

## 11. Scale-up path (10x)

- Fetchers shard by `hash(host) % N` on separate Spot VMs, each with its own IP, so one host is still only ever touched by one shard.
- Postgres moves to Cloud SQL or a bigger VM (connection string only).
- The capture queue moves from Postgres to Pub/Sub behind the existing `trait CaptureQueue`.
- Raw data tiers Standard → Coldline → Archive.
- Gemini becomes the main cost; the caps are configuration.

## 12. Testing strategy

- **Unit + property (proptest):**
  - URL canonicalisation idempotence;
  - robots against Google's robots test corpus;
  - the politeness state machine: never more than 1 in flight per host, delay ≥ crawl-delay, park on challenge, under a simulated clock;
  - WARC round-trip; simhash; schema validation.
- **Integration (testcontainers Postgres + `testkit` fake web + fake Gemini):** frontier claim fairness and SKIP LOCKED concurrency; conditional GET; 429/Retry-After; robots 401/403 → host blocked; redirect to a disallowed path; sitemap-index discovery; segment upload ordering (no capture is visible before its upload is acknowledged); the token-cap stop; batch-job lifecycle; API contract snapshots (insta).
- **E2E (docker compose in CI):** a "mini internet" of 6 fake sites (a blog with RSS, an SPA attractions site, an events site with JSON-LD, a hostile site that returns a challenge, a slow site with Crawl-delay, a visa page). Run the whole stack for N minutes and assert: no politeness violations (the fake web logs every request with timestamps), every fetched response present in the fake GCS (fake-gcs-server) before any entity, entities and versions created, re-crawl after a content change creates a new version, API answers.
- **Live smoke (manual + scheduled on the VM):** 3 allowed real sources, low rate; check that `/v1/stats` grows.
- **Coverage gate:** the core crates (policy, frontier, store) ≥ 85% lines.

## 13. Voyager integration (main repo, later phase, feature-flagged)

`CRAWLER_API_URL` + `CRAWLER_API_TOKEN` settings; `backend/app/services/crawler_client.py`. Agents get "grounding context" (top-k entities/excerpts for destination + kind) prepended to their prompts when `CRAWLER_GROUNDING_ENABLED=true`. Two-way compatible: missing or failing crawler → agents behave exactly as today. Separate design and PR.

## 14. Delivery phases

- **P0 (foundation, sequential):** workspace skeleton, `vc-core` types and traits, DB migrations, config, CI, Claude scaffolding, testkit skeleton.
- **P1 (parallel, one agent per crate, worktrees):** vc-policy, vc-frontier, vc-fetch, vc-render, vc-store, vc-parse, vc-llm, vc-extract + schemas, vc-sources + adapters + YAML generation, vc-index, vc-api, testkit (fake web, fake Gemini), deploy kit + bootstrap.
- **P2:** `vcrawl` roles wiring, E2E suite, docs, review rounds.
- **P3:** provision GCP, first deploy, staged ramp (10 sources → all allowed), live verification, alerts.
- **P4:** Voyager backend integration (main repo).

## 15. Left to the builder

Exact crate versions (pinned in `Cargo.toml` from the research), table index tuning, prompt wording, fake-web site contents, the detailed metric names.
