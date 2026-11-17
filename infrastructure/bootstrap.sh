#!/usr/bin/env bash
# Voyager Crawler on GCP: one-time, idempotent bootstrap of exactly spec §7 +
# §17.5 (docs/superpowers/specs/2026-11-15-voyager-crawler-design.md) in
# project voyager-prod-ohzkl (us-east1), next to the travel planner's VM.
#
#   CRAWLER_ALERT_EMAIL=… infrastructure/bootstrap.sh --dry-run   print every command, call nothing
#   CRAWLER_ALERT_EMAIL=… infrastructure/bootstrap.sh             create what is missing
#   infrastructure/bootstrap.sh --rotate-tls [--dry-run]          new API key + certificate only
#   infrastructure/bootstrap.sh --update-host [--dry-run]         refresh the VM's host bundle only
#   infrastructure/bootstrap.sh --update-host --standard          Spot → standard provisioning (one command)
#   … --spot / --standard (with a full run or --update-host): the provisioning
#   model of the VM's template; without either, the group keeps its current one
#   (a fresh group: Spot)
#
# Creates (skipping anything that exists; every resource that takes labels
# gets app=voyager-crawler):
#   APIs: compute, storage, secretmanager, monitoring, logging, aiplatform,
#         billingbudgets, iap, oslogin, iam, cloudresourcemanager
#   service accounts crawler-runtime (the VM) and crawler-drill (the monthly
#     Spot restore-drill VM)
#   custom roles voyagerCrawlerVertex (predict + batch jobs) and
#     voyagerCrawlerDrill (create/delete instances and disks, bound with an IAM
#     condition: names starting crawler-drill-, the drill template only)
#   buckets voyager-crawler-raw and voyager-crawler-ops: us-east1, uniform
#     access, public access prevention, versioning, soft delete 7 d, lifecycle
#     per §17.7 (infrastructure/gcs/*-lifecycle.json)
#   IAM: crawler-runtime objectAdmin on -raw, objectCreator + objectViewer on
#     -ops, logWriter, metricWriter, voyagerCrawlerVertex, secretAccessor on
#     each crawler-* secret; vm-runtime secretAccessor on
#     crawler-api-token-voyager and crawler-api-tls-cert only. Not yet (until
#     the Spot drill and batch prediction are wired): voyagerCrawlerDrill on
#     crawler-runtime, and the Vertex AI service agent on -ops
#   secrets crawler-pg-password, crawler-api-token-voyager (random values
#     generated when they have no version), crawler-api-tls-key,
#     crawler-api-tls-cert (a self-signed P-256 certificate for the static
#     internal IP, valid 2 years)
#   static internal IP crawler-internal-ip (default subnet) and static external
#     IP crawler-external-ip (the crawl identity published in BOT.md)
#   data disk crawler-data: 30 GB pd-balanced, no snapshot schedule (wal-g
#     PITR to GCS + the weekly restore drill cover recovery)
#   firewall: allow-crawler-api (tcp:8443, source SA vm-runtime → target SA
#     crawler-runtime), allow-crawler-iap-ssh (tcp:22 from IAP),
#     allow-crawler-health-check (tcp:8443 from Google's health-check ranges
#     130.211.0.0/22 and 35.191.0.0/16 only), and
#     deny-crawler-ingress (everything else, so the project-wide
#     default-allow-internal and the app's voyager-web (0.0.0.0/0:80,443,
#     every instance) do not open the crawler VM; DENY wins at equal priority)
#   instance template crawler-drill-template (Spot e2-standard-2, as crawler-drill)
#   health check crawler-healthz: HTTPS GET /healthz on tcp:8443 (the read
#     API, TLS only), every 60 s, unhealthy after 15 failures (free; used only
#     for autohealing, no load balancer)
#   instance template voyager-crawler-{spot,std}-<hash>: e2-medium, Debian 12,
#     10 GB pd-balanced boot disk (device crawler-boot), Shielded VM, OS
#     Login, as crawler-runtime; Spot (termination action STOP) or standard
#     (live migration + automatic restart); startup-script =
#     deploy/host/first-boot.sh, shutdown-script = deploy/host/shutdown.sh,
#     metadata crawler-bundle = deploy/ (without tests and ci). Templates are
#     immutable: <hash> covers the bundle's content and the template flags, so
#     a changed deploy/ makes a new template, which the group adopts with a
#     metadata refresh (no restart)
#   stateful managed instance group voyager-crawler-mig (zonal, size 1,
#     opportunistic updates, repair on failure + autohealing on
#     crawler-healthz after a 30 min initial delay, force-update on repair)
#     with one instance voyager-crawler whose per-instance config keeps the
#     boot disk, the data disk crawler-data and both static IPs (never
#     auto-deleted): a preempted or unhealthy VM is recreated with the same
#     name, disks, internal IP and egress IP
#   log bucket voyager-crawler-logs (us-east1, 30-day retention) and sink
#     voyager-crawler-logs: the VM's container logs (Ops Agent log
#     crawler_docker, first-boot.sh), DEBUG excluded; the _Default sink excludes
#     them (ingested once)
#   notification channel "Voyager crawler alerts" (email $CRAWLER_ALERT_EMAIL)
#   alert policies infrastructure/monitoring/*.json on that channel
#   budget "voyager-crawler": ₹9,000/month on label app=voyager-crawler, 50/90/100 %
#
# Never: a load balancer, NAT, Cloud SQL, any CI identity in GCP
# (CI signs with Sigstore and pushes to GHCR; the VM pulls), or a change to
# the travel planner's resources other than the two secret bindings for
# vm-runtime and one exclusion on the project's _Default log sink. Existing
# resources are not modified, except what is re-asserted on every run: IAM
# bindings, custom-role permissions, bucket settings and labels, firewall rules
# (all but direction/action), the health check, the group's autohealing and
# update policy (an emergency stop may have cleared autohealing), the group's
# template (a new one when deploy/ changed), the log bucket's retention and the
# sink's filter, the alert channel's email, every alert policy (from its file)
# and the budget.
#
# Every gcloud call passes --project (the local default project is another
# one); billing calls use the project as their quota project. Needs gcloud's
# beta component (notification channels, address labels) and openssl.
set -euo pipefail

DRY_RUN=0
MODE=all
WANT_MODEL=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --rotate-tls) MODE=rotate-tls ;;
    --update-host) MODE=update-host ;;
    --standard) WANT_MODEL=STANDARD ;;
    --spot) WANT_MODEL=SPOT ;;
    -h | --help) sed -n '2,95p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

PROJECT_ID="${PROJECT_ID:-voyager-prod-ohzkl}"
BILLING_ACCOUNT="${BILLING_ACCOUNT:-010A0B-F39593-14A899}"
REGION="${REGION:-us-east1}"
ZONE="${ZONE:-us-east1-b}"
VM="${VM:-voyager-crawler}"
MIG="${MIG:-voyager-crawler-mig}"
MACHINE_TYPE="${MACHINE_TYPE:-e2-medium}"
TEMPLATE_PREFIX="voyager-crawler"
BOOT_DEVICE="crawler-boot"
HEALTH_CHECK="crawler-healthz"
# The read API (vc-api, TLS only) answers GET /healthz on 8443, published on
# the VM's internal IP; the roles' own /healthz (:9464) never leaves Docker
HEALTH_PORT="${HEALTH_PORT:-8443}"
HC_RANGES="130.211.0.0/22,35.191.0.0/16"
# First boot (Docker, Ops Agent, image pulls, migrations) before autohealing counts
INITIAL_DELAY=1800
LABELS="app=voyager-crawler"
RAW_BUCKET="voyager-crawler-raw"
OPS_BUCKET="voyager-crawler-ops"
RUNTIME_SA="crawler-runtime@$PROJECT_ID.iam.gserviceaccount.com"
DRILL_SA="crawler-drill@$PROJECT_ID.iam.gserviceaccount.com"
APP_SA="vm-runtime@$PROJECT_ID.iam.gserviceaccount.com"
DISK="crawler-data"
INTERNAL_IP_NAME="crawler-internal-ip"
EXTERNAL_IP_NAME="crawler-external-ip"
DRILL_TEMPLATE="crawler-drill-template"
CHANNEL_NAME="Voyager crawler alerts"
LOG_BUCKET="voyager-crawler-logs"
LOG_NAME="crawler_docker"
# The billing account is in INR (docs/research/gcp.md §0): ₹9,000 ≈ $100
BUDGET_NAME="voyager-crawler"
BUDGET_AMOUNT="${BUDGET_AMOUNT:-3000INR}"
IAP_RANGE="35.235.240.0/20"
SECRETS=(crawler-pg-password crawler-api-token-voyager crawler-api-tls-key crawler-api-tls-cert)
VERTEX_ROLE="voyagerCrawlerVertex"
VERTEX_PERMS="aiplatform.endpoints.predict,aiplatform.batchPredictionJobs.create,aiplatform.batchPredictionJobs.get,aiplatform.batchPredictionJobs.list,aiplatform.batchPredictionJobs.cancel"
DRILL_ROLE="voyagerCrawlerDrill"
DRILL_PERMS="compute.instances.create,compute.instances.delete,compute.instances.get,compute.instances.setMetadata,compute.instances.setLabels,compute.instances.setServiceAccount,compute.disks.create,compute.disks.setLabels,compute.instanceTemplates.useReadOnly,compute.subnetworks.use,compute.subnetworks.useExternalIp,compute.zoneOperations.get"
TLS_DAYS=730

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
if [[ -n "${BOOTSTRAP_WORK:-}" ]]; then
  WORK="$BOOTSTRAP_WORK"
  mkdir -p "$WORK"
else
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
fi
chmod 0700 "$WORK"
export CLOUDSDK_BILLING_QUOTA_PROJECT="$PROJECT_ID"

say() { printf '\n==> %s\n' "$*"; }

# run CMD… — executes, or prints it (to stderr) in dry-run mode
run() {
  if ((DRY_RUN)); then
    { printf '  %q' "$@"; printf '\n'; } >&2
  else
    "$@"
  fi
}
# exists CMD… — true when the describe-style command succeeds; dry-run: false
exists() {
  ((DRY_RUN)) && return 1
  "$@" >/dev/null 2>&1
}
# value CMD… — a describe/list value; dry-run: the placeholder in $PLACEHOLDER
value() {
  if ((DRY_RUN)); then
    echo "$PLACEHOLDER"
  else
    "$@" 2>/dev/null || true
  fi
}

g() { run gcloud --project="$PROJECT_ID" --quiet "$@"; }
gx() { exists gcloud --project="$PROJECT_ID" "$@"; }
gv() { value gcloud --project="$PROJECT_ID" "$@"; }

preflight() {
  ((DRY_RUN)) && return 0
  command -v gcloud >/dev/null || { echo "gcloud is not installed" >&2; exit 1; }
  command -v openssl >/dev/null || { echo "openssl is not installed" >&2; exit 1; }
  if ! gcloud components list --only-local-state --format='value(id)' 2>/dev/null | grep -qx beta; then
    echo "gcloud's beta component is needed: gcloud components install beta --quiet" >&2
    exit 1
  fi
}

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum | cut -d' ' -f1; else shasum -a 256 | cut -d' ' -f1; fi
}

host_bundle() {
  # deploy/ without tests/ and ci/, as a base64 tar.gz (instance metadata,
  # ≤ 256 KB). C-sorted, no extended attributes: the same bytes everywhere.
  local list="$WORK/bundle.list"
  local -a flags
  (cd "$ROOT/deploy" && find . -type f ! -path './tests/*' ! -path './ci/*' \
    ! -name '.DS_Store' ! -name '._*' | LC_ALL=C sort) >"$list"
  # The bundle's identity: paths and contents (not tar's mtimes)
  BUNDLE_HASH="$(cd "$ROOT/deploy" && while IFS= read -r f; do
    printf '%s %s\n' "$(sha256 <"$f")" "$f"
  done <"$list" | sha256)"
  if tar --version 2>/dev/null | grep -q 'GNU tar'; then
    flags=(--no-xattrs --no-acls --owner=0 --group=0 --numeric-owner)
  else
    flags=(--no-xattrs --no-acls --no-mac-metadata --uid=0 --gid=0)
  fi
  COPYFILE_DISABLE=1 tar -C "$ROOT/deploy" "${flags[@]}" -czf "$WORK/bundle.tgz" -T "$list"
  base64 <"$WORK/bundle.tgz" | tr -d '\n' >"$WORK/bundle.b64"
  local size
  size="$(wc -c <"$WORK/bundle.b64" | tr -d ' ')"
  ((size < 250000)) || { echo "host bundle is $size bytes (metadata limit 256 KB)" >&2; exit 1; }
}

enable_apis() {
  say "APIs"
  g services enable compute.googleapis.com storage.googleapis.com secretmanager.googleapis.com \
    monitoring.googleapis.com logging.googleapis.com aiplatform.googleapis.com \
    billingbudgets.googleapis.com iap.googleapis.com oslogin.googleapis.com iam.googleapis.com \
    cloudresourcemanager.googleapis.com
}

service_accounts() {
  say "Service accounts"
  gx iam service-accounts describe "$RUNTIME_SA" ||
    g iam service-accounts create crawler-runtime --display-name="Voyager crawler VM runtime"
  gx iam service-accounts describe "$DRILL_SA" ||
    g iam service-accounts create crawler-drill --display-name="Voyager crawler restore drill (Spot VM)"
}

# custom_role ID TITLE PERMS — create, or re-assert the permission list
custom_role() {
  if gx iam roles describe "$1"; then
    g iam roles update "$1" --permissions="$3"
  else
    g iam roles create "$1" --title="$2" --permissions="$3" --stage=GA
  fi
}

custom_roles() {
  say "Custom roles"
  custom_role "$VERTEX_ROLE" "Voyager crawler Vertex (predict + batch)" "$VERTEX_PERMS"
  custom_role "$DRILL_ROLE" "Voyager crawler restore drill (crawler-drill-* only)" "$DRILL_PERMS"
}

bucket() {
  local name="$1" lifecycle="$2"
  gx storage buckets describe "gs://$name" ||
    g storage buckets create "gs://$name" --location="$REGION" --default-storage-class=STANDARD \
      --uniform-bucket-level-access --public-access-prevention --soft-delete-duration=7d
  # re-asserted on every run
  g storage buckets update "gs://$name" --versioning --soft-delete-duration=7d \
    --lifecycle-file="$lifecycle" --update-labels="$LABELS"
}

buckets() {
  say "Buckets"
  bucket "$RAW_BUCKET" "$ROOT/infrastructure/gcs/raw-lifecycle.json"
  bucket "$OPS_BUCKET" "$ROOT/infrastructure/gcs/ops-lifecycle.json"
}

has_version() {
  ((DRY_RUN)) && return 1
  [[ -n "$(gcloud --project="$PROJECT_ID" secrets versions list "$1" --filter='state=ENABLED' \
    --limit=1 --format='value(name)' 2>/dev/null)" ]]
}

# random_secret NAME — a 256-bit random value as the first version
# random_secret NAME [PREFIX] — PREFIX (e.g. `voyager.` for an API token,
# whose part before the dot is the client id) goes before the random part
random_secret() {
  has_version "$1" && return 0
  if ((DRY_RUN)); then
    echo "  # generate ${2:-}32 random bytes (base64url) → $WORK/$1" >&2
  else
    (umask 077 && { printf '%s' "${2:-}"; openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n'; } >"$WORK/$1")
  fi
  g secrets versions add "$1" --data-file="$WORK/$1"
  ((DRY_RUN)) || rm -f "$WORK/$1"
}

secrets() {
  say "Secrets"
  local s
  for s in "${SECRETS[@]}"; do
    gx secrets describe "$s" ||
      g secrets create "$s" --replication-policy=automatic --labels="$LABELS"
  done
  random_secret crawler-pg-password
  random_secret crawler-api-token-voyager voyager.   # `vcrawl admin client add --name voyager`
}

addresses() {
  say "Static IPs"
  local -a want=()
  [[ -n "${CRAWLER_INTERNAL_IP:-}" ]] && want=(--addresses="$CRAWLER_INTERNAL_IP")
  gx compute addresses describe "$INTERNAL_IP_NAME" --region="$REGION" ||
    g compute addresses create "$INTERNAL_IP_NAME" --region="$REGION" --subnet=default \
      --purpose=GCE_ENDPOINT ${want[@]+"${want[@]}"}
  gx compute addresses describe "$EXTERNAL_IP_NAME" --region="$REGION" ||
    g compute addresses create "$EXTERNAL_IP_NAME" --region="$REGION" --network-tier=STANDARD
  g beta compute addresses update "$INTERNAL_IP_NAME" --region="$REGION" --update-labels="$LABELS"
  g beta compute addresses update "$EXTERNAL_IP_NAME" --region="$REGION" --update-labels="$LABELS"
  PLACEHOLDER="<crawler-internal-ip>"
  INTERNAL_IP="$(gv compute addresses describe "$INTERNAL_IP_NAME" --region="$REGION" --format='value(address)')"
  PLACEHOLDER="<crawler-external-ip>"
  EXTERNAL_IP="$(gv compute addresses describe "$EXTERNAL_IP_NAME" --region="$REGION" --format='value(address)')"
  if ((!DRY_RUN)) && [[ ! "$INTERNAL_IP" =~ ^10\. || -z "$EXTERNAL_IP" ]]; then
    echo "could not read the reserved IPs (internal '$INTERNAL_IP', external '$EXTERNAL_IP')" >&2
    exit 1
  fi
}

# tls — a self-signed P-256 certificate for the static internal IP (§17.5).
# The key goes in first: deploy.sh only installs a matching key/cert pair.
tls() {
  local force="$1"
  if [[ "$force" != force ]] && has_version crawler-api-tls-cert; then
    return 0
  fi
  say "API TLS certificate (IP:$INTERNAL_IP, $TLS_DAYS days)"
  # Our own config: no dependency on the local openssl.cnf (OpenSSL or LibreSSL)
  cat >"$WORK/tls.cnf" <<EOF
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = voyager-crawler-api
[ext]
subjectAltName = IP:$INTERNAL_IP
basicConstraints = critical,CA:FALSE
keyUsage = critical,digitalSignature
extendedKeyUsage = serverAuth
EOF
  run openssl req -x509 -config "$WORK/tls.cnf" -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -days "$TLS_DAYS" -keyout "$WORK/tls.key" -out "$WORK/tls.crt"
  g secrets versions add crawler-api-tls-key --data-file="$WORK/tls.key"
  g secrets versions add crawler-api-tls-cert --data-file="$WORK/tls.crt"
  ((DRY_RUN)) || rm -f "$WORK/tls.key"
}

project_binding() {
  g projects add-iam-policy-binding "$PROJECT_ID" --member="serviceAccount:$1" --role="$2" --condition=None >/dev/null
}

iam() {
  say "IAM bindings (re-asserted)"
  local s
  g storage buckets add-iam-policy-binding "gs://$RAW_BUCKET" --member="serviceAccount:$RUNTIME_SA" \
    --role=roles/storage.objectAdmin >/dev/null
  g storage buckets add-iam-policy-binding "gs://$OPS_BUCKET" --member="serviceAccount:$RUNTIME_SA" \
    --role=roles/storage.objectCreator >/dev/null
  g storage buckets add-iam-policy-binding "gs://$OPS_BUCKET" --member="serviceAccount:$RUNTIME_SA" \
    --role=roles/storage.objectViewer >/dev/null
  # wal-g uploads to GCS in chunks, composes them, then deletes the chunks: it needs
  # objects.delete. Only under walg/ (an IAM condition); the bucket keeps deleted
  # versions 7 days, so nothing it removes is unrecoverable.
  g storage buckets add-iam-policy-binding "gs://$OPS_BUCKET" --member="serviceAccount:$RUNTIME_SA" \
    --role=roles/storage.objectUser \
    --condition="expression=resource.name.startsWith('projects/_/buckets/$OPS_BUCKET/objects/walg/'),title=walg-prefix-only,description=wal-g chunk compose cleanup under walg/ only" >/dev/null
  g storage buckets add-iam-policy-binding "gs://$OPS_BUCKET" --member="serviceAccount:$DRILL_SA" \
    --role=roles/storage.objectViewer >/dev/null
  project_binding "$RUNTIME_SA" roles/logging.logWriter
  project_binding "$RUNTIME_SA" roles/monitoring.metricWriter
  project_binding "$RUNTIME_SA" "projects/$PROJECT_ID/roles/$VERTEX_ROLE"
  project_binding "$DRILL_SA" roles/logging.logWriter
  project_binding "$DRILL_SA" roles/monitoring.metricWriter

  # Not yet: the conditional voyagerCrawlerDrill binding and serviceAccountUser
  # on crawler-drill. They let the VM create instances, so they arrive with the
  # monthly Spot restore drill that uses them, not before (go-live review).

  for s in "${SECRETS[@]}"; do
    g secrets add-iam-policy-binding "$s" --member="serviceAccount:$RUNTIME_SA" \
      --role=roles/secretmanager.secretAccessor >/dev/null
  done
  g secrets add-iam-policy-binding crawler-api-token-voyager --member="serviceAccount:$APP_SA" \
    --role=roles/secretmanager.secretAccessor >/dev/null
  g secrets add-iam-policy-binding crawler-api-tls-cert --member="serviceAccount:$APP_SA" \
    --role=roles/secretmanager.secretAccessor >/dev/null

  # No Vertex AI service-agent binding on -ops: batch prediction is not wired.
  # When it is, grant roles/storage.objectUser conditioned on objects/batch/.
}

# disk — the data disk, stateful in the group (never auto-deleted). No
# snapshot schedule: wal-g base backups + WAL in gs://voyager-crawler-ops and
# the weekly restore drill cover recovery (the disk holds nothing else that
# is not rebuilt by first boot and deploy).
disk() {
  say "Data disk"
  gx compute disks describe "$DISK" --zone="$ZONE" ||
    g compute disks create "$DISK" --zone="$ZONE" --size=30GB --type=pd-balanced --labels="$LABELS"
}

# fw_rule NAME ACTION PRIORITY ARGS… — create, or re-assert everything but the
# direction and action (which gcloud cannot update)
fw_rule() {
  local name="$1" action="$2" prio="$3"
  shift 3
  if gx compute firewall-rules describe "$name"; then
    g compute firewall-rules update "$name" --priority="$prio" "$@"
  else
    g compute firewall-rules create "$name" --network=default --direction=INGRESS \
      --action="$action" --priority="$prio" "$@"
  fi
}

firewall() {
  say "Firewall (re-asserted)"
  fw_rule allow-crawler-api ALLOW 900 --rules=tcp:8443 \
    --source-service-accounts="$APP_SA" --target-service-accounts="$RUNTIME_SA" \
    --description="Voyager app VM to the crawler read API"
  fw_rule allow-crawler-iap-ssh ALLOW 900 --rules=tcp:22 --source-ranges="$IAP_RANGE" \
    --target-service-accounts="$RUNTIME_SA" --description="IAP TCP forwarding (SSH) to the crawler VM"
  fw_rule allow-crawler-health-check ALLOW 900 --rules="tcp:$HEALTH_PORT" --source-ranges="$HC_RANGES" \
    --target-service-accounts="$RUNTIME_SA" \
    --description="Google health-check probers to the crawler API /healthz (MIG autohealing)"
  fw_rule deny-crawler-ingress DENY 1000 --rules=all --source-ranges=0.0.0.0/0 \
    --target-service-accounts="$RUNTIME_SA" \
    --description="Nothing else reaches the crawler VM (overrides default-allow-internal and voyager-web)"
}

# logs — the §9 crawler log bucket (30-day retention) and a sink routing the
# VM's container logs (Ops Agent log name crawler_docker, INFO and up: the
# sink also excludes DEBUG) into it; the _Default sink excludes them, so they
# are ingested once
logs() {
  say "Log bucket + sink (re-asserted)"
  local dest="logging.googleapis.com/projects/$PROJECT_ID/locations/$REGION/buckets/$LOG_BUCKET"
  local filter="logName=\"projects/$PROJECT_ID/logs/$LOG_NAME\""
  if gx logging buckets describe "$LOG_BUCKET" --location="$REGION"; then
    g logging buckets update "$LOG_BUCKET" --location="$REGION" --retention-days=30 \
      --description="Voyager crawler container logs (spec §9)"
  else
    g logging buckets create "$LOG_BUCKET" --location="$REGION" --retention-days=30 \
      --description="Voyager crawler container logs (spec §9)"
  fi
  if gx logging sinks describe "$LOG_BUCKET"; then
    g logging sinks update "$LOG_BUCKET" "$dest" --log-filter="$filter"
  else
    g logging sinks create "$LOG_BUCKET" "$dest" --log-filter="$filter" \
      --exclusion=name=debug,filter="severity<INFO"
  fi
  PLACEHOLDER=""
  if [[ "$(gv logging sinks describe _Default --format='value(exclusions.name)')" != *"$LOG_BUCKET"* ]]; then
    g logging sinks update _Default --add-exclusion=name="$LOG_BUCKET",filter="$filter"
  fi
}

drill_template() {
  say "Restore-drill instance template"
  gx compute instance-templates describe "$DRILL_TEMPLATE" ||
    g compute instance-templates create "$DRILL_TEMPLATE" --machine-type=e2-standard-2 \
      --provisioning-model=SPOT --instance-termination-action=DELETE --max-run-duration=4h \
      --image-family=debian-12 --image-project=debian-cloud --boot-disk-size=100GB \
      --boot-disk-type=pd-balanced --network=default \
      --service-account="$DRILL_SA" --scopes=cloud-platform \
      --shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring \
      --metadata=enable-oslogin=TRUE,block-project-ssh-keys=TRUE \
      --labels="$LABELS,role=restore-drill"
}

# health_check — HTTPS GET /healthz on the read API, for autohealing only
# (health checks are not billed without a load balancer). 10 failed checks
# 90 s apart (15 min; GCE allows at most 10) before the group recreates the VM: a deploy's API
# roll or a short restart never trips it.
health_check() {
  say "Autohealing health check (re-asserted)"
  local -a hc=(--global --port="$HEALTH_PORT" --request-path=/healthz --check-interval=90s --timeout=10s
    --healthy-threshold=1 --unhealthy-threshold=10
    --description="Voyager crawler: read API /healthz (MIG autohealing)")
  if gx compute health-checks describe "$HEALTH_CHECK" --global; then
    g compute health-checks update https "$HEALTH_CHECK" "${hc[@]}"
  else
    g compute health-checks create https "$HEALTH_CHECK" "${hc[@]}"
  fi
}

# provisioning — MODEL (SPOT or STANDARD): --spot/--standard, else the
# group's current template (a fresh group starts on Spot)
provisioning() {
  if [[ -n "$WANT_MODEL" ]]; then
    MODEL="$WANT_MODEL"
    return 0
  fi
  PLACEHOLDER=""
  local cur
  cur="$(gv compute instance-groups managed describe "$MIG" --zone="$ZONE" --format='value(instanceTemplate.basename())')"
  if [[ "$cur" == "$TEMPLATE_PREFIX-std-"* ]]; then MODEL=STANDARD; else MODEL=SPOT; fi
}

# instance_template — TEMPLATE, created when missing. Spot: STOP on
# preemption (the group then recreates the VM, keeping its stateful disks
# and IPs; a stopped Spot VM costs no vCPU/RAM); no live migration or
# automatic restart (Spot allows neither). Standard: both.
instance_template() {
  local -a sched
  if [[ "$MODEL" == STANDARD ]]; then
    sched=(--provisioning-model=STANDARD --maintenance-policy=MIGRATE --restart-on-failure)
  else
    sched=(--provisioning-model=SPOT --instance-termination-action=STOP --maintenance-policy=TERMINATE
      --no-restart-on-failure)
  fi
  local -a args=(--machine-type="$MACHINE_TYPE" --image-family=debian-12 --image-project=debian-cloud
    --boot-disk-size=10GB --boot-disk-type=pd-balanced --boot-disk-device-name="$BOOT_DEVICE"
    --network=default --subnet=default --region="$REGION" --network-tier=STANDARD
    --service-account="$RUNTIME_SA" --scopes=cloud-platform
    --shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring
    "${sched[@]}" --labels="$LABELS"
    "--metadata=enable-oslogin=TRUE,block-project-ssh-keys=TRUE")
  local short=spot
  [[ "$MODEL" == STANDARD ]] && short=std
  TEMPLATE="$TEMPLATE_PREFIX-$short-$( { printf '%s\n' "${args[@]}"; echo "$BUNDLE_HASH"; } | sha256 | cut -c1-12)"
  say "Instance template $TEMPLATE"
  gx compute instance-templates describe "$TEMPLATE" ||
    g compute instance-templates create "$TEMPLATE" "${args[@]}" \
      --metadata-from-file="startup-script=$ROOT/deploy/host/first-boot.sh,shutdown-script=$ROOT/deploy/host/shutdown.sh,crawler-bundle=$WORK/bundle.b64"
}

# roll_template — point the group at TEMPLATE and apply it to the VM: a host
# bundle change is metadata only (refresh, no restart); Spot ↔ standard needs
# a restart or replace, which the group picks (stateful disks and IPs stay)
roll_template() {
  PLACEHOLDER="<current-template>"
  local cur
  cur="$(gv compute instance-groups managed describe "$MIG" --zone="$ZONE" --format='value(instanceTemplate.basename())')"
  if [[ "$cur" == "$TEMPLATE" ]]; then
    echo "  $MIG already on $TEMPLATE" >&2
    return 0
  fi
  g compute instance-groups managed set-instance-template "$MIG" --zone="$ZONE" --template="$TEMPLATE"
  g compute instance-groups managed update-instances "$MIG" --zone="$ZONE" --instances="$VM" \
    --minimal-action=refresh --most-disruptive-allowed-action=replace
}

mig() {
  say "Managed instance group $MIG ($MODEL)"
  local -a policy=(--health-check="projects/$PROJECT_ID/global/healthChecks/$HEALTH_CHECK" --initial-delay="$INITIAL_DELAY"
    --default-action-on-vm-failure=repair --force-update-on-repair --update-policy-type=opportunistic)
  if gx compute instance-groups managed describe "$MIG" --zone="$ZONE"; then
    g compute instance-groups managed update "$MIG" --zone="$ZONE" "${policy[@]}"
    roll_template
  else
    g compute instance-groups managed create "$MIG" --zone="$ZONE" --template="$TEMPLATE" --size=0 \
      --base-instance-name="$VM" --description="Voyager crawler: one stateful Spot/standard VM" \
      --stateful-disk=device-name="$BOOT_DEVICE",auto-delete=never \
      --stateful-internal-ip=interface-name=nic0,auto-delete=never \
      --stateful-external-ip=interface-name=nic0,auto-delete=never \
      "${policy[@]}"
  fi
  PLACEHOLDER=""
  if ! gv compute instance-groups managed list-instances "$MIG" --zone="$ZONE" \
    --format='value(instance.basename())' | grep -qx "$VM"; then
    # The per-instance config: this name, the existing data disk and both
    # reserved addresses, kept on every recreate and never auto-deleted
    g compute instance-groups managed create-instance "$MIG" --zone="$ZONE" --instance="$VM" \
      --stateful-disk="device-name=$DISK,source=projects/$PROJECT_ID/zones/$ZONE/disks/$DISK,mode=rw,auto-delete=never" \
      --stateful-internal-ip="address=projects/$PROJECT_ID/regions/$REGION/addresses/$INTERNAL_IP_NAME,interface-name=nic0,auto-delete=never" \
      --stateful-external-ip="address=projects/$PROJECT_ID/regions/$REGION/addresses/$EXTERNAL_IP_NAME,interface-name=nic0,auto-delete=never"
  fi
}

update_host() {
  say "Host bundle → $MIG"
  if ! ((DRY_RUN)) && ! gx compute instance-groups managed describe "$MIG" --zone="$ZONE"; then
    echo "no group $MIG: run the full bootstrap first" >&2
    exit 1
  fi
  provisioning
  instance_template
  roll_template
  echo "  then on the VM: sudo google_metadata_script_runner startup" >&2
}

monitoring() {
  say "Notification channel + alert policies (re-asserted)"
  PLACEHOLDER=""
  CHANNEL="$(gv beta monitoring channels list --filter="displayName=\"$CHANNEL_NAME\"" --format='value(name)' | head -n1)"
  if [[ -z "$CHANNEL" ]]; then
    g beta monitoring channels create --display-name="$CHANNEL_NAME" --type=email \
      --channel-labels=email_address="$CRAWLER_ALERT_EMAIL" --user-labels="$LABELS"
    PLACEHOLDER="<channel>"
    CHANNEL="$(gv beta monitoring channels list --filter="displayName=\"$CHANNEL_NAME\"" --format='value(name)' | head -n1)"
  else
    g beta monitoring channels update "$CHANNEL" --update-channel-labels=email_address="$CRAWLER_ALERT_EMAIL" \
      --update-user-labels="$LABELS"
  fi
  local f title out name
  for f in "$ROOT"/infrastructure/monitoring/*.json; do
    title="$(sed -n 's/^  "displayName": "\(.*\)",$/\1/p' "$f" | head -n1)"
    out="$WORK/$(basename "$f")"
    sed "s|__CHANNEL__|$CHANNEL|" "$f" >"$out"
    PLACEHOLDER=""
    name="$(gv monitoring policies list --filter="displayName=\"$title\"" --format='value(name)' | head -n1)"
    if [[ -n "$name" ]]; then
      # the whole policy from the file: thresholds, conditions and docs follow the repo
      g monitoring policies update "$name" --policy-from-file="$out"
    else
      g monitoring policies create --policy-from-file="$out"
    fi
  done
}

budget() {
  say "Budget (re-asserted)"
  PLACEHOLDER=""
  local name
  name="$(value gcloud billing budgets list --billing-account="$BILLING_ACCOUNT" \
    --filter="displayName=\"$BUDGET_NAME\"" --format='value(name)' | head -n1)"
  if [[ -n "$name" ]]; then
    run gcloud billing budgets update "$name" --billing-account="$BILLING_ACCOUNT" \
      --budget-amount="$BUDGET_AMOUNT" --filter-projects="projects/$PROJECT_ID" --filter-labels="$LABELS" \
      --clear-threshold-rules --add-threshold-rule=percent=0.5 --add-threshold-rule=percent=0.9 \
      --add-threshold-rule=percent=1.0 --notifications-rule-monitoring-notification-channels="$CHANNEL"
    return 0
  fi
  run gcloud billing budgets create --billing-account="$BILLING_ACCOUNT" --display-name="$BUDGET_NAME" \
    --budget-amount="$BUDGET_AMOUNT" --calendar-period=month \
    --filter-projects="projects/$PROJECT_ID" --filter-labels="$LABELS" \
    --threshold-rule=percent=0.5 --threshold-rule=percent=0.9 --threshold-rule=percent=1.0 \
    --notifications-rule-monitoring-notification-channels="$CHANNEL"
}

main() {
  preflight
  ((DRY_RUN)) && echo "DRY RUN: nothing is called. project=$PROJECT_ID quota project: $CLOUDSDK_BILLING_QUOTA_PROJECT" >&2
  case "$MODE" in
    rotate-tls)
      PLACEHOLDER="<crawler-internal-ip>"
      INTERNAL_IP="$(gv compute addresses describe "$INTERNAL_IP_NAME" --region="$REGION" --format='value(address)')"
      ((DRY_RUN)) || [[ "$INTERNAL_IP" =~ ^10\. ]] || { echo "no reserved internal IP" >&2; exit 1; }
      tls force
      say "Rotated. Voyager accepts the current and the next certificate; the crawler VM installs the new pair within an hour"
      return 0
      ;;
    update-host)
      host_bundle
      update_host
      return 0
      ;;
  esac
  [[ -n "${CRAWLER_ALERT_EMAIL:-}" ]] || { echo "set CRAWLER_ALERT_EMAIL (the crawler's alert address)" >&2; exit 2; }
  host_bundle
  enable_apis
  service_accounts
  custom_roles
  buckets
  secrets
  addresses
  tls keep
  iam
  disk
  firewall
  logs
  drill_template
  health_check
  provisioning
  instance_template
  mig
  monitoring
  budget
  say "Done. Manual steps: infrastructure/README.md"
}

main
