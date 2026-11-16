# Crate: vcrawl

**Purpose.** The single binary. It wires the library crates together into roles, and
provides the admin CLI and the migration runner. It is the only crate that depends on
implementations rather than only on `vc-core` traits.

Path: `crates/vcrawl`. Dependencies: `clap`, `tokio`, `anyhow`, `sqlx`, `fail`.

> **Status: stub, being wired now (plan Task 15).** Today `vcrawl <role>` loads the
> configuration and exits with "role … is not wired yet". This page describes the planned
> behaviour from the plan and the deploy contract.

## Roles (planned)

| Command | Job |
|---|---|
| `vcrawl scheduler` | sources to seeds, sitemaps and feeds; enqueue; recrawl; robots.txt refresh |
| `vcrawl fetcher` | the claim loop: robots first when due; adapters; render when flagged; spool; `mark_fetched`; segment roller; cross-key redirect to enqueue; challenge to park |
| `vcrawl processor` | captures to records |
| `vcrawl embedder` | embeddings |
| `vcrawl api` | the `/v1` API |
| `vcrawl maintenance` | partitions, purge, ephemeral expiry, dumps, restore drills, daily digest, Parquet export (stub) |
| `vcrawl admin …` | `status`; `source validate`, `source list`, `source approve`, `source disable`; `purge --url`, `--host` or `--author`; `rebuild-captures`; `reextract --kind` |
| `vcrawl migrate [--contract]` | apply migrations; `--contract` only after 7 days without rollback |
| `vcrawl health` | exits 0 when this container's role is healthy (progress-based) |

## Deploy contract

From `deploy/compose.yaml` and `infrastructure/README.md`:

- `ENTRYPOINT ["vcrawl"]`; the role is the compose command.
- Runs as uid 65532 on a read-only root filesystem.
- Secrets arrive as files: `VC_DATABASE_PASSWORD_FILE`, `VC_API_TOKEN_FILE`,
  `VC_API_TLS_KEY_FILE`, `VC_API_TLS_CERT_FILE`.
- Image labels `io.voyager-crawler.schema` and `io.voyager-crawler.min-schema` are stamped
  by the deploy workflow from `migrations/`.

## Planned tests

- the failpoint suite at each raw-store step with a restart:
  `no_lost_no_duplicate_no_orphan_304`;
- `rebuild_captures_from_cdxj_equivalent`;
- `migration_n_minus_1`: the previous image's queries against the new schema.

## Config

`[vcrawl]`: instance name, data directory, fetcher concurrency (16 keys), processor
concurrency (4), partitions ahead (2 months), contract minimum days (7).

Planned agentic loops live under `crates/vcrawl/src/agentic/`; see [Agentic Loops](Agentic-Loops).
