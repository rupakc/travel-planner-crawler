# Crate: testkit

**Purpose.** Shared test tools for every crate: a real Postgres per test, fakes of every
trait, a fake internet, a fake Gemini, a fake GCS, and the politeness and raw-first oracles.
It is a dev-dependency only; production code never uses it.

Path: `testkit/`.

## Contents

| Item | What it gives |
|---|---|
| `pg()` | a fresh, fully migrated Postgres 17 + pgvector database per call, cloned from a per-process template. One `pgvector/pgvector:pg17` container per test binary, or the server in `VC_TEST_DATABASE_URL` |
| `FakeClock` | settable time |
| `MemFrontier`, `MemBlobStore`, `MemQueue`, `FakeLlm` (also an `Embedder`), `MemLedger` | in-memory fakes of the vc-core traits |
| `request_log` | the **frozen** request-log schema the politeness oracle reads |
| `fake_web` | the scripted "mini internet" of seven sites, with cycles, a request log and admin endpoints; binary `fake_web`, image built from `testkit/Dockerfile.fake-web` |
| `fake_gemini` | the Vertex online and batch wire format, scripted; binary `fake_gemini` |
| `fake_gcs` | a helper for the `fsouza/fake-gcs-server` container and a `BlobStore` over it |
| `oracle::assert_polite` | checks a request log against a `PolicySnapshot` |
| `oracle::assert_raw_first` | checks that everything visible comes from committed, stored captures |
| `<crate>_fixtures.rs` | per-crate fixture modules (each P1 crate owns its file) |

The seven fake sites, the oracle rules and the review-focus tests are described on
[Testing](Testing).

## Tests

`testkit/tests/`: `migrations.rs` (the migration set applies cleanly), `fakes.rs`,
`fake_web.rs`, `fake_gemini.rs`, `fake_gcs.rs`, `oracles.rs`, `raw_first.rs` (the oracles
catch planted violations).

## Rules

- The politeness oracle is deliberately strict and must never be loosened to make a test
  pass. It uses its own robots parser (`texting_robots`), independent of `vc-policy`.
- `Reply::no_response()` makes a fake-web route hang without ever answering; the request is
  logged (status 0) when the client gives up, and the oracle treats such a robots.txt as
  missing (allow all, or the rules of the last 2xx robots.txt served before it).
- Fixtures under `testkit/fixtures/<crate>/` belong to that crate's owner.

## Used by the E2E suite

The fakes and oracles also drive the end-to-end run in `e2e/` (not part of testkit): a
compose stack with the real `vcrawl` roles (scheduler, two fetchers, processor, embedder,
maintenance, api), Postgres, Chromium, `fake_web` and fake-gcs-server, with the LLM off.
`e2e/runner` drives the crawl cycles and checks 8 assertions: `polite`, `raw_first`,
`version_bump`, `api_provenance`, `challenge_parked`, `spa_rendered`, `private_ip_blocked`,
`two_fetchers`. The `XFAIL` list of expected failures is empty, so every assertion blocks.
CI runs it in `.github/workflows/e2e.yml`. Details: [Testing](Testing).

Fixtures for vcrawl's own tests live in `testkit/fixtures/vcrawl/` (`data/` for the data
path; `ops/` for admin and migrate, including `n_minus_1_queries.json`, the recorded queries
of the previous image that the N-1 migration test replays).
