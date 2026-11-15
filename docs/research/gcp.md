# GCP hosting for a 24x7 polite Rust crawler + headless Chromium (~$100/month)

Date: 2026-11-15. Project `voyager-prod-ohzkl`, region us-east1 (South Carolina).

**How the prices were collected.** Unless marked *(web)* or *(unverified)*, every price below was read from the
**Cloud Billing Catalog API** (`cloudbilling.googleapis.com/v1/services/*/skus?currencyCode=USD`), queried read-only
with the project as quota project on 2026-11-15. These are list prices in USD before tax and before any discounts.
Months are 730 h. **The billing account `010A0B-F39593-14A899` is billed in INR**, so budgets must be written in INR,
at roughly ₹88 per USD (check the rate when you create them).

---

## 0. What already exists (read-only gcloud, 2026-11-15)

| Resource | State |
|---|---|
| VMs | `voyager-vm`, e2-micro, us-east1-b, on-demand, external IP 35.207.18.35, **Standard network tier**, SA `vm-runtime@…` with the cloud-platform scope |
| Addresses | `voyager-ip` 35.207.18.35 static, regional us-east1, IN_USE |
| Disks | `voyager-vm` 30 GB pd-standard |
| Cloud Routers / NAT | none |
| Cloud SQL | **`sqladmin.googleapis.com` is not enabled** (`sql-component` is), so there are no instances |
| Buckets | `voyager-prod-ohzkl-voyager-backup` (US-EAST1, Standard, versioned lifecycle: delete noncurrent after 30 d or past 100 newer versions, **soft delete off**). `vm-runtime` has `roles/storage.objectUser` **on this bucket only** (the least-privilege pattern to copy) |
| Service accounts | Compute default SA, `vm-runtime` (custom roles `voyagerGeminiInvoker` = `aiplatform.endpoints.predict` only, and `voyagerFcmSender`), `firebase-adminsdk-fbsvc` |
| Secrets | admin-password, jwt-secret-key, serpapi-key, serper-key, smtp-password (user-managed, us-east1) |
| Monitoring | uptime check `voyager-health`, alert policy `voyager-down` |
| Firewall | default-allow-icmp, default-allow-internal (10.128.0.0/9), voyager-iap-ssh (35.235.240.0/20:22), voyager-web (0.0.0.0/0:80,443) |
| APIs enabled | aiplatform, compute, secretmanager, monitoring, logging, billingbudgets, bigquery, pubsub, datastore, iam, iamcredentials, … (no sqladmin, no artifactregistry, no container) |
| Budgets | `gcloud billing budgets list` works with `--billing-project=voyager-prod-ohzkl` (without it the call fails on the default quota project `experiments-390711`). It shows **"Voyager total (alerts)" ₹2,300/month** for project 581729147869 (voyager-prod), thresholds 50/80/100 % plus 100 % forecast, and a "Reduce Cost" ₹5,000 budget on another project. `docs/Deployment-GCP.md` also documents a **"Voyager Gemini spend cap" of ₹1,800** that enforces a hard cap on the Agent Platform (Vertex) service for this project. |

**Important consequence.** If the crawler runs in `voyager-prod-ohzkl`, its Gemini spend counts against the ₹1,800
spend cap. A busy month of crawler extraction would trip the cap and turn off the travel planner's AI (the app's
`ai_limit` path) until the 1st, and the ₹2,300 total budget would alert every month. See §8 for the fix.

---

## 1. Compute

### Price sheet (us-east1, list prices)

| SKU | Price |
|---|---|
| E2 core, on-demand | $0.02181159/h |
| E2 RAM, on-demand | $0.00292353/GiB-h |
| E2 core / RAM, **Spot** | $0.01309/h · $0.001754/GiB-h |
| E2 core / RAM, 1-year resource CUD | $0.013741/h · $0.0018418/GiB-h |
| E2 core / RAM, 3-year resource CUD | $0.009815/h · $0.0013156/GiB-h |
| pd-standard / pd-balanced / pd-ssd | $0.04 / $0.10 / $0.17 per GiB-mo |
| Hyperdisk Balanced capacity | $0.09/GiB-mo (+ IOPS/throughput above the baseline) |
| Snapshot storage | $0.05/GiB-mo |
| Cloud Run **worker pools** (us-east1) | $0.000011244/vCPU-s · $0.000001235/GiB-s |
| Cloud Run services, instance-based billing | $0.000018/vCPU-s · $0.000002/GiB-s |
| GKE Autopilot pod (regular) | $0.0445/vCPU-h · $0.0049225/GiB-h |
| GKE Autopilot pod (Spot) | $0.0133/vCPU-h · $0.0014767/GiB-h |
| GKE cluster management fee | $0.10/h ($73/mo); the GKE free tier gives $74.40/mo of credit per billing account, enough for one Autopilot or zonal cluster *(web, long-standing)* |

### Monthly cost for an always-on footprint

| Option | Shape | $/month |
|---|---|---|
| **e2-standard-2 VM, on-demand** | 2 vCPU / 8 GB | **$48.92** |
| e2-standard-2, 1-yr CUD | same | **$30.82** |
| e2-standard-2, 3-yr CUD | same | $22.01 |
| e2-standard-2, **Spot** | same | **$29.35** |
| e2-medium, on-demand | 2 shared vCPU (1 sustained) / 4 GB | $24.46 |
| e2-medium, Spot | same | $14.68 |
| e2-highmem-2 on-demand | 2 vCPU / 16 GB | ~$66.03 |
| e2-standard-4 on-demand | 4 vCPU / 16 GB | $97.84 |
| Cloud Run worker pool 24x7 | 2 vCPU / 4 GiB | $72.08 (no DB, see below) |
| Cloud Run service, 1 min-instance 24x7 | 1 vCPU / 2 GiB | $57.82 |
| GKE Autopilot, regular pods | 2 vCPU / 8 GiB total requests | $93.73 (+ fee, covered by the credit) |
| GKE Autopilot, Spot pods | 2 vCPU / 8 GiB | $28.04 (+ fee, covered by the credit) |

E2 VMs get **no sustained-use discount**. The CUD is a regional resource commitment (vCPU + GB), so it still applies
if you later resize within the E2 family.

### Trade-offs

* **One VM + docker compose** (crawler workers, Chromium, Postgres, read API, Ops Agent). This is the cheapest
  option, it matches the existing deploy pattern (signed GHCR images pulled by a host script), and it gets the
  Standard-tier egress allowance (§4). e2-standard-2 is the right size: Postgres with a ~1.5–2 GB buffer, a Chromium
  pool of 2–3 contexts at ~300–500 MB each, the Rust fetchers (small), and the API. e2-medium (4 GB, 1 sustained
  vCPU) works only if Chromium is limited to one context and Postgres to ~512 MB. Use it only to start lean.
* **Spot.** $29.35 vs $48.92. Spot VMs can be preempted at any time with 30 s notice and have no maximum runtime or
  SLA. A preempted VM with `instanceTerminationAction=STOP` stays stopped until something restarts it (a MIG of size 1
  with a stateful disk, or a Cloud Scheduler job that runs `instances start`). Postgres recovers from a crash
  cleanly and the crawler resumes from its frontier, but the **read API the travel planner calls goes dark** during
  preemptions. For a 24x7 API, a **1-year CUD ($30.82) costs about the same as Spot without the outages**. Spot is
  the right choice for stateless *extra* fetchers at 10x (§10), not for the box that holds the DB and API.
* **Cloud Run.** Worker pools ($72/mo for 2 vCPU/4 GiB) are a good fit for a long-running crawler, but Postgres
  cannot live there. That forces Cloud SQL (≥ $33, §2), and a fixed egress IP for polite crawling (a stable,
  documented User-Agent and IP) needs Direct VPC egress + Cloud NAT ($0.045/GiB **processed in both directions**,
  ≈ $20–25/mo on ~500 GB of raw downloads, §4). That is already $125+ before Gemini. Cloud Run *jobs* are still a
  good choice for the batch-extraction submitter at 10x.
* **GKE Autopilot.** Spot pods are cheap ($28) and the free-tier credit covers the cluster fee, but you take on a
  StatefulSet Postgres on a PD, pod evictions, a NAT or public-node decision, and Kubernetes ops for one node's worth
  of work. Not worth it below ~10x.

---

## 2. Database (metadata + frontier + index, with pgvector)

### Cloud SQL for PostgreSQL, Enterprise edition, zonal (HA off), Americas SKUs

| Item | Price | $/month |
|---|---|---|
| db-f1-micro (shared, 0.6 GB) | $0.0105/h | **$7.67** |
| db-g1-small (shared, 1.7 GB) | $0.035/h | **$25.55** |
| dedicated vCPU | $0.0413/h | — |
| dedicated RAM | $0.007/GiB-h | — |
| 1 vCPU / 3.75 GB custom | 0.0413 + 0.02625 | $49.31 |
| 2 vCPU / 8 GB | 0.0826 + 0.056 | $101.18 |
| SSD storage | $0.17/GiB-mo | 50 GB = $8.50 |
| HDD storage | $0.09/GiB-mo | |
| Backups | $0.08/GiB-mo | |
| Regional (HA) | doubles instance + storage | |

* **Shared-core is still offered.** SKUs for both shared tiers exist today, and the docs list db-f1-micro and
  db-g1-small for Enterprise edition (PostgreSQL 16/17, with 18 the default). They are **not covered by the Cloud
  SQL SLA**, and they are not eligible for CUDs *(web)*. **pgvector** is supported on Cloud SQL PostgreSQL.
* A crawl frontier of tens of millions of rows, with constant upserts and politeness-queue scans, does not fit in
  0.6 or 1.7 GB of RAM. The realistic minimum is **db-g1-small + 50 GB SSD + backups ≈ $38/mo**, and a comfortable
  setup is 1 vCPU/3.75 GB ≈ $62/mo. You would also need to enable `sqladmin.googleapis.com` and use the Auth Proxy or
  connector. Private IP needs Private Service Access (`servicenetworking` is already enabled).

### Self-hosted Postgres 17 + pgvector on the VM

* Separate **100 GB pd-balanced data disk: $10.00/mo**. pd-standard ($4) is too slow for random I/O, so don't use it
  for Postgres.
* Backups: a nightly `pg_dump -Fc` (zstd) to GCS, keeping 7 daily + 4 weekly dumps (~10 × 5–10 GB on Standard/Nearline
  ≈ $1–2), plus a weekly **snapshot schedule** on the data disk (incremental, ~30 GiB × $0.05 = $1.50). Write a
  `last_backup_ok` heartbeat for alerting (§6). Restore drill: snapshot → new disk, or `pg_restore` from the dump.
* Cost **≈ $12–13/mo** instead of $38–62 for Cloud SQL. You give up managed PITR, automatic minor upgrades and HA,
  and none of those is needed at this size.

### Alternatives (why not)

| Store | Price signal (us-east1) | Fit |
|---|---|---|
| AlloyDB | vCPU $0.07467/h; smallest practical instance ≈ 2 vCPU/16 GB ≈ $250+/mo *(memory price unverified)* | Way over budget |
| Bigtable | node $0.65/h = **$474.50/mo** minimum + storage | Way over budget, and no SQL/pgvector |
| Firestore | per-operation pricing; a frontier does ~10 writes per crawled URL (enqueue, lease, done, recrawl schedule) → 50M+ writes/mo ≈ $90+ | Wrong access pattern for a frontier |
| BigQuery | storage $0.02/GiB, 1 TiB of queries free per month | Not OLTP. Good later as an analytics sink for extracted records, not as the frontier |

**Pick: self-hosted Postgres 17 + pgvector** on the VM's data disk.

---

## 3. Raw storage (versioned WARC/zstd)

### Price sheet (us-east1 regional)

| | Standard | Nearline | Coldline | Archive |
|---|---|---|---|---|
| Storage $/GiB-mo | 0.020 (first 5 GiB free) | 0.010 | 0.004 | 0.0012 |
| Class A per 1k ops | $0.005 (first 5k free) | $0.010 | $0.020 | $0.050 |
| Class B per 1k ops | $0.0004 (first 50k free) | $0.001 | $0.010 | $0.050 |
| Retrieval $/GiB | 0 | 0.01 | 0.02 | 0.05 |
| Minimum duration | none | 30 d | 90 d | 365 d |

GCS → VM in the same region: no transfer charge.

### Volume: 5M pages/month × 30 KB = 150 GB ≈ 139.7 GiB/month

**Rule #1: never write one object per page.** 5M PUTs cost **$25/mo** on Standard, and the lifecycle transitions
would add **$50 (Nearline) or $100 (Coldline) per month**, because SetStorageClass bills a Class A op at the
destination class's rate per object. Write **segmented WARC files** of ~128 MB, each record zstd-compressed
(`raw/YYYY/MM/DD/<worker>-<seq>-<uuid>.warc.zst`, write-once names). That's ~1,200 objects/month, so ops cost
≈ **$0.01**. Postgres stores `(url, segment, offset, length, sha256, fetched_at, status)` so the processor reads
one record with a ranged GET (Class B, 200k reads ≈ $0.08).

| Policy | Month 1 | Month 12 | Year-1 total |
|---|---|---|---|
| Standard forever | $2.69 | $33.43 | ≈ $218 |
| Standard 30 d → Nearline | $2.69 | $18.16 | ≈ $134 |
| **Standard 30 d → Coldline** | $2.69 | **$8.94** | **≈ $70** |
| Standard 30 d → Coldline → delete after 365 d | same | $8.94, then flat ≈ $9 | |

Recommended bucket (`<project>-crawl-raw`, us-east1, uniform access, public access prevention):

* **Object versioning on**, plus lifecycle `Delete` for noncurrent versions after 30 days (the same pattern as the
  existing backup bucket). Because names are write-once, versioning only costs anything on accidental overwrite or
  delete. For hard immutability, add a bucket **retention policy** (e.g. 30 days, unlocked at first).
* **Soft delete:** the default is 7 days, billed on deleted bytes. Keep it on this bucket; it is cheap because almost
  nothing is deleted.
* Lifecycle: `SetStorageClass COLDLINE` at age 30 d (`matchesStorageClass: STANDARD`), optional `Delete` at age 365 d
  if raw pages don't need to be kept forever. Keep anything you might re-extract in the first 30 days in Standard;
  re-reading Coldline costs $0.02/GiB (a full month's re-read is $2.80).
* Autoclass is an alternative. It charges a $0.0025 per 1,000 objects/month management fee, which is negligible with
  segments, but it moves data to Coldline only after 90 days without access, so it costs more than the explicit rule
  here.

---

## 4. Networking / egress

| Item | Price |
|---|---|
| Internet **ingress** | free (all tiers) |
| **Standard tier** egress from South Carolina | **first 200 GiB/mo free**, then $0.085/GiB (to 10 TiB) |
| Premium tier egress Americas→Americas | first 1 GiB free, then $0.12/GiB |
| External IPv4 on a standard VM | $0.005/h = **$3.65/mo** (same for a static IP in use) |
| External IPv4 on a Spot VM | $0.0025/h = $1.83/mo |
| Cloud NAT | $0.0014/h per gateway-VM + **$0.045/GiB processed (both directions)** + $0.005/h per NAT IP |
| VM↔VM in the same zone (internal IP) | free; inter-zone in region $0.01/GiB |

* **Cloud NAT is not needed.** Give the crawler VM its own external IPv4 on the **Standard tier** (like `voyager-vm`).
  A **static** IP is worth the same $3.65: a fixed, published crawler IP plus a User-Agent with a contact URL is part
  of being polite, and it lets site owners allowlist or rate-limit you. NAT would cost about $0.045 × ~500 GiB of
  raw (uncompressed) downloads ≈ **$22/mo** for nothing.
* Crawl egress is small: 5M requests × ~2–5 KB (headers + TLS handshakes, more with HTTP/1.1 and new connections)
  ≈ 10–25 GiB/month. Chromium adds subresource requests, so block images, media and fonts in the headless profile.
  This stays inside the 200 GiB Standard-tier allowance. Note that the allowance is per region/SKU and is **shared
  with `voyager-vm`'s web traffic**. Budget $1.
* Travel-planner → crawler API: internal IP in the same VPC (free in the same zone, so put the crawler in
  **us-east1-b** too). If the crawler lives in its own project (§8), either peer the VPCs (the crawler project needs a
  **custom-mode subnet outside 10.128.0.0/9**, e.g. 10.200.0.0/24, because two auto-mode default networks overlap and
  cannot peer), or expose the API on 443 with a firewall rule limited to `35.207.18.35/32` plus a bearer token or
  Google ID token. Volume is KBs, so the cost is $0.

---

## 5. Vertex AI (Gemini) — extraction and embeddings

### Text models (global endpoint, per 1M tokens, from catalog SKUs)

| Model | Input | Output (incl. thinking) | Batch / Deferred / Off-peak input | Batch output | Cached input |
|---|---|---|---|---|---|
| **gemini-3.5-flash-lite** (the app's model) | **$0.30** | **$2.50** | $0.15 | $1.25 | $0.03 (batch $0.015) |
| gemini-3.1-flash-lite | $0.25 | $1.50 | $0.125 | $0.75 | $0.025 |
| gemini-2.5-flash-lite | $0.10 | $0.40 | $0.05 | $0.20 | — **retired on Vertex 2026-10-20** *(web)*, don't plan on it |
| gemini-3.8-flash (reference) | $1.50 | $7.50 | $0.75 | $3.75 | |
| Regional endpoint (3.5 FL) | $0.33 | $2.75 | $0.165 | $1.375 | |

The batch discount is exactly 50 %. For 3.5 Flash-Lite, "Deferred" and "Off-peak" online SKUs exist at the batch
price, which is a useful alternative to batch jobs when you want per-call code instead of JSONL files. Thinking
tokens bill as output, so keep thinking at `minimal` as the app already does.

### The requested workload: 200k pages × (3,000 in + 500 out) = 600M in + 100M out

| Model / mode | $/month |
|---|---|
| 3.5 Flash-Lite, online | **$430** |
| 3.5 Flash-Lite, batch | **$215** |
| 3.1 Flash-Lite, online | $300 |
| 3.1 Flash-Lite, batch | $150 |

**This alone breaks the $100 budget.** Output tokens drive the cost (on 3.5 FL batch, 100M out = $125). To fit
about $35:

1. **Don't call the LLM when you don't need to.** Skip pages whose cleaned-content hash is unchanged since the last
   extraction (recrawls are mostly unchanged). Extract JSON-LD/schema.org/OpenGraph/microdata deterministically in
   Rust first, and only send pages where that is insufficient. Expect 30 %+ fewer calls.
2. **Shrink the input to ~1,000 tokens.** Use readability-style main-content extraction, strip nav/boilerplate, and
   truncate. Keep the system prompt as an identical prefix so implicit caching bills it at 10 %.
3. **Shrink the output to ~150 tokens.** Use a compact response schema (short keys, enums, no prose, null-omission).
4. **Use batch** (or 3.5 FL Off-peak/Deferred): results within 24 h at 50 % off. That's fine for a crawler.

| Scenario (batch) | Calls | Tokens in / out | 3.1 FL | 3.5 FL |
|---|---|---|---|---|
| Full spec | 200k | 3,000 / 500 | $150 | $215 |
| Trimmed, all pages | 200k | 1,000 / 150 | $47.50 | $67.50 |
| **Trimmed + skip unchanged/deterministic (≈140k calls)** | 140k | 1,000 / 150 | **$33.25** | $47.25 |

Recommendation: evaluate **gemini-3.1-flash-lite** against 3.5 FL with the existing `backend/scripts/llm_eval.py`
approach on a crawl sample. It is 40 % cheaper on output. Hard-cap spend in the crawler itself: count
`usage_metadata` tokens × price per day and stop submitting past a daily $ limit, because billing data and budget
alerts lag by up to a day.

### Embeddings (per 1M tokens unless noted)

| Model | Online | Batch |
|---|---|---|
| text-embedding-005 ("Embeddings for Text") | $0.025 per 1M **characters** *(the SKU is per count; Google's docs price this model per character)* | $0.020 |
| gemini-embedding-001 ("Large Text Embedding Model") | $0.15 | $0.12 |
| Gemini Embedding 2 (multimodal, text input) | $0.20 | $0.10 |

Embedding the extracted summary, not the page (~300 tokens ≈ 1,200 chars × 200k = 60M tokens / 240M chars):
text-embedding-005 batch ≈ **$4.80**, gemini-embedding-001 ≈ $7.20–9.00. Store 768-dim vectors in pgvector
(`halfvec` halves disk use) with an HNSW index.

---

## 6. Secrets, logging, monitoring, budgets

| Service | Free tier | Beyond | Expected |
|---|---|---|---|
| Secret Manager | 6 active versions, 10k accesses/mo (billing-account wide) | $0.06/version-mo, $0.03 per 10k accesses, $0.05 per rotation (after 3) | ≤ $0.30 (the 5 existing secrets already use most of the free versions) |
| Cloud Logging | **50 GiB ingest/project/mo**, 30-day retention | $0.50/GiB, $0.01/GiB-mo extra retention | $0 if you log a summary per batch, not per fetch, and drop Chromium stdout in the Ops Agent config |
| Monitoring metrics (Ops Agent) | 150 MiB/mo | $0.258/MiB | ~$0–1 for one VM |
| Uptime checks | 1M executions/project/mo | $0.30 per 1k | $0 |
| Synthetic monitors | 100 executions | $1.20 per 1k | not needed |
| Alerting policies | Google announced per-condition charges for 2026 (reported figures vary between $0.10 and $1.50 per condition/month, plus per-time-series query fees). **No alerting SKU appears in the catalog today**, so it is not billed yet *(unverified)*. | | budget $2 |

Monitoring plan: an uptime check on the read API's `/health` (public 443 with a firewall, or a private uptime check
via Service Directory), and alerts for VM down, disk > 80 % (data disk and boot), crawl throughput below X pages per
10 min (a log-based or custom metric from the crawler), `last_backup_ok` older than 26 h, and a Gemini $/day log
metric. Send them to the same email channel `voyager-down` uses.

**Budget (INR!):**

```bash
gcloud billing budgets create \
  --billing-account=010A0B-F39593-14A899 --billing-project=voyager-prod-ohzkl \
  --display-name="Crawler total (alerts)" \
  --budget-amount=8800INR \
  --filter-projects=projects/<crawler-project-number> \
  --threshold-rule=percent=0.5 --threshold-rule=percent=0.8 \
  --threshold-rule=percent=1.0 --threshold-rule=percent=1.0,basis=forecasted-spend
```

The amount has to be in the account currency (INR); `USD` is rejected on an INR account. Add a separate **Gemini
spend cap** on the Agent Platform service (`C7E2-9256-1C43`) for the crawler project, e.g. ₹4,000 (~$45). Per
`docs/Deployment-GCP.md` that is console-only: Billing → Budgets → *Spend cap enforcement*.

---

## 7. Identity and least privilege

"Workload identity" for a GCE VM means the **attached service account**: tokens come from the metadata server, and
there are no keys on disk. Workload Identity *Federation* is only for things outside GCP (GitHub Actions). The
existing pipeline gives CI **no GCP access** at all (the VM pulls signed images from GHCR), so nothing is needed there.

Create `crawler-runtime@<project>.iam.gserviceaccount.com`, attach it to the VM with the `cloud-platform` scope (IAM
is the real boundary), and grant **resource-level** bindings, not project-level ones where avoidable:

```bash
# raw bucket only (objectAdmin lets it delete/overwrite; objectCreator+objectViewer is tighter if the crawler never deletes)
gcloud storage buckets add-iam-policy-binding gs://<proj>-crawl-raw \
  --member=serviceAccount:crawler-runtime@<proj>.iam.gserviceaccount.com --role=roles/storage.objectCreator
gcloud storage buckets add-iam-policy-binding gs://<proj>-crawl-raw \
  --member=serviceAccount:crawler-runtime@… --role=roles/storage.objectViewer
# backups bucket (pg_dump): objectUser, the same as vm-runtime on the existing backup bucket
# Vertex: copy the existing pattern, a custom role, not roles/aiplatform.user (which is broad)
#   online: aiplatform.endpoints.predict
#   batch:  + aiplatform.batchPredictionJobs.create/get/list, plus object access on a batch-I/O bucket,
#           and the Vertex service agent needs read/write on that bucket
# secrets: per secret, not project-wide
gcloud secrets add-iam-policy-binding crawler-api-token --role=roles/secretmanager.secretAccessor \
  --member=serviceAccount:crawler-runtime@…
# Ops Agent
roles/logging.logWriter, roles/monitoring.metricWriter (project level)
# only if Cloud SQL were chosen: roles/cloudsql.client (+ roles/cloudsql.instanceUser for IAM DB auth)
```

The travel-planner's `vm-runtime` SA needs nothing new if the API is reached over the VPC with a shared token. If
you use Google ID tokens instead, the crawler API verifies an ID token minted by `vm-runtime` (audience = API URL),
with no extra IAM.

Images: keep the existing pattern. GHCR images, `cosign sign` keyless in CI, and the VM's deployer runs
`cosign verify --certificate-identity-regexp '^https://github.com/rupakc/<repo>/.github/workflows/deploy.yml@refs/heads/main$' --certificate-oidc-issuer https://token.actions.githubusercontent.com`
and pulls **by digest**. Use a host deploy script modelled on `deploy/gcp/host/deploy.sh` with a 1-minute timer.

---

## 8. Recommended architecture (fits $100)

**One e2-standard-2 VM (1-year E2 resource CUD) in us-east1-b running docker compose. Postgres 17 + pgvector on a
100 GB pd-balanced data disk, raw WARC segments in a versioned GCS bucket with a Standard → Coldline lifecycle, and
Gemini Flash-Lite in batch mode. Put it in a separate project, e.g. `voyager-crawler-xxxx`, on the same billing
account.**

```
GitHub Actions ── build ─► GHCR (cosign keyless) ◄── pull by digest + cosign verify (systemd timer, 1 min)
                                                      │
 ┌─────────────────────── crawler VM (e2-standard-2, us-east1-b, static IPv4, Standard tier) ───────┐
 │ fetcher (Rust: robots.txt cache, per-host politeness queues, conditional GETs, WARC writer)       │
 │ renderer (headless Chromium, 2–3 contexts, images/media/fonts blocked) ◄─ fetcher sends JS pages  │
 │ processor (Rust: readability + JSON-LD extract, content hash, LLM batch JSONL builder, embedder)  │
 │ api (Rust axum, read-only; bearer token / ID token; listens on the internal IP only)             │
 │ postgres 17 + pgvector (data disk /mnt/pg)        ops-agent (logs/metrics)                         │
 └─────────┬───────────────────────────────┬────────────────────────────┬────────────────────────────┘
           │ segments (.warc.zst, ~128 MB)  │ batch JSONL in / out      │ VPC peering or 443 allowlist
           ▼                                ▼                           ▼
   gs://…-crawl-raw (versioned,     Vertex AI batch prediction    voyager-vm (travel-planner backend)
   Std 30 d → Coldline)             gemini-3.x-flash-lite,
   gs://…-crawl-backups (pg_dump)   text-embedding-005
```

Why a separate project:

1. The crawler's Gemini spend stays out of the travel planner's ₹1,800 spend cap and ₹2,300 alert budget, so a heavy
   extraction month can never switch off the app's AI.
2. Its own budget and spend cap make the $100 enforceable.
3. Blast radius: `crawler-runtime` can't see the app's secrets, and vice versa.

The cost is a VPC peering setup (custom-mode subnet outside 10.128.0.0/9). If you'd rather keep a single project,
raise the Gemini spend cap to cover both workloads, add a project budget, and rely on the crawler's own daily $ cap.
The travel planner's AI then shares the cap's fate.

### Line-item budget (steady state, year 1)

| Item | Basis | $/month |
|---|---|---|
| VM e2-standard-2, **1-yr CUD** | 2 × $0.013741 + 8 × $0.0018418 per h | **30.82** |
| Boot disk 20 GB pd-balanced | $0.10/GiB | 2.00 |
| Data disk 100 GB pd-balanced (Postgres) | $0.10/GiB | 10.00 |
| Snapshot schedule (weekly, incremental ~30 GiB) | $0.05/GiB | 1.50 |
| Static external IPv4 (Standard tier) | $0.005/h | 3.65 |
| Internet egress | ≤ 25 GiB, inside the 200 GiB Standard-tier allowance | 0–1.00 |
| GCS raw (5M pages/mo, 140 GiB/mo, Std 30 d → Coldline) | month 1 $2.69 → month 12 $8.94; ops ≈ $0.01 | **≈ 6 (avg), 9 (month 12)** |
| GCS backups (pg_dump, 7 daily + 4 weekly) | ~100 GiB mixed Standard/Nearline | 1.50 |
| Gemini extraction (≈140k calls, 1,000 in / 150 out, **3.1 FL batch**) | §5 | **33.25** |
| Embeddings (200k × 300 tokens, text-embedding-005 batch) | §5 | 4.80 |
| Secret Manager, Logging, Monitoring, uptime, alerting | mostly free tier | 2.00 |
| **Total** | | **≈ $97–100** |

Notes:

* **On-demand instead of CUD: +$18 → ≈ $115.** Run on-demand for the first 2–4 weeks to confirm sizing, then
  commit. Spot gives the same saving without a commitment but takes the API offline on each preemption (§1).
* **With gemini-3.5-flash-lite** instead of 3.1: extraction $47.25 → total ≈ $111. Close the gap by extracting
  ~100k pages/month, or by pushing more pages through the deterministic path.
* **If the original 3,000/500-token spec is kept**, extraction alone is $150–430/month (§5). That doesn't fit $100,
  whatever the hosting choice.
* Cloud SQL instead of self-hosted: +$25–50/mo (db-g1-small + SSD + backups ≈ $38). Only worth it at the 10x stage.

### Hardening checklist

* The firewall allows SSH only from IAP (35.235.240.0/20) and the API port only from `voyager-vm`'s internal IP
  or `35.207.18.35/32`. No 0.0.0.0/0 ingress at all; the crawler only needs egress.
* Crawler politeness: obey robots.txt (cache for 24 h), honour `Crawl-delay`, keep one connection per host,
  back off adaptively on 429/503 and `Retry-After`, send a descriptive UA with a contact URL, use conditional GETs
  (ETag/Last-Modified, which also cut cost), and enforce per-host daily caps.
* An egress governor like the app VM's `governor.py` keeps a runaway Chromium from burning through the 200 GiB.
* Daily $ caps inside the processor for Gemini and embeddings, and a kill switch in the DB.

---

## 9. Phased rollout

1. New project + billing link, budget (INR) and Gemini spend cap, enable compute/storage/aiplatform/secretmanager/
   monitoring/logging.
2. Buckets (raw: versioning + lifecycle + retention; backups), service account and bindings, custom Vertex role.
3. VM (on-demand), data disk + snapshot schedule, static IP, firewall, Ops Agent, deployer timer pulling
   cosign-verified GHCR digests.
4. Peering or allowlist to `voyager-vm`; uptime check + alerts.
5. After 2–4 weeks: buy the 1-year E2 CUD at the observed shape.

---

## 10. Scale-up path (10x: 50M pages/month, ~1.4–2M LLM extractions)

| Layer | Change | Est. $/month |
|---|---|---|
| Fetchers | Split the stateless fetchers out of the main box: a **MIG of 2–4 e2-standard-2/-4 Spot VMs**, each with its **own external IP** (not NAT: at ~5 TB/month of raw downloads, NAT processing would cost ~$225). Frontier leases come from the DB, so a preemption just loses a lease. | 60–120 |
| Chromium | Its own Spot MIG (e2-highmem-2), or Cloud Run worker pools if render volume is bursty | 40–70 |
| DB | Frontier ≈ 500M URLs. Move Postgres to an **e2-highmem-4 (32 GB) + 500 GB pd-ssd** (~$130 + $85), or Cloud SQL Enterprise 4 vCPU/16 GB (~$202 + 500 GB SSD $85, + HA ×2 if the API needs an SLA). Partition the frontier by host hash; consider moving the hot frontier into an embedded RocksDB/LMDB in the scheduler and keeping Postgres for metadata, index and vectors. | 200–300 |
| Raw GCS | ~1.4 TiB/month; Std 30 d → Coldline → Archive at 365 d. Month 12 ≈ $28 Std + $61 Coldline ≈ $90 | 50–90 |
| Egress | 100–250 GiB/month crawl requests; just over the 200 GiB free, at $0.085/GiB | 0–5 |
| Gemini | 1.4–2M batch calls at 1,000/150 on 3.1 FL ≈ $330–475. **This is the dominant cost.** Levers: stronger deterministic extraction, a distilled classifier to gate LLM calls, Provisioned Throughput only if latency matters | 330–475 |
| Embeddings | 2M × 300 tokens, text-embedding-005 batch | ~50 |
| Ops | Managed Service for Prometheus or more custom metrics; logs likely past 50 GiB, so add exclusions | 10–30 |
| Analytics | Export extracted records to BigQuery (storage $0.02/GiB, 1 TiB of queries free) | 5–20 |
| **Total** | | **≈ $750–1,150** |

Re-evaluate GKE Autopilot at this point (Spot pods at $0.0133/vCPU-h, cluster fee covered by the free credit). It
starts paying off once there are 5+ distinct worker types to schedule.

---

## Sources

* Cloud Billing Catalog API, SKU listings for Compute Engine (6F81-5844-456A), Cloud SQL (9662-B51E-5089), Cloud
  Storage (95FF-2EF5-5EA1), Vertex AI / Agent Platform (C7E2-9256-1C43), Cloud Run (152E-C115-5142), GKE
  (CCD8-9BF1-090E), Networking (E505-1604-58F8), Secret Manager (EE82-7A5E-871C), Cloud Logging (5490-F7B7-8DF6),
  Cloud Monitoring (58CD-E7C3-72CA), AlloyDB (C49F-B7F2-7416), Bigtable (C3BE-24A5-0975), queried 2026-11-15
* [Cloud SQL instance settings](https://docs.cloud.google.com/sql/docs/postgres/instance-settings) and [operational guidelines (SLA scope)](https://docs.cloud.google.com/sql/docs/operational-guidelines)
* [Vertex AI model versions and lifecycle](https://docs.cloud.google.com/vertex-ai/generative-ai/docs/learn/model-versions); [Vertex AI shutdowns, Oct 2026](https://www.developersdigest.tech/blog/vertex-ai-shutdowns-october-2026)
* [Requesty: Vertex gemini-3.5-flash-lite pricing](https://www.requesty.ai/models/vertex/gemini-3.5-flash-lite) (matches the catalog)
* [Google Cloud Observability pricing](https://cloud.google.com/products/observability/pricing); [alerting cost control](https://cloud.google.com/monitoring/alerts/cost-control)
* Repo: `docs/Deployment-GCP.md` (budgets, spend cap, cost notes)
