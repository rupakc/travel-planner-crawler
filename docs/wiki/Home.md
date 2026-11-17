# Voyager Crawler

The Voyager Crawler collects public travel information, stores it safely, cleans it up, and
serves it to [Voyager](https://github.com/rupakc/travel-planner), the open-source AI travel
planner. Voyager's planning agents can then cite current, attributed facts instead of
guessing.

It is one Rust program (`vcrawl`) with several roles, one Postgres database, a Google Cloud
Storage (GCS) bucket for raw pages, and Gemini on Vertex AI for the few jobs that need a
language model. It runs on one small Google Cloud VM for about $74 to $104 a month
([Cost](Cost)). Code licence: MIT.

## The three promises

1. **Honest crawling.** The bot is called `VoyagerBot`. It always says who it is, obeys
   robots.txt and every opt-out signal, waits between requests, and never tries to get past
   bot protection. A block means "stop", never "try another way". See
   [Politeness and Robots](Politeness-and-Robots).
2. **Raw first.** Every response is saved to durable storage before anything built from it
   becomes visible. Raw data is never changed, so any result can be traced back and rebuilt.
   See [Storage and Raw Data](Storage-and-Raw-Data).
3. **Licence aware.** Every item carries its source, licence and attribution. Articles from
   blogs and news are served only as facts, a headline, a short excerpt and a link. See
   [Sources and Licences](Sources-and-Licences).

## Architecture at a glance

```mermaid
flowchart TB
  SITES["Public web: sites, feeds, sitemaps, APIs, dumps"]
  subgraph VM["Crawler VM, docker compose"]
    direction TB
    GATE{"Politeness gate: permit, robots, denylist, SSRF, rate token"}
    SCH["scheduler"]
    FET["fetcher"]
    CHR["Chromium, no network"]
    PG[("Postgres 17 + pgvector: frontier, queue, records, index")]
    PRO["processor"]
    EMB["embedder"]
    MNT["maintenance"]
    API["api: /v1 on internal IP"]
    OTL["otelcol: scrapes /metrics"]
  end
  GCS[("GCS: WARC segments, CDXJ, text tier")]
  GEM["Gemini on Vertex AI, budgeted"]
  MON["Cloud Monitoring and Logging"]
  VOY["Voyager backend"]

  GATE -->|"HTTPS as VoyagerBot"| SITES
  FET --> GATE
  CHR <-->|"CDP Fetch interception"| FET
  SCH -->|"enqueue URLs"| PG
  FET <-->|"claim, lease, commit"| PG
  FET -->|"WARC segments"| GCS
  PG -->|"capture_ready"| PRO
  PRO -->|"records, entities, links"| PG
  PRO -->|"read raw"| GCS
  PRO -->|"extract, triage"| GEM
  EMB -->|"embed"| GEM
  EMB -->|"vectors"| PG
  MNT -->|"partitions, purge, digest"| PG
  MNT -->|"rewrites, drill"| GCS
  MNT -->|"scout, auditor (weekly)"| GEM
  API -->|"hybrid search"| PG
  VOY -->|"GET /v1, token, TLS"| API
  OTL -->|"crawler_* metrics"| MON
```

More detail: [Architecture](Architecture) and [How a Page Gets Crawled](How-a-Page-Gets-Crawled).

## Status (mid-November 2026)

| Part | State |
|---|---|
| Design (spec v3) and plan | Done |
| Library crates (`vc-*`), migrations, testkit with fakes and oracles | Done, tested in CI |
| `vcrawl` binary: every role (`scheduler`, `fetcher`, `processor`, `embedder`, `api`, `maintenance`, `admin`, `migrate`, `health`) | Wired and tested |
| End-to-end test in CI (fake internet, two fetchers, 8 assertions) | Runs on every pull request and push to `main` |
| Agentic loops | Wired: link triage in the processor; source scout and quality auditor weekly in maintenance. No-ops while the LLM is off. |
| Deploy kit (`deploy/`) and GCP bootstrap (`infrastructure/`) | Done, tested with fakes; 12 alert policies, log bucket, Ops Agent config |
| Nightly fuzzing (`vc-policy`, `vc-parse`, `vc-store`) | Runs every night |
| Provisioning and go-live on GCP | Not yet. The crawl IP goes into `BOT.md` before the first crawl. The LLM stays off until the owner raises the project's Vertex spend cap. |
| Voyager backend reading the API | Not started (main repo, behind a flag) |

Still not done:

| Item | Today |
|---|---|
| Vertex batch prediction (half price) | The batch runner exists in `vc-llm`; no role uses it yet. |
| Monthly full restore drill on a Spot VM | Template and account exist; the weekly in-place equivalence drill runs instead. |
| Parquet export of partitions older than 12 months | Stub: the maintenance job only lists due partitions. |
| Contract (destructive) migrations | The mechanism exists (`vcrawl migrate --contract`); none written yet. |
| Embedder and api metrics in Cloud Monitoring | Both log. The embedder serves no `/metrics` and writes no heartbeat; the api's `/metrics` is localhost-only, so otelcol cannot scrape either. |

Pages describe what the code does today. Where a part is designed but not yet built, the
page says so in a **Status** note.

## Where to start

| If you want to… | Read |
|---|---|
| Understand the system | [Architecture](Architecture), [How a Page Gets Crawled](How-a-Page-Gets-Crawled) |
| Know how the bot behaves on your site | [Politeness and Robots](Politeness-and-Robots), [FAQ](FAQ) |
| Use the data | [Read API](Read-API), [Data Model](Data-Model) |
| Change the code | [Contributing](Contributing), [Testing](Testing), the crate pages in the sidebar |
| Run it | [Operations and Deploy](Operations-and-Deploy), [Configuration](Configuration), [Cost](Cost) |
| Look up a word | [Glossary](Glossary) |

## Source of truth

These pages summarise the repository. When they disagree, the repository wins:
the design spec `docs/superpowers/specs/2026-11-15-voyager-crawler-design.md` (its §17 overrides
earlier sections), the plan `docs/superpowers/plans/2026-11-15-voyager-crawler.md`, the
decision ledger `MEMORY.md`, and the rustdoc of each crate.
