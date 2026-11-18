# Incident playbook

One entry per alert policy in `infrastructure/monitoring/` (email to the "Voyager crawler
alerts" channel). Each says what the alert means, the first three checks, and the usual
fix. Commands use the VM helpers from [go-live.md](go-live.md#conventions) (`VC`, `vcadm`,
`psqlc`), over IAP SSH:

```bash
gcloud compute ssh voyager-crawler --zone=us-east1-b --tunnel-through-iap --project=voyager-prod-ohzkl
```

The VM is a **Spot** e2-medium, the single instance of the stateful group
`voyager-crawler-mig`: GCE may preempt it at any time (about 30 s notice; the shutdown
script stops the crawler, then Postgres), and the group recreates it with the same name,
disks and IPs when capacity allows. Where the VM stands, and recent preemptions:

```bash
gcloud compute instance-groups managed list-instances voyager-crawler-mig --zone=us-east1-b \
  --project=voyager-prod-ohzkl --format='table(instance.basename(),instanceStatus,currentAction,instanceHealth[0].detailedHealthState)'
gcloud compute instance-groups managed list-errors voyager-crawler-mig --zone=us-east1-b --project=voyager-prod-ohzkl --limit=10
gcloud compute operations list --project=voyager-prod-ohzkl \
  --filter='operationType=compute.instances.preempted AND targetLink~voyager-crawler' --limit=10
```

Rule zero: if the crawler might be **impolite** (too fast, retrying a site that refused us,
fetching something it should not), stop the fetcher first (go-live.md §13.2) and diagnose
afterwards. Every other incident can wait for a diagnosis.

---

## voyager-crawler: crawler down

**Means.** A role stopped sending `crawler/heartbeat` for 15 minutes, or the whole VM is
down: on the Spot VM most often a **preemption** (the group recreates the VM; the alert
closes by itself once the roles beat again, usually within 15–30 minutes).

**Check.**
1. Laptop: the group listing and the preemption operations above. `RUNNING NONE` with a
   recent `compute.instances.preempted` → it was a preemption and the VM is back; wait
   for the stack. `TERMINATED`/`STOPPED` or `currentAction` `RECREATING` with errors in
   `list-errors` → no Spot capacity yet; the group keeps retrying (see the next entry
   if it lasts).
2. `sudo "$VC" ps --format '{{.Service}} {{.State}} {{.Health}}'`: which role is missing,
   exited or unhealthy? Then `sudo "$VC" logs --tail 100 <role>`.
3. `tail -20 /var/lib/voyager-crawler/deploy/watchdog.log` and `sudo "$VC" ps otelcol`:
   restart loop, or only the metrics path (otelcol) down while the roles are fine?

**Fix.** VM preempted or stopped → nothing to do: the group recreates it (do not
`instances start` or `delete` it by hand); the stack comes up from
`voyager-crawler.service`, first boot re-runs and keeps the data disk as it is. One role
crashing after a deploy → `deploy.sh --rollback`
(go-live.md §13.1). otelcol down → `sudo "$VC" up -d otelcol`. Postgres down → its logs;
disk full is the usual cause (see disk). Expected during a deliberate fetcher stop.

## voyager-crawler: crawler down for over 6 h (Spot capacity?)

**Means.** No role on any instance has sent a heartbeat for 6 hours. Almost always: the
Spot VM was preempted and the group cannot get Spot e2-medium capacity in us-east1-b
(the zone is fixed by the zonal data disk). Possibly the VM is up but the stack is not
(then the 15-minute entry above applies).

**Check.**
1. The group listing and `list-errors` (top of this page): `ZONE_RESOURCE_POOL_EXHAUSTED`,
   "does not have enough resources available" or a stockout → no Spot capacity.
2. `QUOTA_EXCEEDED` → quota, not capacity: Spot uses the regular `CPUS` quota here
   (`PREEMPTIBLE_CPUS` is 0, never requested): `gcloud compute regions describe us-east1
   --project=voyager-prod-ohzkl --format=json | jq '.quotas[] | select(.metric=="CPUS" or .metric=="PREEMPTIBLE_CPUS")'`.
3. The VM is `RUNNING` → it is the stack: follow the crawler-down entry above.

**Fix.** No capacity → switch to standard provisioning, one command (laptop):

```bash
infrastructure/bootstrap.sh --update-host --standard
```

The group gets a standard e2-medium template and recreates the VM with it (same name,
disks and IPs; on demand, about $24.5 a month prorated). Check with the group listing
(`RUNNING`) and `gcloud compute instances describe voyager-crawler --zone=us-east1-b
--project=voyager-prod-ohzkl --format='value(scheduling.provisioningModel)'` →
`STANDARD`. Go back to Spot later with `--update-host --spot`, or keep standard if the
commitment is due anyway (go-live.md §13.9).

## voyager-crawler: no captures for 60 min while work is due

**Means.** `crawler/captures_committed` did not grow for an hour although `crawler/due_urls`
> 0: the fetcher is stuck, everything due is parked, or commits fail.

**Check.**
1. `sudo "$VC" ps fetcher` and `sudo "$VC" logs --tail 100 fetcher`: running, healthy,
   errors?
2. `vcadm status | jq '{keys, urls, heartbeats}'`: are most keys parked? are URLs stuck in
   `leased` / `fetched_pending`?
3. `du -sh /var/lib/voyager-crawler/spool/*` and the backpressure metric: is the spool
   growing because segments don't upload?

**Fix.** Unhealthy fetcher → the watchdog restarts it; if it keeps failing, roll back.
All keys parked → see parked keys. Spool growing → see spool backpressure. Chromium wedged
(renders time out) → `sudo "$VC" restart chromium`.

## voyager-crawler: parked keys over 25% of active keys

**Means.** More than a quarter of the sites we crawl have parked us (challenge, 401/403,
repeated errors). Something about us (rate, identity, a bad source batch) or the network.

**Check.**
1. `psqlc -c "SELECT park_reason, count(*) FROM politeness_keys WHERE parked_until > now() GROUP BY 1 ORDER BY 2 DESC"`
2. `psqlc -c "SELECT key, park_reason, park_level, parked_until FROM politeness_keys WHERE parked_until > now() ORDER BY park_level DESC LIMIT 30"`:
   one source group (e.g. all news sites behind one CDN) or everything?
3. The gap query (go-live.md §10, query 3): are we faster than our own limits?

**Fix.** Gaps below the limits → politeness bug: stop the fetcher, fix, then resume.
Everything parked with timeouts → egress/network trouble; check the VM's external IP and
firewall. One group of sites refusing bots → disable those sources (`vcadm source disable
<id>`) and open a PR setting them `enabled: false` with the reason. **Never** unpark a key
by hand or retry with anything else.

## voyager-crawler: spool backpressure

**Means.** The fetchers paused because the local spool passed 2 GB or segment uploads to
`gs://voyager-crawler-raw` failed for over 15 minutes. No data is lost; crawling waits.

**Check.**
1. `sudo "$VC" logs --tail 200 fetcher | grep -iE 'upload|segment|gcs'`: the error class.
2. `du -sh /var/lib/voyager-crawler/spool/*` and `df -h /var/lib/voyager-crawler`.
3. Laptop: `gcloud storage buckets describe gs://voyager-crawler-raw --project=voyager-prod-ohzkl`
   and the runtime account's `objectAdmin` binding on it (re-run `bootstrap.sh` to
   re-assert IAM).

**Fix.** IAM or bucket problem → `infrastructure/bootstrap.sh` (idempotent) re-asserts the
bindings; the spool drains by itself once uploads work. A GCS outage → wait. Disk nearly
full → see disk.

## voyager-crawler: backup or WAL archive too old

**Means.** The newest archived WAL is older than 2 h, the newest base backup older than
8 days (base backups are weekly, Saturdays), or either has not been reported (maintenance down, or never measured).

**Check.**
1. `psqlc -c "SELECT archived_count, last_archived_time, failed_count, last_failed_wal, last_failed_time FROM pg_stat_archiver"`
2. `sudo "$VC" logs --tail 100 postgres | grep -i archive` and
   `systemctl status voyager-crawler-basebackup.service --no-pager`;
   `sudo journalctl -u voyager-crawler-basebackup.service -n 50 --no-pager`.
3. `sudo "$VC" exec -T -u postgres postgres wal-g backup-list`: the last backup's date.

**Fix.** `archive_command` failing on an object that already exists with different bytes
(wal-g's overwrite guard) → find the segment in the log, compare with the bucket, and ask
before deleting anything (the runtime account cannot delete on `-ops`; the owner can).
Credentials/IAM → re-run bootstrap. Base backup failed → fix the cause, then
`sudo systemctl start voyager-crawler-basebackup.service`. WAL piling up in `pg_wal` fills
the data disk: treat it as urgent.

## voyager-crawler: restore drill failed

**Means.** The weekly light drill (`restore-drill.sh`: latest base backup into a throwaway
read-only Postgres) failed or has not succeeded for 8 days, or has never reported. The
monthly Spot drill is not wired yet, so its two conditions stay silent.

**Check.**
1. `sudo journalctl -u voyager-crawler-restore-drill.service -n 80 --no-pager`: which step
   failed (space check, backup-fetch, recovery start, sanity query)?
2. `df -h /var/lib/voyager-crawler` against `du -sh /var/lib/voyager-crawler/pgdata`: the
   drill needs pgdata + 10 % free.
3. `sudo "$VC" exec -T -u postgres postgres wal-g backup-list`: is there a backup at all?

**Fix.** Not enough space → see disk, then rerun
`sudo systemctl start voyager-crawler-restore-drill.service`. No backup → see backup age.
Backup fetched but recovery fails → backups are not restorable: treat as a data-safety
incident, take a disk snapshot by hand
(`gcloud compute disks snapshot crawler-data --zone=us-east1-b --project=voyager-prod-ohzkl`)
and investigate before anything else.

## voyager-crawler: disk over 80%

**Means.** The 10 GB boot disk (Debian, Docker images, the 2 GB swap file) or the 30 GB
data disk is over 80 % full. The light restore drill needs pgdata + 10 % free on the
data disk, so pgdata above about 13 GB leaves the drill without room.

**Check.**
1. `df -h / /var/lib/voyager-crawler`
2. `sudo du -sh /var/lib/voyager-crawler/* /var/lib/docker 2>/dev/null | sort -h`
3. `sudo du -sh /var/lib/voyager-crawler/pgdata/pgdata/pg_wal`: WAL piling up because
   archiving fails?

**Fix.** `pg_wal` large → fix archiving first (backup age). Spool large → backpressure.
Docker images on the boot disk → every successful deploy now prunes old crawler images
itself (`prune_old_images` in `deploy.sh`: keeps the running image, the rollback target in
`/var/lib/voyager-crawler/deploy/previous` and anything a container uses). In an emergency,
by hand: `sudo docker images ghcr.io/rupakc/voyager-crawler --digests` and `sudo docker rmi
<id>` for each image that is neither in `image.env` nor in `previous` (never `docker image
prune -f`: digest-pinned images are untagged, so it would delete the rollback target). Real growth →
plan a disk resize with the owner: `gcloud compute disks resize crawler-data --size=…
--zone=us-east1-b --project=voyager-prod-ohzkl`, then `sudo resize2fs
/dev/disk/by-id/google-crawler-data` on the VM (the disk stays in the group's
per-instance config; nothing else changes). The boot disk: `gcloud compute disks resize
voyager-crawler --size=…` (the boot disk carries the instance's name), then
`sudo growpart /dev/sda 1 && sudo resize2fs /dev/sda1`.

## voyager-crawler: API TLS certificate expires within 30 days

**Means.** The self-signed certificate of the crawler API (internal IP) expires in under
30 days, or `deploy.sh` has not reported its expiry for an hour.

**Check.**
1. `sudo openssl x509 -enddate -noout -in /run/voyager-crawler/secrets/api_tls_cert`
2. `systemctl status voyager-crawler-deploy.timer --no-pager` (the reporter).
3. Laptop: `gcloud secrets versions list crawler-api-tls-cert --project=voyager-prod-ohzkl --limit=3`.

**Fix.** `infrastructure/bootstrap.sh --rotate-tls` (laptop). Voyager accepts the current
or the next certificate; the VM installs the matched new pair within an hour (deploy.sh
refuses a key that does not match its certificate). "Not reported" → see deploy drift.

## voyager-crawler: LLM monthly spend over 80% of cap

**Means.** The crawler's Gemini spend this month passed 80 % of its monthly cap ($30 by
default), or the ratio has not been reported for 30 minutes (maintenance down).

**Check.**
1. `vcadm status | jq .llm_budget`: spent, reserved and cap for today and the month.
2. `vcadm status | jq .queue`: an extraction backlog that keeps growing?
3. The Vertex AI lines labelled `app=voyager-crawler` in Billing → Reports.

**Fix.** Nothing urgent: reservations stop at the cap, and Voyager's AI is protected by the
project cap. If the spend is unexpected (a loop, a huge new source), turn the LLM off
(go-live.md §12.5) and investigate. Not reported → see crawler down (maintenance).

## voyager-crawler: LLM forced off by an invalid [llm] config

**Means.** `VC_LLM_ENABLED=true` but the `[llm]` settings are invalid
(`VC_PROJECT_CAP_USD` missing or 0, or the monthly cap above 50 % of it), so every role
started with the LLM **off**.

**Check.**
1. `sudo cat /var/lib/voyager-crawler/deploy/host.local.env`
2. `sudo "$VC" config | grep -E 'VC_LLM_ENABLED|VC_PROJECT_CAP_USD' | sort -u`: what the
   containers actually got (host.env is rewritten from host.local.env at boot or by
   `google_metadata_script_runner startup`).
3. `sudo "$VC" logs scheduler 2>&1 | grep -i llm | tail -5`: the reason logged.

**Fix.** Set `VC_PROJECT_CAP_USD` to the project cap the owner set in the console (≥ 60
with the default $30 cap), then `sudo google_metadata_script_runner startup && sudo
systemctl restart voyager-crawler` (go-live.md §12). Or set `VC_LLM_ENABLED=false`.

## voyager-crawler: running image differs from prod for over 30 min

**Means.** The VM has not been running `:prod` for 30 minutes (the digest is held, or
verify/pull/migrate keep failing), or `deploy.sh` has not reported for an hour.

**Check.**
1. `cat /var/lib/voyager-crawler/deploy/held`: held digests and why (signature, schema,
   unhealthy roll, manual rollback).
2. `sudo journalctl -u voyager-crawler-deploy.service -n 80 --no-pager`: cosign, registry,
   migrate or health errors.
3. `systemctl list-timers voyager-crawler-deploy.timer --no-pager`: is the timer running at
   all (stopped on purpose during an incident?).

**Fix.** Unhealthy roll → the previous image is running (good); fix forward on `main`, the
new `:prod` ends the hold. Signature refused → the image did not come from `deploy.yml` on
`main`: do not release it; investigate the registry. Migrate failing → read the error;
migrations are expand-only, so the old image keeps working meanwhile. Timer stopped on
purpose → start it when done. Deleting a line from `held` releases that digest: only once
you know why it was held.

## voyager-crawler: a host unit keeps failing

**Means.** A crawler systemd unit (deploy, watchdog, base backup, restore drill, the stack)
failed its last run and has not succeeded for 30 minutes.

**Check.**
1. `systemctl list-units 'voyager-crawler*' --failed --no-pager`
2. `psqlc -c "SELECT instance, progress FROM heartbeats WHERE role = 'host' AND (progress->>'failed')::boolean"`:
   which unit is recorded as failed (also shows a leftover alert test row).
3. `sudo journalctl -u <unit> -n 80 --no-pager`

**Fix.** Fix the unit's cause (the other entries here cover deploy, backup and drill), then
run it once by hand (`sudo systemctl start <unit>`): its success clears the row. A leftover
test row from go-live.md §9 → `sudo /opt/voyager-crawler/host/unit-status.sh ok
voyager-crawler-alerttest.service` and delete the row.

## Budget email (not a policy)

The "voyager-crawler" budget (₹9,000/month on label `app=voyager-crawler`) emails at 50, 90
and 100 %. 50 % before mid-month or 90 % before the 25th: check Billing → Reports by SKU
(VM, disk, GCS, Vertex). LLM above plan → §12.5 of go-live.md. GCS growth → check the
lifecycle rules are on both buckets (re-run bootstrap). The budget does not stop anything
by itself.

## Complaints from a site

Not an alert, but the most important incident: go-live.md §14 (stop, purge, denylist,
reply).
