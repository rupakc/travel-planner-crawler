# Architecture

The crawler is **one Rust binary, `vcrawl`**, whose jobs ("roles") run as tasks of **one
process** (`vcrawl run`, one container) on one small VM; each role can also run alone
(`vcrawl fetcher`, …). The roles never talk to each other directly. They share state through **Postgres** (the
to-do list of URLs, the work queue, the records) and **GCS** (the raw pages).

## Diagram

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
The **politeness gate** is not a separate service. It is a set of checks that every request
to a crawl target must pass, built into `vc-frontier` (who may request) and `vc-fetch` (how
the request is made). Chromium's requests go through the same gate because Chromium has no
network: the fetcher answers each of its requests itself. See
[Politeness and Robots](Politeness-and-Robots) and [Rendering with Chromium](Rendering-with-Chromium).

## Roles

| Role | Job | Main crates |
|---|---|---|
| `scheduler` | Turns sources into URLs: seeds, sitemaps, feeds, adapter rows. Applies source limits, retires rows of disabled sources, refreshes expired robots.txt. Runs the LLM startup probe when the LLM is on. | vc-sources, vc-frontier |
| `fetcher` | The claim loop: claim a site, fetch robots.txt first, fetch its leased URLs politely, render when needed, run API and dump adapters, write raw records to the spool, commit segments. | vc-frontier, vc-fetch, vc-render, vc-store |
| `processor` | Turns committed captures into records: parse, structured data, LLM extraction, grounding checks, place resolution, index, extracted-text tier. Enqueues found links (with [link triage](Agentic-Loops) when the LLM is on). | vc-parse, vc-extract, vc-kinds, vc-llm, vc-index |
| `embedder` | Adds vector embeddings for current records. | vc-llm, vc-index |
| `api` | The private `/v1` read API for Voyager. | vc-api, vc-index |
| `maintenance` | Partitions, purges, ephemeral expiry, weekly restore drill, daily digest, weekly [source scout and quality auditor](Agentic-Loops). | vc-store, vc-index, vc-sources |
| `admin` | Command-line tools: `status`, `source` (list, validate, approve), `purge`, `rebuild-captures`, `reextract`. | all |
| `migrate` | Applies expand-only migrations before a deploy rolls `vcrawl`. `--contract` runs destructive ones by hand; `--print-schema` gives the image's schema labels. | sqlx migrations |
| `health` | The container health check: exits 0 when every role in this container is up and making progress. | vc-obs |

**`vcrawl run`** (the production command) runs the six long-running roles in one process:
one tokio runtime, one Postgres pool sized for the roles (about 19 connections), one
`/metrics` + `/healthz` listener with a heartbeat per role (`crawler_heartbeat{role}`). A
failed role is restarted inside the process with a backoff while `/healthz` reports it down;
SIGTERM stops everything within 25 s (fetches in flight finish, the spool is flushed, leases
are released).

Memory (Spot e2-medium, 4 GB, plus a 2 GB swap file as a safety net only): Postgres
1152 MiB, Chromium 768 MiB, `vcrawl` 1 GiB, the metrics collector (`otelcol`) 96 MiB and the
one-shot `migrate` 128 MiB, 3.3 GB in all. The E2E job measures the peak and fails above
3.2 GB ([Testing](Testing)).

> **Status:** every role is wired (plan Task 15), and the end-to-end test runs them all in
> CI ([Testing](Testing)). See [vcrawl](Crate-vcrawl) for what is still open.

## Networks

Each container joins only the docker networks it needs. `db`, `render` and `metrics` are
internal: they have no route out of the VM.

```mermaid
flowchart LR
  subgraph render["render (internal)"]
    CHR["chromium"]
  end
  subgraph metrics["metrics (internal, 192.168.250.0/29)"]
    FM["vcrawl /metrics + /healthz"]
  end
  subgraph db["db (internal)"]
    PG[("postgres")]
  end
  VC["vcrawl run: every role"]
  OTL["otelcol"]
  OUT["egress: GCS, Vertex, metadata server, the web (fetcher only, with a permit)"]

  CHR <-->|"CDP"| VC
  VC --- FM
  OTL -->|"scrape :9464"| FM
  VC --> PG
  VC --> OUT
  OTL --> OUT
  PG -->|"wal-g backups, pgbackup network"| OUT
```

| Network | Members | Why |
|---|---|---|
| `db` | Postgres and `vcrawl` | Database access; no route out. |
| `render` | Chromium and `vcrawl` | Chromium can only talk to the fetcher, which answers each of its requests under the page's permit. |
| `metrics` | `vcrawl` and `otelcol` | `vcrawl` binds `/metrics` and `/healthz` here only, so Chromium (on `render`) cannot reach them. |
| `egress` | `vcrawl`, `otelcol` | GCS, Vertex AI, the metadata server; only the fetcher reaches crawl targets, and only through permits. |
| `pgbackup` | Postgres alone | wal-g's route to the ops bucket. |

`otelcol` (OpenTelemetry Collector) scrapes `vcrawl`'s `/metrics` every 60 s (`:9464`, every role's series; a compose test checks the scrape target against the listener), keeps only
`crawler_*` series and writes them to Cloud Monitoring as `custom.googleapis.com/crawler/*`.
The Ops Agent on the host sends host metrics and container logs. See
[Operations and Deploy](Operations-and-Deploy).

## Crates: a star graph

All shared types and **every trait between crates** live in `vc-core`. Each library crate
depends only on `vc-core`, `vc-config` and (for tests) `testkit`. Only the `vcrawl` binary
depends on the implementations and plugs them together. This lets each crate be built and
tested alone, with fakes.

```mermaid
flowchart TB
  CORE["vc-core: ids, types, all traits"]
  CFG["vc-config: settings"]
  subgraph LIBS["Library crates, each implements vc-core traits"]
    POL["vc-policy"]
    FRO["vc-frontier"]
    FE["vc-fetch"]
    REN["vc-render"]
    STO["vc-store"]
    PAR["vc-parse"]
    KIN["vc-kinds"]
    LLM["vc-llm"]
    EXT["vc-extract"]
    SRC["vc-sources"]
    QUE["vc-queue"]
    IDX["vc-index"]
    APIC["vc-api"]
    OBS["vc-obs"]
    SEC["vc-secrets"]
  end
  BIN["vcrawl binary"]
  LIBS --> CORE
  LIBS --> CFG
  BIN --> LIBS
```

| Trait (in vc-core) | What it hides | Implemented by |
|---|---|---|
| `Policy` | robots, denylist, SSRF, challenge detection | [vc-policy](Crate-vc-policy) |
| `Frontier`, `LeaseCheck` | claims, leases, rate tokens, enqueue, park | [vc-frontier](Crate-vc-frontier) |
| `PermitClient` | permit-gated HTTP | [vc-fetch](Crate-vc-fetch) |
| `Renderer` | Chromium through CDP interception | [vc-render](Crate-vc-render) |
| `BlobStore`, `CaptureSink`, `SegmentCommitter` | GCS or disk, spool, commit | [vc-store](Crate-vc-store) |
| `ContentHandler` | one parser per media type | [vc-parse](Crate-vc-parse) |
| `KindRegistry` | entity kinds from data files | [vc-kinds](Crate-vc-kinds) |
| `LlmClient`, `Embedder`, `BudgetLedger`, `BatchRunner` | Gemini, embeddings, spend control | [vc-llm](Crate-vc-llm) |
| `PlaceResolver` | place names to GeoNames ids | [vc-extract](Crate-vc-extract) |
| `SourceAdapter` | API and dump sources | [vc-sources](Crate-vc-sources) |
| `WorkQueue` | jobs between roles | [vc-queue](Crate-vc-queue) |
| `RecordSink`, `IndexReader` | write and read the index | [vc-index](Crate-vc-index) |
| `SecretSource` | Secret Manager or env | [vc-secrets](Crate-vc-secrets) |

## Data flow in one paragraph

Source files in `sources/` become URLs in the **frontier** (Postgres tables `urls` and
`url_state`, grouped by **politeness key**, usually one per site). A fetcher **claims** one
key at a time and gets a **permit** plus up to 5 due URLs. It fetches them one by one, with
a delay, after checking robots.txt, the denylist and the address. Each response is written as
a WARC record to a local **spool** file and fsynced, and the URL moves to
`fetched_pending`. A **segment roller** packs spool files into compressed WARC segments,
uploads them to GCS, and only then commits the capture rows in one transaction. That commit
also queues `capture_ready` work. The processor parses the
capture, extracts records (structured data first, Gemini second, under a budget), checks
them against the page text, resolves places, and writes versioned records and entities. The
API serves them, with licence rules applied, to Voyager.

## Why this design

| Choice | Why |
|---|---|
| Own Rust core, not the `spider` crate | `spider` has bot-evasion features on by default. That conflicts with the core promise. |
| One VM, not managed services | Cloud Run + Pub/Sub + Cloud SQL would cost $150–250+ a month. Traits keep that path open. |
| Postgres as frontier and queue | One place for claims, leases and rate tokens, so politeness holds across processes. `FOR UPDATE SKIP LOCKED` gives fair, lock-free claiming. |
| WARC segments in GCS, not one object per page | One object per page would cost about $125 a month in GCS operations. |
| Self-hosted Postgres 17 + pgvector | Cloud SQL is too small or too expensive at this size. Backups go to GCS with wal-g. |
| Gemini only, off by default | The crawler shares a GCP project with Voyager. Its spend must never switch off Voyager's AI. See [Cost](Cost). |
| gcloud bootstrap script, no Terraform | Same pattern as the main repo; idempotent, with a dry run and tests. |

## Scaling later

| Area | Today | At 10x |
|---|---|---|
| Fetchers | one VM | more VMs, each leasing a set of `shard`s, each with its own published IP |
| Frontier | Postgres | already partitioned; a bigger Postgres or another store behind the `Frontier` trait |
| Work queue | Postgres | Pub/Sub behind the `WorkQueue` trait |
| Embeddings | in the main database | a dedicated pgvector instance or Vertex Vector Search |
| Old versions and captures | Postgres | exported to Parquet after 12 months (the maintenance job only lists due partitions today) |
| Raw storage | Standard, then Coldline | Standard, Coldline, then Archive |

See also: [How a Page Gets Crawled](How-a-Page-Gets-Crawled), [Data Model](Data-Model),
[Operations and Deploy](Operations-and-Deploy).
