# Go-live runbook (plan Task 18)

From an empty footprint to the full crawl, in order. Each step has the exact commands and
what you should see; if you see something else, stop and use
[incident.md](incident.md) or the rollback section (§13). Background:
`infrastructure/README.md` (what bootstrap creates, owner steps), spec §15 and §17.

Time: about 2 hours of hands-on work on day 1, then a 24-hour watch, then 30 minutes to
enable everything and another 24-hour watch. The LLM stays off throughout; switching it
on (§12) is a separate owner decision.

## Conventions

- **Laptop** commands run from a clean checkout of `main` of
  `rupakc/travel-planner-crawler`, with `gcloud` signed in as a project owner. Every
  `gcloud` call passes `--project=voyager-prod-ohzkl` (your default project is another one).
- **VM** commands run on the crawler VM over IAP SSH:

  ```bash
  gcloud compute ssh voyager-crawler --zone=us-east1-b --tunnel-through-iap --project=voyager-prod-ohzkl
  ```

  and, once per SSH session, these helpers:

  ```bash
  VC=/opt/voyager-crawler/host/vc-compose.sh
  vcadm() { sudo "$VC" exec -T scheduler vcrawl admin "$@"; }
  psqlc() { sudo "$VC" exec -T postgres psql -U vcrawl -d vcrawl -v ON_ERROR_STOP=1 "$@"; }
  ```

  `vcadm` runs `vcrawl admin …` in the scheduler container (it has the database and GCS);
  every admin command prints JSON.
- `RAMP` is the list of first-ramp sources, `sources/ramp-1.txt` without comments:

  ```bash
  RAMP="$(sed 's/#.*//' sources/ramp-1.txt | awk 'NF {print $1}')"      # laptop
  RAMP="$(sed 's/#.*//' /tmp/vc-sources/ramp-1.txt | awk 'NF {print $1}')"  # VM, after §6.1
  ```

- Never write the alert address, tokens or keys into the repository, a commit message or an
  issue.

## 0. Before you start

1. `main` is green and contains every go-live fix (at the time of writing: PRs #78, #80,
   #81, #83). Check:

   ```bash
   gh pr list -R rupakc/travel-planner-crawler --state open
   gh run list -R rupakc/travel-planner-crawler --branch main --workflow deploy.yml --limit 1
   ```

   Expected: no open fix PRs; the last `deploy.yml` run on `main` is `completed success`
   (that run promoted `ghcr.io/rupakc/voyager-crawler:prod`).
2. Local tools: `gcloud` with the beta component (`gcloud components list --only-local-state
   | grep beta`), `openssl`, `jq`, `gh`.
3. Every ramp source still validates (no network, no database):

   ```bash
   cargo build -p vcrawl
   for id in $RAMP; do ./target/debug/vcrawl admin source validate "$id" | jq -c '{id, ok, warnings}'; done
   ```

   Expected: ten lines, each `"ok":true`, `"warnings":[]`.
4. Choose the alert address (a mailbox someone reads daily) and keep it out of shell
   history:

   ```bash
   read -r -p 'crawler alert email: ' CRAWLER_ALERT_EMAIL && export CRAWLER_ALERT_EMAIL
   ```

## 1. Review the bootstrap dry run

1. Run the tests, then the dry run (it calls nothing; `gcloud` is not even needed):

   ```bash
   bash infrastructure/tests/test_bootstrap.sh
   infrastructure/bootstrap.sh --dry-run 2>&1 | tee /tmp/bootstrap-dry-run.txt
   ```

   Expected: the tests end with no `FAIL`; the transcript starts with
   `DRY RUN: nothing is called. project=voyager-prod-ohzkl quota project: voyager-prod-ohzkl`
   and ends with `==> Done. Manual steps: infrastructure/README.md`.
2. Review it. Each of these must hold:

   ```bash
   grep -E '^  gcloud ' /tmp/bootstrap-dry-run.txt | grep -v -- '--project=voyager-prod-ohzkl' | grep -v ' billing '
   grep -c '^==> ' /tmp/bootstrap-dry-run.txt
   ```

   - the first command prints nothing (every non-billing `gcloud` call is pinned to the
     project);
   - the sections are, in order: APIs, Service accounts, Custom roles, Buckets, Secrets,
     Static IPs, API TLS certificate, IAM bindings, Data disk + snapshot schedule,
     Firewall, Log bucket + sink, Restore-drill instance template, VM voyager-crawler,
     Notification channel + alert policies, Budget, Done (16 `==>` lines);
   - every created name starts with `crawler-`, `voyager-crawler` or
     `allow-crawler-`/`deny-crawler-`; nothing deletes or modifies a travel-planner
     resource except the two `vm-runtime` secret bindings and the `_Default` sink
     exclusion;
   - the VM line has `--deletion-protection`, `--shielded-secure-boot`,
     `enable-oslogin=TRUE`, `--machine-type=e2-standard-2`;
   - the channel line has `email_address=<your address>` (check it is spelled right);
   - the budget is `9000INR` with thresholds 0.5/0.9/1.0.

## 2. Provision (the real run) and confirm the alert channel

Once the VM exists it boots and **deploys on its own within minutes**, fetcher included.
Read §3 before you start this step and keep a second terminal ready.

1. Run it:

   ```bash
   infrastructure/bootstrap.sh 2>&1 | tee /tmp/bootstrap-run.txt
   ```

   Expected: exit 0, the same 16 sections as the dry run, `==> Done.` last. A second run
   must create nothing (you may run it again later to re-assert IAM and policies).
2. As soon as `==> VM voyager-crawler` has printed, do §3 in the second terminal.
3. Note the two IPs:

   ```bash
   gcloud compute addresses describe crawler-external-ip --region=us-east1 --project=voyager-prod-ohzkl --format='value(address)'
   gcloud compute addresses describe crawler-internal-ip --region=us-east1 --project=voyager-prod-ohzkl --format='value(address)'
   ```

   Expected: a public IPv4 address, and a `10.142.0.x` address.
4. Confirm the alert channel:

   ```bash
   gcloud beta monitoring channels list --project=voyager-prod-ohzkl \
     --filter='displayName="Voyager crawler alerts"' \
     --format='value(name,labels.email_address,enabled,verificationStatus)'
   gcloud monitoring policies list --project=voyager-prod-ohzkl \
     --filter='displayName:"voyager-crawler:"' --format='value(displayName,enabled)' | wc -l
   ```

   Expected: one channel with your address and `True` (an email channel needs no
   verification; if `verificationStatus` says `UNVERIFIED`, click the link in the email
   Cloud Monitoring sent you, then re-run). 12 policies, all enabled. In the console
   (Monitoring → Alerting → Edit notification channels → Voyager crawler alerts) use
   **Send test notification** and check that the email arrives.
5. Open the BOT.md PR now (§5), so it is merged before the fetcher starts in §6.

## 3. Hold the fetcher during first boot

The first deploy must not start the fetcher: on its first pass the scheduler would enqueue
every enabled source (94 of them), not the ten of the ramp. The deployer rolls only the
roles in `VC_ROLES`, and starts missing services only from that list, so a systemd
drop-in that leaves the fetcher out keeps it from ever being created.

1. SSH in as soon as the VM answers (retry until it does; first boot spends several
   minutes installing Docker and the Ops Agent before it installs the units):

   ```bash
   sudo install -d /etc/systemd/system/voyager-crawler-deploy.service.d
   printf '[Service]\nEnvironment="VC_ROLES=maintenance scheduler processor embedder api"\n' |
     sudo tee /etc/systemd/system/voyager-crawler-deploy.service.d/10-ramp-hold.conf
   sudo systemctl daemon-reload
   systemctl show voyager-crawler-deploy.service -p Environment
   ```

   Expected: `Environment=VC_ROLES=maintenance scheduler processor embedder api`.
2. Check that you were in time:

   ```bash
   systemctl show voyager-crawler-deploy.service -p ExecMainStartTimestamp
   sudo docker ps -a --filter label=com.docker.compose.service=fetcher --format '{{.Names}} {{.Status}}'
   ```

   Expected: the timestamp is empty or later than the drop-in, and **no fetcher
   container**. If a fetcher container exists, you were too late: stop it at once and
   keep it stopped (the deploy timer and the watchdog would restart it):

   ```bash
   sudo systemctl stop voyager-crawler-deploy.timer voyager-crawler-watchdog.timer
   sudo "$VC" stop fetcher
   ```

   then continue with §4 and start the two timers again at the end of §6.
3. Until §6 is finished: do not reboot the VM and do not restart
   `voyager-crawler.service`. Its `ExecStart` is a plain `compose up`, which creates every
   service, the fetcher included.

## 4. First boot and the first deploy (LLM off)

1. Follow first boot, then the deployer:

   ```bash
   sudo journalctl -u google-startup-scripts.service -f     # until "provisioned; starting the stack and the first deploy"
   sudo journalctl -u voyager-crawler-deploy.service -f      # until "deployed ghcr.io/rupakc/voyager-crawler@sha256:… (previous: none)"
   ```

   Expected in the deploy log, in order: `verifying …`, `pulling …`, `migrating with …`,
   `rolling maintenance → …`, `rolling scheduler → …`, `rolling processor → …`,
   `rolling embedder → …`, `rolling api → …`, `deployed …`. No `HELD` line. If a digest is
   held, see incident.md (deploy drift).
2. The LLM is off and the project cap unset:

   ```bash
   sudo test ! -e /var/lib/voyager-crawler/deploy/host.local.env && echo "no host.local.env"
   sudo "$VC" config | grep -E 'VC_LLM_ENABLED|VC_PROJECT_CAP_USD' | sort -u
   ```

   Expected: `no host.local.env`; `VC_LLM_ENABLED: "false"` and `VC_PROJECT_CAP_USD: "0"`.
3. Services:

   ```bash
   sudo "$VC" ps --format '{{.Service}} {{.State}} {{.Health}}'
   ```

   Expected: `postgres`, `chromium`, `otelcol`, `maintenance`, `scheduler`, `processor`,
   `embedder`, `api` running, the five roles and postgres `healthy`; **no `fetcher`**.

## 5. Publish the egress IP in BOT.md (before any crawl)

BOT.md promises the address is published before the first crawl, and the User-Agent
links to BOT.md on `main`, so this PR must be merged before §6.4.

1. On a branch (`agent/t18-…` or a controller branch), replace `<egress-ip>` in `BOT.md`
   with the external IP from §2.3, and `crawler-contact@<domain>` with the real contact
   address if the owner has chosen it (it must be a working mailbox: site owners will
   write to it). Open the PR, wait for green, squash-merge.
2. Check the published page:

   ```bash
   gh api repos/rupakc/travel-planner-crawler/contents/BOT.md --jq .content | base64 -d | grep -n 'Egress IP'
   ```

   Expected: the line shows the address from §2.3.
3. The address the VM really uses for egress is that one:

   ```bash
   curl -s -H 'Metadata-Flavor: Google' \
     http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/access-configs/0/external-ip; echo   # VM
   ```

   Expected: the same address. (The merge builds a new image; it deploys with the fetcher
   still held, which is fine.)

## 6. Ramp 1: hold every source but ten, then release the fetcher

1. Wait for the scheduler's first pass and copy the image's source specs to the VM:

   ```bash
   sudo "$VC" logs scheduler 2>&1 | grep -m1 '"scheduler pass"'
   sudo rm -rf /tmp/vc-sources && sudo docker cp "$(sudo "$VC" ps -q scheduler)":/app/sources /tmp/vc-sources
   ls /tmp/vc-sources/*.yaml | wc -l
   vcadm source list | jq 'length'
   ```

   Expected: a `scheduler pass` line with `sources_inserted` equal to the number of
   `*.yaml` files (117 at the time of writing), and the same number from `source list`.
2. Disable every enabled source that is not in the ramp:

   ```bash
   RAMP="$(sed 's/#.*//' /tmp/vc-sources/ramp-1.txt | awk 'NF {print $1}')"
   for id in $(vcadm source list | jq -r '.[] | select(.enabled) | .source_id'); do
     grep -qx "$id" <<<"$RAMP" || vcadm source disable "$id" | jq -c '{source_id, urls_disabled}'
   done
   ```

   Expected: one line per disabled source (84 at the time of writing).
3. Check the hold:

   ```bash
   vcadm source list | jq -r '.[] | select(.enabled) | .source_id' | sort | diff - <(sort <<<"$RAMP") && echo "ramp only"
   psqlc -c "SELECT s.source_id, count(*) FROM url_state u JOIN sources s USING (source_id)
             WHERE u.state = 'due' AND NOT s.enabled GROUP BY 1"
   ```

   Expected: `ramp only`; the query returns `(0 rows)`.
4. Release the fetcher (only once §5 is merged):

   ```bash
   sudo rm /etc/systemd/system/voyager-crawler-deploy.service.d/10-ramp-hold.conf
   sudo systemctl daemon-reload
   sudo systemctl start voyager-crawler-deploy.timer voyager-crawler-watchdog.timer   # if §3.2 stopped them
   sudo systemctl start voyager-crawler-deploy.service
   sudo journalctl -u voyager-crawler-deploy.service -n 20 --no-pager
   ```

   Expected: `up to date (sha256:…); starting stopped services: fetcher`, then
   `sudo "$VC" ps fetcher` shows it `healthy` within about 3 minutes.
5. **While the ramp lasts, a deploy whose image changes a disabled source's spec
   re-enables that source** (the scheduler rewrites the row when the spec hash changes).
   Don't merge changes under `sources/` until §11, and after every deploy during the ramp
   re-run the check in step 3; if it fails, re-run step 2.

## 7. Verify the VM

1. Host units and timers:

   ```bash
   systemctl list-units 'voyager-crawler*' --all --no-pager
   systemctl list-timers 'voyager-crawler*' --no-pager
   systemctl list-units 'voyager-crawler*' --failed --no-pager
   ```

   Expected: `voyager-crawler.service` `active (exited)`; four timers (deploy, watchdog,
   basebackup, restore-drill) with a next run; no failed units.
2. Containers and role health:

   ```bash
   sudo "$VC" ps --format '{{.Service}} {{.State}} {{.Health}}'
   for r in scheduler maintenance fetcher processor embedder api; do
     sudo "$VC" exec -T "$r" vcrawl health && echo "$r ok"
   done
   vcadm status | jq '{keys, sources, heartbeats}'
   ```

   Expected: all nine services running and the six roles `healthy`; six `… ok` lines;
   `sources.enabled` = 10; a heartbeat per role, each a few seconds old, and
   `host/deploy`.
3. The API answers on the internal IP (on the VM; the firewall admits only the app VM from
   outside):

   ```bash
   IP="$(curl -s -H 'Metadata-Flavor: Google' http://metadata.google.internal/computeMetadata/v1/instance/network-interfaces/0/ip)"
   sudo curl -sS --cacert /run/voyager-crawler/secrets/api_tls_cert "https://$IP:8443/readyz"; echo
   sudo curl -s -o /dev/null -w '%{http_code}\n' --cacert /run/voyager-crawler/secrets/api_tls_cert "https://$IP:8443/v1/stats"
   ```

   Expected: `{"status":"ready"}`, then `401` (no token sent).
4. Metrics in Cloud Monitoring (laptop):

   ```bash
   TOKEN="$(gcloud auth print-access-token)"
   curl -sS -G -H "Authorization: Bearer $TOKEN" \
     https://monitoring.googleapis.com/v3/projects/voyager-prod-ohzkl/metricDescriptors \
     --data-urlencode 'filter=metric.type = starts_with("custom.googleapis.com/crawler/")' |
     jq -r '.metricDescriptors[].type' | sort
   ```

   Expected (within about 5 minutes of the roles starting): at least `heartbeat`,
   `captures_committed`, `due_urls`, `backpressure`, `parked_keys_ratio`,
   `wal_archive_age_seconds`, `deploy_drift_seconds`, `tls_cert_days_left`,
   `host_units_failed`, `llm_month_spend_ratio`, `llm_config_invalid`, each under
   `custom.googleapis.com/crawler/`. `base_backup_age_seconds` appears after step 7,
   `backup_restore_age_seconds` after §8. `restore_drill_ok` /
   `restore_drill_age_seconds` stay absent: the monthly Spot drill is not wired yet.
   In the console, Metrics Explorer → `custom.googleapis.com/crawler/heartbeat`, grouped by
   `role`, shows six lines.
5. Logs in the 30-day bucket, and only there (laptop):

   ```bash
   gcloud logging read 'log_id("crawler_docker")' --project=voyager-prod-ohzkl \
     --bucket=voyager-crawler-logs --location=us-east1 --view=_AllLogs --freshness=15m --limit=3 \
     --format='value(timestamp,severity,jsonPayload.target)'
   gcloud logging buckets describe voyager-crawler-logs --location=us-east1 --project=voyager-prod-ohzkl --format='value(retentionDays)'
   gcloud logging sinks describe _Default --project=voyager-prod-ohzkl --format='value(exclusions.name)'
   ```

   Expected: three recent entries at `INFO` or above (never `DEBUG`); `30`; the exclusion
   list contains `voyager-crawler-logs`.
6. WAL archiving with wal-g:

   ```bash
   psqlc -c "SELECT pg_switch_wal()"
   sleep 60
   psqlc -c "SELECT archived_count, last_archived_wal, last_archived_time, failed_count, last_failed_wal FROM pg_stat_archiver"
   gcloud storage ls gs://voyager-crawler-ops/walg/ --project=voyager-prod-ohzkl                                      # laptop
   ```

   Expected: `archived_count` ≥ 1, `last_archived_time` within the last 2 minutes,
   `failed_count` 0; the bucket lists `wal_005/`.
7. The first base backup (don't wait for 03:15 UTC):

   ```bash
   sudo systemctl start voyager-crawler-basebackup.service
   systemctl status voyager-crawler-basebackup.service --no-pager | head -5
   sudo "$VC" exec -T -u postgres postgres wal-g backup-list
   psqlc -c "SELECT progress FROM heartbeats WHERE role = 'host' AND instance = 'unit:voyager-crawler-basebackup.service'"
   ```

   Expected: the unit finished `status=0/SUCCESS`; `backup-list` shows one
   `base_000000010000000000000…` row; `progress` has `"failed": false` and a `last_ok_at`.

## 8. The light restore drill, by hand

Run it now rather than on Sunday: its age alert fires when it has never reported for 24 h.

```bash
sudo systemctl start voyager-crawler-restore-drill.service      # blocks until done (minutes)
systemctl status voyager-crawler-restore-drill.service --no-pager | head -5
sudo journalctl -u voyager-crawler-restore-drill.service -n 30 --no-pager
psqlc -c "SELECT progress FROM heartbeats WHERE role = 'host' AND instance = 'unit:voyager-crawler-restore-drill.service'"
sudo docker ps -a --filter name=voyager-crawler-restore-drill --format '{{.Names}}'; ls /var/lib/voyager-crawler/restore-drill 2>&1
```

Expected: `status=0/SUCCESS`; the log shows the backup fetched, Postgres started in
recovery and the sanity queries passing; `"failed": false` with a fresh `last_ok_at`; no
drill container left and no `restore-drill` directory. A failure here blocks go-live:
backups that cannot be restored are not backups (incident.md, restore drill).

## 9. Alert test-fire

The channel test in §2.4 proves email delivery. This proves the whole path: Postgres →
maintenance role → otelcol → Cloud Monitoring → policy → email, using the "host unit keeps
failing" policy (fires after 30 minutes).

1. Record a fake failed unit:

   ```bash
   sudo /opt/voyager-crawler/host/unit-status.sh failed voyager-crawler-alerttest.service
   ```

2. Expected: within a few minutes `custom.googleapis.com/crawler/host_units_failed` = 1 in
   Metrics Explorer; after 30–35 minutes an email "voyager-crawler: a host unit keeps
   failing" arrives and Monitoring → Alerting lists an open incident.
3. Clear it:

   ```bash
   sudo /opt/voyager-crawler/host/unit-status.sh ok voyager-crawler-alerttest.service
   psqlc -c "DELETE FROM heartbeats WHERE role = 'host' AND instance = 'unit:voyager-crawler-alerttest.service'"
   ```

   Expected: the metric goes back to 0 and the incident closes (a "resolved" email)
   within about 10 minutes.

No email within 45 minutes: check the channel (§2.4), that the metric moved (if not, the
maintenance role or otelcol is the problem: incident.md, crawler down), and the policy's
notification channel in the console.

## 10. The 24-hour ramp watch

Start the clock when the fetcher went healthy (§6.4). Look at least at +1 h, +4 h, +12 h
and +24 h. Make sure the 24 hours include a 02:00–05:00 UTC dump window (Wikivoyage).

**Dashboards** (Cloud Monitoring → Metrics Explorer, resource `gce_instance`; or one custom
dashboard with these charts):

| Metric | Healthy during the ramp |
|---|---|
| `custom.googleapis.com/crawler/captures_committed` (rate) | rises in bursts; never flat for an hour while `due_urls` > 0 |
| `custom.googleapis.com/crawler/due_urls` | falls after the first hours; no steady climb |
| `custom.googleapis.com/crawler/parked_keys_ratio` | **0** |
| `custom.googleapis.com/crawler/backpressure` | 0 |
| `custom.googleapis.com/crawler/heartbeat` by `role` | six roles, no gaps |
| `custom.googleapis.com/crawler/queue_items` by `kind`, `state` | `ready` drains; `dead` stays 0 |
| `custom.googleapis.com/crawler/wal_archive_age_seconds` | < 1800 (archive_timeout is 15 min) |
| `agent.googleapis.com/disk/percent_used` (data disk) | grows slowly; extrapolated to < 80 % in 30 days |

Alerting → no open incidents. Logs Explorer on the `voyager-crawler-logs` bucket:
`log_id("crawler_docker") severity>=WARNING` should be rare, and must never show page
content, tokens or secrets.

**Politeness checks** (VM; run each at every look):

```bash
# 1. Per-site state: parked keys, penalties, budget use
psqlc -c "SELECT key, enabled, delay_ms, crawl_delay_ms, ewma_latency_ms, penalty_ms, park_level,
                 park_reason, parked_until, consecutive_5xx, used_today, budget_day
          FROM politeness_keys ORDER BY used_today DESC"

# 2. Answers per site in the last 24 h (role robots/primary/manifest; subresources excluded)
psqlc -c "SELECT key, role, status, count(*) FROM captures
          WHERE captured_at > now() - interval '24 hours' AND role <> 'subresource'
          GROUP BY 1, 2, 3 ORDER BY 1, 2, 3"

# 3. Gap between consecutive requests to the same site
psqlc -c "SELECT key, count(*) AS requests, round(min(gap)::numeric, 1) AS min_gap_s,
                 round((percentile_cont(0.5) WITHIN GROUP (ORDER BY gap))::numeric, 1) AS p50_gap_s
          FROM (SELECT key, extract(epoch FROM captured_at - lag(captured_at)
                         OVER (PARTITION BY key ORDER BY captured_at)) AS gap
                FROM captures WHERE captured_at > now() - interval '24 hours' AND role <> 'subresource') t
          WHERE gap IS NOT NULL GROUP BY key ORDER BY min_gap_s"

# 4. robots.txt as each origin answered it
psqlc -c "SELECT origin, robots_state, robots_http_status, robots_fetched_at FROM origins ORDER BY robots_state, origin"

# 5. Sites fetched that no enabled source points at (must be empty during the ramp)
psqlc -c "SELECT key, role, count(*) FROM captures
          WHERE captured_at > now() - interval '24 hours'
            AND key NOT IN (SELECT DISTINCT u.key FROM url_state u JOIN sources s USING (source_id)
                            WHERE s.enabled)
          GROUP BY 1, 2"

# 6. Progress: captures and records
vcadm status | jq '{urls, queue, segments, records}'
```

**Stop thresholds.** Any one of these means stop and fix before going on (§13 has the
commands):

| Seen | Action |
|---|---|
| A key parked with `park_reason` challenge, 401 or 403, or an origin with `robots_state` `unauthorized`/`challenge` | Disable that source (`vcadm source disable <id>`). Never retry it another way. Investigate why we were refused; the source stays off until a PR explains and fixes it. Two or more such keys: stop the fetcher. |
| `parked_keys_ratio` > 0 for any other reason | Look at `park_reason`; repeated 5xx/timeouts on one site → disable that source and raise its `min_delay_s` in a PR. |
| Any `429`, or `5xx` > 5 % of a site's answers, or `consecutive_5xx` ≥ 3 | We are too fast for that site: disable the source, raise `min_delay_s` / lower `daily_budget`, re-enable after the PR deploys. |
| `min_gap_s` below 4 s for any site, or below 16 s for `cdc.gov` (its Crawl-delay is 20) | **Politeness bug: stop the fetcher at once** (§13.2) and fix it before anything else. |
| A site at its `budget_day`, or more than 1,000 requests to one site in 24 h | Not expected in the ramp: find which URLs (query 2 by `source_id`), disable the source, fix its spec. |
| Query 5 returns rows | Scope leak: stop the fetcher; fix before resuming. |
| `backpressure` = 1, `dead` queue items, watchdog restarts (`/var/lib/voyager-crawler/deploy/watchdog.log`), a role heartbeat gap | Investigate (incident.md) before §11. |
| Any complaint from a site | §14, then review the whole ramp list. |

**Pass**: 24 hours with none of the above; every ramp source with at least one committed
capture (query 2, or `SELECT source_id, count(*) FROM captures GROUP BY 1`); records
growing (`records` in `vcadm status`); base backup and drill green.

## 11. Enable all sources

1. Only after §10 passed. Copy the specs again if a deploy happened since §6.1, then
   re-enable every source whose spec says `enabled: true` (restricted and deliberately
   disabled specs say `false` and stay off):

   ```bash
   sudo rm -rf /tmp/vc-sources && sudo docker cp "$(sudo "$VC" ps -q scheduler)":/app/sources /tmp/vc-sources
   for f in /tmp/vc-sources/*.yaml; do
     id="$(basename "$f" .yaml)"
     grep -qx 'enabled: true' "$f" || continue
     vcadm source list | jq -e --arg id "$id" '.[] | select(.source_id == $id and .enabled)' >/dev/null && continue
     vcadm source approve "$id" | jq -c '{source_id, urls_resumed}'
   done
   ```

   Expected: one line per source (84 at the time of writing). An `approve` that fails
   prints the reason (restricted licence, spec no longer valid): leave that source off and
   open an issue.
2. Check:

   ```bash
   vcadm source list | jq '[.[] | select(.enabled)] | length'
   grep -lx 'enabled: true' /tmp/vc-sources/*.yaml | wc -l
   ```

   Expected: the two numbers are equal (94 at the time of writing).
3. Watch another 24 hours with §10's dashboards, queries and thresholds. Two differences:
   query 5 no longer applies, and with about 100 sites a `parked_keys_ratio` up to 0.05
   (a few sites that refuse bots) is normal: each parked site is handled by disabling its
   source, never by retrying. The alert fires at 0.25.
4. The ramp is over: `sources/` changes may merge again.

## 12. Switch the LLM on (owner decision, not part of the ramp)

The crawler's Gemini calls share the project's Vertex spend cap with Voyager. Voyager's own
AI must never be starved, so:

1. The **owner** raises the project's Vertex AI spend cap in the console (Billing → Budgets
   → *Spend cap enforcement*; console only) to at least
   `(Voyager's monthly AI peak + 30) × 1.25` USD (spec §10.3, with the crawler's monthly
   cap of $30, `VC_LLM_MONTHLY_USD`'s default). Call that value `CAP`.
2. Check `CAP ≥ 60` (twice the crawler's monthly cap; the crawler refuses a monthly cap
   above 50 % of the project cap and alerts `llm_config_invalid` instead).
3. On the VM:

   ```bash
   printf 'VC_LLM_ENABLED=true\nVC_PROJECT_CAP_USD=%s\n' "<CAP>" |
     sudo tee /var/lib/voyager-crawler/deploy/host.local.env
   sudo google_metadata_script_runner startup     # rewrites host.env from host.local.env
   sudo systemctl restart voyager-crawler
   sudo "$VC" config | grep -E 'VC_LLM_ENABLED|VC_PROJECT_CAP_USD' | sort -u
   sudo "$VC" logs scheduler 2>&1 | grep -iE 'llm|probe' | tail -5
   vcadm status | jq .llm_budget
   ```

   Expected: `VC_LLM_ENABLED: "true"`, `VC_PROJECT_CAP_USD: "<CAP>"`; the scheduler's
   startup probe succeeded (no error line); a month row with `cap` 30;
   `custom.googleapis.com/crawler/llm_config_invalid` stays 0 (no alert).
4. Watch the first days: `llm_month_spend_ratio` should not run ahead of the month
   (≈ day-of-month ÷ 30, the daily cap keeps it near that); the budget email at 50 % of
   ₹9,000 is the second line of defence.
5. Off again: `VC_LLM_ENABLED=false` in the same file, then the same
   `google_metadata_script_runner` + `systemctl restart`.

## 13. Rollback and emergency stops

1. **Bad image**: back to the previous digest (refused if that image needs a newer schema
   than the database has, §17.6):

   ```bash
   sudo /opt/voyager-crawler/host/deploy.sh --rollback
   ```

   Expected: `rolled back to ghcr.io/…@sha256:…`; the bad digest is listed in
   `/var/lib/voyager-crawler/deploy/held` and is not redeployed; a newer `:prod` (the fix)
   ends the hold. Fix forward on `main`.
2. **Stop all crawling now** (API and data stay up):

   ```bash
   sudo systemctl stop voyager-crawler-deploy.timer voyager-crawler-watchdog.timer
   sudo "$VC" stop fetcher chromium
   ```

   Expected: `sudo "$VC" ps` shows no fetcher; captures stop. Alerts for the fetcher's
   heartbeat (15 min) and the deploy heartbeat (1 h) will fire: expected. Resume with
   `sudo systemctl start voyager-crawler-deploy.timer voyager-crawler-watchdog.timer` (the
   next deploy tick starts the missing services).
3. **Stop one site now** (every source on that registrable domain; takes effect at the
   next claim):

   ```bash
   psqlc -c "UPDATE politeness_keys SET enabled = false, updated_at = now() WHERE key = 'example.org'"
   ```

   Nothing re-enables a key by itself; undo with `enabled = true`.
4. **Stop one source**: `vcadm source disable <id>`; undo with `vcadm source approve <id>`.
5. **Back to the ramp**: re-run §6.2.
6. **Whole stack down**:
   `sudo systemctl stop voyager-crawler-deploy.timer voyager-crawler-watchdog.timer voyager-crawler.service`.
   Data stays on the data disk; `sudo systemctl start voyager-crawler.service` and the two
   timers bring it back.
7. **LLM**: §12.5.
8. Infrastructure is not rolled back: bootstrap only adds, the VM has deletion protection,
   and removing the footprint is the owner's call.

## 14. When a site complains (opt-out and removal)

Promise in BOT.md: stop at once, API/index within 24 h, raw within 7 days, backups within
21 days. Use the request's reference (issue number, e.g. `gh-123`) as `requested_by`;
never a person's name or email.

1. Acknowledge within 24 hours (issue reply, private advisory or email) and say what you
   will do.
2. Stop requests now: the whole site → §13.3 (key); one source → §13.4; specific paths →
   the purge in step 3 tombstones those URLs, and tombstoned URLs are never re-crawled.
3. Purge what we hold (pick one selector: `--host`, `--url`, `--url-prefix`, `--source`,
   `--author` for a person's name in a forum dump):

   ```bash
   vcadm purge --host example.org --reason opt_out --requested-by gh-123
   psqlc -c "SELECT id, state, requested_at, index_done_at, raw_done_at, done_at FROM purge_requests ORDER BY id DESC LIMIT 5"
   ```

   Expected: the command reports `state` `index_done` (records gone from the API and
   index) or already `raw_done` (segments rewritten too); otherwise the maintenance role
   finishes the raw stage, well within 7 days. It becomes `done` after the 21-day backup
   window. Check daily until `raw_done`.
4. Keep it excluded: a PR adding the host (and `path_prefix` if only part of the site) to
   `policy/denylist.yaml` with `reason: opt-out request` and `source: "opt-out gh-123"`;
   if a `sources/*.yaml` covers the site, set it `enabled: false` in the same PR (for
   seed-derived sources, in `scripts/seeds_to_sources.py`'s tables). Adding a denylist
   entry needs no owner approval. Keep the key disabled after the deploy.
5. Reply with the dates for each stage, and close the request when the purge is `done`.

A complaint about **load** rather than collection: stop the site (§13.3) first, check
queries 2 and 3 of §10 for that key, ask what rate they accept, put it in the source's
`min_delay_s` / `daily_budget` by PR, and re-enable only with their agreement. If we broke
our own limits, that is a politeness bug: stop the fetcher (§13.2).
