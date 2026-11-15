# Security policy

## Reporting a vulnerability

Please report privately through GitHub:
[**Report a vulnerability**](https://github.com/rupakc/travel-planner-crawler/security/advisories/new).
Do not open a public issue for security problems. We aim to acknowledge within 3 days
and to fix or mitigate critical issues within 14 days.

In scope, for example:

- SSRF or a way to make the crawler (or its headless browser) reach private/internal
  addresses, the metadata server, or hosts outside a claim's scope;
- bypassing robots.txt, the denylist, the rate limits or the `ClaimPermit` gate;
- prompt injection that makes extracted records carry planted data past the grounding
  checks (fake emergency numbers, phishing URLs);
- authentication or authorisation flaws in the `/v1` read API;
- supply-chain issues: unsigned or wrongly signed images, unpinned actions, malicious
  dependencies;
- secrets or page content leaking into logs.

Opt-out and data-removal requests are not security issues: see [BOT.md](BOT.md).

## Supported versions

Only the image currently deployed from `main` (`ghcr.io/rupakc/voyager-crawler:prod`)
is supported.

## Hardening in place

- CI has no cloud credentials. Images are built in GitHub Actions, scanned with Trivy and
  signed keylessly with cosign; the VM verifies the signature (pinned to this repo's
  `deploy.yml` on `main`) before running anything.
- Actions are pinned by commit SHA, images by digest; `cargo deny` checks advisories,
  licences, sources and bans evasion crates.
- The headless browser has no network of its own; every request it makes is intercepted
  and fetched under the same policy (SSRF, robots, denylist, rate limit).
- The read API listens on an internal IP only, behind TLS and per-client tokens.
- Secrets live in Secret Manager, never in the repository.
