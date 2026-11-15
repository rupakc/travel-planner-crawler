# Path ownership

Every agent writes **only** the paths its task owns (spec §17.10, review r5 §(d)).
`scripts/check-ownership.sh` enforces this in CI on every pull request from an
`agent/*` branch: a PR that touches a path its task does not own fails.

## Rules

1. **One owner per path.** A task writes its crate, its config section, its migration
   block, and the extra paths listed below. Nothing else.
2. **`vc-core` is frozen after P0.** Change requests go to the controller, who batches
   additive changes into one PR and rebases the worktrees.
3. **Dependencies** are added only in the root `[workspace.dependencies]`, by the controller.
   Never run `cargo update`. On a lock conflict: take `main`'s `Cargo.lock`, run
   `cargo metadata`, push.
4. **Migrations** live in reserved blocks: frontier 11xx, store 12xx, queue 13xx, llm 14xx,
   index 15xx, api 16xx, sources 17xx, ops 18xx. P0 owns 0001–0099. Editable until P3
   (fresh databases only); after the first production deploy they are append-only,
   expand/contract, covered by the N-1 test.
5. **Test fixtures.** A P1 task may edit its `testkit/src/<crate>_fixtures.rs` (crate name
   with `_`, e.g. `vc_policy_fixtures.rs`; already registered in `testkit/src/lib.rs` by P0A)
   and add `testkit/fixtures/<crate>/**`.
6. **Integration is P2a's job.** No P1 task edits `crates/vcrawl`.
7. **Controller** (branches not named `agent/*`) may touch anything; it is reviewed by the owner.

## Branch names

`agent/<task>[-<slug>]`, where `<task>` is one of the ids below (case-insensitive;
`task-1`, `t1`, `task1` all mean `t1`; `task-d` means `td`). Examples:
`agent/p0a`, `agent/t2-frontier`, `agent/task-d`.

## Map

The machine-readable block below is what the script reads: one `<task> <glob>` per line.
In a glob, `*` also matches `/` (so `crates/vc-policy/*` is the whole crate).
A line `<task> !<glob>` excludes paths from that task's own grants.
A path may have several owners (e.g. `sources/*` is shared by P0B, T10 and T18 in
different phases; phases do not overlap).

| Task | What | Paths (summary) |
|---|---|---|
| P0A | Rust workspace, vc-core contracts, migrations 00xx, minimal testkit | root Cargo files and toolchain/lint configs, `crates/**`, `testkit/**`, `migrations/00*` |
| P0B | Scaffolding, Claude setup, policy, sources, CI | `CLAUDE.md`, `MEMORY.md`, `.claude/**`, top-level docs, `.github/**` (not deploy.yml), `docs/ownership.md`, `scripts/**`, `policy/**`, `sources/**`, `schemas/source.schema.json` |
| T1 | vc-policy | `crates/vc-policy/**`, `testkit/fixtures/challenges/**` |
| T2 | vc-frontier | `crates/vc-frontier/**`, `migrations/11xx` |
| T3 | vc-store | `crates/vc-store/**`, `migrations/12xx` |
| T4 | vc-fetch | `crates/vc-fetch/**` |
| T5 | vc-render | `crates/vc-render/**`; `deploy/chromium/**` only to wire its renderer (say so in the PR; TD owns it) |
| T6 | vc-parse | `crates/vc-parse/**` |
| T7 | vc-kinds, kind schemas, prompts | `crates/vc-kinds/**`, `schemas/*.v*.json`, `prompts/**`, `kinds.toml` |
| T8 | vc-llm | `crates/vc-llm/**`, `migrations/14xx` |
| T9 | vc-extract | `crates/vc-extract/**` |
| T10 | vc-sources + adapters | `crates/vc-sources/**`, `migrations/17xx`, `sources/**` |
| T11 | vc-index | `crates/vc-index/**`, `migrations/15xx` |
| T12 | vc-api | `crates/vc-api/**`, `migrations/16xx` |
| T13 | vc-queue, vc-obs, vc-secrets | those crates, `migrations/13xx`, `migrations/18xx` |
| T14 | testkit full (fake web, fake Gemini, oracles) | `testkit/**` |
| TD | deploy kit + bootstrap | `deploy/**` (including `deploy/chromium/**`), `infrastructure/**`, `.github/workflows/deploy.yml` |
| T15 | vcrawl binary | `crates/vcrawl/**`, `migrations/contract/**` |
| T16 | E2E in CI | `e2e/**`, `.github/workflows/e2e.yml` |
| T17 | agentic loops | `crates/vcrawl/src/agentic/**`, `prompts/agentic/**`, `docs/proposals/**` |
| T18 | provisioning and go-live | `docs/runbooks/**`, `sources/**`, `BOT.md` (egress IP) |

Every task may also edit its own section of `vc-config` (`crates/vc-config/src/sections/<x>.rs`, e.g. `policy.rs`; `mod.rs` and `database.rs` belong to the controller).

```ownership
# --- P0A: workspace + contracts ---
p0a Cargo.toml
p0a Cargo.lock
p0a rust-toolchain.toml
p0a deny.toml
p0a clippy.toml
p0a rustfmt.toml
p0a .cargo/*
p0a .gitignore
p0a crates/*
p0a testkit/*
p0a migrations/00[0-9][0-9]_*
# --- P0B: scaffolding ---
p0b CLAUDE.md
p0b MEMORY.md
p0b README.md
p0b BOT.md
p0b CONTRIBUTING.md
p0b SECURITY.md
p0b .claude/*
p0b .github/CODEOWNERS
p0b .github/dependabot.yml
p0b .github/pull_request_template.md
p0b .github/ISSUE_TEMPLATE/*
p0b .github/workflows/ci.yml
p0b docs/ownership.md
p0b scripts/*
p0b policy/*
p0b sources/*
p0b schemas/source.schema.json
# --- P1 crates ---
t1 crates/vc-policy/*
t1 crates/vc-config/src/sections/policy.rs
t1 testkit/fixtures/challenges/*
t1 testkit/fixtures/vc-policy/*
t1 testkit/src/vc_policy_fixtures.rs
t2 crates/vc-frontier/*
t2 crates/vc-config/src/sections/frontier.rs
t2 migrations/11[0-9][0-9]_*
t2 testkit/fixtures/vc-frontier/*
t2 testkit/src/vc_frontier_fixtures.rs
t3 crates/vc-store/*
t3 crates/vc-config/src/sections/store.rs
t3 migrations/12[0-9][0-9]_*
t3 testkit/fixtures/vc-store/*
t3 testkit/src/vc_store_fixtures.rs
t4 crates/vc-fetch/*
t4 crates/vc-config/src/sections/fetch.rs
t4 testkit/fixtures/vc-fetch/*
t4 testkit/src/vc_fetch_fixtures.rs
t5 crates/vc-render/*
t5 crates/vc-config/src/sections/render.rs
t5 deploy/chromium/*   # renderer wiring only; note it in the PR (TD owns deploy/**)
t5 testkit/fixtures/vc-render/*
t5 testkit/src/vc_render_fixtures.rs
t6 crates/vc-parse/*
t6 crates/vc-config/src/sections/parse.rs
t6 testkit/fixtures/vc-parse/*
t6 testkit/src/vc_parse_fixtures.rs
t7 crates/vc-kinds/*
t7 crates/vc-config/src/sections/kinds.rs
t7 schemas/*.v[0-9]*.json
t7 prompts/*
t7 kinds.toml
t7 schemas/kinds.toml
t7 testkit/fixtures/vc-kinds/*
t7 testkit/src/vc_kinds_fixtures.rs
t8 crates/vc-llm/*
t8 crates/vc-config/src/sections/llm.rs
t8 migrations/14[0-9][0-9]_*
t8 testkit/fixtures/vc-llm/*
t8 testkit/src/vc_llm_fixtures.rs
t9 crates/vc-extract/*
t9 crates/vc-config/src/sections/extract.rs
t9 testkit/fixtures/vc-extract/*
t9 testkit/src/vc_extract_fixtures.rs
t10 crates/vc-sources/*
t10 crates/vc-config/src/sections/sources.rs
t10 migrations/17[0-9][0-9]_*
t10 sources/*
t10 testkit/fixtures/vc-sources/*
t10 testkit/src/vc_sources_fixtures.rs
t11 crates/vc-index/*
t11 crates/vc-config/src/sections/index.rs
t11 migrations/15[0-9][0-9]_*
t11 testkit/fixtures/vc-index/*
t11 testkit/src/vc_index_fixtures.rs
t12 crates/vc-api/*
t12 crates/vc-config/src/sections/api.rs
t12 migrations/16[0-9][0-9]_*
t12 testkit/fixtures/vc-api/*
t12 testkit/src/vc_api_fixtures.rs
t13 crates/vc-queue/*
t13 crates/vc-obs/*
t13 crates/vc-secrets/*
t13 crates/vc-config/src/sections/queue.rs
t13 crates/vc-config/src/sections/obs.rs
t13 crates/vc-config/src/sections/secrets.rs
t13 migrations/13[0-9][0-9]_*
t13 migrations/18[0-9][0-9]_*
t13 testkit/fixtures/vc-queue/*
t13 testkit/fixtures/vc-obs/*
t13 testkit/fixtures/vc-secrets/*
t13 testkit/src/vc_queue_fixtures.rs
t13 testkit/src/vc_obs_fixtures.rs
t13 testkit/src/vc_secrets_fixtures.rs
t14 testkit/*
# --- deploy kit ---
td deploy/*
td infrastructure/*
td .github/workflows/deploy.yml
td .github/workflows/prod-scan.yml
# --- P2 / P3 ---
t15 crates/vcrawl/*
t15 crates/vc-config/src/sections/vcrawl.rs
t15 migrations/contract/*
t15 testkit/fixtures/vcrawl/*
t16 e2e/*
t16 .github/workflows/e2e.yml
t17 crates/vcrawl/src/agentic/*
t17 prompts/agentic/*
t17 docs/proposals/*
t18 docs/runbooks/*
t18 sources/*
t18 BOT.md
```
