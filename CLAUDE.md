# Voyager Crawler

Polite, raw-first, versioned travel-data crawler + index + private read API for Voyager
(rupakc/travel-planner). One Rust binary `vcrawl`, Postgres 17 + pgvector, GCS, Gemini on
Vertex. Public MIT repo `rupakc/travel-planner-crawler`; runs on one GCP VM in
`voyager-prod-ohzkl` (us-east1) for about $100/month.

Source of truth: spec `docs/superpowers/specs/2026-11-15-voyager-crawler-design.md`
(**§17 overrides earlier sections**), plan `docs/superpowers/plans/2026-11-15-voyager-crawler.md`,
interface decisions `docs/reviews/r5_builder_check.md`, decision ledger `MEMORY.md`.

## Hard rules (politeness contract and anti-goals)

Breaking one of these is a failure even if the code "works". The `politeness-auditor`
agent and the `check-politeness` skill review diffs against them.

1. **No bot-detection evasion, ever.** One UA constant:
   `VoyagerBot/<ver> (+https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md)`,
   for reqwest and Chromium. No UA/header/TLS-fingerprint rotation, no proxies or IP
   rotation, no stealth, no CAPTCHA solving. `deny.toml` bans `spider_fingerprint`,
   `ua_generator`, `chrome_stealth`, `wreq`, `rquest`, `spider`.
2. **Challenge, 401 or 403 → detect → park the key** (24 h, doubling to 30 d). Never
   retry with a browser or another identity.
3. **Every network request to a crawl target needs a `ClaimPermit`** (only `vc-frontier`
   mints one; `clippy.toml` `disallowed-methods` enforces it) **plus a lease check and a
   global DB rate token per request** (`PermitClient::pace`). That includes robots.txt,
   sitemaps, feeds, adapters, render sub-requests, admin validate and scout checks.
4. **robots.txt per RFC 9309, stricter:** 401/403 on robots → that origin do-not-crawl
   (robots.txt speaks for its own origin; other hosts of the key keep their own robots.txt);
   a challenge on robots → whole key do-not-crawl; 5xx → disallow; `VoyagerBot` group else `*`; AI-bot block → AI
   reservation. Crawl-delay, X-Robots-Tag, meta robots, TDM, Content-Signal are honoured
   via the UsePolicy matrix (spec §5.2), which can only restrict (meet).
5. **Delay per key** = max(Crawl-delay, `min_delay_s` ≥ 5 s, 10 × EWMA, penalty) ≤ 1 h;
   one request in flight per key; cross-key redirects are enqueued, never followed inline.
6. **Never fetch `policy/denylist.yaml`** hosts (brand domains included). SSRF guard on
   every hop (private ranges, metadata server, ports 80/443 only).
7. **Raw first:** nothing derived from a capture is visible before `vc_commit_segment`
   commits it (spool fsync → segment upload → one commit tx).
8. **Licences:** `restricted` is never ingested; `share_alike` is never merged with other
   licences; `facts_only` serves facts + headline + ≤ 300-char excerpt + link only.
   High-stakes kinds (`visa_rule`, `advisory`, `emergency_numbers`) only from `official`
   sources (numbers also from two agreeing `reference` sources).
9. **LLM spend never threatens Voyager's AI:** `VC_LLM_ENABLED=false` by default; crawler
   monthly cap ≤ 50% of `VC_PROJECT_CAP_USD`; every call reserves through the ledger
   (day + month rows) and carries `labels {app: voyager-crawler, purpose}`.
10. **Logs never contain page content, tokens or secrets.** No secrets in the repo.
11. Every item must be purgeable (opt-out SLA in BOT.md: API 24 h, raw 7 d, backups 21 d).

## Architecture map

```
sources/*.yaml + adapters → scheduler → Postgres frontier (claims, fenced leases, rate tokens)
  → fetcher (vc-fetch; vc-render = Chromium with NO network, CDP Fetch interception)
  → spool (fsync) → segment (WARC/1.1 + zstd, CDXJ) → GCS → vc_commit_segment (one tx)
  → processor (vc-parse handlers → structured-first → vc-extract LLM + grounding checks)
  → vc-index (records/versions/entities/observations, FTS + pgvector) → vc-api /v1
```

- `crates/vc-core`: every cross-crate trait and shared type (frozen after P0). P1 crates
  depend only on vc-core, vc-config and dev-dep testkit (star graph); `crates/vcrawl` wires
  implementations together (roles: scheduler, fetcher, processor, embedder, api,
  maintenance, admin, migrate).
- `vc-config`: figment, TOML + `VC_*` env; one section file per crate; every setting has
  a default.
- `testkit/`: `pg()` testcontainer (pgvector/pgvector:pg17, migrated), `FakeClock`,
  in-memory fakes of every trait, frozen `request_log` schema for the politeness oracle.
- `migrations/`: 0001–0099 P0; blocks frontier 11xx, store 12xx, queue 13xx, llm 14xx,
  index 15xx, api 16xx, sources 17xx, ops 18xx, vcrawl 19xx. Expand-only;
  `SET lock_timeout = '5s'` first; no FKs into partitioned tables; contract migrations in
  `migrations/contract/`.
- `sources/`, `schemas/`, `prompts/`, `kinds.toml`, `policy/`: data, not code.

## Ownership (parallel agents)

- Write only the paths your task owns: `docs/ownership.md`. CI runs
  `scripts/check-ownership.sh` on `agent/*` branches; branch = `agent/<task>[-slug]`.
- `vc-core` is frozen after P0: ask the controller for changes.
- Dependencies only in the root `[workspace.dependencies]`, by the controller. **Never run
  `cargo update`.** Lock conflict → take `main`'s lock, `cargo metadata`, push.
- No P1 task edits `crates/vcrawl`. Cross-crate behaviour is tested through testkit fakes.

## Commands

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                 # Docker needed (testcontainers)
cargo test -p vc-policy <name>         # one crate / one test
cargo deny check
python3 -m pytest -q scripts/tests     # sources/policy/schema generator
bash scripts/check-ownership.sh --self-test
bash scripts/check-ownership.sh --base origin/main      # your branch vs main
python3 scripts/seeds_to_sources.py    # regenerate seed-derived sources/*.yaml
python3 scripts/sources_doc.py         # regenerate docs/SOURCES.md + docs/wiki/Sources.md
shellcheck -x $(git ls-files '*.sh')
```

Toolchain `1.99.0` (`rust-toolchain.toml`), edition 2024. TDD: failing test first.

## Extending

- **New source:** skill `add-source` (agent `source-onboarder`). `sources/<id>.yaml` valid
  against `schemas/source.schema.json`; enums are snake_case; `restricted` → `enabled: false`;
  seed-derived sources are changed in `scripts/seeds_to_sources.py` tables.
  **Relevant and pertinent sites only** (owner rule, 2026-11-17): every source states which
  app section(s) it serves and why (required `relevance: {sections, why}`, closed section
  list; `vc_sources::relevance` rejects a missing/empty one, and news/blog feeds that are not
  travel-focused); no non-travel sites; the owner approves every addition. Scout proposals
  are never auto-approved. Regenerate `docs/SOURCES.md` with `python3 scripts/sources_doc.py`
  (CI checks it is current).
- **New API/dump:** `impl SourceAdapter` in `crates/vc-sources/src/adapters/`, register in
  `adapters::REGISTRY`, recorded wiremock fixture.
- **New kind:** skill `add-kind`: `schemas/<kind>.v1.json` + `prompts/<kind>.md` +
  `kinds.toml` entry (source classes, verbatim fields, record|observation) + fixture.
- **New media type / store / queue / LLM:** implement the vc-core trait and register it.

## Testing

Unit + proptest; fuzz (`cargo-fuzz`) for robots/sitemap/feed/HTML/WARC; integration on
PG testcontainers; failpoints (`fail`) at every raw-store step; E2E compose with fake
web/Gemini/GCS and the politeness + raw-first oracles (skill `run-e2e`). Review-focus
tests (plan "Review Focus") must never be weakened.

## Deploy model

CI has **no GCP credentials**. On `main`: build `ghcr.io/rupakc/voyager-crawler:<sha>`,
Trivy, cosign keyless sign, promote `:prod` only if CI passed. The VM's `deploy.sh`
(systemd timer) resolves `:prod` → digest, `cosign verify` (issuer
`https://token.actions.githubusercontent.com`, identity `deploy.yml@refs/heads/main`),
pulls, runs `vcrawl migrate`, rolls roles one by one on progress health, rolls back on
failure. Read API on the internal IP 8443 only. Skill `deploy-status`.

## Git / PRs

Identity `rupakc <4364388+rupakc@users.noreply.github.com>` (never change it). Commit
trailer `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. PR body ends with
`🤖 Generated with [Claude Code](https://claude.com/claude-code)`. Every `gh` call passes
`-R rupakc/travel-planner-crawler`. Wait for CI with
`gh pr checks <n> -R rupakc/travel-planner-crawler --watch --fail-fast --interval 60`.
Record new decisions in `MEMORY.md` (dated, one line each, with a link).
