# Decision ledger

One dated entry per decision, newest at the bottom of each day. Link the source. Never
rewrite an entry: supersede it with a new one that says so.

Abbreviations: spec = `docs/superpowers/specs/2026-11-15-voyager-crawler-design.md`,
plan = `docs/superpowers/plans/2026-11-15-voyager-crawler.md`, r1–r5 = `docs/reviews/`.

## 2026-11-15 — owner choices

- **Same GCP project** `voyager-prod-ohzkl` (not a separate project); isolation is by
  in-code hard Gemini caps, labels and a separate crawler budget alert. (spec §3; r3 #1
  "separate project" not taken, spec §16)
- **Budget about $100/month** all-in; steady state ~$75–90, ≤ $105 worst case. (spec §10.3, G7)
- **No evasion of any kind** (stealth, UA/fingerprint spoofing, proxy rotation, CAPTCHA
  solving, TLS impersonation); challenge/401/403 → park. (spec §2 anti-goals)
- **Owner raises the project Gemini spend cap** in the console; until then
  `VC_LLM_ENABLED=false`. Crawler monthly cap ≤ 50% of `VC_PROJECT_CAP_USD`. (spec §10.3, §17.4)
- **Keep Chromium and the vector index** despite r3 #11 "defer": the owner asked for both;
  isolated and budgeted. (spec §16 "Not taken")
- **Rust, Gemini only (Vertex, ADC), separate public MIT repo, GitHub Actions without
  GCP credentials, cosign-signed GHCR images pulled by the VM.** (spec §3)

## 2026-11-15 — v1 design (research + draft)

- Approach A: own Rust core + Postgres frontier on one VM; not `spider` (evasion on by
  default), not managed services (2× budget). (spec §4)
- Raw storage = WARC/1.1 + zstd segments in GCS indexed by CDXJ, not one object per page. (spec §4)
- Self-hosted Postgres 17 + pgvector with wal-g, not Cloud SQL. (spec §4)
- Infra as an idempotent gcloud `bootstrap.sh` with `--dry-run`, no Terraform. (spec §4)
- Forums in v1 = Stack Exchange Travel data dump only (GDPR). (spec §5.10, r3 #7)

## 2026-11-15 — v2 (review round 1: r1 scale, r2 correctness, r3 compliance/cost)

- Crash-consistent ordering: spool fsync → segment upload (`ifGenerationMatch=0`) → one
  commit tx; validators only move in that tx (no 304 without a stored body). (spec §5.5, r2 #1)
- 128-bit url keys; `urls`/`url_state` split and hash-partitioned; captures/record_versions/
  observations range-partitioned by month. (spec §5.3, §5.6, r1)
- Fenced leases with DB time and an epoch; DB-only permits; DB-backed global rate tokens. (r2)
- UsePolicy matrix (signals → allowed uses); AI-bot robots block = AI reservation; no
  retention lock, purge + opt-out SLA. (spec §5.2, r3 #2)
- Dumps are streamed and filtered; manifest + filtered output stored, not the full dump;
  Wikidata via SPARQL, not the dump. (spec §5.10, r3 #3)
- Licence classes with share-alike separation; non-commercial/unknown = restricted, not
  ingested; MET Norway replaces Open-Meteo. (spec §5.9, r3 #6)
- Source scout is propose-only; a human approves; brand-domain denylist. (spec §5.7, r3 #8)
- Expand/contract migrations with an N-1 test. (spec §8, r3 #9)
- Separate crawler notification channel; metric-absence alerts; daily digest for parks. (spec §7, r3 #10)
- Agentic loops (triage, scout, auditor) deferred to P2b behind the core. (spec §14)

## 2026-11-15 — v3 (review round 2: r4 verify, r5 builder check) — normative §17

- **`vc-egress` removed.** Chromium has no network; CDP `Fetch` interception sends every
  sub-request through the policy checks and `PermitClient`. Render budget 90 s / 60
  sub-requests; sub-resource cache 24 h. (spec §17.1, r4 B1)
- **Claims lease ≤ 5 URLs**; states due → leased → fetched_pending → due;
  `vc_commit_segment` has a monotonic `last_captured_at` guard (not epoch-gated). (spec §17.2, r4 B2)
- **Batch prediction lifecycle** `batch_jobs` prepared → submitted → reconcile by label →
  ingest by key → 72 h cancel; batch reservations not auto-settled. (spec §17.3, r4 B3)
- **Budget reserves against day and month rows in one statement**; dynamic daily cap;
  purpose shares 70/8/8/7/4/3; `llm_cache`; default model `gemini-3.1-flash-lite`. (spec §17.4)
- ~~Plain VM, no MIG~~ → **one Spot e2-medium in a stateful MIG of size 1** (owner,
  2026-11-17: Spot this month, then a standard e2-medium with a 1-year commitment):
  per-instance config keeps the name `voyager-crawler`, boot + data disks and both static
  IPs across preemption recreates; autohealing on the API `/healthz`; termination action
  STOP; no disk snapshots (wal-g PITR + weekly drill); weekly full base backups.
  Static internal + external IP; watchdog timer; self-signed TLS for the internal API.
  (spec §17.5)
- **Opt-out SLA:** API/index 24 h, GCS 7 d, backups/snapshots 21 d; `ephemeral/` prefix
  for noarchive/TDM. (spec §17.7)
- **`source_class`** (official | reference | editorial | community | commercial_api) and
  licence class **`api_terms`** (Ticketmaster, Nager.Date); Wikipedia/Wikidata are
  reference, not official. (spec §17.8, r5 Q12)
- **Change feed by `xid8`** below the snapshot xmin; monthly Spot-VM restore drill. (spec §17.9)
- **All cross-crate traits in `vc-core`** (star graph); permits unforgeable via private
  `mint`; rate token per request; vc-policy pure; one owner per path; migration blocks;
  minimal testkit in P0. (r5 Q1–Q16, spec §17.10)

## 2026-11-15 — P0B implementation

- `sources/*.yaml` generated from `docs/research/seeds.yaml` by `scripts/seeds_to_sources.py`:
  117 files, 98 enabled, 19 disabled (restricted licences; non-goal live prices; partner
  APIs without approval; forums other than the SE dump; Wikidata dump; stale/paid sources).
- SourceSpec enums serialise **snake_case**; cadence values `hourly | h2 | h6 | h12 | daily |
  weekly | monthly | quarterly | yearly | on_demand` (seeds `never`/`on_change` → `on_demand`,
  `semiannual` → `quarterly`, `4x_daily` → `h6`). (schemas/source.schema.json)
- Denylist entries `{host, reason, source}` + optional `path_prefix`, `except_hosts`;
  `<brand>.*` = that label under any public suffix. Partner-API hosts of denied brands
  (Booking Demand, Viator, GYG) are excepted but their sources stay disabled.
- Branch convention `agent/<task>[-slug]`; ownership map in `docs/ownership.md`.
- 2026-11-17: the crawler VM is a Spot e2-medium (4 GB); every role runs in one process,
  `vcrawl run` (one pool, one `/metrics` + `/healthz`, heartbeat per role, ≤ 25 s graceful
  stop), in one `vcrawl` container; limits sum to 3.3 GB and the E2E job fails above a
  3.2 GB measured peak. otelcol is kept (≈ 50 MB) rather than a Cloud Monitoring exporter
  in vcrawl. (crates/vcrawl/src/run.rs, deploy/compose.yaml, e2e/mem-sampler.sh)
- 2026-11-17: adapter captures become records through each adapter's own record mapping
  (`vc_sources::adapters::records`, method `adapter`), not the content handlers: ECB → fx_rate
  observations `fx:EUR:<ccy>`; Canada, State Dept and FCDO country pages → `advisory`;
  OurAirports → `airport` (large/medium, scheduled service or IATA only); Wikivoyage →
  geo-tagged `place` (country from the place resolver). Dump batches never stale records
  (all batches share the dump URL). `admin reprocess --source` re-queues captures.
  (crates/vc-sources/src/adapters/records.rs, crates/vcrawl/tests/adapter_records.rs)
- 2026-11-17: the permit client pins the first IPv4 address of a host (AAAA came first and the
  VM has no IPv6 route: every dual-stack host failed). (crates/vc-fetch/src/lib.rs, #107)
- 2026-11-17: owner rule "only relevant and pertinent sites": every source spec needs
  `relevance {sections, why}` (closed app-section list); enabled news/blog feeds must be
  travel-focused; the owner approves additions; registry `docs/SOURCES.md` is generated and
  CI-checked. travel.state.gov denylisted (403 bot wall; cadataapi.state.gov unaffected).
  (crates/vc-sources/src/relevance.rs, scripts/sources_doc.py)
- 2026-11-17: a source only crawls its own sites: discovered links (also with include regexes)
  and cross-key redirect targets are enqueued only under the politeness keys of its seeds,
  sitemaps and feeds; others are dropped (cdc-travel → medscape.com leak).
  (crates/vcrawl/src/data/sources.rs, crates/vcrawl/src/roles/fetcher.rs)
- 2026-11-17: sources batch 2 (research 2026-11-17): 140 new specs (transport, visa, customs,
  embassies, events, tourism boards); only html/rss/sitemap sources with robots checked are on;
  api/dump ones wait for adapters or owner keys. Owner decision: Voyager is free and
  non-commercial, so sources whose only blocker is unconfirmed terms are on as facts_only
  (never over an explicit ban, a 401/403/challenge or a missing adapter/key). Time Out (AI/RAG
  ban), usembassy.gov (robots 403) off; 62 denylist entries added. Kinds: customs → tip/
  packing_hint, hospitals → place, holidays → event, price indicators → tip (no new kinds).
  sources/ramp-2.txt = ramp-1 + the runnable batch-2 sources. (scripts/seeds_to_sources.py
  "Batch 2", policy/denylist.yaml, docs/research/licence_only.yaml)
- 2026-11-17: Travelpayouts Data API enabled with the owner's free token as **cached**
  cheapest fares (not live prices; supersedes "travelpayouts disabled: live fares are a
  non-goal"). Terms allow storing: the provider recommends caching responses ("save the
  received information in the database"; cache 24 h). New observation kind `cheapest_fare`,
  series `fare:<ORIG>:<DEST>:<YYYY-MM>`; adapter `travelpayouts_fares` (grouped_prices
  group_by=month, one call per route, 100 committed routes, token only in `X-Access-Token`,
  stripped before spooling), weekly cadence + 6-day refetch floor in the fetcher, 150/day
  in-adapter cap, `daily_budget` 200, `crawler_adapter_calls_total{adapter}`. Affiliate marker
  belongs app-side when linking out. (crates/vc-sources/src/adapters/travelpayouts_fares.rs,
  https://support.travelpayouts.com/hc/en-us/articles/4402565416594)
- 2026-11-17: scam-safety sources (owner request): travelerset-scams, avoidtravelscam,
  twopassportspacked-scams (tips, editorial, facts_only, monthly, depth ≤ 1 pinned by include
  or seed page only, ≤ 50/day; in ramp-2) and a Smartraveller scam article as an extra seed
  (its robots.txt times out = disallow until it answers). worldpackers, moneycrashers,
  eurochange, travelersunited and consumer.ftc.gov (exact host, not ftc.gov) denylisted for
  403s. (scripts/seeds_to_sources.py "Travel safety", policy/denylist.yaml)
- 2026-11-18: open-data adapters. New kinds `public_holiday` (record) and `price_level`
  (observation, series `price_level:<ISO2>:<category>:<basis>`, basis `eu27_2020_100` or
  `us_1`, observed at `<year>-01-01`). Adapters: `openholidays` (/Countries → Public/School
  holidays per country, this year + next), `json_events` (one generic paged-JSON event adapter:
  per-endpoint field mappings in `MAPPINGS`, Socrata offset or page-number paging, resumable),
  `eurostat_jsonstat` (JSON-stat 2.0, PLI_EU27_2020), `wdi_csv` (streams WDI_CSV.zip; new
  streaming ZIP member reader in dump.rs, `zip_stream.rs`). On: openholidays, nyc-parks-events,
  stockholm-open-api (keyless www.visitstockholm.com/api/public-v1/events/), eurostat-prices,
  worldbank-wdi-bulk; listed in sources/ramp-3.txt (prod stays on ramp-2 until the owner
  switches). Off with reasons: bcn (robots disallows /data/api*), madrid (robots disallows the
  CKAN downloads), toronto (dataset retired), taiwan (data.gov.tw robots 500), datagovhk (LCSD
  XML on another host). (crates/vc-sources/src/adapters/, scripts/seeds_to_sources.py)
- 2026-11-18: GTFS Schedule adapter `gtfs_static`: one permit-gated zip stream per feed, read
  as it arrives (own streaming zip reader; nested zips = one feed each), size cap
  `[sources] gtfs_max_bytes` (600 MiB; over it = skipped with a manifest) plus an inflate cap;
  keeps agencies, routes, stations (location_type 1), fares and feed_info only (never
  stop_times/trips). Items are stored only after the whole archive checked out; a primary status
  capture carries the validators, so an unchanged feed costs one 304. New record kind
  `transit_item` (operator / line / station): GTFS has no city, which `transport_option`
  requires. Zip types allowed for dump downloads only (`[fetch] dump_content_types`). On:
  ovapi-gtfs-nl, opentransportdata-swiss, vic-gtfs, translink-seq-gtfs, entur-gtfs
  (ET-Client-Name: voyagerbot-crawler), auckland-gtfs, listed in sources/ramp-3.txt; keyed feeds
  wired but off. (crates/vc-sources/src/adapters/gtfs_static.rs)
- 2026-11-18: official embassy, medical and Wikipedia visa sources on, with adapters
  (`govuk_world_orgs`, `canada_offices`, `govuk_medical_lists`, `enwiki_visa`) and three
  adapter-only kinds: `embassy` and `medical_facility` (`official` only, not high-stakes so
  different countries' offices in one city never "conflict"), `visa_reference` (`reference`
  only, cross-check data; Wikipedia never yields `visa_rule`). Phones keep the written form;
  E.164 only when written internationally (`+`/`00`, `(0)` trunk dropped), never from local
  forms. Medical lists keep each list's own updated date. The Wikipedia articles are read from
  the multistream dump: the 290 MB index is streamed and filtered, then one Range request per
  bzip2 stream (about 200), never the 27 GB dump. Adapters can now send conditional GETs
  (`Fetch::conditional`: the leased row's validators; a 304 is spooled as a revisit).
  (crates/vc-sources/src/adapters/{govuk_world_orgs,canada_offices,govuk_medical_lists,enwiki_visa,phone,countries}.rs)
- 2026-11-18: politeness-key splits. `policy/key_splits.yaml` (compiled into vc-core) lists
  registrable domains whose hosts are keyed one by one; first entry `europa.eu` (EU bodies run
  separate sites: a 403 from taxation-customs.ec.europa.eu on 2026-11-17 parked europa.eu and
  stopped the ECB FX file). `PolitenessKey::for_host` applies it (frontier, fetcher, scope, the
  oracle); `PolitenessKey::registrable` keeps "which organisation" (denylist brands,
  publishers, grounding). Migration 1104 re-keys europa.eu rows per host, keeps the park on
  taxation-customs only, disables the old `europa.eu` key. Off + denylisted after production
  bot walls: eu-customs-travellers (403), mfat-embassies (challenge), secretldn (challenge).
  Fetcher park/robots log lines carry origin + path. (crates/vc-core/src/ids.rs,
  migrations/1104_key_split_europa_eu.sql)
- 2026-11-18: **robots.txt with no response = missing (owner decision, "revisit later").**
  Connect/read timeout, connection refused/reset/closed before a status line, TLS handshake
  failure, DNS failure → `RobotsStatus::NoResponse` (`origins.robots_state = 'no_response'`,
  migration 1105): allow all, or the last good copy if one was ever read; TTL
  `policy.robots_no_response_ttl_s` = 6 h; rules read later apply and tombstone. Deliberately
  departs from RFC 9309 §2.3.1.4. Any HTTP answer keeps its meaning (5xx/429 disallow +
  back-off, 401/403 origin do-not-crawl, challenge key park); a response that started but broke
  (corrupt encoding, cut mid-body) stays `unavailable`. vc-fetch maps `send()` failures to the
  new `FetchError::Connect` (TLS handshake failures included: no HTTP answer was received, and
  pages over the same handshake fail and back off anyway). (crates/vc-policy/src/lib.rs,
  crates/vcrawl/src/roles/fetcher.rs, BOT.md)
- 2026-11-18: alternatives to bot-walled sites (owner: "the DATA those sites would have given, from
  other legitimate sources"). On: safetravel-nz (NZ advice + embassy per country, CC BY 4.0; replaces
  mfat-embassies), eu-youreurope-travel and revenue-ie-travellers (EU/Irish allowances, CC BY 4.0;
  replace eu-customs-travellers). Off with reason: icp-uae-customs, egymonuments, kws-parks (terms not
  found), dirco-sa, capetown-gov (owner decision), state-dept-country-info (cadataapi
  CountryTravelInformation; replaces travel.state.gov + usembassy.gov), fsa-food-hygiene,
  nyc-restaurant-inspections, toronto-dinesafe (adapters pending). philippines-travel and
  dubai-customs-passengers off after production challenges; 14 hosts denylisted.
  (scripts/seeds_to_sources.py, policy/denylist.yaml)
- 2026-11-18: **US country information and official food-hygiene registers on** (ramp-3).
  `state_dept_country_info`: one conditional daily request to cadataapi.state.gov's listed
  `/api/CountryTravelInformation` (the adapter refuses any other path before sending: other paths
  answer a Cloudflare block); tags are FIPS 10-4 (`FIPS_ISO2`); maps the US embassy block →
  `embassy` (phones as written, E.164 only when written with +), the entry text → `visa_rule`
  for US passports only when stated plainly (conditional sentences skipped, contradictions →
  `other`, nothing stated → no rule; deciding sentences kept as `evidence`), titled paragraphs of
  local laws/health/safety → `tip`. New adapter-only kind `restaurant` (official only; required
  `hygiene.{scheme, rating, rating_date}`). `fsa_fhrs` (23 tourist-city authorities x
  restaurants/pubs/takeaways, `x-api-version: 2` via the new `Fetch.headers`), and
  `nyc_restaurant_inspections` / `toronto_dinesafe` with group-aligned offset paging (a full
  page's last establishment waits for the next page; the record mapping makes the same cut).
  Toronto reads the CKAN datastore API (robots.txt disallows `/download/`). Licence
  attributions set per source (FSA: "… Open Government Licence v3.0 — Food Standards Agency").
  (crates/vc-sources/src/adapters/{state_dept_country_info,fsa_fhrs,nyc_restaurant_inspections,toronto_dinesafe,inspections}.rs)
