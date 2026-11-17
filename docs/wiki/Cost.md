# Cost

The crawler runs on one e2-medium VM in the same GCP project as Voyager
(`voyager-prod-ohzkl`, us-east1): a **Spot** VM this month, then a standard VM with a
1-year commitment. The VM is the single instance of a stateful managed instance group,
so a preempted VM comes back with the same disks and IPs. All amounts are USD per month,
from Cloud Billing catalog list prices (us-east1, November 2026); details in
`infrastructure/README.md`.

## Line items

| Item | This month: Spot | Next month: standard, 1-year commitment | Notes |
|---|---|---|---|
| VM, e2-medium (2 shared vCPUs, 4 GB; billed as 1 vCPU) | about 14 | about 15.4 | Spot: core $0.012440/h, RAM $0.001666/GiB·h (Spot prices can change daily). On demand would be about 24.5 (core $0.021812/h, RAM $0.002924/GiB·h). A commitment covers standard VMs only. |
| Disks: 10 GB boot + 30 GB data, pd-balanced | about 4 | about 4 | $0.10/GiB·month. No snapshots: wal-g backups to GCS and a weekly restore drill instead. |
| Static external IP | about 2–3.65 | about 3.65 | The one published crawl IP. Cheaper on a Spot VM; $0.01/h while the VM is stopped. |
| GCS (raw and ops buckets) | about 2–4 | about 2–4 | Segments move to Coldline after 90 days; weekly full base backups, kept 14 days. |
| Other | about 1 | about 1 | Logging and monitoring inside the free tiers, egress; the autohealing health check is free. |
| Gemini on Vertex AI | 0–30 | 0–30 | Hard monthly cap, embeddings included. Off by default. |

## Totals

| Case | This month: Spot | Next month: standard, 1-year commitment |
|---|---|---|
| LLM off (the default) | about 23–27 | about 25–28 |
| Worst case (LLM at the full $30 cap) | about 53–57 | about 55–58 |

A preemption costs nothing extra: a stopped VM bills only its disks and IP. If Spot
capacity runs out for hours, `infrastructure/bootstrap.sh --update-host --standard`
moves the VM to on demand (about 24.5 a month, prorated) until the commitment.

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
