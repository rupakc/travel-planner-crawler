# R4: round-2 verification of design v2

P = partial, N = not resolved; Nn → §2.

## 1. Round-1 blockers and majors

**Resolved:** r1-2, r1-4, r1-5, r1-7, r1-8, r1-9, r1-11, r2-2, r2-7, r2-10, r2-12, r2-14, r2-19, r3-7, r3-8, r3-10.

| Item | Verdict | Missing |
|---|---|---|
| r1-1 B sizing | P | HNSW (~2 GB) won't fit in 1 GB `shared_buffers`. |
| r1-3 M hot spot | P | Not HOT: `state`/`next_fetch_at` are indexed. Partitioning by url_key makes each claim probe 16 partitions |
| r1-6 M re-extract | P | No per-run `budget_usd` |
| r1-10 M API | P | No up/downcasters. N8 |
| r2-1 B raw-first | P | N1 |
| r2-3 M at-least-once | P | No LLM result cache before reserving, so redeliveries pay twice. No dead-letter |
| r2-4 B lease | P | N1, N2 |
| r2-5 B one gate | P | One rate token per *claim* (20 URLs + 200 sub-requests) |
| r2-6 B Chromium | N | N3 |
| r2-9 B sandbox | P | No fresh profile/recycle/`--jitless`. Needs tmpfs `/dev/shm` with the read-only FS. |
| r2-11 M injection | P | §13 grounding not marked untrusted. No `maxLength`/enums. "Official" includes Wikipedia |
| r2-13 M transport | P | N5 |
| r2-15 B budget | P | N9 |
| r2-16 B batch | N | Tests only. No `batch_jobs`-first state machine, reconcile, key mapping, 72 h cancel |
| r2-17 M restore | P | N10 |
| r2-18 M healing | P | N5, N6 |
| r3-1 B Vertex cap | P | N9 |
| r3-2 B opt-out | P | Reserved content still stored raw in full. N7 |
| r3-3 B dumps | P | enwiki dump still implied (use the API for 30k pages). Raw-bytes cap: downloaded or stored? |
| r3-4 M volume | P | No per-purpose sub-budgets |
| r3-5 M sub-requests | N | N3 |
| r3-6 M licences | P | N11 |
| r3-9 M migrations | P | N6 |
| r3-11 M overbuilt | owner | Chromium is now the riskiest part. Default `render: never` |

## 2. New problems

**N1 (B) Lease, fencing and commit don't compose.**
- The deadline is per-URL (~300 s), but a claim leases 20 URLs.
- Validators commit ≤ 10 min after the fetch, long after the next claim, so the fenced completion can't sit in that transaction.
- Fetched URLs stay `due`, so the next claim re-fetches them.
- Late or orphan segments can overwrite newer validators.

*Fix:*
- The claim marks URLs `leased` with their epoch.
- A fenced per-URL completion → `fetched_pending` + `next_fetch_at`.
- The commit writes validators only `WHERE fetched_at > last_fetched_at`.
- Lease 1–3 URLs, or use fenced extension.

**N2 (M)** A request can outlive its deadline. *Fix:* socket timeout ≤ `deadline − now` from a monotonic timer anchored to DB `now()`.

**N3 (B) The egress proxy can't see HTTPS.** Chromium uses `CONNECT`, so the proxy sees only host:port. Per-URL robots, serialisation (h2 multiplexes) and WARC records are impossible. *Fix:*
- CDP `Fetch.requestPaused`: requests go through `vc-fetch`, then `fulfillRequest`; Chromium gets no network.
- Or a local TLS-terminating proxy with an SPKI-pinned CA, HTTP/1.1.

**N4 (M) Renders miss 60 s.** Serialised: 200 × ~0.55 s ≈ 110 s; a typical SPA (40–80 requests) takes 25–65 s. Also a DB token per sub-request at a shared 4 req/s, and network-idle never fires. *Fix:*
- A cross-render sub-resource cache (URL + ETag).
- Cap ~60 requests, 90 s inside the deadline.
- Take rate tokens in local chunks.
- Measure in E2E.

**N5 (M) The MIG breaks its own health check and the network identity.**
- Health probes (35.191.0.0/16, 130.211.0.0/22) aren't allowed → a recreate loop.
- A progress `/healthz` recreates the VM on a GCS blip; use liveness-only.
- Internal and external IPs must be stateful, or the app URL, cert IP SAN and BOT.md IP break.
- The boot disk is wiped: deploy state belongs on the stateful disk, with the initial delay ≥ boot + pull + PG recovery.
- No cert rotation (pin two). The app SA can't read `crawler-api-tls`.

**N6 (M) Expand/contract on one VM.**
- Roll back to N-1, then N+1's contract breaks N-1.
- No `lock_timeout`.
- `CONCURRENTLY` fails on partitioned parents.

*Fix:* a `schema_versions(min_image)` row; contract only when all roles are on N.

**N7 (M) Purge.**
- Versioning + 7 d soft delete, text tier, Parquet, wal-g (14 d) and snapshots keep copies, so "7 days" is false. Restate the SLA (API 24 h, copies ≤ 21 d) and delete noncurrent generations.
- `noarchive` → its own prefix with a 30 d lifecycle.
- CDXJ beside segments → Coldline. Put it in a Standard `index/` prefix with every captures field.

**N8 (M)** `/v1/changes`: sequence ≠ commit order, so readers skip rows. Expose only rows below `pg_snapshot_xmin`.

**N9 (M) Budget.**
- 1.50 × 30 = 45 > 40, and reserve checks only the daily row. Check both caps in one statement.
- The two project-cap formulas disagree: use the max.
- `VC_PROJECT_CAP_USD` is self-declared.
- Batch jobs must be exempt from the 1 h auto-settle, and settling replaces the estimate.
- "3.1 Flash-Lite" vs the app's `gemini-3.5-flash-lite`: pin one and alert on 404.
- Totals hold (~$80–102).

**N10 (M) Restore drill:** a scratch Postgres on a VM at 7.3/8 GB. Use a 512 MB limit or a Spot VM.

**N11 (M) Licence classes.**
- API ToS (Ticketmaster) fall to `restricted` → no events, which contradicts §10.1's 40k. Add an `api_terms` class.
- `weather_normal` has no source: MET Norway is forecast-only.

**N12 (M) Partitioning.**
- PK/UNIQUE must include the partition key: `captures(capture_id, month)`, so no FK from `record_versions`.
- Idempotency keys need a deterministic partition column (capture time, not `now()`).

## 3. Minor
- HNSW needs `(vec::halfvec(768))` expression indexes.
- Persist the segment manifest before upload.
