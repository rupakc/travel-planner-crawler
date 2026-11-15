---
name: run-e2e
description: Run the Voyager Crawler end-to-end suite locally - the compose stack with fake web, fake Gemini and fake GCS, two fetchers, and the politeness and raw-first oracles - and interpret failures. Use before merging changes to fetch, render, frontier, store, processor wiring, or when CI's E2E job fails.
---

# Run the E2E suite

Plan Task 14 (fakes + oracles) and Task 16 (`e2e/compose.yaml`, runs on every PR, ≤ 10 min). Spec §12. Needs Docker.

## Run

```bash
docker version                                    # Docker must be up
cargo build --release -p vcrawl                   # or let compose build the image
docker compose -f e2e/compose.yaml build
docker compose -f e2e/compose.yaml up --abort-on-container-exit --exit-code-from e2e-runner
docker compose -f e2e/compose.yaml logs fake-web | tail -100     # request log for the oracle
docker compose -f e2e/compose.yaml down -v                        # always clean up
```

If `e2e/compose.yaml` does not exist yet, Task 16 has not landed: run the integration layers instead:

```bash
cargo test -p testkit                       # oracle self-tests: oracle_detects_overlap / _short_gap / _disallowed
cargo test --workspace                      # PG testcontainers + fakes
```

## What must hold (assertions)

- `assert_polite(&RequestLog, &PolicySnapshot)`: per politeness key no overlapping requests, gaps ≥ delay (Crawl-delay site!), no disallowed or denylisted path, nothing after a park (challenge host).
- `assert_raw_first(pool, blobstore)`: every record's capture exists in fake GCS.
- A content change creates a new record version; the API answers with provenance.
- The challenge host is parked; the SPA page is rendered; a private-IP sub-request is blocked.

## Interpreting failures

- Politeness oracle failure = a hard-rule bug. Never "fix" by loosening the oracle, adding sleeps in the test, or lowering the cycle count. Find the request that broke it in the `RequestLog` (key, url, started_at, ended_at, status, ua) and trace it to the code path that skipped `PermitClient::pace` or the park.
- Raw-first failure = something became visible before `vc_commit_segment`.
- Flaky timing: check that the code uses DB time / `FakeClock`, not wall clock.
- Run the `politeness-auditor` agent on the diff that introduced it.
