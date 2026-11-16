# Security

This page explains how the crawler protects other people's sites, its own data, and
Voyager. To report a vulnerability, use GitHub's
[private reporting](https://github.com/rupakc/travel-planner-crawler/security/advisories/new)
(see [SECURITY.md](https://github.com/rupakc/travel-planner-crawler/blob/main/SECURITY.md)).
Do not open a public issue. Opt-out requests are not security issues; see BOT.md.

## Threats and defences

| Threat | Defence |
|---|---|
| **SSRF**: making the crawler or its browser call internal addresses or the metadata server | DNS resolved once per hop and pinned; private, link-local, CGNAT, loopback, ULA and IPv4-mapped ranges blocked; ports 80/443 only; IP forms normalised. Chromium has no network at all; its requests are checked the same way. Docker networks `db` and `render` are internal. |
| Bypassing robots.txt, the denylist or rate limits | One gate: no request without a `ClaimPermit` (only `vc-frontier` can mint one, enforced by clippy), a lease check and a rate token per request. Failing closed on missing robots. |
| **Prompt injection**: a page plants fake facts (emergency numbers, phishing links) | Page text is untrusted input inside delimiters; no tools; schema-only output; verbatim and URL grounding checks; high-stakes kinds from official sources only; cross-source conflict flag. Voyager wraps items as untrusted reference data. |
| Hostile content (zip bombs, deep DOMs, XML entities) | decoded byte caps, nested gzip limit, DOM depth and node caps, no DTDs, CPU time per capture, content-type allowlist; fuzzed parsers. |
| API access by the wrong party | internal IP only; firewall allows only the app VM's service account; TLS with a pinned self-signed certificate; per-client bearer tokens hashed with argon2id; per-client rate limits and licence filters; `/metrics` on localhost only. |
| Supply chain | Actions pinned by SHA, images by digest; `cargo deny` checks advisories, licences and sources and bans evasion crates; Trivy scans; cosign keyless signing; the VM verifies the signature (this repo's `deploy.yml` on `main`) before running anything. CI has no cloud credentials. |
| Secret leaks | Secrets only in Secret Manager; delivered as tmpfs files, never env values; `Secret` values never appear in `Debug`, errors or logs; Secret Manager payload CRC32C verified. |
| Content in logs | a log **field allowlist**: only named fields are written; `body`, `token`, `headers` and similar are dropped by name; long values are cut. |
| Runaway LLM spend | off by default; race-free day + month reservation; monthly cap ≤ 50 % of the project cap or no startup; billing errors open a 6 h breaker. |
| Losing or corrupting raw data | fsynced spool; idempotent uploads with `ifGenerationMatch=0`; one commit transaction; failpoint tests; WAL archiving, snapshots and restore drills. |
| Backups deleted by a compromised VM | the runtime account can create and read backups but not delete them; the bucket lifecycle deletes them. |

## Container hardening

- Roles: read-only root filesystem, non-root user 65532, all capabilities dropped,
  `no-new-privileges`, small tmpfs.
- Chromium: its own sandbox on (no `--no-sandbox`), a seccomp profile, non-root, read-only
  root, 1.5 GB limit, internal network only, fresh context per render.
- `unsafe` Rust is forbidden workspace-wide (`unsafe_code = "forbid"`).

## Least-privilege IAM

| Account | Can |
|---|---|
| `crawler-runtime` (the VM) | `objectAdmin` on the raw bucket (purges need delete); create and read on the ops bucket (no delete); log and metric writer; Vertex predict and batch jobs; create/delete only `crawler-drill-*` VMs; read the four `crawler-*` secrets |
| `crawler-drill` | runs the monthly restore drill VM |
| `vm-runtime` (the Voyager app VM) | read `crawler-api-token-voyager` and `crawler-api-tls-cert` only |

## Supported versions

Only the image currently deployed from `main` (`ghcr.io/rupakc/voyager-crawler:prod`) is
supported.

See also: [Politeness and Robots](Politeness-and-Robots), [Operations and Deploy](Operations-and-Deploy).
