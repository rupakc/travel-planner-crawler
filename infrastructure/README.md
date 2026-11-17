# Infrastructure: the crawler's GCP footprint

`bootstrap.sh` creates exactly spec §7 + §17.5 in `voyager-prod-ohzkl` (us-east1),
next to the travel planner's VM, and nothing else. It is idempotent: a second run
creates nothing and re-asserts IAM bindings, custom-role permissions, bucket settings
and labels, the firewall rules (all but direction/action), the log bucket's retention
and sink filter, the alert channel's email (`CRAWLER_ALERT_EMAIL`), every alert policy
(updated from its file) and the budget. Every `gcloud` call passes `--project` (the local default
project is a different one).

```bash
CRAWLER_ALERT_EMAIL=you@… infrastructure/bootstrap.sh --dry-run   # review: prints every command
CRAWLER_ALERT_EMAIL=you@… infrastructure/bootstrap.sh             # provision (Task 18)
infrastructure/bootstrap.sh --rotate-tls                           # new API key + certificate
infrastructure/bootstrap.sh --update-host                          # push a changed deploy/ to the VM
```

Needs `gcloud` with the `beta` component (`gcloud components install beta`) and
`openssl`. Overrides: `PROJECT_ID`, `BILLING_ACCOUNT`, `REGION`, `ZONE`, `VM`,
`BUDGET_AMOUNT` (account currency, default `9000INR`), `CRAWLER_INTERNAL_IP` (pick
the reserved internal address instead of letting GCP choose).

## What it creates

| Kind | Name | Notes |
|---|---|---|
| VM | `voyager-crawler` | e2-standard-2, Debian 12, 20 GB boot, Shielded VM, OS Login, auto-restart + live migration, deletion protection; startup script `deploy/host/first-boot.sh`, metadata `crawler-bundle` = `deploy/` |
| Disk | `crawler-data` | 100 GB pd-balanced at `/var/lib/voyager-crawler` (pgdata, spool, deploy state); snapshot schedule `crawler-data-daily`, kept 7 days |
| IPs | `crawler-internal-ip`, `crawler-external-ip` | static internal (API, in the TLS certificate) and static external (the crawl identity in `BOT.md`) |
| Buckets | `voyager-crawler-raw`, `voyager-crawler-ops` | uniform access, PAP, versioning, soft delete 7 d, lifecycle in `gcs/` (below) |
| Service accounts | `crawler-runtime`, `crawler-drill` | the VM; the monthly Spot restore-drill VM |
| Custom roles | `voyagerCrawlerVertex`, `voyagerCrawlerDrill` | predict + batch jobs; instance/disk create/delete bound with an IAM condition on names `crawler-drill-*` and the drill template |
| Instance template | `crawler-drill-template` | Spot e2-standard-2, runs as `crawler-drill`, max 4 h, deleted on stop |
| Secrets | `crawler-pg-password`, `crawler-api-token-voyager`, `crawler-api-tls-key`, `crawler-api-tls-cert` | the first two get random values; the TLS pair is a self-signed P-256 cert for the internal IP, valid 2 years |
| Firewall | `allow-crawler-api`, `allow-crawler-iap-ssh`, `deny-crawler-ingress` | tcp:8443 from SA `vm-runtime` only; SSH from IAP only; everything else denied (overrides `default-allow-internal`) |
| Monitoring | channel "Voyager crawler alerts", 12 policies in `monitoring/` | its own email channel, so the app's channel stays quiet |
| Logs | bucket + sink `voyager-crawler-logs` | us-east1, 30-day retention; the VM's container logs (Ops Agent log `crawler_docker`, INFO and up), excluded from `_Default` so they are ingested once (spec §9) |
| Budget | `voyager-crawler` | ₹9,000/month on label `app=voyager-crawler`, alerts at 50/90/100 % |

IAM: `crawler-runtime` has `objectAdmin` on `-raw` (purges delete), `objectCreator` +
`objectViewer` on `-ops` (it can't delete backups; the lifecycle does), `logWriter`,
`metricWriter`, `voyagerCrawlerVertex` and `secretAccessor` on the four `crawler-*`
secrets. `vm-runtime` (the app) gets `secretAccessor` on `crawler-api-token-voyager`
and `crawler-api-tls-cert` only. **Not granted yet:** the conditional
`voyagerCrawlerDrill` binding and `serviceAccountUser` on `crawler-drill` (they arrive
with the monthly Spot restore drill, which is not wired; the role, the account and the
template exist so that change is one binding), and any Vertex AI service-agent access
to `-ops` (batch prediction is not wired; when it is, `objectUser` conditioned on
`objects/batch/`).

Backups: wal-g runs with `WALG_PREVENT_WAL_OVERWRITE=true` (`deploy/postgres/walg.env`).
`crawler-runtime` cannot overwrite objects on `-ops`, so a retried `archive_command` for
a segment that already reached GCS must not try to: wal-g compares the stored object,
exits 0 when it is identical and fails loudly when it differs.

Bucket lifecycle (§17.7): `-raw`: `segments/` → Coldline at 90 days, `ephemeral/`
deleted at 30 days, `cdxj/` and `text/` stay Standard, noncurrent versions deleted
after 7 days. `-ops`: `walg/` (base backups + WAL) deleted at 14 days, `batch/` at
7 days, noncurrent versions at once; soft delete keeps 7 more days, so backups are
gone within 21 days (the opt-out SLA).

Ops Agent (`deploy/host/first-boot.sh` writes `/etc/google-cloud-ops-agent/config.yaml`):
host metrics without `agent.googleapis.com/processes/*`, no syslog, only the containers'
json-file logs with DEBUG/TRACE dropped on the VM.

## Cost

The LLM's monthly cap defaults to **$30** (`VC_LLM_MONTHLY_USD`, down from the spec's
$40) until batch prediction is wired (half price) or the 1-year e2-standard-2 commitment
is bought. Raise it only after one of those: put `VC_LLM_MONTHLY_USD=…` in
`/var/lib/voyager-crawler/deploy/host.local.env` (compose passes it to every role; unset,
the $30 default applies), keep `VC_PROJECT_CAP_USD` at least twice the new cap, and
restart as in owner step 3. The daily cap (`VC_LLM_DAILY_USD`, $1.50, not forwarded)
still bounds each day.

Estimate (USD/month, go-live review): e2-standard-2 48.92 on demand (30.82 with the
1-year commitment), disks + snapshots 13.50, static external IP 3.65, GCS 5–8, LLM ≤ 30
(cap; embeddings included), logging/monitoring ≈ 1–2 (crawler INFO logs ~1–3 GiB inside
the project's 50 GiB free tier, no process metrics, custom metrics within the free
150 MiB), egress ≈ 1. Worst case at the full LLM cap: ≈ $104 on demand (the first 8
weeks), ≈ $86 committed; typical (LLM spend 15–25): ≈ $90–99 on demand, ≈ $71–81
committed; with the LLM off (the default): ≈ $74 / $56.

## Owner's manual steps (spec §15)

1. **Before the first run**: choose the crawler alert address and export it as
   `CRAWLER_ALERT_EMAIL` (never committed). After the run, confirm the channel's
   verification email if Cloud Monitoring sends one.
2. **Gemini spend cap**: raise the project's Agent Platform (Vertex) spend cap in the
   console (Billing → Budgets → *Spend cap enforcement*; console-only), to the value
   from spec §10.3. Then set `VC_PROJECT_CAP_USD` to the matching USD amount: with a
   monthly LLM cap above 50 % of it (or no cap, or 0) the crawler keeps the LLM off
   and alerts (`crawler_llm_config_invalid`) instead of starting it (§17.4). Until
   then the crawler runs with `VC_LLM_ENABLED=false` (the default).
3. **Turn the LLM on** (after step 2): on the VM, put `VC_LLM_ENABLED=true` and
   `VC_PROJECT_CAP_USD=…` in `/var/lib/voyager-crawler/deploy/host.local.env`, then
   `sudo google_metadata_script_runner startup && sudo systemctl restart voyager-crawler`.
4. **Publish the crawl identity**: put the external IP (`gcloud compute addresses
   describe crawler-external-ip --region=us-east1 --project=voyager-prod-ohzkl
   --format='value(address)'`) in `BOT.md`.
5. **After 8 weeks**: decide on the 1-year commitment for e2-standard-2.
6. **Source scout**: review proposals with `vcrawl admin source list --proposed`.
7. **TLS rotation** (yearly, or when the expiry alert fires 30 days ahead):
   `infrastructure/bootstrap.sh --rotate-tls`. The key and certificate get new
   versions; Voyager accepts the current or the next certificate, and the VM
   installs the new pair within an hour (`deploy.sh` refuses a key that doesn't match
   its certificate, so a half-done rotation keeps the old pair).

## Runtime contract (for the `vcrawl` image, Task 15)

- `ENTRYPOINT ["vcrawl"]`; the role is the compose command; `vcrawl health` exits 0
  when the role is healthy (progress-based); runs as uid 65532 on a read-only root.
- Settings are `VC_<SECTION>_<FIELD>` (vc-config); `crates/vc-config/tests/deploy_env.rs`
  fails on any other `VC_*` name in `deploy/compose.yaml`. The DB password arrives as a file
  (`VC_DATABASE_PASSWORD_FILE`); the API TLS key/cert are read by secret name through
  vc-secrets; client tokens are argon2 hashes in `api_clients` (see `deploy/compose.yaml`).
- Image labels `io.voyager-crawler.schema` / `min-schema` are stamped by
  `.github/workflows/deploy.yml` from `migrations/`.
- Metrics the alert policies read (`custom.googleapis.com/crawler/…`, resource
  `gce_instance`): `heartbeat` (label `role`), `captures_committed` (cumulative),
  `due_urls`, `backpressure` (0/1), `wal_archive_age_seconds`,
  `base_backup_age_seconds`, `restore_drill_ok` (0/1), `restore_drill_age_seconds`,
  `llm_month_spend_ratio`, `parked_keys_ratio`, `tls_cert_days_left`,
  `llm_config_invalid` (0/1), `deploy_drift_seconds`, `host_units_failed`,
  `backup_restore_age_seconds`, `spool_quarantined` (cumulative). Disk usage
  comes from the Ops Agent. The role metrics reach Cloud Monitoring through the
  `otelcol` compose service (`deploy/otelcol/config.yaml`): it scrapes each role's
  `:9464` and renames `crawler_<x>[_total]` to `custom.googleapis.com/crawler/<x>`; the
  Ops Agent's Prometheus receiver cannot, as it always writes `prometheus.googleapis.com`.
- The backup, WAL, TLS, drift, host-unit and LLM gauges are set by the maintenance role
  from Postgres (`vc_obs::ops`: `pg_stat_archiver`, `llm_budget`, and the
  `role = 'host'` rows of `heartbeats` that `deploy.sh` and `unit-status.sh` write). A
  gauge with no known value is not exported, and each of them has an absence condition,
  so "never measured" alerts too.

## Tests

`bash infrastructure/tests/test_bootstrap.sh` (golden `--dry-run` transcript in
`tests/golden/`, regenerate with `UPDATE_GOLDEN=1`; a fake `gcloud` for the fresh,
second-run and `--rotate-tls` cases) and `bash deploy/tests/test_deploy.sh`
(fake docker/cosign: unsigned refused, transient cosign/migrate failures retried and
never held, rollback on unhealthy, migrate before roll, idempotent on an unchanged
digest, watchdog, the deploy/drift heartbeat, unit outcomes, the light restore drill).

## Host jobs and their alerts

- `voyager-crawler-restore-drill.timer` (Sundays): `deploy/host/restore-drill.sh` restores
  the latest wal-g base backup into a scratch directory on the data disk, in a throwaway
  container of the postgres image on the `pgbackup` network only, starts it as a
  read-only hot standby (`archive_mode=off`), checks it answers (applied migrations,
  `segments`), and removes everything.
- Every crawler unit has `OnFailure=voyager-crawler-failed@%n.service` and records its
  outcome (`deploy/host/unit-status.sh`) in `heartbeats` (`role = 'host'`, instance
  `unit:<unit>`); `deploy.sh` records the running vs the prod digest and the API
  certificate's expiry (instance `deploy`) every tick. The maintenance role turns these
  rows into alert metrics. Both need no GCP, Docker or root.
