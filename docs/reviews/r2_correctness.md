# R2: correctness, reliability, security, testability

Spec: `2026-11-15-voyager-crawler-design.md`. B = blocker, M = major, m = minor.

## Raw store and processing

**1 (B) Frontier state can advance before the raw copy exists.** Captures commit only after upload (up to 10 min later), but §5.3 sets etag/content_hash/next_fetch "after the fetch". After a crash, the next conditional GET returns 304 for a body that was never stored, and it is lost for good.
*Change:* a `segments` table (open→sealed→uploaded→committed); each spool record carries a crc and is fsynced. Validators and `last_capture_id` are written in the same transaction as the `captures` insert, and the URL stays `fetched_pending` until then. Revisit and 304 logic use committed captures only.

**2 (M) Uploads are not idempotent.** `<role>-<host>-<seq>` collides if seq resets after a restart.
*Change:* ULID object names, `ifGenerationMatch=0`, `captures ON CONFLICT DO NOTHING`, and finish sealed or uploaded segments on start.

**3 (M) Processing is at-least-once, and the spec doesn't say so.**
*Change:* a unique key `(doc_id, capture_id, extractor_version, schema_version, prompt_hash)`, an LLM result cache by `(content_hash, prompt_hash, schema, model)` checked before reserving budget, and a dead-letter state after 3 failures.

## Politeness

**4 (B) The lease can be shorter than a fetch.** Robots + 5 hops × 30 s + a 45 s render + a slow body can outlast the lease, and a second claim then runs in parallel. A hung process never stops.
*Change:* one hard deadline over the whole operation, below the lease; a fencing `lease_epoch` checked on completion; Postgres `now()` only, so the VM clock never matters.

**5 (B) `GatePermit` holds only inside one process.** Sitemap/feed fetches, adapters, robots, `admin source validate`, scout `/terms` checks and the renderer all bypass the host claim, and the 8 req/s cap is per process.
*Change:* a permit can come only from a DB host claim that carries its epoch. Robots fetches use the host slot. The global rate becomes a DB token bucket or a single egress process.

**6 (B) Redirects and Chromium break "≤ 1 in flight".** A cross-domain redirect hits a host that was never claimed. Chromium loads sub-resources in parallel, allowlisted CDNs get no robots or rate check, and Chromium does its own DNS, which bypasses SSRF pinning.
*Change:* enqueue cross-domain redirects instead of following them. Route all Chromium traffic through a vcrawl egress proxy (`--proxy-server`; the container has no other route): per-URL robots, serialised under the host claim, about 50 sub-requests at most, third-party requests blocked.

**7 (M) Per-IP limiting has no mechanism and would serialise every Cloudflare site.** *Change:* lock `ip_slots` in the claim, only for IPs outside CDN ranges. (m) When robots newly disallows a path, tombstone its entities.

## Security

**9 (B) The Chromium sandbox and the metadata server.** `headless-shell` usually runs `--no-sandbox`, so a renderer exploit reaches the metadata server (an SA token with objectAdmin and Vertex), unauthenticated CDP on :9222, and Postgres. "No other secrets" implies Postgres has no password.
*Change:* sandbox on (Chrome's seccomp profile), `--jitless`, a fresh profile for each page, the browser recycled every N pages. Chromium gets its own network whose only peer is the proxy. A DOCKER-USER rule drops 169.254.169.254 for containers that don't need ADC. Postgres gets a password from Secret Manager, scram auth, and only the internal network.

**10 (M) SSRF details are missing.** Re-check DNS at every redirect hop. Block 169.254/16, 100.64/10, 0/8, `::ffff:` mapped addresses, fc00::/7, decimal and octal IPs, and ports other than 80/443.

**11 (M) Prompt injection becomes poisoned facts, and those reach Voyager's agents.** The schema prevents tool abuse, but a page can still plant a fake emergency number, a fake visa rule or a phishing URL, and steer triage, the scout or the auditor (same model). §13 grounding carries it into Voyager's prompts.
*Change:* `visa_rule`, `advisory`, `emergency_numbers` and `fx_rate` come only from allowlisted official sources or adapters. Reject any URL, phone number or email that isn't verbatim in the source. Schemas get `maxLength` and enums. Scout promotion needs a human `admin approve`; grounding is delimited as untrusted.

**12 (M) Malicious content.** Count decompressed bytes while streaming, nested `.gz` sitemaps included. Cap DOM depth. No XML DTDs. A time and memory limit for each capture.

**13 (M) API transport.** No TLS is specified, and the allowlist uses the app VM's *external* IP, so the token crosses the internet.
*Change:* use the VPC internal IP with a firewall rule keyed by source service account; TLS or internal-only. Constant-time compare, two live tokens for rotation.

**14 (M) Supply chain and backups.** Pin cosign to issuer + `workflow@refs/heads/main`. Pin images by digest and Actions by SHA. cargo-deny `bans` for `spider_fingerprint`, `ua_generator`, `chrome_stealth` (CI enforces the anti-goal) and `sources` = crates.io only. objectAdmin on the ops bucket can delete backups: use objectCreator + viewer plus retention.

## Budget, batch, healing

**15 (B) Budget reservation race.** *Change:* reserve with one conditional statement: `UPDATE … SET reserved=reserved+$e WHERE spent+reserved+$e <= cap RETURNING`. The estimate is conservative input tokens + `maxOutputTokens` + thinking tokens. After a crash, the reservation counts as spent. A batch reserves its whole cost when submitted. Retries, place resolution, triage, scout, auditor **and embeddings** all go through the ledger.

**16 (B) Batch lifecycle.** A crash between `create` and saving the job id leaves an orphan job, and the items get resubmitted and paid for twice.
*Change:* write the `batch_jobs` row first (building→submitted→done/failed/expired), label the job with `batch_id`, and reconcile by listing on start. Map results by request key, not line order. Handle partial failures one item at a time. Cancel after 72 h PENDING and fall back to online within the sub-budget. Alert before the 7-day lifecycle deletes unconsumed outputs.

**17 (M) Restore is never tested.** *Change:* a weekly restore drill into a scratch container that checks schema version, row counts and how recent the newest capture is, and exports `restore_drill_ok` with an alert. `admin rebuild-captures` from the GCS `.cdxj` files, because rows committed after the dump otherwise orphan their segments. A CI test: dump → wipe → restore → rebuild → equivalence.

**18 (M) Self-healing is not designed.** Progress-based health checks. Memory limits per container (OOM could kill Postgres). Spool backpressure: a GCS outage must not fill the Postgres disk. Metric-absent alerts as a dead-man's switch. A single-instance MIG with autohealing. `migrate` doesn't roll back: expand/contract migrations with an N-1 test.

## Missing tests and invariants

**19 (M)**
- Failpoint crash tests (`fail` crate) at spool fsync, upload, captures commit, frontier update and batch create. After a restart, assert: every 2xx the fake web served is committed or refetched, no capture lacks its object, nothing is duplicated.
- A politeness oracle over *all* fake-web requests (robots, sub-resources, redirects, CLI, adapters): per domain, request intervals never overlap and the gaps are ≥ the delay. Run it with 2 fetchers and a site that holds connections longer than the lease.
- A Chromium escape page that reaches for 10.x, the metadata server, :9222 and Postgres.
- Budget: a property test with 50 concurrent reservers that never exceed the cap, plus crashes with reservations outstanding.
- A fake Vertex batch: crash after create, partial or shuffled output, stuck PENDING.
- Injection fixtures: no extracted URL or phone number that is missing from the source.
- `cargo-fuzz` on WARC, robots, sitemaps and URL canonicalisation; `cargo-mutants` on `vc-policy` (line coverage won't catch politeness bugs).
