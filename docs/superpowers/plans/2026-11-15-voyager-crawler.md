# Voyager Crawler Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task by task. Steps use checkbox (`- [ ]`) syntax for tracking. Each P1 task is executed by its own agent in its own git worktree and is TDD within its crate: write the failing test, watch it fail, implement, watch it pass, commit.

**Goal:** a polite, raw-first, versioned, agentic travel-data crawler + index + read API, running 24x7 on GCP.

**Architecture:** one Rust binary `vcrawl` with roles (scheduler, fetcher, processor, embedder, api, maintenance, admin, migrate) sharing Postgres 17 + pgvector and GCS. Every cross-crate trait lives in `vc-core`, so the crates are built in parallel against those traits and the testkit fakes, and are wired together only in `vcrawl` (P2a).

**Tech stack:** Rust 1.99.0 (edition 2024), tokio, reqwest 0.13, chromiumoxide 0.9 + `chromedp/headless-shell`, texting_robots 0.2, quick-xml 0.42, feed-rs 3, scraper 0.29, dom_smoothie 0.19, whatlang 0.18, warc 0.4 / own writer, zstd 0.14, sqlx 0.9 + pgvector 0.4, official `google-cloud-storage` 1.x, Vertex REST via reqwest + `google-cloud-auth`, axum 0.8, tracing, figment, wiremock, testcontainers 0.28, insta, proptest, cargo-fuzz, fail.

**Spec:** `docs/superpowers/specs/2026-11-15-voyager-crawler-design.md` (v3; **§17 overrides earlier sections**). Interface decisions: `docs/reviews/r5_builder_check.md` (adopted).

## Global Constraints

- **No bot-detection evasion.** One UA: `VoyagerBot/<ver> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)`. A challenge, 401 or 403 means detect → park; never retry with another identity. `deny.toml` bans `spider_fingerprint`, `ua_generator`, `chrome_stealth`, `wreq`, `rquest`, `spider`.
- Every network request to a crawl target needs a `ClaimPermit` (only `vc-frontier` mints one; `clippy.toml` `disallowed-methods` enforces this), plus a lease check and a global rate token per request.
- Raw first: nothing derived from a capture becomes visible before `vc_commit_segment` commits it (§5.5, §17.2).
- Toolchain `1.99.0` (`rust-toolchain.toml`), edition 2024; `cargo clippy --workspace --all-targets -- -D warnings`; `cargo fmt --check`.
- Dependencies are only added in the root `[workspace.dependencies]`, by the controller. Agents never run `cargo update`.
- An agent writes only the paths it owns (`docs/ownership.md`); `vc-core` is frozen after P0.
- Every setting has a default (`VC_*` env overrides TOML). No secrets in the repo.
- Logs never contain page content or tokens.
- Git identity: `rupakc <4364388+rupakc@users.noreply.github.com>`. Commit trailer: `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. PR body ends with `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Every `gh` call passes `-R rupakc/travel-planner-crawler`. Wait for CI with `gh pr checks <n> --watch --fail-fast --interval 60`. Don't hand-roll a polling loop.

## Review Focus

1. **A site that changes robots.txt mid-crawl to disallow a path** → no further requests to that path once the cache refreshes, and records derived from it are tombstoned. Test: `vc-policy::robots_refresh_tombstones` + E2E oracle (Task 14).
2. **A redirect chain that crosses to another domain, or to a private IP** → it isn't followed inline; a cross-key target is enqueued and a private IP is blocked. Test: `vc-fetch::redirect_cross_key_enqueued`, `redirect_to_private_ip_blocked` (Task 4).
3. **A process killed between the spool fsync and the segment commit** → after restart there's no lost capture, no duplicate, and no 304 without a stored body. Test: `vcrawl` failpoint suite (Task 15).
4. **A page with prompt-injection text** (a fake emergency number, a phishing URL, "ignore previous instructions") → the verbatim/grounding checks drop the planted fields; high-stakes kinds from non-official sources are rejected. Test: `vc-extract::injection_fixtures` (Task 9).
5. **100 concurrent LLM reservations against a nearly exhausted cap** → the cap is never exceeded on either the day or the month row. Test: `vc-llm::budget_race_never_exceeds_cap` (Task 8).

---

## Phase P0: Foundation (two agents in parallel, disjoint paths)

### Task 0A: Rust workspace, vc-core contracts, migrations, minimal testkit

**Files:** create the root `Cargo.toml` (workspace, `[workspace.dependencies]` with every crate in Tech Stack pinned, plus `async-trait`, `thiserror`, `anyhow`, `serde`, `serde_json`, `bytes`, `ulid`, `blake3`, `chrono`, `url`, `psl`, `jsonschema`, `fail`, `proptest`, `insta`), `Cargo.lock`, `rust-toolchain.toml`, `deny.toml`, `clippy.toml`, `rustfmt.toml`, `crates/<all 17 crates of spec §5.1 minus vc-egress>/` (each a compiling stub: `Cargo.toml` listing its deps with `workspace = true`, `src/lib.rs` with `todo!()` impls), `testkit/`, `migrations/0001..0011_*.sql`, `crates/vc-config/src/sections/<crate>.rs`.

**Produces (frozen after P0):** everything in r5 §(c) "Shared types" and the trait sketches, exactly as named there, with these amendments:
- No `vc-egress` / HMAC. `vc-core::render` has `trait Renderer { async fn render(&self, p: &ClaimPermit, url: &CanonicalUrl, budget: RenderBudget) -> Result<RenderOutcome> }`; `RenderOutcome { dom_html: Bytes, exchanges: Vec<Exchange>, blocked: Vec<(CanonicalUrl, BlockReason)> }`.
- `Frontier::claim(shard, max_urls)` with `max_urls ≤ 5`; `Frontier::mark_fetched(&ClaimPermit, &UrlKey, next_fetch_at) -> Result<(), LeaseLost>` (the `fetched_pending` transition, §17.2).
- `BudgetLedger::reserve(purpose, est)` reserves against both the day and the month row (§17.4); `Purpose` enum = `Extraction | Embedding | Triage | Auditor | Scout | Place`.
- `trait BatchRunner { async fn prepare(...) ; async fn submit(...); async fn reconcile(...); async fn ingest(...) }` in `vc-core::llm` (§17.3).
- `SourceSpec` (serde, `deny_unknown_fields`) with fields:
  - `id`, `name`, `category`, `source_class: SourceClass`, `licence: Licence { class: LicenceClass, spdx: Option<String>, attribution: String, url: Option<Url> }`, `method: Method { Html, Rss, Sitemap, Api, Dump }`;
  - `seeds`, `sitemaps`, `feeds: Vec<Url>`, `adapter: Option<String>`, `include`, `exclude: Vec<String>` (regex), `max_depth: u8 = 2`;
  - `kinds`, `llm_kinds: Vec<KindId>`, `render: RenderMode { Never, Auto, Always } = Never`, `cadence: Cadence`, `min_delay_s: u32 = 5`, `daily_budget: u32 = 3000`, `politeness_key: Option<String>`;
  - `enabled: bool = true`, `needs_key: bool = false`, `key_secret: Option<String>`, `max_retention_days: Option<u32>`, `display_requirements: Option<String>`, `notes: Option<String>`.
  - Enums:
    - `LicenceClass = Open | ShareAlike | FactsOnly | ApiTerms | Restricted`
    - `SourceClass = Official | Reference | Editorial | Community | CommercialApi`
    - `Cadence = Hourly | H2 | H6 | H12 | Daily | Weekly | Monthly | Quarterly | Yearly | OnDemand`
- Migrations: the r5 list `0001`–`0011`, with the §17 amendments:
  - `url_state.state ∈ {due, leased, fetched_pending, gone, tombstoned, disabled}`, plus `lease_epoch`, `last_captured_at`;
  - `batch_jobs` per §17.3; `llm_budget(period text PK, cap, reserved, spent)`; `llm_cache`;
  - `changes(seq, xid xid8 DEFAULT pg_current_xact_id(), …)`;
  - SQL functions `vc_commit_segment(jsonb)`, `vc_enqueue(...)`, `vc_ensure_partitions(months_ahead int)`;
  - every migration begins with `SET lock_timeout = '5s'`.
- testkit (minimal): `testkit::pg() -> PgPool` (testcontainers `pgvector/pgvector:pg17`, migrated; reuses one container per test binary), `FakeClock`, in-memory fakes `MemFrontier`, `MemBlobStore`, `MemQueue`, `FakeLlm` (scripted responses), `MemLedger`, and `request_log::{RequestLogEntry {key, url, started_at, ended_at, status, ua}, RequestLog}` (the frozen oracle schema).

- [ ] **Step 1:** Write `crates/vc-core/tests/contracts.rs`:
  - `canonical_url_is_idempotent` (proptest: canon(canon(u)) == canon(u));
  - `url_key_is_16_bytes_and_stable` (a known vector: `UrlKey::of("https://example.com/")` equals the hex fixed in the test, computed once by blake3);
  - `use_policy_meet_only_restricts` (proptest over signal sets: meet(a,b) ≤ a and ≤ b);
  - `source_spec_rejects_unknown_fields`.
- [ ] **Step 2:** Write `testkit/tests/migrations.rs`:
  - `migrations_apply_on_empty_db`;
  - `commit_segment_guard_is_monotonic` (calling `vc_commit_segment` with an older `captured_at` leaves the validators unchanged);
  - `partitions_exist_for_current_and_next_two_months`.
- [ ] **Step 3:** Run `cargo test --workspace`. It must FAIL (`todo!`/missing).
- [ ] **Step 4:** Implement the vc-core types, migrations, config sections and testkit.
- [ ] **Step 5:** `cargo test --workspace` passes; `cargo clippy --workspace --all-targets -- -D warnings` is clean; `cargo deny check` passes.
- [ ] **Step 6:** Commit, open a PR, wait for CI green, merge.

### Task 0B: Repo scaffolding, Claude setup, policy and sources, CI

**Files:**
- `CLAUDE.md`, `MEMORY.md` (repo decision ledger: dated entries, one per decision, linking to the spec/reviews);
- `.claude/settings.json` (allow cargo/gh/docker/git read-only commands; deny `cargo update`, `git push --force`);
- `.claude/agents/{crate-implementer,politeness-auditor,source-onboarder,rust-reviewer}.md`;
- `.claude/skills/{add-source,add-kind,run-e2e,check-politeness,deploy-status}/SKILL.md`;
- `README.md`, `BOT.md`, `CONTRIBUTING.md`, `SECURITY.md`, `.github/CODEOWNERS`, `.github/ISSUE_TEMPLATE/opt-out.yml`, `.github/dependabot.yml`, `.github/pull_request_template.md` (no-evasion checklist);
- `.github/workflows/ci.yml`: fmt, clippy, test (Postgres via testcontainers on ubuntu-latest), cargo-deny, ownership check on `agent/*` branches, shellcheck; jobs pinned by SHA;
- `docs/ownership.md`, `scripts/check-ownership.sh`;
- `policy/denylist.yaml` (all `allowed: false` seeds + brand domains: booking.com, expedia.*, hotels.com, vrbo.com, tripadvisor.*, skyscanner.*, kayak.*, airbnb.*, reddit.com, getyourguide.*, viator.com, flyertalk.com, google.com/travel, momondo.*, trivago.*, agoda.com, priceline.com, plus the blocked feeds; each entry `{host, reason, source}`);
- `policy/subresource_allowlist.yaml` (cdnjs.cloudflare.com, cdn.jsdelivr.net, unpkg.com, ajax.googleapis.com, fonts.googleapis.com: CSS only);
- `sources/*.yaml`: one per allowed seed, generated by `scripts/seeds_to_sources.py` from `docs/research/seeds.yaml`, mapping licence strings to `LicenceClass` per spec §5.9/§17.8, `restricted` → `enabled: false`; Open-Meteo is disabled and replaced by MET Norway;
- `schemas/source.schema.json` (JSON Schema mirroring the SourceSpec fields in Task 0A, verbatim).

**Produces:** the ownership map (paths → task id) that every P1 agent obeys; the CI that every PR must pass.

- [ ] **Step 1:** Write `scripts/tests/test_seeds_to_sources.py`:
  - every allowed seed yields a file valid against `schemas/source.schema.json`;
  - no file's host is in the denylist;
  - `wikivoyage*` → `share_alike`; `fcdo*` → `open` + `official`;
  - `ticketmaster` → `api_terms` + `commercial_api`.
- [ ] **Step 2:** Run it (FAIL), implement the script, generate, run it (PASS).
- [ ] **Step 3:** `bash scripts/check-ownership.sh` self-test: a fake diff touching a non-owned path exits 1.
- [ ] **Step 4:** Commit, PR, CI green (Rust jobs may be skipped until 0A merges; the workflow tolerates a missing workspace), merge. **0B merges after 0A** if both are ready.

---

## Phase P1: crates in parallel (one agent each, worktree `agent/<task>`)

Each P1 agent works only inside its own paths and migration block. It may add a `testkit/src/<crate>_fixtures.rs` file. It writes tests first, then implementation, then opens a PR, rebases on `main` and merges itself once CI is green. Cross-crate behaviour is tested only against testkit fakes.

### Task 1: vc-policy (pure)
Implements `Policy`:
- robots per RFC 9309 + our stricter 401/403 rule, the `VoyagerBot` group then `*`, AI-bot block → AI reservation;
- `use_policy` matrix §5.2;
- denylist matching (registrable domain + path prefix);
- SSRF (§5.2 list incl. decimal/octal/IPv4-mapped normalisation);
- challenge detector (status + `cf-mitigated`, Akamai/DataDome/Imperva/PerimeterX interstitial markers, captcha forms; fixtures in `testkit/fixtures/challenges/`).

Tests:
- `robots_google_corpus` (Google's robots test cases vendored);
- `robots_401_403_blocks_key`; `robots_5xx_disallows`; `robots_404_allows`;
- `ai_bot_block_sets_ai_reservation`;
- `ssrf_blocks_all_listed_ranges` (proptest over encodings);
- `challenge_fixtures_detected`, `normal_pages_not_challenge`;
- `robots_refresh_tombstones` (returns the newly disallowed path set);
- fuzz target `fuzz_robots`.

Coverage ≥ 85%.

### Task 2: vc-frontier (Postgres) — migration block 11xx
`PgFrontier` implements `Frontier` + `LeaseCheck`:
- the claim SQL (§5.3 + §17.2), epoch fencing, `extend`, the per-request `take_rate_token` (token bucket row, default 4/s), `mark_fetched`, `complete` (sets the key's next_allowed_at = max(deadline, end + delay));
- park ladder 24 h → ×2 → 30 d; delay = max(robots crawl-delay, min_delay, 10 × EWMA, penalty) ≤ 1 h;
- batched enqueue (1k rows, `ON CONFLICT DO NOTHING`), the Cho–Garcia-Molina estimator, `mint` is the only `ClaimPermit` constructor.

Tests:
- `two_claimers_never_share_a_key` (2 tasks × 500 claims);
- `stale_epoch_completion_rejected`; `lease_expiry_returns_leased_urls_to_due`;
- `fetched_pending_never_released`;
- `rate_token_global_across_pools` (2 pools, measured ≤ 4/s ± 10%);
- `park_ladder_doubles_to_30d`;
- `delay_respects_crawl_delay`; `budget_exhaustion_blocks_key`;
- `enqueue_dedupes`; `estimator_converges` (proptest).

Coverage ≥ 85%.

### Task 3: vc-store — block 12xx
- WARC/1.1 writer/reader (request, response, revisit, resource, metadata; decoded bodies per r5 Q2) and CDXJ;
- `Spool` (append + fsync, replay); `SegmentRoller` (128 MB / 10 min, ULID name, segment-at-a-time reads);
- `GcsBlobStore` (official crate, `ifGenerationMatch=0`, crc32c), `FsBlobStore`;
- `PgSegmentCommitter` calling `vc_commit_segment`; prefix routing `segments/` vs `ephemeral/` (noarchive/TDM) vs `text/` vs `cdxj/`;
- purge rewrite (`rewrite_segment_without(records)`).

Tests:
- `warc_roundtrip` (proptest), `cdxj_roundtrip`;
- `spool_replay_after_crash` (failpoint between append and roll);
- `upload_idempotent_same_bytes`, `upload_mismatch_errors`;
- `no_capture_visible_before_commit`;
- `revisit_on_same_payload`;
- `purge_rewrite_removes_record_keeps_others`;
- `gcs_integration` against fake-gcs-server (container);
- fuzz `fuzz_warc_reader`.

### Task 4: vc-fetch
`ReqwestPermitClient` implements `PermitClient`:
- conditional GET; streaming decoded-byte caps incl. nested gzip;
- the content-type allowlist; DNS resolved and pinned per hop via the `Policy::ssrf_check` resolver;
- ≤ 5 redirects, robots per hop, **cross-key redirect returns `FetchError::CrossKeyRedirect(url)`** (the caller enqueues);
- timeouts = min(own, deadline − now); a lease check + rate token before every request; the UA constant; challenge → `FetchError::Challenge`.

Tests (wiremock):
- `conditional_get_304`; `gzip_bomb_capped`; `nested_gz_capped`;
- `redirect_cross_key_enqueued`; `redirect_to_private_ip_blocked`; `redirect_disallowed_path_stopped`;
- `timeout_bounded_by_deadline`; `ua_is_exact_constant`;
- `lost_lease_aborts_before_request`;
- `retry_after_parsed` (seconds and HTTP date).

### Task 5: vc-render
`ChromiumRenderer` implements `Renderer` via chromiumoxide + CDP `Fetch` interception (§17.1):
- every paused request goes through the policy checks then `PermitClient`; blocked types fail;
- sub-resource cache (content-addressed, 24 h);
- auto-scroll; allowlisted expand-click selectors from the SourceSpec; network idle = 1 s without a paused request;
- budget 90 s / 60 sub-requests; the UA set to the constant.
- Ships `deploy/chromium/` (compose service: `chromedp/headless-shell` pinned by digest, sandbox on with seccomp profile, read-only, internal network only).

Tests:
- unit tests with a mock CDP for the decision logic;
- integration tests (`#[ignore]` locally, run in CI with the container) `spa_page_rendered`, `subrequest_to_private_ip_blocked`, `disallowed_subresource_failed`, `third_party_not_allowlisted_failed`, `render_budget_enforced`, `all_requests_recorded_as_exchanges`.

### Task 6: vc-parse
`ContentHandler`s (HTML, RSS/Atom, sitemap + index + gz, JSON, CSV) and `HandlerRegistry::for_media`:
- main content (dom_smoothie); JSON-LD + microdata → `serde_json::Value`;
- links with anchor text; body UseSignals (meta robots, TDMRep link); language (whatlang); simhash64;
- the media-type sniff; no DTDs; DOM depth/node caps.

Tests:
- `jsonld_event_extracted`; `microdata_place_extracted`;
- `sitemap_index_and_gz`; `feed_rss_and_atom`;
- `meta_noindex_signal`; `xml_entity_expansion_rejected`; `dom_depth_capped`;
- `simhash_near_duplicates_close` (proptest);
- fuzz `fuzz_html`, `fuzz_sitemap`, `fuzz_feed`.

### Task 7: vc-kinds + schemas + prompts
`schemas/<kind>.v1.json` + `prompts/<kind>.md` + a `kinds.toml` entry for each of the 19 kinds in spec §5.7, and the `FileKindRegistry`. Policy per §17.8: verbatim fields (phones, URLs, prices, dates, emergency numbers) and allowed source classes. `fx_rate` and `weather_normal` use `storage = observation`.

Tests:
- `every_kind_has_schema_prompt_policy_fixture` (registry test);
- `schemas_are_valid_draft2020`; `fixture_records_validate`;
- `high_stakes_kinds_official_only`.

### Task 8: vc-llm — block 14xx
- `VertexLlm` (REST `generateContent` on the global endpoint, `responseSchema`, `labels {app, purpose}`, `maxOutputTokens`, thinking budget off/minimal, ADC token via google-cloud-auth);
- `VertexEmbedder` (text-embedding-005, 768);
- `PgLedger` (§17.4 two-row CTE, dynamic daily cap, purpose shares, 1 h stale settle except batch);
- `llm_cache`; `VertexBatchRunner` (§17.3); the 6 h breaker on billing 403/429; the startup model probe; `VC_LLM_ENABLED=false` → every call returns `LlmError::Disabled`.

Tests:
- `budget_race_never_exceeds_cap` (100 concurrent reservations, PG);
- `dynamic_daily_cap`; `purpose_share_enforced`;
- `stale_reservation_settled_as_spent`; `batch_reservation_not_auto_settled`;
- `batch_crash_after_submit_reconciled_no_resubmit` (fake Vertex via wiremock);
- `batch_results_mapped_by_key_missing_nacked`; `batch_72h_cancel`;
- `billing_403_opens_breaker`; `labels_on_every_request`;
- `disabled_never_calls_network`; `cache_hit_no_call`.

### Task 9: vc-extract
The pipeline: structured-first (JSON-LD/microdata → kinds via mapping tables); then LLM (trimmed ≤ 1,200 tokens, ≤ 3 chunks, untrusted-data system prompt); JSON Schema validation with one repair attempt.
- Verbatim checks: normalised phone/URL/price/date must occur in the source text; URL domain rule.
- Source-class gate; cross-source conflict flag.
- `FilePlaceResolver` (exported GeoNames subset, country hints; LLM disambiguation under budget).
- Output `ExtractedRecord` with a stable `record_key` (§5.6).

Tests:
- `injection_fixtures` (fake emergency number, phishing URL and "ignore instructions" pages → planted fields dropped);
- `blog_cannot_produce_visa_rule`;
- `record_key_stable_across_runs`; `structured_data_needs_no_llm`;
- `unresolved_place_empty_ids`; `use_policy_blocks_llm`.

### Task 10: vc-sources + adapters — block 17xx
- `SourceSpec` loader + validator (`vcrawl admin source validate` logic as a library fn); seeding sources into PG.
- Adapters behind `SourceAdapter`, each streaming via `PermitClient` and writing raw `Exchange`s/manifests to `CaptureSink`:
  - `wikivoyage_dump` (pages-articles, streamed, filtered);
  - `wikidata_sparql` (emergency numbers P2852, plugs P2853/P2884, airports);
  - `ecb_fx`; `fcdo_content_api` (+ Atom change feed); `state_dept_advisories`; `canada_advisories`;
  - `met_norway`; `ticketmaster_discovery` (key from a secret, `api_terms`);
  - `nager_holidays`; `mobility_database_catalog`; `ourairports_csv`; `se_travel_dump` (CC BY-SA; author → attribution only).

Tests: one per adapter, with recorded fixtures via wiremock (`<adapter>_parses_fixture`, `<adapter>_writes_raw_first`), `all_sources_yaml_load_and_validate`, `dump_stream_respects_extend`.

### Task 11: vc-index — block 15xx
- `PgRecordSink` (records, versions on `data_hash` change, observations, entities + claims + redirects with the v1 resolution rule and licence-compatible merges only, `changes` rows) in one transaction.
- FTS columns; embeddings table + per-model HNSW (halfvec, m=16).
- Hybrid query (RRF of `ts_rank` + vector × quality × freshness, place/kind/licence filters).
- Purge/tombstone API.

Tests:
- `version_only_on_change`; `share_alike_not_merged_with_open`;
- `entity_redirect_followed`; `observation_series_append`;
- `hybrid_ranks_exact_place_first`; `tombstoned_never_returned`;
- `changes_ordered_by_xid` (concurrent writers).

### Task 12: vc-api — block 16xx
axum `/v1` per spec §5.11 + §17.9:
- per-client bearer tokens (argon2-hashed in `api_clients`);
- TLS (rustls, cert/key from `SecretSource`); per-client rate limit;
- OpenAPI (utoipa) with an insta snapshot; `/v1/changes` xid cursor; `/metrics` on localhost only;
- `facts_only` items never carry text beyond the 300-char excerpt.

Tests:
- `unauthenticated_401`; `openapi_snapshot`;
- `changes_cursor_no_skips`; `facts_only_excerpt_capped`;
- `licence_and_provenance_on_every_item`; `redirected_entity_301`.

### Task 13: vc-queue + vc-obs + vc-secrets — block 13xx/18xx
- `PgWorkQueue` (SKIP LOCKED, lease, ack/nack with backoff, dedupe key, dead-letter after 8 attempts).
- vc-obs: tracing JSON subscriber with field allowlist, Prometheus registry, progress-based health (`ProgressTracker`), heartbeat writer.
- vc-secrets: `SecretManagerSource` (REST + ADC), `EnvSource` for tests.

Tests:
- `queue_at_least_once_redelivery`; `dead_letter_after_8`; `dedupe_key`;
- `health_fails_without_progress`; `log_fields_allowlisted_no_body`.

### Task 14: testkit full: fake web, fake Gemini, oracles
- `fake-web`: an axum server hosting the 7 scripted sites of spec §12. Each site can change content, robots and status between cycles. Every request is logged to the frozen `RequestLog`.
- `fake-gemini`: Vertex online + batch wire format, scripted responses, usage metadata.
- The `fake-gcs-server` helper.
- Oracles:
  - `assert_polite(&RequestLog, &PolicySnapshot)`: per key, no overlap, gaps ≥ delay, no disallowed/denylisted paths, nothing after a park;
  - `assert_raw_first(pool, blobstore)`.
- Ships a docker image `testkit-fake-web` for E2E.

Tests:
- `oracle_detects_overlap`; `oracle_detects_short_gap`; `oracle_detects_disallowed`;
- `fake_gemini_batch_lifecycle`.

### Task D: deploy kit + bootstrap (paths `deploy/`, `infrastructure/`)
`deploy/compose.yaml` (all roles, memory limits per spec §6, networks `db`, `render` (internal), `egress`), `deploy/host/{first-boot.sh,deploy.sh,watchdog.sh}` + systemd units:
- `deploy.sh`: `:prod` digest → `cosign verify` pinned to `https://token.actions.githubusercontent.com` + `rupakc/travel-planner-crawler/.github/workflows/deploy.yml@refs/heads/main` → pull → `migrate` → roll per role → health → roll back;
- wal-g config.

`infrastructure/bootstrap.sh` (idempotent, `--dry-run`, `--rotate-tls`) creates exactly spec §7 + §17.5 (VM, disks, static IPs, SA + custom roles incl. the drill role, buckets with lifecycle per §17.7, secrets, firewall, snapshot schedule, notification channel, alert policies, budget, labels), plus `infrastructure/tests/` (bash-mock tests in the main repo's style). `.github/workflows/deploy.yml` (build, Trivy, cosign sign, promote `:prod`; no GCP credentials).

Tests:
- `bootstrap --dry-run` golden output;
- `deploy.sh` with a fake cosign/docker: `rollback_on_unhealthy`, `refuses_unsigned`;
- shellcheck clean.

## Phase P2a: wiring and E2E (after P1 merges)

### Task 15: vcrawl binary
Roles + admin CLI (clap):
- `scheduler` (sources → seeds/sitemaps/feeds → enqueue, recrawl, robots refresh);
- `fetcher` (claim loop: robots_due first; adapters; render when flagged; spool; `mark_fetched`; segment roller; cross-key redirect → enqueue; challenge → park);
- `processor`, `embedder`, `api`;
- `maintenance` (partitions, purge, ephemeral expiry, restore drills, digest, Parquet export stub);
- `admin` (`status`, `source validate|list|approve|disable`, `purge --url|--host|--author`, `rebuild-captures`, `reextract --kind`);
- `migrate [--contract]`.

Tests:
- failpoint suite at each §5.5 step + restart (`no_lost_no_duplicate_no_orphan_304`);
- `rebuild_captures_from_cdxj_equivalent`;
- `migration_n_minus_1` (previous image's queries against the new schema, via a recorded query set).

### Task 16: E2E in CI
`e2e/compose.yaml` = the stack + fake-web + fake-gemini + fake-gcs. Two fetchers, a fixed number of cycles. Assertions:
- `assert_polite`, `assert_raw_first`;
- a version bump on a content change; the API answers with provenance;
- the challenge host is parked; a SPA page is rendered; a private-IP sub-request is blocked.

Runs on every PR (≤ 10 min).

## Phase P2b: agentic loops
### Task 17: link triage, source scout (propose only), quality auditor
Under the purpose budgets (§17.4).

Tests:
- `scout_never_crawls_before_approval`; `scout_rejects_denylisted_brand_domain`;
- `triage_cached_per_pattern`; `auditor_lowers_quality_score`.

## Phase P3: provisioning and go-live
### Task 18
1. Run `bootstrap.sh --dry-run` and review it, then run it for real.
2. First deploy with `VC_LLM_ENABLED=false`.
3. Enable 10 sources, watch for 24 h (politeness metrics, parks, captures), then enable everything.
4. Live smoke; alert test-fire; restore drill.
5. LLM is switched on only after the owner raises the project cap and sets `VC_PROJECT_CAP_USD`.

## Phase P4: Voyager integration (main repo; its own design note and PR, spec §13)
