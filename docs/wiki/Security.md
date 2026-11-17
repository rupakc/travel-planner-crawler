# Security

This page explains how the crawler protects other people's sites, its own data, and
Voyager. To report a vulnerability, use GitHub's
[private reporting](https://github.com/rupakc/travel-planner-crawler/security/advisories/new)
(see [SECURITY.md](https://github.com/rupakc/travel-planner-crawler/blob/main/SECURITY.md)).
Do not open a public issue. Opt-out requests are not security issues; see BOT.md.

## Threats and defences

| Threat | Defence |
|---|---|
| **SSRF** (server-side request forgery): making the crawler or its browser call internal addresses or the metadata server | DNS resolved once per hop and pinned; private, link-local, CGNAT, loopback, ULA and IPv4-mapped ranges blocked; a host with any blocked address is refused; ports 80/443 only; IP forms normalised. Chromium has no network at all; its requests are checked the same way. Docker networks `db`, `render` and `metrics` are internal. Tests run the real `VoyagerPolicy` and policy files against loopback, RFC 1918, the metadata IP and `metadata.google.internal`, CGNAT, `::1`, `::ffff:127.0.0.1`, IP literals and a blocked port; the mock server must receive nothing. |
| Credentials leaking on redirects | `Authorization` is sent only while scheme, host **and** port stay the same; any change drops it. An API call redirected from `https` to `http` is refused, never followed in clear text. |
| Bypassing robots.txt, the denylist or rate limits | One gate: no request without a `ClaimPermit` (only `vc-frontier` can mint one, enforced by clippy), a lease check and a rate token per request. Failing closed on missing robots. |
| **Prompt injection**: a page plants fake facts (emergency numbers, phishing links) | Page text is untrusted input inside delimiters; no tools; schema-only output; verbatim and URL grounding checks; high-stakes kinds from official sources only; cross-source conflict flag. Voyager wraps items as untrusted reference data. |
| Hostile content (zip bombs, deep DOMs, XML entities) | decoded byte caps, nested gzip limit, DOM depth and node caps, no DTDs, CPU time per capture, content-type allowlist; fuzzed parsers. |
| API access by the wrong party | internal IP only; firewall allows only the app VM's service account; TLS with a pinned self-signed certificate; per-client bearer tokens hashed with argon2id; per-client rate limits and licence filters; the API's `/metrics` on localhost only. |
| A compromised Chromium reaching crawler internals | Chromium shares only the `render` network with the fetcher. The fetcher's `/metrics` and `/healthz` listen on a separate internal `metrics` network (only the fetcher and the OpenTelemetry collector), never on `render`. |
| Supply chain | Actions pinned by SHA, images by digest; `cargo deny` checks advisories, licences and sources and bans evasion crates; Trivy scans; cosign keyless signing; the VM verifies the signature (this repo's `deploy.yml` on `main`) before running anything. CI has no cloud credentials. |
| Secret leaks | Secrets only in Secret Manager; delivered as tmpfs files, never env values; `Secret` values never appear in `Debug`, errors or logs; Secret Manager payload CRC32C verified. |
| Content in logs | a log **field allowlist**: only named fields are written; `body`, `token`, `headers` and similar are dropped by name; long values are cut. |
| Runaway LLM spend | off by default; race-free day + month reservation; monthly cap ≤ 50 % of the project cap, or the LLM is forced off; billing errors open a 6 h breaker shared by every role and kept across restarts ([Gemini and Budgets](Gemini-and-Budgets)). |
| Losing or corrupting raw data | fsynced spool; idempotent uploads with `ifGenerationMatch=0`; one commit transaction; failpoint tests; WAL archiving, weekly full base backups and restore drills. |
| Backups deleted by a compromised VM | the runtime account can create and read backups but not delete them; the bucket lifecycle deletes them. |

## Container hardening

- Roles: read-only root filesystem, non-root user 65532, all capabilities dropped,
  `no-new-privileges`, small tmpfs.
- Chromium: its own sandbox on (no `--no-sandbox`), a seccomp profile, non-root, read-only
  root, 1.5 GB limit, internal network only, fresh context per render. `deploy/chromium/run.sh`
  also turns off everything that could send traffic outside CDP interception: DNS prefetch,
  network prediction and preconnect, prerendering, pings, proxy lookup, domain reliability,
  phishing detection, crash upload, component updates and sync. The internal network is
  the hard guarantee; the flags stop Chromium from even trying.
- API TLS: the api loads the certificate and key from `/run/secrets/api_tls_{cert,key}`,
  which `deploy.sh` installs only as a matched pair. It falls back to Secret Manager only
  when those files are missing or unusable. rustls refuses a mismatched pair, so a
  half-done rotation cannot take the API down.
- `unsafe` Rust is forbidden workspace-wide (`unsafe_code = "forbid"`).

## Known limitation: child targets

Chromium runs workers and out-of-process iframes (OOPIFs) as **child targets**. With
chromiumoxide 0.9.1 (the CDP library) the renderer cannot turn on `Fetch.enable` for a child
target before it starts, so their requests are not intercepted. What limits the risk:

- every document other than the page's own is refused, so no OOPIF document loads;
- a worker script loaded by the page passes the same checks as any script, and non-http(s)
  URLs (`blob:`, `data:`) are refused;
- Chromium is only on the internal `render` network, which has no route out, so a child
  target's own requests fail;
- the only other listener the fetcher had on that network, its metrics endpoint, moved to
  the separate `metrics` network.

See [Rendering with Chromium](Rendering-with-Chromium).

## Least-privilege IAM

| Account | Can |
|---|---|
| `crawler-runtime` (the VM) | `objectAdmin` on the raw bucket (purges need delete); create and read on the ops bucket (no delete, no overwrite); log and metric writer; Vertex predict and batch jobs (custom role `voyagerCrawlerVertex`); read the four `crawler-*` secrets |
| `crawler-drill` | read the ops bucket, write logs and metrics; for the planned monthly Spot restore-drill VM |
| `vm-runtime` (the Voyager app VM) | read `crawler-api-token-voyager` and `crawler-api-tls-cert` only |

Not granted yet, on purpose: the right to create `crawler-drill-*` VMs (custom role
`voyagerCrawlerDrill`) and `serviceAccountUser` on `crawler-drill`. They arrive with the
monthly Spot drill that uses them. Until then a weekly light restore drill runs on the VM itself
([Operations and Deploy](Operations-and-Deploy)). The Vertex AI service agent has no access
to the ops bucket while batch prediction is not wired.

## Supported versions

Only the image currently deployed from `main` (`ghcr.io/rupakc/voyager-crawler:prod`) is
supported.

See also: [Politeness and Robots](Politeness-and-Robots), [Operations and Deploy](Operations-and-Deploy).
