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
