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
- Fixtures under `testkit/fixtures/<crate>/` belong to that crate's owner.

> **Status:** the fakes and oracles are built. The end-to-end compose run that uses them
> with real roles (plan Task 16) is not started.
