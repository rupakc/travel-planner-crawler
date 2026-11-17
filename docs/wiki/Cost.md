# Cost

The crawler runs on one VM in the same GCP project as Voyager (`voyager-prod-ohzkl`,
us-east1). This page gives the monthly estimate from the go-live review
(`infrastructure/README.md`). All amounts are USD per month.

## Line items

| Item | On demand | With a 1-year commitment | Notes |
|---|---|---|---|
| VM, e2-standard-2 (2 vCPU, 8 GB) | 48.92 | 30.82 | Decide on the commitment after 8 weeks. |
| Disks and snapshots | 13.50 | 13.50 | |
| Static external IP | 3.65 | 3.65 | The one published crawl IP. |
| GCS (raw and ops buckets) | 5–8 | 5–8 | Segments move to Coldline after 90 days. |
| Gemini on Vertex AI | 0–30 | 0–30 | Hard monthly cap, embeddings included. Off by default. |
| Logging and monitoring | about 1–2 | about 1–2 | Crawler INFO logs (1–3 GiB) fit the project's 50 GiB free tier; custom metrics fit the free 150 MiB; no process metrics. |
| Egress | about 1 | about 1 | |

## Totals

| Case | On demand | With a 1-year commitment |
|---|---|---|
| LLM off (the default) | about 74 | about 56 |
| Typical (LLM spend $15–25) | about 90–99 | about 71–81 |
| Worst case (LLM at the full $30 cap) | about 104 | about 86 |

The first 8 weeks run on demand.

## The LLM cap

| Setting | Default | Rule |
|---|---|---|
| `llm.monthly_usd` (`VC_LLM_MONTHLY_USD`) | $30 | Down from the spec's $40 until batch prediction (half price) is wired or the 1-year commitment is bought. |
| `llm.daily_usd` | $1.50 | Daily slice of the monthly cap. |
| `VC_PROJECT_CAP_USD` | 0 (unset) | The project's Vertex spend cap in USD. Keep it at least **twice** the monthly LLM cap. With the LLM cap above 50 % of it, or with it unset, the LLM stays off and the `crawler_llm_config_invalid` alert fires ([forced_off](Glossary)). |

Raise the LLM cap only after batch prediction is wired or the commitment is bought: put
`VC_LLM_MONTHLY_USD=…` in `/var/lib/voyager-crawler/deploy/host.local.env` (with
`VC_PROJECT_CAP_USD` at least twice as high) and restart the service; `deploy/compose.yaml`
passes it to every role, and without it the $30 default applies. The daily cap
(`VC_LLM_DAILY_USD`, $1.50) is not forwarded, so it still bounds each day: a monthly cap
above about $45 is only reachable after that is forwarded too. See
[Configuration](Configuration) and [Gemini and Budgets](Gemini-and-Budgets).

## Guard rails

- A GCP budget `voyager-crawler` (₹9,000 a month, on the label `app=voyager-crawler`) alerts at 50, 90 and
  100 %.
- The LLM ledger reserves before every call, and a shared [breaker](Glossary) stops every
  role on a billing or budget error. The crawler's spend can never switch off Voyager's AI.
- Why the design is this cheap (one VM, WARC segments instead of one object per page,
  self-hosted Postgres): see [Architecture](Architecture).
