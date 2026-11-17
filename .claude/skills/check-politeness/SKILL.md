---
name: check-politeness
description: Audit the current branch or a PR against the VoyagerBot politeness contract and anti-goals (no evasion, permit gate, robots, delays, denylist, SSRF, UsePolicy, raw-first, licences, LLM caps, log hygiene) with fast greps plus the politeness-auditor agent. Use before opening or merging any PR that touches crawling, rendering, policy, sources or extraction.
---

# Check politeness

The contract: `CLAUDE.md` hard rules; spec §2 anti-goals, §5.2, §17.1, §17.7, §17.8.

## 1. Mechanical checks (fast, run all)

```bash
BASE=origin/main
git fetch -q origin
git diff --name-only $BASE...HEAD
# a) only the UA constant; no other user agents or browser-mimicking headers
git diff $BASE...HEAD | grep -nE '^\+.*(User-Agent|user_agent|USER_AGENT|Mozilla/|Chrome/|Sec-CH-UA|navigator\.webdriver)' || echo "ok: no UA/header changes"
# b) no evasion crates or patterns
git diff $BASE...HEAD | grep -niE '^\+.*(stealth|fingerprint|ua_generator|wreq|rquest|spider_fingerprint|captcha|proxy|rotate|impersonat|undetect)' || echo "ok: no evasion terms"
# c) network clients only in vc-fetch (and render via PermitClient)
git diff $BASE...HEAD -- 'crates/*' ':!crates/vc-fetch/*' | grep -nE '^\+.*(reqwest::|hyper::|Client::new|ClientBuilder)' || echo "ok: no stray HTTP clients"
# d) permits minted only in vc-frontier/testkit
git grep -nE 'ClaimPermit::mint|\bmint\(' -- 'crates/*' ':!crates/vc-frontier/*' ':!crates/vc-core/*' || echo "ok: mint confined"
# e) no page content / tokens in logs
git diff $BASE...HEAD | grep -nE '^\+.*(info!|warn!|error!|debug!|trace!).*(body|html|text|token|secret|password)' || echo "ok: log fields look clean"
# f) policy data still valid
python3 -m pytest -q scripts/tests
```

Then the toolchain gates (bans live in `deny.toml`; `clippy.toml` holds `disallowed-methods`):

```bash
cargo deny check bans
cargo clippy --workspace --all-targets -- -D warnings
```

Every hit in a)–e) needs a justification or a fix. A hit is not automatically a violation (e.g. the UA constant itself, a test fixture named `challenge`), but you must look at each one.

## 2. Semantic review

Spawn the `politeness-auditor` agent with the branch or PR number. It returns PASS / PASS WITH NOTES / FAIL with file:line findings.

## 3. Tests that must stay green and unweakened

`vc-policy::robots_refresh_tombstones`, `robots_401_403_disallows_the_origin_not_the_key`, `robots_challenge_blocks_key`, `challenge_fixtures_detected`, `ssrf_blocks_all_listed_ranges`; `vc-fetch::redirect_cross_key_enqueued`, `redirect_to_private_ip_blocked`, `ua_is_exact_constant`, `lost_lease_aborts_before_request`; `vc-frontier::rate_token_global_across_pools`, `delay_respects_crawl_delay`; render `subrequest_to_private_ip_blocked`, `third_party_not_allowlisted_failed`; the E2E politeness oracle (skill `run-e2e`).

## 4. Fill the PR template checklist honestly. Any FAIL blocks the merge.
