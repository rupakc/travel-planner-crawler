# Voyager Crawler — Design (v2)

Status: v2, revised after review round 1 (`docs/reviews/r1_*.md`) · 2026-11-15 · Owner: rupakc
Research: `docs/research/{rust_stack,sources,gcp}.md`, `docs/research/seeds.yaml`

Changes from v1 are listed in §16.

## 1. Intent

Voyager (travel-planner: web, Android, iOS) currently gets its data from the model's own knowledge, a few live API calls and three ad-hoc fetchers. The owner wants **sovereignty over travel data**: a 24x7 crawler that collects, versions, curates and indexes the data behind every agent, plus travel blogs, forums and news, and serves it to Voyager's agents through a read API. Data quality decides product quality, so this is the foundation.

## 2. Goals, non-goals, anti-goals

**Goals**
- G1. Continuously and politely ingest the sources for all of Voyager's agent domains plus blogs, forums and news: the 117 allowed seeds in `seeds.yaml`, growing over time. HTML, feeds, sitemaps, APIs and dumps.
- G2. **Raw first:** every fetched response (HTML, API, feed; for dumps see §5.10) is durably stored in GCP before anything derived from it becomes visible. Raw data is immutable, content-addressed, versioned and replayable.
- G3. Curate: dedupe, language, quality, place resolution, licence class, and versioned records with full provenance (source, URL, capture, extractor, schema, model and prompt version, licence, attribution).
- G4. Index with Postgres FTS + pgvector; serve a private read API to the Voyager backend.
- G5. Extensible without touching the core: new sources are YAML; new source types, media types, entity kinds and stores are trait implementations registered in one place.
- G6. Agentic where it pays: Gemini extraction from unstructured pages, link triage, a source scout (propose only), and a quality auditor. Always under a hard, race-free token budget.
- G7. 24x7 in `voyager-prod-ohzkl` for about $100/month: self-healing, observable, alerting without noise.
- G8. Tested at every level, including crash/failpoint tests, a politeness oracle, fuzzing and restore drills.

**Non-goals**
- Live flight and hotel prices; these stay on SerpAPI in the main app.
- Training models.
- Republishing articles (blogs and news are served as headline + ≤ 300-char excerpt + link + derived facts).
- Changing Voyager's agents in this repo (§13).

**Anti-goals (failure even if it "works")**
- **Bot-detection evasion** of any kind (stealth, fingerprint or UA spoofing, proxy rotation, CAPTCHA solving, TLS impersonation). Enforced in CI by cargo-deny bans on known evasion crates (`spider_fingerprint`, `ua_generator`, `chrome_stealth`, `wreq`, and the like).
- Ignoring robots.txt, Crawl-delay, X-Robots-Tag, meta robots, TDM reservations or Content-Signal.
- Crawling anything on the denylist (`policy/denylist.yaml`, from the research: Booking, Expedia, Tripadvisor, Skyscanner, Google Flights/Travel, Airbnb, Reddit, Kayak, GetYourGuide, Viator, FlyerTalk and others, including their brand domains).
- Crawler spend switching off Voyager's AI (§10.3).
- Data without provenance or licence, or used against its licence.
- Unpurgeable data: every item must be removable on an opt-out or erasure request.

## 3. Constraints

- GCP project `voyager-prod-ohzkl`, us-east1, `default` VPC (the app VM `voyager-vm` is at 10.142.0.2, service account `vm-runtime`). The billing account is in INR.
- **Owner decision 2026-11-15:** same project; the crawler has its own hard Gemini caps in code; the owner raises the project's Gemini spend cap in the console; a separate crawler budget alert.
- Budget about $100/month. Gemini only (Vertex, ADC). Rust. Headless Chromium for JavaScript-only pages.
- Separate public MIT repo `rupakc/travel-planner-crawler`; GitHub Actions CI with **no** GCP credentials; cosign-signed GHCR images pulled by the VM.
- The read API is additive-only under `/v1` (the main app's two-way compatibility rule).

## 4. Approaches considered

| | A. Own Rust core + Postgres frontier on one VM (**chosen**) | B. Build on `spider` | C. Managed (Cloud Run + Pub/Sub + Cloud SQL + Dataflow) |
|---|---|---|---|
| Politeness | Full, enforced in one DB-backed gate | Speed-first; evasion features on by default (`spider_fingerprint` non-optional) | Full, spread over services |
| Cost now | ~$95–100 | similar | $150–250+ |
| Scale path | Roles are separate processes behind traits → separate VMs / managed services | — | native |

**Chosen: A.** B conflicts with an anti-goal. C costs twice the budget for scale we don't have yet; A keeps the path to C open through the traits in §5.1.

Other decided choices:
- **Raw storage:** WARC/1.1 + zstd segments in GCS, indexed by CDXJ. Not one object per page, which costs about $125/month in operations at volume.
- **Database:** self-hosted Postgres 17 + pgvector on the VM SSD, with WAL archiving to GCS (wal-g). Cloud SQL is too small or too expensive here; switching later goes behind the same schema, and §11 says what else changes.
- **Infra as code:** an idempotent gcloud `bootstrap.sh` with `--dry-run` and tests, like the main repo (no Terraform in this project).

## 5. Architecture

```
 sources/*.yaml + adapters ──▶ scheduler ──▶ ┌──────────── Postgres ────────────┐
                                              │ origins · politeness_keys · urls │
 discovered links ──▶ triage ───────────────▶ │ url_state · claims (fenced)      │
                                              │ global rate tokens · llm_ledger  │
                                              └───────┬──────────────────────────┘
                                         claim (key, epoch, deadline)
                                              ┌───────▼────────┐
                                              │ fetcher        │── HTTP via vc-fetch (permit-only)
                                              │                │── JS pages: Chromium ──▶ vc-egress proxy
                                              └───────┬────────┘    (same permit; robots, SSRF, rate
                                                      │              for every sub-request)
                                       spool (fsync) ─▶ segment ─▶ GCS (crc32c ok)
                                                      │  then ONE tx: captures + url_state validators
                                              ┌───────▼────────┐
                                              │ processor      │ handler per media type → structured data
                                              │ (at-least-once,│ → main text (Standard store) → LLM
                                              │  idempotent)   │   extraction (ledger) → records/versions
                                              └───────┬────────┘
                                              embedder (ledger) → index → read API (internal IP only)
```

Single binary `vcrawl` with roles: `scheduler`, `fetcher`, `egress` (the proxy), `processor`, `embedder`, `api`, `maintenance` (partitions, retention, restore drill, stats), `admin` CLI, `migrate`.

### 5.1 Workspace and extension traits

```
crates/
  vc-core      ids (UrlKey = 128-bit blake3 of the canonical URL + the canonical URL stored, unique),
               OriginKey, PolitenessKey, CaptureId (ULID), RecordKey, Clock, error types;
               NO hard-coded entity kinds
  vc-config    figment (TOML + VC_* env), validation, defaults for every setting
  vc-policy    robots (texting_robots + RFC 9309 fetch rules), opt-out signal parser → UsePolicy,
               denylist, SSRF guard, challenge detector, licence classes
  vc-frontier  trait Frontier (claim/complete/enqueue/park) + Postgres impl; fenced leases;
               DB-backed global rate limit; recrawl estimator
  vc-fetch     reqwest fetch requiring a ClaimPermit; conditional GET, streaming limits, redirects
  vc-egress    forward proxy for Chromium: every sub-request checked (permit scope, robots,
               SSRF, per-key serialisation + delay, third-party allowlist)
  vc-render    chromiumoxide: network-idle, auto-scroll, allowlisted expand clicks, resource blocking
  vc-store     trait BlobStore (GCS, local FS, fake), WARC writer/reader, Spool, SegmentRoller, CDXJ
  vc-parse     trait ContentHandler per media type (html, feed, sitemap, json, csv; pdf later),
               main content (dom_smoothie), JSON-LD/microdata, lang, simhash, links
  vc-kinds     data-driven kind registry: schemas/<kind>.v<N>.json + prompt + policy (allowed source
               classes, verbatim-check fields); registry test
  vc-llm       trait LlmClient (Vertex online + batch, fake); trait Embedder; BudgetLedger
  vc-extract   pipeline: structured-first, LLM second; validation; verbatim/grounding checks; PlaceResolver
  vc-sources   SourceSpec loader + trait SourceAdapter (API/dump) + built-in adapters
  vc-queue     trait WorkQueue (capture_ready, extract, embed); Postgres impl (SKIP LOCKED); Pub/Sub later
  vc-index     records/entities/versions/observations/embeddings tables; hybrid query
  vc-api       axum /v1, auth (per-client tokens), OpenAPI, /v1/changes feed
  vc-obs       tracing JSON, Prometheus metrics, progress-based health, heartbeat
  vc-secrets   trait SecretSource (Secret Manager, env-for-tests)
  vcrawl       roles + admin CLI
testkit/       fake-web (scriptable sites + request log for the politeness oracle), fake-gemini (Vertex
               online + batch wire format), fake-gcs (fake-gcs-server), PG testcontainers, fixtures
sources/  schemas/  prompts/  policy/  deploy/  docs/  .claude/
```

### 5.2 Politeness and compliance (the core contract)

**One gate, in the database.** A network request to a crawl target is only possible with a `ClaimPermit`. Only `Frontier::claim` creates one, from a committed row in `claims(politeness_key, holder, epoch, deadline)`. That covers page fetches, robots.txt, sitemaps, feeds, API calls by adapters, `admin source validate` and scout checks. `vc-fetch` and `vc-egress` reject requests outside the permit's scope (its politeness key, plus allowlisted third-party sub-resource hosts for renders). The global fetch rate (default 4 req/s) is a token bucket row in Postgres, so it holds across processes.

- **Keys.** The **origin** (scheme + host + port) owns robots.txt. The **politeness key** (default: registrable domain via the PSL; a source may group several domains) owns rate, in-flight and budgets. No IP grouping by default; a source may opt in for known shared hosts. The shard (§11) is a leased column, not `hash % N`.
- **Identity.** `User-Agent: VoyagerBot/<ver> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)`, the same string for HTTP and Chromium. BOT.md lists the published egress IP, the robots token `VoyagerBot`, the opt-out paths (robots, a GitHub issue form) and the opt-out SLA (honoured within 24 h of the next robots fetch; purge on request within 7 days).
- **Leases and fencing.**
  - Each claim gets a hard deadline = sum of the worst-case budgets (robots 30 s + 5 hops × 30 s + render 60 s + body 60 s + margin), and a monotonically increasing epoch.
  - Every stage checks `now < deadline` against Postgres `now()` (no local clocks) and aborts when it isn't.
  - Completion is `UPDATE … WHERE epoch = $e`. A stale holder's completion is rejected, and its spool record is still kept but marked `orphan_epoch`, then replayed as a normal capture (data isn't lost; politeness isn't violated, because the request already happened within its own valid lease).
  - The next claim for a key is only possible after `max(deadline, last_request_end + delay)`.
- **robots.txt (RFC 9309).** Cached 24 h per origin; first 500 KiB.
  - Status rules: 2xx parse; 404/410 allow all; **401/403 or a robots.txt answered by a challenge page → the whole key is do-not-crawl** (by policy); 5xx or network error → disallow, retry with backoff, and after 30 days keep the last good copy.
  - Groups for the `VoyagerBot` token, otherwise `*`. A site that blocks the major AI crawlers (GPTBot, ClaudeBot, CCBot, Google-Extended) but allows `*` is treated as an AI reservation: crawl allowed, no LLM/embedding use (see UsePolicy).
  - Every redirect hop is checked; a **cross-key redirect is never followed inline**. It is enqueued as a new URL under its own key.
  - When robots newly disallows a path, the records derived from it are tombstoned.
- **Delay and budgets.**
  - Delay per key = max(robots Crawl-delay, source `min_delay` (default 5 s), 10 × EWMA response time, penalty), capped at 1 h.
  - At most one request in flight per key, including render sub-requests: the egress proxy serialises them.
  - Daily request budget per key (default 3,000; sources can only lower it) and a daily raw-bytes cap overall (default 3 GB).
- **Back-off.**
  - 429/503: honour Retry-After, else ×2 up to 24 h.
  - 401/403/challenge page: **park the key** for 24 h, doubling to 30 days. Never retry with a browser or another identity. Counted in metrics and in a daily digest, not as individual alerts.
  - 5 consecutive 5xx: park 1 h.
- **UsePolicy (signals → allowed uses), stored per capture and per derived record:**

  | Signal | store raw | index/serve | LLM extract | embed | serve to Voyager agents |
  |---|---|---|---|---|---|
  | none | ✓ | ✓ | ✓ | ✓ | ✓ |
  | `noindex` / X-Robots-Tag noindex | ✓ | ✗ | ✓ | ✗ | facts only |
  | `noarchive` | change detection only, purged after 30 d | ✓ | ✓ | ✓ | ✓ (no cached copy) |
  | TDM reservation (header/TDMRep) · `Content-Signal ai-input=no` · AI-bot block in robots | ✓ | ✓ (title + link) | ✗ | ✗ | link only |
  | `ai-train=no` | recorded; we never train | | | | |

- **Safety and content limits.**
  - The SSRF guard resolves DNS once per hop, pins the IP, and blocks loopback, RFC 1918, 169.254/16, 100.64/10, 0/8, fc00::/7, IPv4-mapped IPv6 and the metadata server; ports 80/443 only; decimal and octal IP forms are normalised.
  - Streaming byte counting after decompression, including nested `.gz` (HTML 5 MB, other 10 MB, dump streams per adapter).
  - No XML DTDs or entity expansion; DOM depth and node caps; a per-capture CPU time limit in processing; content-type allowlist; 5 redirects max.

### 5.3 Frontier

- `origins(origin PK, robots_state, robots_body_ref, robots_fetched_at, robots_expires_at)`
- `politeness_keys(key PK, next_allowed_at, delay_ms, ewma_latency_ms, parked_until, park_level, park_reason, budget_day, used_today, shard, enabled)`
- `claims(key PK, holder, epoch, deadline)`, plus an `epoch` sequence.
- `urls(url_key bytea PK [16 B], url text UNIQUE-by-hash, key, source_id, discovered_from, first_seen)`: identity, almost never updated. **Hash-partitioned** (16 parts).
- `url_state(url_key PK, key, state, priority, next_fetch_at, last_capture_id, etag, last_modified, payload_hash, change_count, fetch_count, est_interval_s)`, also hash-partitioned and `fillfactor = 70` so updates stay HOT. The claim uses a partial index `(key, next_fetch_at, priority) WHERE state = 'due'`.
- **Claim:** take a ready key with `FOR UPDATE SKIP LOCKED` (`next_allowed_at ≤ now()`, not parked, under budget), take a global rate token, insert or update the claim with a new epoch, and lease up to 20 due URLs of that key in one go (fetched sequentially with the delay; the deadline covers the batch).
- **Enqueue:** link inserts are batched (`INSERT … ON CONFLICT DO NOTHING`, 1k rows).
- **Recrawl:** the Cho & Garcia-Molina estimator, bounded by the source cadence. Conditional GET; a 304 writes a metadata-only capture.
- **Priority:** freshness class (feeds/news/advisories > events > places) × source weight × depth decay.
- **Vacuum and sizing:** autovacuum is tuned per table, and the sizing table (§10.2) is checked by a monthly maintenance report.

### 5.4 Rendering (Chromium, isolated)

- `render: never | auto | always` per source. In `auto`, the processor flags captures with little text or SPA markers, and those URLs are re-queued for rendering under a new claim.
- **Isolation:** Chromium (`chromedp/headless-shell`, pinned by digest) runs:
  - with its own sandbox enabled (seccomp profile, not `--no-sandbox`), non-root, read-only filesystem, memory limit 1.5 GB;
  - on a Docker network **with no route out**, so its only reachable peer is `vc-egress` (which blocks the metadata server and every private range). It has no access to Postgres or GCS;
  - with CDP exposed only on that internal network.
- Sub-requests go through `vc-egress` under the page's permit: robots-checked, serialised per key with a small delay (default 250 ms for sub-resources; the page-level delay still applies between pages). Images, fonts, media and known trackers are blocked. Third-party hosts are allowed only from `policy/subresource_allowlist.yaml` (CDNs for JS/CSS/JSON), which still checks SSRF and counts against the global rate limit.
- Max 2 tabs. Per-render budget: 60 s, 200 sub-requests, 20 MB.
- The final DOM is saved as a WARC `resource` record, and the HTTP exchanges as `request`/`response` records.

### 5.5 Raw store: crash-consistent ordering

1. The fetch completes, and a **spool record** (WARC record bytes + metadata: claim epoch, url_key, validators, UsePolicy) is appended to the local spool file and fsynced.
2. The `SegmentRoller` closes a segment at 128 MB or 10 minutes and uploads it as `gs://voyager-crawler-raw/segments/YYYY/MM/DD/<segment-ULID>.warc.zst` with an `ifGenerationMatch=0` precondition. That makes the upload idempotent: on retry, an identical existing object counts as success and a mismatching one is an alert. The `.cdxj` index is uploaded next to it.
3. After GCS confirms and the crc32c matches, **one transaction** inserts the `segments` row, the `captures` rows and the `url_state` updates (validators, payload hash, next_fetch_at), and enqueues `capture_ready` work.
4. The spool file is deleted only after that commit. On start, every spool and un-uploaded segment is replayed (steps 2–3 are idempotent by segment ULID).

Until step 3, the frontier still holds the **old** validators, so a crash can never produce a 304 for a body we don't have.

- **Backpressure:** spool over 2 GB, or uploads failing for more than 15 minutes → fetchers pause and an alert fires.
- **Dedupe:** a payload equal to the URL's last payload is written as a WARC `revisit` record pointing to the earlier record.
- **Bucket:** versioning on, soft delete 7 days, Standard → Coldline after 90 days, no retention lock (purges must be possible).
- **Extracted text tier:** each capture's main text and structured data are written to `gs://voyager-crawler-raw/text/YYYY/MM/<segment-ULID>.jsonl.zst` (Standard storage). Re-extraction reads these, not Coldline WARCs. Re-processing raw WARCs reads one whole segment at a time.
- **`captures` is range-partitioned by month.** `admin rebuild-captures` reconstructs captures and segments rows from the `.cdxj` files in GCS. This is the disaster-recovery path for rows newer than the last backup.

### 5.6 Records, versions, entities, observations

- **`records`** (`record_id`, `kind`, `source_id`, `url_key`, `record_key`): one per (url, record_key). A page can produce many records (e.g. 30 events). The `record_key` is stable: the JSON-LD `@id` or URL anchor, else a hash of (kind, normalised name, date/geo).
- **`record_versions`** (`record_id`, `version`, `capture_id`, `data` jsonb, `data_hash`, `kind_schema@ver`, `extractor_ver`, `model_id`, `prompt_hash`, `extraction_run_id`, `quality`, `lang`, `place_ids`, `licence_class`, `attribution`, `use_policy`, `valid_from`, `superseded_at`), **range-partitioned by month**. A new version is written only when `data_hash` changes. Old partitions are exported to Parquet in GCS after 12 months.
- **`observations`** (`series_key`, `observed_at`, `value` jsonb, `capture_id`, `source_id`): time series such as FX rates, weather normals and advisory levels, and later prices and review scores. Partitioned by month.
- **`entities`** (canonical, cross-source) + `entity_claims(entity_id, attribute, value, record_id, version, confidence)` for per-attribute provenance + `entity_redirects(old_id → new_id)` so merged ids never 404.
  - Merges happen only between licence-compatible records (§5.9).
  - v1 entity resolution is deliberately simple: same kind + same place + normalised-name trigram ≥ 0.8 + distance ≤ 300 m. Anything else stays a separate entity.
- **`extraction_runs`** (id, kind, schema@ver, prompt_hash, model, scope, status, canary_stats). A prompt or schema change first runs as a canary on 200 records and is compared against the previous run, then rolls out under the token budget.
- **Curation states:** `active` · `stale` (not seen for 3× its cadence) · `gone` (404/410 twice) · `tombstoned` (opt-out, denylist or purge: removed from the API and index, raw data purged by `admin purge`).

### 5.7 Processing and agentic extraction

The processor takes work from `WorkQueue`. Delivery is **at-least-once**; every write is idempotent on (`capture_id`, `extraction_run_id`, `record_key`).

1. A `ContentHandler` per media type (HTML, feed, sitemap, JSON, CSV) produces links, main text and structured data.
2. Free extraction: JSON-LD/microdata (schema.org Event, Place, TouristAttraction, LodgingBusiness, Article, FAQPage) and source adapters/parsers.
3. LLM extraction runs only if all of these hold:
   - the payload changed;
   - the source asks for an LLM kind;
   - UsePolicy allows it;
   - the kind policy allows this source class;
   - the ledger reserves the cost.

   Input: trimmed main text ≤ 1,200 tokens (≤ 3 chunks). Output: the kind's `responseSchema`, `maxOutputTokens` 400.
4. **Grounding checks (prompt-injection containment).** The system prompt treats page text as data. The output can only be JSON of the schema type; there are no tools. Then:
   - fields marked `verbatim` in the kind (phone numbers, URLs, prices, dates, emergency numbers) must appear verbatim, after normalisation, in the source text, or they're dropped;
   - URLs must be on the source's own domain or in the record's links;
   - **high-stakes kinds** (`visa_rule`, `advisory`, `emergency_numbers`) are accepted only from **official** source classes (government sites/APIs, Wikidata/Wikipedia for numbers with cross-checks), never from blogs or forums;
   - cross-source disagreement on a high-stakes field sets `conflict = true` and the API flags it.
5. Place resolution: Voyager's GeoNames-based place data (exported file) + country hints. Gemini disambiguation only under budget; an unresolved record keeps `place_ids = []`.

**Entity kinds v1** (registry entries): `place`, `attraction`, `activity`, `walking_tour`, `event`, `hotel` (descriptive facts only), `visa_rule`, `advisory`, `sim_offer`, `transport_option`, `emergency_numbers`, `fx_rate` (observation), `weather_normal` (observation), `tip`, `packing_hint`, `article`, `qa_thread`, `airport`, `route`.

**Agentic loops** (phase P2b, after the core is live; each has its own share of the budget):
- **Link triage:** batches of 100 (URL + anchor + context) → relevant/kind; cached per URL pattern. Only for in-scope links that the heuristics leave uncertain.
- **Source scout (propose only):** a weekly job, for under-covered (destination, kind) cells. Gemini proposes sites; automatic checks (robots for VoyagerBot, the denylist incl. brand domains, ToS keyword scan of /terms, feed/sitemap presence, licence hints) produce a **proposal** in `source_candidates` and `docs/proposals/<date>.md`. Only `vcrawl admin source approve <id>` (a human) turns a proposal into a `sources/*.yaml` PR. Nothing is crawled before approval except robots.txt and /terms, each under a normal permit.
- **Quality auditor:** weekly, 100 sampled record versions checked by Gemini against the stored text; results go to `audits` and a per-source quality score, which feeds ranking and alerts only when a source drops below a threshold.

### 5.8 Token budget (race-free)

- `llm_budget(day, cap_usd, reserved_usd, spent_usd)` and `llm_ledger(id, day, purpose, model, est_usd, actual_usd, input/output/thinking tokens, status)`.
- **Reserve:** `UPDATE llm_budget SET reserved = reserved + $e WHERE day = $d AND spent + reserved + $e <= cap RETURNING …`. The estimate is input tokens (counted with a conservative chars/3.5 ratio) + `maxOutputTokens` + the thinking budget, at list price. If no row comes back, there's no call.
- **Settle** with the actual usage. A reservation that is still open 1 h later (crashed caller) is settled as spent.
- **Batch jobs** reserve their whole estimate at submit time.
- Every model use goes through the ledger: extraction, triage, scout, auditor, place disambiguation and embeddings.
- **Caps:** daily (`VC_LLM_DAILY_USD`, default 1.50), monthly (`VC_LLM_MONTHLY_USD`, default 40), and `VC_LLM_ENABLED` (**default false; enabled by the owner after raising the project spend cap**).
- Vertex 403/429 errors naming billing or budget open a 6 h breaker.
- Every Vertex request carries `labels: {app: voyager-crawler, purpose: <p>}` so billing can attribute it.

### 5.9 Licences

Each source declares a `licence_class`:

| Class | Examples | Use |
|---|---|---|
| `open` | CC0, public domain, OGL, CC BY | merge into entities; attribution where required |
| `share_alike` | CC BY-SA (Wikipedia, Wikivoyage, Stack Exchange), ODbL (OSM) | kept as separate records, **not merged** with other licences' attributes; served with licence + attribution; derived summaries carry the same licence. ODbL data stays in its own layer (a "produced work" for consumers, the database itself under ODbL), documented in `docs/Licensing.md` |
| `facts_only` | blogs, news, other sites' editorial content | only extracted facts + headline + ≤ 300-char excerpt + link; full text never served |
| `restricted` | non-commercial (Open-Meteo free tier, Meteostat) or unknown | **not ingested in v1**. MET Norway (CC BY 4.0) replaces Open-Meteo for weather |

Every API item carries `licence {class, spdx, attribution, url}`.

### 5.10 Dumps and APIs

**Adapters stream.** A dump is downloaded through a permit-gated streaming reader, filtered on the fly (e.g. Wikivoyage pages, Geofabrik extracts per region with the relevant tags), and two things go to raw:
- a `dump_manifest` WARC `metadata` record (URL, size, sha256, dump date, filter version);
- the **filtered output** as `resource` records.

The full dump is not stored. It is a versioned, publicly archived artefact, and the manifest names it exactly. Wikidata is queried through SPARQL with pagination, not the full dump. Raw API responses are WARC `response` records like any page. Dumps run in the `maintenance` window, under the daily raw-bytes cap.

**Forums (GDPR):** v1 ingests only the **Stack Exchange Travel data dump** (CC BY-SA). Author display names are kept only as the required attribution string. A `qa_thread` stores question, answers and score, and the API serves excerpts with attribution. Erasure requests use `admin purge --author`. No other forums in v1. The research shows that the major open forums block honest bots anyway.

### 5.11 Index and read API

- **FTS** per language. **Embeddings** live in an `embeddings(owner_type, owner_id, model_id, dim, vec halfvec)` table with a pgvector HNSW index per model. Only `record_versions` that are current and allowed by UsePolicy are embedded. A model change means a backfill under the budget, then a switch.
- **Hybrid query:** place filter (exact place_id, else radius) + kind + freshness; reciprocal rank fusion of `ts_rank` and vector similarity, × quality × freshness; licence-class filtering per client.
- **API** `/v1`, served on the crawler VM's **internal IP only**. The firewall allows TCP 8443 only from service account `vm-runtime` (the app VM). TLS uses a self-signed certificate pinned by the client, so the token never crosses the internet. Endpoints:
  - `GET /v1/entities?place=&kind=&q=&limit=&cursor=`
  - `/v1/entities/{id}` (claims, sources, provenance; redirects followed)
  - `/v1/records/{id}/versions`
  - `/v1/search`
  - `/v1/articles` (excerpts)
  - `/v1/observations?series=&from=&to=`
  - `/v1/places/resolve`
  - `/v1/changes?since=&kinds=` (a change feed so clients can sync)
  - `/v1/sources`, `/v1/stats`, `/healthz`, `/readyz`
  - `/metrics` (localhost only)
- Every item has `schema: kind@ver`, `provenance`, `licence`, `conflict`.
- Per-client bearer tokens (Secret Manager; hashed in DB), rate-limited per client. OpenAPI at `/v1/openapi.json`; the snapshot test fails on any breaking change.

### 5.12 Extending

| Want | Do |
|---|---|
| A new site | `sources/<id>.yaml` (seeds, sitemaps, feeds, include/exclude, kinds, render, cadence, licence_class, attribution, optional lower rate) → `vcrawl admin source validate <id>` (robots, denylist, 3 permit-gated sample fetches) → fixture + golden test |
| An API or dump | `impl SourceAdapter` in `vc-sources/adapters/`, register it in `adapters::REGISTRY`, fixture |
| A new kind | `schemas/<kind>.v1.json` + `prompts/<kind>.md` + `policy` entry in `kinds.toml` (source classes, verbatim fields, observation or record) + fixture. The registry test enforces all four |
| A new media type | `impl ContentHandler`, register it |
| A new store, queue or LLM | implement the `BlobStore`, `WorkQueue`, `LlmClient`, `Embedder` or `Frontier` trait |

## 6. Runtime layout (one VM, docker compose)

| Service | Memory limit | Notes |
|---|---|---|
| postgres (17 + pgvector, wal-g archiving) | 3 GB (`shared_buffers` 1 GB) | password from Secret Manager; only on the internal `db` network |
| scheduler, maintenance | 256 MB each | |
| fetcher | 512 MB | |
| egress | 256 MB | |
| chromium | 1.5 GB | `render` network only |
| processor | 768 MB | |
| embedder | 256 MB | |
| api | 512 MB | |

Plus the Ops Agent on the host. Total ≈ 7.3 GB of 8 GB. Health checks are **progress-based** (e.g. the fetcher fails health if it has done no completion for 10 minutes while due work exists). Restart policy `unless-stopped`; the VM is a **single-instance MIG with autohealing** on `/healthz`.

## 7. GCP footprint (`voyager-prod-ohzkl`, us-east1, every resource labelled `app=voyager-crawler`)

- **VM** `voyager-crawler` (instance template + single-instance MIG):
  - e2-standard-2, Debian 12, Shielded VM, OS Login;
  - 20 GB boot disk + 100 GB pd-balanced **stateful** data disk (preserved by the MIG);
  - `default` VPC, static external IP (for honest egress identity, published in BOT.md);
  - no inbound from the internet except IAP SSH.
  - On-demand for 8 weeks, then the owner decides on a 1-year commitment (runbook step).
- **GCS:**
  - `voyager-crawler-raw`: segments, text, CDXJ; versioning; lifecycle Standard→Coldline at 90 days for `segments/`.
  - `voyager-crawler-ops`: wal-g base backups + WAL kept 14 days, Parquet exports, batch-prediction I/O kept 7 days.
- **IAM:**
  - `crawler-runtime` SA: `objectCreator` + `objectViewer` on `-ops` (it cannot delete backups; lifecycle does) and `objectAdmin` on `-raw` (purge needs delete);
  - a custom role `voyagerCrawlerVertex` (predict, batchPredictionJobs create/get/list/cancel);
  - logWriter, metricWriter; secretAccessor on the `crawler-*` secrets only.
  - The app VM SA gets `secretAccessor` on `crawler-api-token-voyager` only.
- **Secrets:** `crawler-pg-password`, `crawler-api-token-voyager`, `crawler-api-tls`.
- **Firewall:** `allow-crawler-api` (tcp:8443 from source SA `vm-runtime` to target SA `crawler-runtime`), plus IAP SSH.
- **Monitoring:**
  - Heartbeat custom metrics (`crawler/heartbeat` per role, `crawler/captures_committed`) with **metric-absence** alerts; no public uptime check needed, because the internal API isn't reachable from the checkers.
  - Alerts:
    - crawler down (absence 15 min)
    - no captures 60 min while work is due
    - spool backpressure
    - disk > 80%
    - backup/WAL archive age > 2 h
    - restore drill failed
    - LLM monthly spend > 80% of cap (the daily cap being hit is normal; it is not an alert)
    - parked keys > 25% of active keys
  - A **separate notification channel** (the crawler email), so the app's channel stays quiet.
- **Budget:** `voyager-crawler` budget ₹9,000/month on the label filter (Vertex calls labelled per §5.8), alerts at 50/90/100%. The project spend cap is raised by the owner (runbook checklist).
- **Snapshots:** a daily snapshot schedule on the data disk, kept 7 days.

## 8. Build, deploy, upgrades

- **CI:**
  - fmt, clippy `-D warnings`, tests (unit + integration with PG + fake web/gemini/gcs), E2E compose, cargo-deny (licences, advisories, **bans**, sources = crates.io), cargo-llvm-cov gate (policy/frontier/store ≥ 85%);
  - a short fuzz smoke on every PR, with the nightly fuzz run longer;
  - docker build, Trivy, shellcheck, bootstrap dry-run tests, OpenAPI snapshot, migration N-1 test.
  - Actions are pinned by SHA and images by digest.
- **On `main`:** build `ghcr.io/rupakc/voyager-crawler:<sha>`, cosign keyless sign; promote `:prod` only if CI passed.
- **VM `deploy.sh`** (systemd timer, 2 min):
  - resolve `:prod` → digest; `cosign verify` pinned to issuer + `repo/.github/workflows/deploy.yml@refs/heads/main`; pull;
  - run `migrate` (**expand/contract only**: a migration must keep the previous image working; destructive steps ship one release later, and the N-1 test enforces this);
  - roll the roles one by one, wait for progress-based health, and roll back to the previous digest on failure.
- **Postgres major upgrades:** a documented `pg_upgrade --link` procedure behind a snapshot (runbook).

## 9. Observability

- JSON logs (`role`, `key`, `url_key`, `source_id`, `capture_id`, `epoch`; never page content), shipped to Cloud Logging under a crawler log bucket with 30-day retention and exclusion filters for debug level.
- Prometheus metrics, scraped by the Ops Agent → Cloud Monitoring.
- `vcrawl admin status` and a daily digest (parked keys, top errors, yields, spend) written to `crawl_digest` and emailed through the crawler channel.

## 10. Volume and cost

### 10.1 Target volume (v1 steady state)

| Category | Pages/month | LLM extractions/month |
|---|---|---|
| Feeds (blogs/news: 50 feeds × ~30 items, re-checked) | 60k | 10k (articles → facts) |
| Official visa/advisory (FCDO, State, Canada, EU) | API + 5k | 2k (only changed pages) |
| Wikivoyage / Wikipedia (dump-filtered) | 30k articles (dump) | 20k (rolling, by priority) |
| Events (Ticketmaster API, JSON-LD sites) | 40k | 5k |
| Attractions/tours/transport sites (HTML) | 50k | 13k |
| FX, weather, GTFS/GBFS catalogue, SE Travel dump | API/dump | 0 |
| **Total** | **~185k HTML + APIs/dumps** | **~50k (cap-bound)** |

### 10.2 Sizing (1x year 1 → 10x)

| | 1x | 10x |
|---|---|---|
| urls | 3M (~1.5 GB with indexes) | 30M (~15 GB) |
| captures | ~2.5M/yr (~1.5 GB) | 25M |
| record_versions | ~3M (~6 GB) | 30M (~60 GB → Parquet archive) |
| embeddings | 1M × 768 halfvec (~1.6 GB + HNSW ~2 GB) | 10M (~35 GB → dedicated DB) |
| raw GCS | ~70 GB/yr | ~700 GB/yr |

The 100 GB disk is enough for year 1 at 1x.

### 10.3 Cost (USD/month)

| Item | Cost |
|---|---|
| e2-standard-2 (on-demand 8 weeks $48.92 → 1-yr commitment) | 30.82 |
| Disks + snapshots | 13.50 |
| Static IP | 3.65 |
| GCS (raw + text + ops) | ~5–8 |
| Gemini (3.1 Flash-Lite, batch ~50k extractions + triage/audit) | ~15–25 (hard cap 40) |
| Embeddings | ~3 |
| Logging/egress/ops | ~3 |
| **Total** | **~$75–90** steady state, ≤ $105 worst case; first 8 weeks ~$95–125 |

**Protecting Voyager's AI:**
- `VC_LLM_ENABLED=false` until the owner raises the project Gemini spend cap to at least (Voyager's own peak + 40 USD) × 1.25.
- The crawler's monthly cap is configured to at most 50% of the configured project cap (`VC_PROJECT_CAP_USD`); startup fails otherwise.
- Calls are labelled and have their own budget alert.

## 11. Scale-up path (10x and beyond)

| Area | Today | At 10x |
|---|---|---|
| Fetchers | one VM | Spot VMs, each taking a leased `shard` set of politeness keys (the claim adds `WHERE shard = ANY($mine)`), each with its own static IP published in BOT.md |
| Frontier | Postgres | Already partitioned. Postgres goes to a highmem VM or Cloud SQL; the `Frontier` trait allows a different store if needed |
| WorkQueue | Postgres | Pub/Sub |
| Embeddings | in the main DB | a dedicated pgvector instance (or Vertex Vector Search) behind `Embedder`/index traits |
| Archives | `record_versions` and `captures` older than 12 months in Postgres | exported to Parquet (BigQuery external tables for analytics) |
| Raw storage | Standard → Coldline | Standard → Coldline → Archive |

## 12. Testing

- **Unit + property (proptest):**
  - URL canonicalisation idempotence;
  - robots against Google's test corpus and RFC 9309 status cases;
  - the UsePolicy matrix; SSRF normalisation;
  - simhash; WARC/CDXJ round trip; budget arithmetic.
- **Fuzzing (cargo-fuzz):** robots parser, sitemap/feed parsers, HTML handler, WARC reader, URL canonicaliser.
- **Integration (PG testcontainers + testkit):**
  - claim fairness and fencing (an expired holder's completion is rejected);
  - two fetcher processes against one fake site;
  - conditional GET; 429 + Retry-After;
  - robots 401/403/challenge → key blocked; a cross-key redirect is enqueued, not followed;
  - the sitemap index; spool → segment → commit ordering;
  - **failpoints** (`fail` crate) at every step of §5.5, each followed by a restart; asserts no lost and no duplicate captures, and no 304 without a stored body;
  - the budget race (100 concurrent reservations never exceed the cap);
  - batch lifecycle (crash after create → reconcile, no resubmit; partial failures);
  - UsePolicy enforcement end to end; grounding checks with prompt-injection fixtures (fake emergency number, phishing URL, "ignore previous instructions");
  - purge; API contract snapshots (insta); migration N-1 (old image against the new schema).
- **E2E (compose in CI):** a "mini internet" of 7 fake sites:
  - a blog with RSS
  - an SPA attractions site needing render
  - events with JSON-LD
  - a hostile site serving a challenge
  - a slow site with Crawl-delay
  - an official visa page
  - a CDN asset host

  The whole stack runs for a fixed number of crawl cycles with two fetchers. The **politeness oracle** checks the fake web's request log: per key, never two overlapping requests, gaps ≥ delay, no request to a disallowed path or denylisted host, and the challenge host gets no more requests after parking. A **raw-first oracle** checks that every record's capture exists in fake GCS. A content change creates a new version, the API answers, and an orphaned Chromium connection to a private IP is blocked.
- **Restore drill:** CI runs dump → wipe → restore → `rebuild-captures` → equivalence check. The VM runs it weekly into a scratch container and exports `restore_drill_ok`.
- **Mutation testing** (cargo-mutants) nightly on `vc-policy` and `vc-frontier`.
- **Live smoke:** after deploy, 3 allowed real sources at a low rate; `/v1/stats` must grow.

## 13. Voyager integration (main repo, separate phase, feature-flagged)

`CRAWLER_API_URL`, `CRAWLER_API_TOKEN` (Secret Manager), `CRAWLER_GROUNDING_ENABLED` (default false). `backend/app/services/crawler_client.py` (2 s timeout, circuit breaker). Agents get top-k grounding records (with licence and conflict flags; `facts_only` items as facts + link) prepended to their prompts. A missing or failing crawler means behaviour exactly as today. This is two-way compatible by construction. It gets its own design note and PR in the main repo.

## 14. Delivery phases

- **P0 (sequential, controller):** workspace, `vc-core`/`vc-config` types and **all traits** (§5.1), migrations, the policy files from research, CI skeleton, Claude scaffolding (CLAUDE.md, MEMORY.md, agents, skills), `sources/*.yaml` generated from seeds.
- **P1 (parallel, one agent per crate in worktrees, each with tests):**
  - vc-policy
  - vc-frontier
  - vc-fetch
  - vc-egress + vc-render
  - vc-store
  - vc-parse
  - vc-kinds + schemas + prompts
  - vc-llm
  - vc-extract
  - vc-sources + adapters
  - vc-index
  - vc-api
  - vc-queue + vc-obs + vc-secrets
  - testkit
  - deploy kit + bootstrap
- **P2a:** `vcrawl` role wiring, E2E + oracles, failpoint suite, docs, review round.
- **P2b:** agentic loops (triage, scout proposals, auditor).
- **P3:** provision GCP (bootstrap), first deploy with LLM off, staged ramp (10 sources → all), live verification, then LLM on after the owner raises the cap.
- **P4:** Voyager backend integration (main repo).

## 15. Owner checklist (manual steps)

1. Raise the project Gemini spend cap in the console (the value comes from §10.3). Until then the crawler runs with LLM off.
2. After 8 weeks, decide on the 1-year commitment for e2-standard-2.
3. Confirm the crawler notification email.
4. Review proposals from the source scout (`vcrawl admin source list --proposed`).

## 16. Changes from v1 (review round 1)

**Scale and extensibility (r1)**
- Sizing table; partitioning (`urls`/`url_state` by hash, `captures`/`record_versions`/`observations` by month); `halfvec`; honest §11.
- Records + `record_key`, observations, entity claims and redirects.
- `url_state` split from identity, with a partial claim index and batched leasing.
- 128-bit url keys.
- Origin vs politeness key; shard column.
- `extraction_runs` with canaries, plus a Standard-storage text tier.
- All traits defined in P0.
- An `embeddings` table per model; a data-driven kind registry; `ContentHandler`.
- Schema tags + `/v1/changes` + per-client tokens + licence flags.
- WAL archiving + rebuild-captures.

**Correctness and security (r2)**
- Crash-consistent spool/segment/commit with validators in the same transaction.
- Fenced leases with DB time and deadlines; DB-only permits incl. robots, sitemaps, adapters and admin; DB rate tokens.
- Cross-key redirects enqueued; Chromium isolated behind a gated egress proxy with its sandbox on; SSRF details.
- Grounding and verbatim checks; official-only high-stakes kinds; malicious-content limits.
- Internal-IP API with TLS.
- Supply chain: SHA pins, evasion crate bans, cosign identity; backups not deletable.
- Race-free budget; batch job lifecycle.
- Restore drill; progress health, memory limits, backpressure, MIG autohealing, absence alerts, expand/contract migrations.
- Failpoint/oracle/fuzz/mutation tests.

**Compliance, cost and ops (r3)**
- LLM off by default; cap ≤ 50% of the project cap; labelled calls.
- The UsePolicy matrix; no retention lock + purge; opt-out SLA.
- Streaming dumps with manifests; SPARQL instead of the Wikidata dump; raw-bytes cap.
- Stated volume and a lower cost; licence classes with share-alike separation and non-commercial excluded.
- Forums limited to the SE dump under GDPR handling.
- Scout is propose-only with human approval; brand-domain denylist.
- N-1 migration test and pg_upgrade runbook.
- Separate channel, de-noised alerts.
- Agentic loops deferred to P2b behind the core.

**Not taken**
- r3's "defer Chromium/vector search": the owner explicitly asked for a headless browser and an index. Both are kept, but isolated and budgeted.
- r3's "separate project": the owner chose the same project.
