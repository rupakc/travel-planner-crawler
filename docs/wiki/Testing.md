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
bash e2e/run.sh                        # the E2E stack (Docker; slow the first time)
```

Toolchain `1.99.0` (`rust-toolchain.toml`), edition 2024. Write the failing test first.

## Levels

| Level | Tools | What it covers |
|---|---|---|
| Unit and property | `proptest` | URL canonicalisation is idempotent; robots against Google's test corpus and RFC 9309 status cases; the UsePolicy matrix; SSRF ranges and IP forms; simhash; WARC and CDXJ round trips; budget arithmetic |
| Integration | Postgres 17 + pgvector in testcontainers (`testkit::pg()`), wiremock | claim fairness and fencing, rate tokens, conditional GET, 429 + Retry-After, robots 401/403/challenge, cross-key redirects, segment commit order, the budget race, batch lifecycle, grounding with injection fixtures, purge, API contract snapshots |
| Failpoints | `fail` crate, feature `failpoints` (vc-store and `vcrawl`) | a crash at each step of spool → upload → commit, then restart; see below |
| Migration N-1 | recorded queries, testcontainers | the previous image's queries still work on the new schema; see below |
| Fuzzing | `cargo-fuzz`, nightly workflow | `fuzz_robots` (vc-policy), `fuzz_warc_reader` (vc-store), `fuzz_html`, `fuzz_feed`, `fuzz_sitemap` (vc-parse) |
| Snapshot | `insta` | the `/v1` OpenAPI document; any change shows in review |
| Real Chromium | `chromedp/headless-shell` in Docker | rendering, blocking, budgets; ignored locally unless `VC_RENDER_IT=1` (CI sets `CI`) |
| Shell | fake `docker`, `cosign`, `gcloud`, `openssl`, `systemctl` | `deploy.sh`, the watchdog, the restore drill, `first-boot.sh` and `bootstrap.sh`; see below |
| End to end | `e2e/compose.yaml`: the real roles against fake web, fake Gemini, fake GCS, two fetchers | runs on every PR; see below |

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
| A crash between spool fsync and segment commit | `vcrawl::failpoints::no_lost_no_duplicate_no_orphan_304` and the vc-store crash tests |
| A page with prompt-injection text | `vc-extract::injection_fixtures` |
| 100 concurrent LLM reservations near the cap | `vc-llm::budget_race_never_exceeds_cap` |

## End to end (E2E)

`e2e/run.sh` builds one test image, starts `e2e/compose.yaml`, drives a fixed number of
crawl cycles, then runs the oracles. The `.github/workflows/e2e.yml` job runs it on every
pull request and on `main`. Every compose network is internal: no secrets, no real web.

| Service | What |
|---|---|
| `postgres` | Postgres 17 + pgvector, migrated by `vcrawl migrate` |
| `web` | the fake web; every `*.test` site and `denied.test` is a DNS alias of it. It sits on a public-looking subnet (`11.22.33.0/24`), so the **production** SSRF guard treats it as the internet |
| trap at `10.0.0.5` | a second fake web on a private IP: the blog redirects to it and the SPA script calls it. The fetchers can route to it, so only the SSRF guard keeps them out |
| trap at `169.254.169.254` | a fake metadata server, the same idea |
| `gcs`, fake Gemini | `fake-gcs-server` and a scripted Vertex endpoint |
| `chromium` | the pinned headless shell, started by `deploy/chromium/run.sh` like production |
| roles | `scheduler`, `fetcher-a`, `fetcher-b`, `processor`, `embedder`, `maintenance`, `api`; config `e2e/vcrawl.toml` with production politeness |

The runner (`e2e/runner`, its own lock file kept equal to the root lock) runs
`E2E_CYCLES` cycles (default 3): set the fake web to cycle *n*, make due URLs due now,
crawl, drain. Then it checks eight assertions:

| Assertion | Passes when |
|---|---|
| `polite` | the politeness oracle finds no violation (including the 250 ms sub-resource gap), and every key was reached |
| `raw_first` | the raw-first oracle passes over the fake GCS bucket, and it checked something |
| `version_bump` | a page that changed between cycles produced a new record version |
| `api_provenance` | the API answers over TLS (trusted only through a throwaway CA), refuses anonymous calls, and returns provenance pointing at the fake web |
| `challenge_parked` | the hostile site's key is parked for a challenge, with no request after the first challenge |
| `spa_rendered` | the render capture holds the script-built DOM, and images were never fetched |
| `private_ip_blocked` | the trap logs are empty, and something did try to reach them |
| `two_fetchers` | both fetcher instances did work |

`run.sh` classifies each role as running, "not wired yet" or crashed. A crashed role fails
the run. Every role is wired today, so no assertion is skipped. Logs go to `e2e/out/`
(uploaded as a CI artifact). `E2E_KEEP=1` leaves the stack up for debugging.

## Failpoints

`vcrawl`'s `failpoints::no_lost_no_duplicate_no_orphan_304` crashes the fetcher at each of
eight points: `spool::before_fsync`, `fetcher::after_spool`, `fetcher::before_complete`,
`spool::before_seal`, `segment::before_upload`, `segment::after_upload`,
`segment::before_commit` and `segment::after_commit`. After each crash it restarts (the spool
is replayed) and recrawls twice, then checks:

- every fsynced capture is committed;
- no duplicate capture rows, WARC records or primary captures per epoch;
- every 304 is a revisit whose chain ends in a committed 2xx with a body;
- the URL is `due` with committed validators, and no sealed spool file is left;
- the raw-first oracle passes.

## Migration N-1

`vcrawl`'s `migration_n_minus_1` replays the **previous image's** SQL statements, recorded in
`testkit/fixtures/vcrawl/ops/n_minus_1_queries.json`, against the schema of the current tree.
Every statement must still prepare, and every recorded `INSERT` must still fill each
`NOT NULL` column without a default. A migration that drops, renames or retypes something
the deployed image uses fails here. Re-record the set after a release with the ignored test
`record_n_minus_1_query_set`. An `expand_only_lint` test checks that every expand migration
starts with `SET lock_timeout` and has no destructive statement (`DROP TABLE`, `DROP COLUMN`,
and so on).

## SSRF tests

- `vc-policy/tests/ssrf.rs`: property tests over every blocked range, plus special forms:
  IPv4-mapped IPv6, NAT64, 6to4, Teredo, multicast, cloud metadata addresses and hosts
  written as a bare number.
- E2E: the real policy, unchanged, blocks the private-IP and metadata traps while the
  public-looking fake web stays reachable.
- Crate integration tests that need loopback servers use a stub policy that lets loopback
  through; they do not test SSRF.

## Shell tests

`deploy/tests/test_deploy.sh` runs `deploy.sh`, the watchdog, `first-boot.sh`, the restore
drill and the compose file against fakes. It covers, among others: unsigned images held,
transient cosign and migrate failures retried, rollback, the min-schema gate (deploy and
rollback), drift and TLS heartbeats, unit outcome heartbeats, the restore drill's isolation
and cleanup, the WAL overwrite guard, Docker waiting for the data disk, the Ops Agent
config, Chromium's network flags, and the fetcher's metrics address off the `render`
network. `infrastructure/tests/test_bootstrap.sh` compares a `--dry-run` with a golden
transcript, checks that a second run re-asserts policies, channel, budget and firewall,
and that a dry run calls nothing.

## CI

`.github/workflows/ci.yml` runs on every pull request and on `main`. Other workflows:

| Workflow | When | What |
|---|---|---|
| `e2e.yml` | every PR and `main` | runner checks (fmt, clippy, unit tests) and the E2E stack |
| `fuzz.yml` | nightly 03:17 UTC, manual, and PRs that touch fuzz code | each of the five fuzz targets for 300 s (configurable); crashes are uploaded as artifacts |
| `deploy.yml` | `main` | build, Trivy, sign and promote; see [Operations and Deploy](Operations-and-Deploy) |
| `wiki.yml` | `main`, when `docs/wiki/` changes | checks internal links, then publishes this wiki |

`ci.yml` jobs:

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

> **Status:** not in CI yet: a coverage gate (at least 85 % on policy, frontier and store),
> a fuzz smoke run on every PR, nightly mutation testing (`cargo-mutants`) on vc-policy and
> vc-frontier, and a live smoke test after deploy.

See also: [Contributing](Contributing).
