## What and why

<!-- One paragraph. Link the plan task (e.g. "Task 4: vc-fetch") and spec sections. -->

## Task and ownership

- Task id / branch: <!-- agent/t4-fetch -->
- [ ] I touched only paths my task owns (`docs/ownership.md`; CI `ownership` job).
- [ ] No new dependencies outside the root `[workspace.dependencies]`; I did not run `cargo update`.
- [ ] `vc-core` is unchanged (or this is a controller PR).

## Politeness and no-evasion checklist (spec §2, §5.2)

- [ ] Exactly one User-Agent: the `VoyagerBot/<ver> (+…/BOT.md)` constant. No UA, header or TLS-fingerprint rotation; no browser-like headers added to look human.
- [ ] No proxy pools, residential/rotating IPs, or other identity changes.
- [ ] A challenge, 401 or 403 means **detect → park**. No retry with a browser, another identity, or a CAPTCHA solver.
- [ ] Every request to a crawl target goes through a `ClaimPermit` + lease check + global rate token (`PermitClient`), including robots, sitemaps, feeds, adapters and render sub-requests.
- [ ] robots.txt, Crawl-delay, X-Robots-Tag, meta robots, TDM reservations and Content-Signal are honoured; nothing new bypasses `Policy`.
- [ ] Nothing on `policy/denylist.yaml` can be fetched; no stealth/evasion crates (`deny.toml` bans).
- [ ] Raw first: nothing derived from a capture is visible before `vc_commit_segment` commits it.
- [ ] Logs carry no page content, tokens or secrets.

## Tests

- [ ] Tests written first and seen failing; now `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --check` pass locally.
- Notable tests: <!-- names -->
