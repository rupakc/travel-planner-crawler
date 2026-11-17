# Contributing

Thanks for helping. This crawler touches other people's servers, so the politeness contract
comes before everything else. The repository version is
[CONTRIBUTING.md](https://github.com/rupakc/travel-planner-crawler/blob/main/CONTRIBUTING.md).

## Hard rules (a PR that breaks one is closed)

1. **No bot-detection evasion.** One User-Agent; no header, UA or TLS-fingerprint rotation;
   no proxies; no stealth; no CAPTCHA solving. Challenge, 401 or 403 means detect and park.
2. **Every request to a crawl target needs a `ClaimPermit`**, a lease check and a global rate
   token.
3. **Honour every opt-out signal:** robots.txt (with the stricter 401/403 rule), Crawl-delay,
   X-Robots-Tag, meta robots, TDM reservations, Content-Signal.
4. **Never fetch anything on `policy/denylist.yaml`**, brand domains included.
5. **Raw first:** nothing derived from a capture is visible before its segment commits.
6. **Provenance and licence on everything;** `restricted` sources are not ingested;
   `share_alike` is never merged with other licences; `facts_only` serves at most a
   300-character excerpt.
7. **LLM spend never threatens Voyager's AI;** every call goes through the ledger.
8. **No secrets in the repo; logs never contain page content, tokens or secrets.**
9. **Everything must be purgeable.**

## Workflow

- Branch from `main`. Write the failing test first, then the code.
- Before pushing, run the commands in [Testing](Testing). CI also runs the end-to-end test
  (`e2e/run.sh`) on every pull request.
- Dependencies are added only in the root `[workspace.dependencies]`, by a maintainer. Never
  run `cargo update` in a feature PR. On a lock conflict, take `main`'s `Cargo.lock`, run
  `cargo metadata`, push.
- Fill in the PR template's no-evasion checklist honestly.
- Record new design decisions in `MEMORY.md` (dated, one line, with a link). Never rewrite
  an entry; supersede it.

## Parallel work and ownership

The code was built by several agents in parallel, each in its own worktree. To keep them
from colliding:

- Agent branches are named `agent/<task>[-slug]` and may only touch the paths their task owns
  (`docs/ownership.md`). CI runs `scripts/check-ownership.sh` on those branches.
- `vc-core` is **frozen**: changes go through the controller as additive, batched PRs.
- Each crate owns one config file, `crates/vc-config/src/sections/<x>.rs`, and one
  migration number block.
- Only the `vcrawl` crate wires implementations together.

## Extending

| You want | Do this |
|---|---|
| A new website | `sources/<id>.yaml` valid against `schemas/source.schema.json` → `vcrawl admin source validate <id>` → fixture + golden test. See [Sources and Licences](Sources-and-Licences). |
| A new API or dump | `impl SourceAdapter` in `crates/vc-sources/src/adapters/`, register it in `adapters::REGISTRY`, add a recorded wiremock fixture. Spool raw before deriving. |
| A new record kind | `schemas/<kind>.v1.json` + `prompts/<kind>.md` + a `kinds.toml` entry (source classes, verbatim fields, record or observation) + a fixture. The registry test enforces all four. See [vc-kinds](Crate-vc-kinds). |
| A new media type | `impl ContentHandler` and register it in `vc-parse`. |
| A new crash-safety step | add a named [failpoint](Glossary) (feature `failpoints`) and a test that crashes there. See [Testing](Testing). |
| A new store, queue or LLM | implement `BlobStore`, `WorkQueue`, `LlmClient`, `Embedder` or `Frontier` from `vc-core`. |
| A new API field | additive only; update the OpenAPI snapshot and `Crawler-API.md` in the same PR. See [Read API](Read-API). |
| A migration | in your block, `SET lock_timeout = '5s'` first, expand-only (the lint and the [N-1](Glossary) test check it); destructive steps go to `migrations/contract/`. The image's schema labels come from `vcrawl migrate --print-schema`, and `deploy.sh` refuses an image when the database lacks any migration it ships. See [Data Model](Data-Model). |

## Style

- Rust: `rustfmt`, clippy with `-D warnings`, `missing_docs` warns, no `unsafe`.
- Docs and comments: short sentences, plain words.
- Wiki pages live in `docs/wiki/` and are published to the GitHub wiki by
  `.github/workflows/wiki.yml` on every push to `main` that touches them. Edit them in the
  repository, not in the wiki: the sync overwrites wiki edits. The workflow first checks that
  every internal wiki link points to an existing page.

## Reporting a site problem

If VoyagerBot misbehaves on your site, see
[BOT.md](https://github.com/rupakc/travel-planner-crawler/blob/main/BOT.md). The robots.txt
opt-out works without waiting for us.
