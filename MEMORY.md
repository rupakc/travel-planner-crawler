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
- 2026-11-18: sustainable/eco and budget travel sources (owner: "add all relevant sites"). 116
  rows: 92 on (html/rss/sitemap, facts only unless CC BY/public domain/EU reuse; terms not found =
  "owner accepted for non-commercial use 2026-11-18"; Crawl-delays as min_delay_s, budgets <= 200,
  AI-reserved robots = no LLM kinds; all in ramp-2 and ramp-3), 24 off: adapter pending (ONTD night
  trains, Big Mac CSV, data.gov.sg hawker centres, EU Ecolabel ECAT, data.gouv.fr ecolabel,
  flixbus-gtfs whose download goes to the 403 origin gtfs.gis.flix.tech), needs an API key (DOC NZ,
  Green Seal, ICAO carbon, Seoul Wi-Fi), owner decision (adeac-bandera-azul: personal-use terms),
  seed unresolved (10, one onboarding request per host), bot wall at onboarding (Ecotourism Kenya,
  Leave No Trace), relevance (TheSmartLocal: lifestyle, not travel). unesco-whc off: UNESCO terms
  ban automated access; unesco.org denylisted with 52 other terms-ban/403/challenge hosts. 37
  travel publications added to TRAVEL_PUBLICATIONS. (scripts/seeds_to_sources.py,
  policy/denylist.yaml, crates/vc-sources/src/relevance.rs, sources/ramp-2.txt, ramp-3.txt)
- 2026-11-18 Owner standing rule: sources whose terms restrict use to personal and/or
  non-commercial use are ENABLED (facts only; note "terms restrict to personal/non-commercial use;
  owner accepted for non-commercial use 2026-11-18 (<clause>)"), because Voyager is non-commercial.
  Explicit bans on robots/scraping/automated access/TDM/AI still mean off + denylist; robots.txt,
  AI-bot blocks and disallowed feeds stay binding. Applied to adeac-bandera-azul, capetown-gov,
  dirco-sa. (scripts/seeds_to_sources.py, docs/research/seeds.yaml)
- 2026-11-18 Corpus expansion, Europe: 151 rows from the vetted research; 128 on (html/rss/sitemap,
  ramp-2 and ramp-3), 23 off with reason (national open-data portals: adapter pending, choose
  tourism datasets + dataset reader; API keys; no-response hosts: re-test from the crawler VM;
  Eurostar: scraping clause, owner to confirm; The Local: relevance). Eight terms-ban hosts
  denylisted; 403/challenge hosts from the research laptop are not denylisted.
- 2026-11-18: North America corpus (USA, Canada, Greenland): 98 rows, 91 on (90 html/rss/sitemap
  in ramp-2 and ramp-3, the 9 personal-use-terms boards and transit sites included;
  od-chicago-events on json_events in ramp-3, Chicago Public Library events only), 7 off (adapter
  pending x3, BART needs an API key, LA events dataset retired, Narcity/Outside relevance). 17
  terms-ban hosts denylisted (www.mbta.com exact, so the cdn.mbta.com GTFS stays allowed);
  json_events reads ISO date-time start times. (scripts/seeds_to_sources.py,
  policy/denylist.yaml, crates/vc-sources/src/adapters/json_events.rs)
- 2026-11-18 Corpus expansion, thematic (outdoors, food/wine, traveller types, health, airports,
  digital-nomad visas): 84 rows, 64 on (12 under the personal/non-commercial owner rule), 20 off
  (13 dataset/API adapters pending, NASA Earthdata account, Museums Association relevance, five
  no-response hosts to re-test from the crawler VM, United and KLM included). Nine terms-ban hosts
  denylisted (nparks.gov.sg among them). Incheon (robots Disallow: / except /ap_en/) not added;
  Parks Canada, USFS, BLM and SFO already came with the North America batch.
- 2026-11-18 Corpus expansion, Latin America and the Caribbean: 108 rows, 85 on (the 12 LatAm
  OWNER-DECISION rows on by the owner's decision: gob.mx SECTUR/INM/CONANP, Cervantino, Medellín
  Travel, Migración Colombia, TransMilenio (no LLM kinds), PeruRail (HTML; /feed/ disallowed),
  Visit Jamaica, Bonaire, TCI, Los Cabos; official-inah stays on under the same rule). 23 off:
  14 provisional no-response hosts (re-test from the crawler VM), 4 open-data portals, BCRA and
  Banxico (adapter/key), 3 general-news feeds (relevance). SAG Chile on as share_alike
  (CC BY-NC-SA 3.0 CL, kept separate). aruba.com, chilango.com, serviciomigraciones.cl denylisted.
- 2026-11-18: generic, mapping-driven open-data readers instead of one adapter per dataset:
  `socrata` (SODA `$select`/`$where`/`$order` paging, optional group-aligned latest-row-per-key),
  `udata_resource` (data.gouv.fr/data.public.lu dataset API → current file, followed only on the
  source's own sites; CSV streamed in the dump window) and `wfs_geojson` (city WFS point layers);
  per-dataset `RowMapping` tables (`adapters/row_map.rs`). New adapter-only kind `amenity`
  (public toilets, drinking water, free Wi-Fi; official only). 7 sources on (ramp-3): Chicago
  and Austin food inspections, Catalonia tourism register (no holiday flats, owner columns never
  requested; facts only, terms unread), NYC public restrooms, Atout France classified
  accommodation, Vienna OGD, Züri WC. Off with reason: SF scores (frozen since 2021), LA County
  and NZ DOC (ArcGIS Hub downloads fail or redirect to services1.arcgis.com, robots 403; no
  arcgis_hub reader), HK FEHD (licence register, no hygiene result), EEA bathing water (already present off; table
  unconfirmed, catalogue query 403), sg-hawker-centres (signed S3 URL on another site). No CKAN
  reader: no vetted dataset needs one. (crates/vc-sources/src/adapters/{socrata,udata_resource,
  wfs_geojson,row_map}.rs, schemas/amenity.v1.json, scripts/seeds_to_sources.py)
- 2026-11-18 Corpus expansion, Africa and the Middle East: 138 rows, 124 on, 14 off (9 adapters
  pending, 4 relevance, Aqaba re-test); personal/non-commercial terms
  rows on under the owner's standing rule (Wesgro, NWR, RAK, DCT Abu Dhabi, ITC Abu Dhabi, NCM
  Saudi, Sharjah Art Foundation, SARS travellers, The National travel, Travel Africa, Istanbul
  Clues, Zambia Tourism, Western Cape Government, eTA Kenya, Oman met); MyCiTi already enabled as
  official-myciti; visitkzn-sa.com robots.txt read once before using its sitemap for KZN;
  visitaqaba.com timed out (re-test); Riyadh Season enabled with render auto (JS app, no terms).
  Seven terms-ban hosts and the hijacked visitethiopia.travel denylisted.
- 2026-11-18 Corpus expansion, Asia-Pacific: 183 rows, 141 on (15 under the personal/non-commercial
  owner rule, japan-guide included: its terms restrict use but do not ban robots); 42 off (29
  no-response hosts to re-test from the crawler VM, SPAs and portals waiting for render or an
  adapter, three API keys). admin.wellingtonnz.com robots.txt read once before using its sitemap.
  visit-fsm.org and visitkiribati.travel denylisted as hijacked domains; saigoneer.com for a
  scraping ban. Not added: english.visitbeijing.com.cn, www.airport.kr, www.tourismlaos.org,
  data.gov.au (robots.txt Disallow: /); nparks.gov.sg is denylisted (thematic batch).
- 2026-11-18: open data at scale. GTFS on gtfs_static: Prague PID, Budapest BKK, MBTA, CTA, King
  County Metro, Ireland NTA (all operators), Lithuania (one merged gtfs_all.zip instead of 61
  municipal zips), Luxembourg; STM Montréal off (its zip 302s into robots-disallowed
  /getcontentasset/). gtfs_static gained a "latest file" resolver: a seed answering JSON
  (udata/CKAN) or HTML is an index, spooled raw; its newest main zip is downloaded only on the
  same politeness key, and the row keeps the index's validators. wikivoyage_dump reads any
  edition (edition from the dump URL; filter v2 adds `lang` and language-neutral `listings` from
  each edition's listing templates, EDITIONS table read from the 2026-11-01 dumps); records add
  attraction (see) and hotel (sleep) per listing, ids `<lang>wikivoyage:<page>:<wikidata|name>`.
  22 more editions on (one share_alike source each, monthly, no LLM kinds, ~327 MB/month). JMA
  normals: 21 verified station pages (html, depth 0, yearly). EA bathing-water profiles off: the
  page is script-driven from robots-disallowed /doc/. Adapter sources in ramp-3, JMA in ramp-2.
  (crates/vc-sources/src/adapters/{gtfs_static,wikivoyage_dump,records}.rs,
  scripts/seeds_to_sources.py, sources/ramp-2.txt, ramp-3.txt)
- 2026-11-18: hidden gems, small islands and remote places, part 1: Asia, Pacific, Indian Ocean,
  Australia/NZ, Antarctica, Africa, Middle East, Russia, Caucasus (90 rows from the vetted
  research): 77 on (html/rss/sitemap, facts only unless an open licence was quoted; terms not found
  or limited to personal/non-commercial use = "owner accepted for non-commercial use 2026-11-18";
  terms forbidding storage in databases/retrieval systems or redistribution = off; Crawl-delays as
  min_delay_s, budgets <= 100/day, depth <= 2, AI-reserved robots = no LLM kinds; all in ramp-2 and
  ramp-3), 13 off (re-test from the crawler VM 12, terms forbid storage/redistribution 1).
  Denylisted: 9 terms/robots-ban hosts and 15 hijacked or parked official tourism domains (4 GoDaddy
  for-sale landers found at onboarding); 403/challenge/timeout hosts not denylisted.
  (scripts/seeds_to_sources.py, policy/denylist.yaml, sources/ramp-2.txt, ramp-3.txt)
- 2026-11-18: hidden gems, small islands and remote places, part 2: Atlantic, Arctic, Europe,
  Mediterranean, Baltic/North Sea, Adriatic, Americas, Caribbean, World (130 rows from the vetted
  research): 117 on (html/rss/sitemap, facts only unless an open licence was quoted; terms not found
  or limited to personal/non-commercial use = "owner accepted for non-commercial use 2026-11-18";
  terms forbidding storage in databases/retrieval systems or redistribution = off; Crawl-delays as
  min_delay_s, budgets <= 100/day, depth <= 2, AI-reserved robots = no LLM kinds; all in ramp-2 and
  ramp-3), 13 off (re-test from the crawler VM 5, terms forbid storage/redistribution 4, seed
  unresolved 2, adapter pending 2). (scripts/seeds_to_sources.py, policy/denylist.yaml,
  sources/ramp-2.txt, ramp-3.txt)
- 2026-11-19: **Carbon footprint corpus + new app section `carbon`** (owner request: trip and
  activity carbon footprint). `AppSection::Carbon` (vc-core), schema enum, `SECTIONS`, registry
  heading "Carbon footprint". 310 rows from 9 research briefs (aviation, ground, stays, food and
  activities, official factors, grid, regional, tourism research, gaps; 253 SKIP not added):
  143 on (140 html/rss/sitemap in ramp-2 and ramp-3, facts only unless an open licence was
  quoted, includes on big sites, PDFs raw; 3 Eurostat datasets on eurostat_jsonstat in ramp-3),
  136 off "adapter pending" (xlsx/PDF/CSV/parquet/API readers, incl. ADD datasets whose generic
  reader has no mapping yet), 25 off "needs an API key", 6 off "owner decision". Denylisted for
  terms bans: climatiq.io, goldstandard.org, verra.org, media.amtrak.com, ghgprotocol.org,
  hestia.earth (except api.hestia.earth). (scripts/seeds_to_sources.py, docs/research/seeds.yaml,
  policy/denylist.yaml)
- 2026-11-19: **Carbon kinds + `factor_table` adapter.** New kinds `emission_factor` (record) and
  `grid_carbon_intensity` (observation, series `grid_ci:<ISO2>:<period>:<basis>`); adapter
  `factor_table` (CSV, XLSX via calamine; one conditional request per file, raw first, mapping
  per dataset in code). On: od-desnz-ghg-factors, od-owid-food-ghg, carbon-owid-grapher-ci (ramp-3).
  Owner decisions on: tr-flix-methodology, carbon-breeam-projects (Crawl-delay 10),
  carbon-greenbooklive. Off with reasons: Ember (CSV host robots Disallow: /), SNCF (/api/
  disallowed), Base Carbone (static.data.gouv.fr disallowed), Climatiq OEFDB (repo gone),
  Electricity Maps (needs key), EPA eGRID/EF Hub, AU NGA, AGRIBALYSE (adapter pending).
- 2026-11-19: adapter `ember_api` -> `grid_carbon_intensity` (annual + monthly, life-cycle, series
  `grid_ci:<ISO2>:<period>:lifecycle`). Source carbon-ember-api on (ramp-3), two rows (yearly from
  2000, monthly from 2019-01, no entity_code = every country), weekly, daily_budget 10. Owner key in
  Secret Manager `crawler-src-ember-key` (new KEY_SECRET override in seeds_to_sources.py); sent as the
  `api_key` query parameter, redacted from stored URLs/redirects/errors; a row carrying it is refused.
- 2026-11-19: carbon factor tables, second batch: `factor_table` gains stacked-table anchors, ODS
  (calamine), CSV-in-ZIP and `region_code`; 8 sources on (eGRID, EF Hub, OWID travel mode + CO2,
  ORR 6100, FuelEconomy.gov, CONCITO, RIVM) + od-eea-co2-cars-avg via Eurostat sdg_13_31. US grid
  rates stay `emission_factor` (lb/MWh, per subregion/state; no unit conversion). Off: AU NGA (no
  response), AGRIBALYSE (robots disallow), FAOSTAT (robots 521), DfT TAG + NHTS (occupancy kind
  pending, owner). New read endpoints `/v1/carbon/factors` and `/v1/carbon/grid` (docs/Crawler-API.md).
- 2026-11-19: new record kind `occupancy` (persons per vehicle: mode, trip_purpose, basis
  per_vehicle_km|per_trip, includes_driver, year, as_of), official/reference only, adapter-only.
  `factor_table` gains `Output::Occupancy` (explicit row-label table), two-row headers and filled-down
  group cells. od-dft-tag-databook on (TAG A1.3.3.2: car by purpose + average, PSV total; ramp-3).
  od-nhts-occupancy stays off: its only published table is inside the SPA's JS bundle (and PDFs);
  computing it from microdata is not copying. New read endpoint `/v1/carbon/occupancy`
  (docs/Crawler-API.md, docs/wiki/Carbon-Footprint-Data.md).
- 2026-11-19: carbon factor tables, third batch. `factor_table` gains `Format::XlsxMerged` (sheet
  by name prefix; merged cells below the header fill their region), headers joined over up to
  four rows, `Geo::Iso2` / `Geo::Label`, `Year::FileNameLeading`; FILTER_VERSION factor-table-v4.
  On (ramp-3): od-mobitool-ch, carbon-aib-residual-mix, od-epa-supply-chain-ghg (41 travel NAICS,
  spend-based), od-owid-tourism-aviation. 16 more left off with precise reasons (Cornell CHSB: CDN
  robots 403; impactco2: no units/years in the CSV; PDFs; wide/block layouts; signed redirects; DNS).
  Review fixes: a file whose rows were read but none mapped is an error (format drift), mobitool
  is refused unless its dropdowns say GWP100a / pkm (`preamble_require`), mobitool and EPA mappings
  match only their edition (v3.1, v1.3, named in the dataset label), mobitool flights carry no
  country code, EPA cruise lines are `other`; od-owid-tourism-aviation is now restricted and off (UN
  Tourism input terms are personal/non-commercial; mapping kept; on again if the owner accepts). Process lesson: a single 1 KB range GET
  reached the Cornell CDN (cf003.cdn.4science.cloud) before its robots.txt was read, because the
  download redirected there. Resolve redirect targets (HEAD or no-follow) and read each new
  host's robots.txt before any GET to it.
- 2026-11-20: place facts. New app section `facts` and record kind `place_fact` (one fact = one
  sentence in our own words + source link, `text` ≤ 280 chars, keyed by GeoNames id / Wikidata
  QID / country). Structured readers first, no LLM: `wikidata_facts` (EntityData item + up to 12
  referenced items for labels, all raw as subresources, one bundle resource as the primary
  capture, carrying the item's status so the row is reconciled in full and removed statements go
  stale; an item that is no longer a place gets an empty bundle; or dump lines without labels;
  English/`mul` labels only, quoted values keep `value_lang`) and `factbook_json` (CIA World
  Factbook final edition 2026-02-04 via factbook.json, CC0; country-only facts get the GeoNames
  id and QID of the Wikidata country fact at read time). A Wikipedia DYK/OTD hook reader was
  dropped in review: a hook is copied CC BY-SA article text and cannot be rewritten without a
  model, so `/v1/facts` serves only `templated`/`rewritten` text. GNIS feature history left
  for later (prose needs the rewrite step and a GNIS→GeoNames join). `GET /v1/facts`
  (geonames_id, wikidata_qid, country, fact_type, offset cursor). No sources added here: the
  1,499 research ADD rows come in a separate PR. See docs/wiki/Place-Facts-Data.md.
- 2026-11-20: **Place facts sources, part 1 of 5 (global + structured readers).** 235 sources
  (`_facts`, category `facts`): 117 on (od-factbook-json on factbook_json and od-wikidata-entitydata
  on wikidata_facts, ramp-3 only; the rest html/rss/sitemap in ramp-2 and ramp-3, place_fact +
  article, no LLM for share-alike or AI-reserved), 118 off (hook adapter removed 37: the
  `wiki-facts-*` Wikipedia DYK/OTD/unusual-articles pages, share-alike text, need an own-words
  extractor; adapter pending 62, needs owner key 4, owner decision 15). Of 165 FACTS_REUSE sources, 137 get
  `facts` + place_fact (facts_reuse_on: LLM kinds on non-share-alike text, or a place_fact adapter);
  wikidata-dump gets wikidata_facts (still off). 48 terms-ban hosts denylisted (with livehistoryindia.com,
  Peepul Tree's other domain); the S3WaaS/a2i families get 20 requests/day per host; tb-uzbekistan off (terms ban automated copying). Europe, Asia, Americas and
  Africa/Middle East/Oceania follow in their own PRs. (scripts/seeds_to_sources.py,
  docs/wiki/Place-Facts-Data.md)
- 2026-11-20: **Place facts sources, part 2 of 5 (Europe, Russia, Turkey, Central Asia and the Caucasus).** 502 sources (`_facts`,
  category `facts`): 395 on (html/rss/sitemap, ramp-2 and ramp-3), 107 off (adapter pending 57, enable after a check from the crawler VM 9, needs owner key 5, owner decision 36).
  (scripts/seeds_to_sources.py, docs/research/seeds.yaml)
- 2026-11-20: **Place facts sources, part 3 of 5 (East, Southeast and South Asia).** 505 sources (`_facts`,
  category `facts`): 416 on (html/rss/sitemap, ramp-2 and ramp-3), 89 off (adapter pending 24, enable after a check from the crawler VM 62, needs owner key 2, owner decision 1).
  (scripts/seeds_to_sources.py, docs/research/seeds.yaml)
