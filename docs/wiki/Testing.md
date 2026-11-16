# Testing

The crawler is tested at every level, with extra weight on the things that would hurt other
people's servers or lose data: politeness, crash safety, prompt injection and spend.

## Commands

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                 # needs Docker (Postgres via testcontainers)
cargo test -p vc-policy <name>         # one crate, one test
cargo deny check                       # licences, advisories, sources, banned evasion crates
python3 -m pytest -q scripts/tests     # source generator, policy files, schema
bash scripts/check-ownership.sh --self-test
shellcheck -x $(git ls-files '*.sh')
bash deploy/tests/test_deploy.sh && bash infrastructure/tests/test_bootstrap.sh
```

Toolchain `1.99.0` (`rust-toolchain.toml`), edition 2024. Write the failing test first.

## Levels

| Level | Tools | What it covers |
|---|---|---|
| Unit and property | `proptest` | URL canonicalisation is idempotent; robots against Google's test corpus and RFC 9309 status cases; the UsePolicy matrix; SSRF ranges and IP forms; simhash; WARC and CDXJ round trips; budget arithmetic |
| Integration | Postgres 17 + pgvector in testcontainers (`testkit::pg()`), wiremock | claim fairness and fencing, rate tokens, conditional GET, 429 + Retry-After, robots 401/403/challenge, cross-key redirects, segment commit order, the budget race, batch lifecycle, grounding with injection fixtures, purge, API contract snapshots |
| Failpoints | `fail` crate, feature `failpoints` in vc-store | a crash at each step of spool → upload → commit, then restart: no lost capture, no duplicate, no orphan 304 |
| Fuzzing | `cargo-fuzz` (nightly) | `fuzz_robots` (vc-policy), `fuzz_warc_reader` (vc-store), `fuzz_html`, `fuzz_feed`, `fuzz_sitemap` (vc-parse) |
| Snapshot | `insta` | the `/v1` OpenAPI document; any change shows in review |
| Real Chromium | `chromedp/headless-shell` in Docker | rendering, blocking, budgets; ignored locally unless `VC_RENDER_IT=1` (CI sets `CI`) |
| Shell | fake `docker`, `cosign`, `gcloud` | `deploy.sh` (unsigned refused, rollback, migrate before roll, watchdog) and `bootstrap.sh` (golden dry-run transcript) |
| End to end | compose with fake web, fake Gemini, fake GCS, two fetchers | **not started** (Task 16) |

## The testkit

`testkit/` is shared by every crate (see [testkit](Crate-testkit)):

- `pg()`: a fresh, fully migrated database per test, cloned from a template; one container
  per test binary, or the server in `VC_TEST_DATABASE_URL`.
- `FakeClock` and in-memory fakes of every trait: `MemFrontier`, `MemBlobStore`,
  `MemQueue`, `FakeLlm`, `MemLedger`.
- **Fake web**: a scripted "mini internet" of seven sites, with a request log.
- **Fake Gemini**: the Vertex online and batch wire format, scripted.
- **Fake GCS**: `fake-gcs-server` in a container, with a `BlobStore` over it.

### The seven fake sites

| Site | What it tests |
|---|---|
| Blog with RSS and sitemap | feed discovery; a post that changes (new version); robots.txt that starts disallowing a path; a cross-key redirect and a redirect to a private IP; a link to a denylisted host; `410 Gone` |
| SPA attractions site | an empty shell that needs rendering; its script tries a private IP, the metadata server and an image (all must be blocked) |
| Events site | schema.org Event JSON-LD; a date change |
| Hostile site | every page is a challenge: detect and park |
| Slow site | `Crawl-delay`, latency, `503 Retry-After` |
| Official visa page | official source; `noarchive` and `noindex` pages; a disallowed `/admin/` |
| CDN host | allowlisted scripts and styles, and an image |

## The oracles

Oracles check the **outcome**, independent of the code under test.

**Politeness oracle** (`assert_polite`) reads the fake web's request log and reports:

| Violation | Meaning |
|---|---|
| Overlap | two requests to one key in flight at once |
| ShortGap | a request sooner than the delay (pages) or 250 ms (sub-resources) after the previous one |
| NoRobots | a request before robots.txt was fetched |
| Disallowed | a path the latest robots.txt disallows (checked with its own parser) |
| KeyBlocked | any request after robots.txt answered 401/403 or a challenge |
| AfterPark | a request inside a park or back-off window |
| Denylisted | a request to a denylisted host |
| WrongUserAgent | anything other than the one VoyagerBot UA |

It is deliberately strict and must never be loosened to make a test pass.

**Raw-first oracle** (`assert_raw_first`) checks the whole database: every record version and
observation points to a committed capture, every capture's segment exists, and its bytes are
in the blob store.

## Review-focus tests

These five tests guard the most important promises. They must never be weakened.

| Scenario | Test |
|---|---|
| robots.txt changes mid-crawl to disallow a path | `vc-policy::robots_refresh_tombstones` (+ the E2E oracle) |
| A redirect to another domain or to a private IP | `vc-fetch::redirect_cross_key_enqueued`, `redirect_to_private_ip_blocked` |
| A crash between spool fsync and segment commit | the `vcrawl` failpoint suite (Task 15, in progress); the vc-store crash tests today |
| A page with prompt-injection text | `vc-extract::injection_fixtures` |
| 100 concurrent LLM reservations near the cap | `vc-llm::budget_race_never_exceeds_cap` |

## CI

`.github/workflows/ci.yml` runs on every pull request and on `main`:

| Job | What |
|---|---|
| rustfmt | `cargo fmt --check` |
| clippy | `-D warnings`, all targets |
| test | `cargo test --workspace --all-targets --locked` and doc tests, with Docker for testcontainers |
| cargo-deny | licences, advisories, sources, banned evasion crates |
| ownership | on `agent/*` branches only: paths must match `docs/ownership.md` |
| shellcheck | every tracked `*.sh` |
| deploy kit tests | `deploy.sh` and `bootstrap.sh` with fakes, no GCP |
| scripts | pytest for the generator, ownership self-test |

> **Status:** the spec also plans a coverage gate (≥ 85 % on policy, frontier and store), a
> fuzz smoke run on every PR with a longer nightly run, nightly mutation testing
> (`cargo-mutants`) on vc-policy and vc-frontier, a migration N-1 test, the E2E job, and a
> live smoke test after deploy. These are not in CI yet.

See also: [Contributing](Contributing).
