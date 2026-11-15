# Voyager Crawler

A polite, raw-first, versioned travel-data crawler, index and read API for
[Voyager](https://github.com/rupakc/travel-planner), the open-source AI travel planner.
It runs 24x7 on one GCP VM and serves attributed, licence-aware facts to Voyager's
planning agents. MIT licence.

The bot's public identity, behaviour and opt-out are in **[BOT.md](BOT.md)**.

## What it does

- **Ingests** ~117 vetted sources (official travel advice, open datasets, event APIs,
  Wikivoyage/Wikipedia dumps, travel blog and news feeds) from [`sources/`](sources/).
- **Raw first:** every response is stored as WARC/1.1 in GCS before anything derived
  from it becomes visible; raw data is immutable and replayable.
- **Curates:** dedupe, language, place resolution, licence class, versioned records with
  full provenance; LLM extraction (Gemini) only under a hard, race-free budget.
- **Serves** a private `/v1` read API (Postgres FTS + pgvector) to the Voyager backend.

What it never does: evade bot detection, ignore robots.txt or opt-out signals, crawl
the [denylist](policy/denylist.yaml), or republish copyrighted articles.

## Status

Under construction. Design: [spec](docs/superpowers/specs/2026-11-15-voyager-crawler-design.md)
(v3, §17 normative) · [plan](docs/superpowers/plans/2026-11-15-voyager-crawler.md) ·
[reviews](docs/reviews/) · [research](docs/research/) · [decisions](MEMORY.md).

## Layout

```
crates/      one Rust workspace: vc-core (shared types + every trait), vc-config, vc-policy,
             vc-frontier, vc-fetch, vc-render, vc-store, vc-parse, vc-kinds, vc-llm,
             vc-extract, vc-sources, vc-queue, vc-index, vc-api, vc-obs, vc-secrets, vcrawl
testkit/     Postgres testcontainer, fakes of every trait, fake web/Gemini/GCS, oracles
migrations/  sqlx migrations (expand-only; contract/ separately)
sources/     one YAML per source (schema: schemas/source.schema.json)
schemas/     JSON Schemas (sources, entity kinds)   prompts/  extraction prompts
policy/      denylist, sub-resource allowlist
deploy/      VM compose kit       infrastructure/  idempotent gcloud bootstrap
scripts/     seeds_to_sources.py, check-ownership.sh
```

## Develop

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # needs Docker (Postgres via testcontainers)
cargo deny check
python3 -m pytest -q scripts/tests
bash scripts/check-ownership.sh --self-test
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Security: [SECURITY.md](SECURITY.md).

## Licence

Code: MIT ([LICENSE](LICENSE)). Collected data keeps its source licence; every API item
carries `licence {class, spdx, attribution, url}`.
