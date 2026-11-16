#!/usr/bin/env bash
# Voyager Crawler on GCP: one-time, idempotent bootstrap of exactly spec §7 +
# §17.5 (docs/superpowers/specs/2026-11-15-voyager-crawler-design.md) in
# project voyager-prod-ohzkl (us-east1), next to the travel planner's VM.
#
#   CRAWLER_ALERT_EMAIL=… infrastructure/bootstrap.sh --dry-run   print every command, call nothing
#   CRAWLER_ALERT_EMAIL=… infrastructure/bootstrap.sh             create what is missing
#   infrastructure/bootstrap.sh --rotate-tls [--dry-run]          new API key + certificate only
#   infrastructure/bootstrap.sh --update-host [--dry-run]         refresh the VM's host bundle only
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
#   data disk crawler-data: 100 GB pd-balanced, daily snapshot schedule
#     crawler-data-daily kept 7 days
#   firewall: allow-crawler-api (tcp:8443, source SA vm-runtime → target SA
#     crawler-runtime), allow-crawler-iap-ssh (tcp:22 from IAP), and
#     deny-crawler-ingress (everything else, so the project-wide
#     default-allow-internal and the app's voyager-web (0.0.0.0/0:80,443,
#     every instance) do not open the crawler VM; DENY wins at equal priority)
#   instance template crawler-drill-template (Spot e2-standard-2, as crawler-drill)
#   VM voyager-crawler: e2-standard-2, Debian 12, 20 GB boot disk, Shielded
#     VM, OS Login, both static IPs, automatic restart + live migration,
#     deletion protection; startup-script = deploy/host/first-boot.sh,
#     metadata crawler-bundle = deploy/ (without tests and ci)
#   notification channel "Voyager crawler alerts" (email $CRAWLER_ALERT_EMAIL)
#   alert policies infrastructure/monitoring/*.json on that channel
#   budget "voyager-crawler": ₹9,000/month on label app=voyager-crawler, 50/90/100 %
#
# Never: a MIG, a load balancer, NAT, Cloud SQL, any CI identity in GCP
# (CI signs with Sigstore and pushes to GHCR; the VM pulls), or a change to
# the travel planner's resources other than the two secret bindings for
# vm-runtime. Existing resources are not modified, except IAM bindings,
# custom-role permissions, bucket settings and labels, which are re-asserted.
#
# Every gcloud call passes --project (the local default project is another
# one); billing calls use the project as their quota project. Needs gcloud's
# beta component (notification channels, address labels) and openssl.
set -euo pipefail

DRY_RUN=0
MODE=all
while [[ $# -gt 0 ]]; do
  case "$1" in
    --dry-run) DRY_RUN=1 ;;
    --rotate-tls) MODE=rotate-tls ;;
    --update-host) MODE=update-host ;;
    -h | --help) sed -n '2,60p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

PROJECT_ID="${PROJECT_ID:-voyager-prod-ohzkl}"
BILLING_ACCOUNT="${BILLING_ACCOUNT:-010A0B-F39593-14A899}"
REGION="${REGION:-us-east1}"
ZONE="${ZONE:-us-east1-b}"
VM="${VM:-voyager-crawler}"
LABELS="app=voyager-crawler"
RAW_BUCKET="voyager-crawler-raw"
OPS_BUCKET="voyager-crawler-ops"
RUNTIME_SA="crawler-runtime@$PROJECT_ID.iam.gserviceaccount.com"
DRILL_SA="crawler-drill@$PROJECT_ID.iam.gserviceaccount.com"
APP_SA="vm-runtime@$PROJECT_ID.iam.gserviceaccount.com"
DISK="crawler-data"
SNAPSHOT_POLICY="crawler-data-daily"
INTERNAL_IP_NAME="crawler-internal-ip"
EXTERNAL_IP_NAME="crawler-external-ip"
DRILL_TEMPLATE="crawler-drill-template"
CHANNEL_NAME="Voyager crawler alerts"
# The billing account is in INR (docs/research/gcp.md §0): ₹9,000 ≈ $100
BUDGET_NAME="voyager-crawler"
BUDGET_AMOUNT="${BUDGET_AMOUNT:-9000INR}"
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

host_bundle() {
  # deploy/ without tests/ and ci/, as a base64 tar.gz (instance metadata,
  # ≤ 256 KB). C-sorted, no extended attributes: the same bytes everywhere.
  local list="$WORK/bundle.list"
  local -a flags
  (cd "$ROOT/deploy" && find . -type f ! -path './tests/*' ! -path './ci/*' \
    ! -name '.DS_Store' ! -name '._*' | LC_ALL=C sort) >"$list"
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
random_secret() {
  has_version "$1" && return 0
  if ((DRY_RUN)); then
    echo "  # generate 32 random bytes (base64url) → $WORK/$1" >&2
  else
    (umask 077 && openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n' >"$WORK/$1")
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
  random_secret crawler-api-token-voyager
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

disk() {
  say "Data disk + snapshot schedule"
  gx compute resource-policies describe "$SNAPSHOT_POLICY" --region="$REGION" ||
    g compute resource-policies create snapshot-schedule "$SNAPSHOT_POLICY" --region="$REGION" \
      --daily-schedule --start-time=04:00 --max-retention-days=7 --storage-location="$REGION" \
      --on-source-disk-delete=keep-auto-snapshots --snapshot-labels="$LABELS"
  if gx compute disks describe "$DISK" --zone="$ZONE"; then
    PLACEHOLDER=""
    if [[ "$(gv compute disks describe "$DISK" --zone="$ZONE" --format='value(resourcePolicies)')" != *"/$SNAPSHOT_POLICY"* ]]; then
      g compute disks add-resource-policies "$DISK" --zone="$ZONE" --resource-policies="$SNAPSHOT_POLICY"
    fi
  else
    g compute disks create "$DISK" --zone="$ZONE" --size=100GB --type=pd-balanced \
      --resource-policies="$SNAPSHOT_POLICY" --labels="$LABELS"
  fi
}

firewall() {
  say "Firewall"
  gx compute firewall-rules describe allow-crawler-api ||
    g compute firewall-rules create allow-crawler-api --network=default --direction=INGRESS \
      --action=ALLOW --rules=tcp:8443 --priority=900 \
      --source-service-accounts="$APP_SA" --target-service-accounts="$RUNTIME_SA" \
      --description="Voyager app VM to the crawler read API"
  gx compute firewall-rules describe allow-crawler-iap-ssh ||
    g compute firewall-rules create allow-crawler-iap-ssh --network=default --direction=INGRESS \
      --action=ALLOW --rules=tcp:22 --priority=900 --source-ranges="$IAP_RANGE" \
      --target-service-accounts="$RUNTIME_SA" --description="IAP TCP forwarding (SSH) to the crawler VM"
  gx compute firewall-rules describe deny-crawler-ingress ||
    g compute firewall-rules create deny-crawler-ingress --network=default --direction=INGRESS \
      --action=DENY --rules=all --priority=1000 --source-ranges=0.0.0.0/0 \
      --target-service-accounts="$RUNTIME_SA" \
      --description="Nothing else reaches the crawler VM (overrides default-allow-internal and voyager-web)"
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

vm() {
  say "VM $VM"
  if gx compute instances describe "$VM" --zone="$ZONE"; then
    echo "  exists; host bundle changes: --update-host" >&2
    return 0
  fi
  g compute instances create "$VM" --zone="$ZONE" --machine-type=e2-standard-2 \
    --image-family=debian-12 --image-project=debian-cloud \
    --boot-disk-size=20GB --boot-disk-type=pd-balanced \
    --disk=name="$DISK",device-name="$DISK",mode=rw,boot=no,auto-delete=no \
    --subnet=default --private-network-ip="$INTERNAL_IP" \
    --address="$EXTERNAL_IP" --network-tier=STANDARD \
    --service-account="$RUNTIME_SA" --scopes=cloud-platform \
    --shielded-secure-boot --shielded-vtpm --shielded-integrity-monitoring \
    --maintenance-policy=MIGRATE --restart-on-failure --provisioning-model=STANDARD \
    --deletion-protection --labels="$LABELS" \
    --metadata=enable-oslogin=TRUE,block-project-ssh-keys=TRUE \
    --metadata-from-file="startup-script=$ROOT/deploy/host/first-boot.sh,crawler-bundle=$WORK/bundle.b64"
}

update_host() {
  say "Host bundle → $VM metadata"
  g compute instances add-metadata "$VM" --zone="$ZONE" \
    --metadata-from-file="startup-script=$ROOT/deploy/host/first-boot.sh,crawler-bundle=$WORK/bundle.b64"
  echo "  then on the VM: sudo google_metadata_script_runner startup" >&2
}

monitoring() {
  say "Notification channel + alert policies"
  PLACEHOLDER=""
  CHANNEL="$(gv beta monitoring channels list --filter="displayName=\"$CHANNEL_NAME\"" --format='value(name)' | head -n1)"
  if [[ -z "$CHANNEL" ]]; then
    g beta monitoring channels create --display-name="$CHANNEL_NAME" --type=email \
      --channel-labels=email_address="$CRAWLER_ALERT_EMAIL" --user-labels="$LABELS"
    PLACEHOLDER="<channel>"
    CHANNEL="$(gv beta monitoring channels list --filter="displayName=\"$CHANNEL_NAME\"" --format='value(name)' | head -n1)"
  fi
  local f title out
  for f in "$ROOT"/infrastructure/monitoring/*.json; do
    title="$(sed -n 's/^  "displayName": "\(.*\)",$/\1/p' "$f" | head -n1)"
    PLACEHOLDER=""
    if [[ -n "$(gv monitoring policies list --filter="displayName=\"$title\"" --format='value(name)')" ]]; then
      continue
    fi
    out="$WORK/$(basename "$f")"
    sed "s|__CHANNEL__|$CHANNEL|" "$f" >"$out"
    g monitoring policies create --policy-from-file="$out"
  done
}

budget() {
  say "Budget"
  PLACEHOLDER=""
  if [[ -n "$(value gcloud billing budgets list --billing-account="$BILLING_ACCOUNT" \
    --filter="displayName=\"$BUDGET_NAME\"" --format='value(name)')" ]]; then
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
  drill_template
  vm
  monitoring
  budget
  say "Done. Manual steps: infrastructure/README.md"
}

main
