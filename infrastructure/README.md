# Infrastructure: the crawler's GCP footprint

`bootstrap.sh` creates exactly spec §7 + §17.5 in `voyager-prod-ohzkl` (us-east1),
next to the travel planner's VM, and nothing else. It is idempotent: a second run
creates nothing and only re-asserts IAM bindings, custom-role permissions, bucket
settings and labels. Every `gcloud` call passes `--project` (the local default
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
| Monitoring | channel "Voyager crawler alerts", 9 policies in `monitoring/` | its own email channel, so the app's channel stays quiet |
| Budget | `voyager-crawler` | ₹9,000/month on label `app=voyager-crawler`, alerts at 50/90/100 % |

IAM: `crawler-runtime` has `objectAdmin` on `-raw` (purges delete), `objectCreator` +
`objectViewer` on `-ops` (it can't delete backups; the lifecycle does), `logWriter`,
`metricWriter`, `voyagerCrawlerVertex`, the conditional `voyagerCrawlerDrill`,
`serviceAccountUser` on `crawler-drill` only, and `secretAccessor` on the four
`crawler-*` secrets. `vm-runtime` (the app) gets `secretAccessor` on
`crawler-api-token-voyager` and `crawler-api-tls-cert` only. The Vertex AI service
agent gets `objectUser` on `-ops` (batch prediction I/O under `batch/`).

Bucket lifecycle (§17.7): `-raw`: `segments/` → Coldline at 90 days, `ephemeral/`
deleted at 30 days, `cdxj/` and `text/` stay Standard, noncurrent versions deleted
after 7 days. `-ops`: `walg/` (base backups + WAL) deleted at 14 days, `batch/` at
7 days, noncurrent versions at once; soft delete keeps 7 more days, so backups are
gone within 21 days (the opt-out SLA).

## Owner's manual steps (spec §15)

1. **Before the first run**: choose the crawler alert address and export it as
   `CRAWLER_ALERT_EMAIL` (never committed). After the run, confirm the channel's
   verification email if Cloud Monitoring sends one.
2. **Gemini spend cap**: raise the project's Agent Platform (Vertex) spend cap in the
   console (Billing → Budgets → *Spend cap enforcement*; console-only), to the value
   from spec §10.3. Then set `VC_PROJECT_CAP_USD` to the matching USD amount: the
   crawler refuses to start with a monthly LLM cap above 50 % of it (§17.4). Until
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
- Secrets arrive as files: `VC_DATABASE_PASSWORD_FILE`, `VC_API_TOKEN_FILE`,
  `VC_API_TLS_KEY_FILE`, `VC_API_TLS_CERT_FILE` (see `deploy/compose.yaml`).
- Image labels `io.voyager-crawler.schema` / `min-schema` are stamped by
  `.github/workflows/deploy.yml` from `migrations/`.
- Metrics the alert policies read (`custom.googleapis.com/crawler/…`, resource
  `gce_instance`): `heartbeat` (label `role`), `captures_committed` (cumulative),
  `due_urls`, `backpressure` (0/1), `wal_archive_age_seconds`,
  `base_backup_age_seconds`, `restore_drill_ok` (0/1), `restore_drill_age_seconds`,
  `llm_month_spend_ratio`, `parked_keys_ratio`, `tls_cert_days_left`. Disk usage
  comes from the Ops Agent.

## Tests

`bash infrastructure/tests/test_bootstrap.sh` (golden `--dry-run` transcript in
`tests/golden/`, regenerate with `UPDATE_GOLDEN=1`; a fake `gcloud` for the fresh,
second-run and `--rotate-tls` cases) and `bash deploy/tests/test_deploy.sh`
(fake docker/cosign: unsigned refused, rollback on unhealthy, migrate before roll,
idempotent on an unchanged digest, watchdog). Both need no GCP, Docker or root.
