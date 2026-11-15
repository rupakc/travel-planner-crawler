---
name: deploy-status
description: Check what Voyager Crawler build is live - CI and deploy workflow runs on main, the signed :prod image digest, the VM deployer and role health, LLM budget and politeness metrics - read-only. Use when asked "is it deployed", "what's running", or after a merge to main.
---

# Deploy status

Deploy model (spec §8, §17.5; plan Task D): CI has no GCP credentials. On `main`, `.github/workflows/deploy.yml` builds `ghcr.io/rupakc/voyager-crawler:<sha>`, runs Trivy, signs with cosign (keyless) and moves `:prod` only if CI passed. The VM `voyager-crawler` (project `voyager-prod-ohzkl`, us-east1) runs `deploy.sh` on a systemd timer: `:prod` → digest → `cosign verify` → pull → `vcrawl migrate` → roll roles one by one on progress health → roll back on failure. Deploy state lives in `/var/lib/voyager-crawler`. This skill is read-only: never deploy, roll back or change anything without the owner.

## 1. GitHub side

```bash
R=rupakc/travel-planner-crawler
gh run list -R $R --branch main --limit 5
gh run list -R $R --workflow deploy.yml --limit 5
gh run view <run-id> -R $R --log-failed           # if one failed
gh api /users/rupakc/packages/container/voyager-crawler/versions --jq '.[] | select(.metadata.container.tags | index("prod")) | {name, tags: .metadata.container.tags, updated_at}'
```

Optional signature check from a workstation:

```bash
cosign verify ghcr.io/rupakc/voyager-crawler:prod \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  --certificate-identity https://github.com/rupakc/travel-planner-crawler/.github/workflows/deploy.yml@refs/heads/main
```

## 2. VM side (IAP SSH; needs the owner's gcloud login)

```bash
gcloud compute ssh voyager-crawler --project voyager-prod-ohzkl --zone us-east1-b --tunnel-through-iap --command '
  systemctl list-timers --all | grep -i voyager
  sudo journalctl -u voyager-crawler-deploy --since "2 hours ago" --no-pager | tail -50
  ls -l /var/lib/voyager-crawler
  sudo docker compose -f /var/lib/voyager-crawler/current/compose.yaml ps
  sudo docker compose -f /var/lib/voyager-crawler/current/compose.yaml exec -T api vcrawl admin status
'
```

Unit, path and zone names come from Task D's `deploy/host/` and `infrastructure/bootstrap.sh`; if they differ, read those files first.

## 3. What to report

- Commit on `main`, last CI and deploy run results, `:prod` digest and its age.
- Deployer: last successful roll, any rollback, current image per role.
- `vcrawl admin status`: roles healthy (progress-based), captures committed in the last hour, spool size/backpressure, parked keys (% of active), LLM enabled?, month-to-date spend vs cap, restore drill status.
- Anything alarming (parked keys > 25%, no captures for 60 min with work due, backup age > 2 h) with the matching alert name from spec §7.
