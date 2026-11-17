#!/usr/bin/env bash
# bootstrap.sh without GCP: a golden --dry-run transcript, and a fake gcloud
# for the idempotency checks. Run: bash infrastructure/tests/test_bootstrap.sh
# Regenerate the golden file after an intended change: UPDATE_GOLDEN=1 bash …
# shellcheck disable=SC2015 # `check && ok || fail` is the intended reporting idiom
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
SCRIPT="$ROOT/infrastructure/bootstrap.sh"
GOLDEN="$HERE/golden/dry-run.txt"
FAILS=0
PASSES=0
ok() { PASSES=$((PASSES + 1)); echo "ok   $1"; }
fail() { FAILS=$((FAILS + 1)); echo "FAIL $1: $2"; }

setup() {
  T="$(mktemp -d)"
  export CALLS="$T/calls" FAKE_STATE="$T/fake" BOOTSTRAP_WORK="$T/work"
  export CRAWLER_ALERT_EMAIL=alerts@example.invalid
  unset FAKE_EXIST PROJECT_ID BILLING_ACCOUNT CRAWLER_INTERNAL_IP
  : >"$CALLS"
}
# mutations CALLS → the calls that change something
mutations() {
  grep -E ' (create|update|add|add-iam-policy-binding|enable|delete|add-resource-policies|add-metadata) ' "$CALLS" || true
}

test_dry_run_golden() {
  setup
  local out="$T/out"
  # no gcloud at all on PATH: a dry run must not need it
  PATH="/usr/bin:/bin" bash "$SCRIPT" --dry-run >"$out" 2>&1 || { fail dry_run_golden "exit $?"; cat "$out"; return; }
  sed -e "s|$BOOTSTRAP_WORK|<work>|g" -e "s|$ROOT|<root>|g" "$out" >"$T/norm"
  if [[ "${UPDATE_GOLDEN:-0}" == 1 ]]; then
    cp "$T/norm" "$GOLDEN"
    echo "     (golden updated)"
  fi
  if diff -u "$GOLDEN" "$T/norm"; then ok dry_run_golden; else fail dry_run_golden "transcript differs (UPDATE_GOLDEN=1 after an intended change)"; fi
}

test_dry_run_passes_project_everywhere() {
  setup
  local bad
  bash "$SCRIPT" --dry-run 2>&1 | grep -E '^  gcloud ' | grep -v ' billing ' >"$T/g"
  bad="$(grep -v -- '--project=voyager-prod-ohzkl' "$T/g" || true)"
  [[ -s "$T/g" && -z "$bad" ]] && ok dry_run_passes_project_everywhere || fail dry_run_passes_project_everywhere "$bad"
}

# fake gcloud, openssl, curl and gsutil on PATH: a dry run calls none of them
test_dry_run_calls_nothing() {
  setup
  export CRAWLER_ALERT_EMAIL=example@example.com
  local p="$HERE/fakes:$HERE/fakes-dry:$PATH"
  PATH="$p" bash "$SCRIPT" --dry-run >/dev/null 2>&1
  PATH="$p" bash "$SCRIPT" --dry-run --rotate-tls >/dev/null 2>&1
  PATH="$p" bash "$SCRIPT" --dry-run --update-host >/dev/null 2>&1
  [[ ! -s "$CALLS" ]] && ok dry_run_calls_nothing || fail dry_run_calls_nothing "$(cat "$CALLS")"
}

test_fresh_project_creates_everything() {
  setup
  export FAKE_EXIST=0
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail fresh_project_creates_everything "exit $?: $(tail -5 "$T/out")"; return; }
  local want
  for want in "iam service-accounts create crawler-runtime" "iam service-accounts create crawler-drill" \
    "iam roles create voyagerCrawlerVertex" "iam roles create voyagerCrawlerDrill" \
    "storage buckets create gs://voyager-crawler-raw" "storage buckets create gs://voyager-crawler-ops" \
    "secrets create crawler-pg-password" "secrets create crawler-api-token-voyager" \
    "secrets create crawler-api-tls-key" "secrets create crawler-api-tls-cert" \
    "secrets versions add crawler-pg-password" "secrets versions add crawler-api-token-voyager" \
    "secrets versions add crawler-api-tls-key" "secrets versions add crawler-api-tls-cert" \
    "compute addresses create crawler-internal-ip" "compute addresses create crawler-external-ip" \
    "snapshot-schedule crawler-data-daily" "compute disks create crawler-data" \
    "firewall-rules create allow-crawler-api" "firewall-rules create allow-crawler-iap-ssh" \
    "firewall-rules create deny-crawler-ingress" "instance-templates create crawler-drill-template" \
    "compute instances create voyager-crawler" "--private-network-ip=10.142.0.50" "--address=34.0.0.7" \
    "monitoring channels create" "email_address=alerts@example.invalid" \
    "billing budgets create" "--budget-amount=9000INR" \
    "--notifications-rule-monitoring-notification-channels=projects/voyager-prod-ohzkl/notificationChannels/42"; do
    grep -qF -- "$want" "$CALLS" || { fail fresh_project_creates_everything "missing: $want"; return; }
  done
  [[ "$(grep -c 'monitoring policies create' "$CALLS")" == 13 ]] || { fail fresh_project_creates_everything "policies: $(grep -c 'monitoring policies create' "$CALLS")"; return; }
  grep -q '"projects/voyager-prod-ohzkl/notificationChannels/42"' "$BOOTSTRAP_WORK/crawler-down.json" ||
    { fail fresh_project_creates_everything "channel not substituted"; return; }
  # TLS: a 2-year P-256 certificate for the internal IP; the key never stays on disk
  openssl x509 -in "$BOOTSTRAP_WORK/tls.crt" -noout -text | grep -q 'IP Address:10.142.0.50' ||
    { fail fresh_project_creates_everything "certificate SAN"; return; }
  [[ ! -e "$BOOTSTRAP_WORK/tls.key" && ! -e "$BOOTSTRAP_WORK/crawler-pg-password" ]] ||
    { fail fresh_project_creates_everything "secret material left in the work dir"; return; }
  grep -E 'projects add-iam-policy-binding' "$CALLS" | grep -v -- '--condition' &&
    { fail fresh_project_creates_everything "project binding without --condition"; return; }
  ok fresh_project_creates_everything
}

test_second_run_creates_nothing() {
  setup
  export FAKE_EXIST=1
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail second_run_creates_nothing "exit $?: $(tail -5 "$T/out")"; return; }
  local bad
  bad="$(mutations | grep -E ' create | versions add | delete | add-resource-policies ' | grep -v 'services identity create' || true)"
  [[ -z "$bad" ]] || { fail second_run_creates_nothing "$bad"; return; }
  # re-asserted settings and bindings still run
  grep -qF "storage buckets update gs://voyager-crawler-raw --versioning" "$CALLS" &&
    grep -qF "iam roles update voyagerCrawlerDrill" "$CALLS" &&
    grep -qF "secrets add-iam-policy-binding crawler-api-tls-cert --member=serviceAccount:vm-runtime@" "$CALLS" &&
    ok second_run_creates_nothing || fail second_run_creates_nothing "re-asserts missing"
}

# Go-live review 7: the §9 log bucket (30 days), its sink with the DEBUG
# exclusion, and the _Default sink's exclusion (logs ingested once)
test_log_bucket_and_sink() {
  setup
  export FAKE_EXIST=0
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail log_bucket_and_sink "exit $?"; return; }
  local want
  for want in "logging buckets create voyager-crawler-logs --location=us-east1 --retention-days=30" \
    "logging sinks create voyager-crawler-logs logging.googleapis.com/projects/voyager-prod-ohzkl/locations/us-east1/buckets/voyager-crawler-logs --log-filter=logName=\"projects/voyager-prod-ohzkl/logs/crawler_docker\" --exclusion=name=debug,filter=severity<INFO" \
    "logging sinks update _Default --add-exclusion=name=voyager-crawler-logs,filter=logName=\"projects/voyager-prod-ohzkl/logs/crawler_docker\""; do
    grep -qF -- "$want" "$CALLS" || { fail log_bucket_and_sink "missing: $want"; return; }
  done
  ok log_bucket_and_sink
}

# Go-live review 17: a second run re-asserts policies (from their files),
# the channel's email, the budget and the firewall rules, creating nothing
test_second_run_reasserts() {
  setup
  export FAKE_EXIST=1 CRAWLER_ALERT_EMAIL=new-owner@example.invalid
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail second_run_reasserts "exit $?"; return; }
  local n want
  n="$(find "$ROOT/infrastructure/monitoring" -name '*.json' | wc -l | tr -d ' ')"
  [[ "$(grep -c 'monitoring policies update projects/voyager-prod-ohzkl/alertPolicies/7 --policy-from-file=' "$CALLS")" == "$n" ]] ||
    { fail second_run_reasserts "policy updates: $(grep -c 'monitoring policies update' "$CALLS"), want $n"; return; }
  for want in "monitoring channels update projects/voyager-prod-ohzkl/notificationChannels/42 --update-channel-labels=email_address=new-owner@example.invalid" \
    "billing budgets update billingAccounts/010A0B-F39593-14A899/budgets/b1 --billing-account=010A0B-F39593-14A899 --budget-amount=9000INR" \
    "--clear-threshold-rules --add-threshold-rule=percent=0.5 --add-threshold-rule=percent=0.9 --add-threshold-rule=percent=1.0 --notifications-rule-monitoring-notification-channels=projects/voyager-prod-ohzkl/notificationChannels/42" \
    "compute firewall-rules update allow-crawler-api --priority=900 --rules=tcp:8443" \
    "compute firewall-rules update allow-crawler-iap-ssh --priority=900 --rules=tcp:22" \
    "compute firewall-rules update deny-crawler-ingress --priority=1000 --rules=all" \
    "logging buckets update voyager-crawler-logs --location=us-east1 --retention-days=30" \
    "logging sinks update voyager-crawler-logs"; do
    grep -qF -- "$want" "$CALLS" || { fail second_run_reasserts "missing: $want"; return; }
  done
  grep -q 'sinks update _Default' "$CALLS" && { fail second_run_reasserts "re-added the _Default exclusion"; return; }
  ok second_run_reasserts
}

# The VM gets no instance-creating role until the Spot drill that needs it
# exists, and no Vertex service agent reaches -ops until batch is wired
test_no_drill_or_vertex_agent_bindings() {
  setup
  export FAKE_EXIST=0
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail no_drill_or_vertex_agent_bindings "exit $?"; return; }
  local bad
  bad="$(grep -E 'roles/voyagerCrawlerDrill|iam.serviceAccountUser|gcp-sa-aiplatform|storage.objectUser|services identity create' "$CALLS" || true)"
  [[ -z "$bad" ]] && ok no_drill_or_vertex_agent_bindings || fail no_drill_or_vertex_agent_bindings "$bad"
}

test_rotate_tls_touches_only_tls() {
  setup
  export FAKE_EXIST=1
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" --rotate-tls >"$T/out" 2>&1 || { fail rotate_tls_touches_only_tls "exit $?"; return; }
  local m
  m="$(mutations | sed 's/.* secrets versions add \([a-z-]*\).*/\1/' | tr '\n' ' ')"
  [[ "$m" == "crawler-api-tls-key crawler-api-tls-cert " ]] && ok rotate_tls_touches_only_tls ||
    fail rotate_tls_touches_only_tls "mutations: $m"
}

test_requires_alert_email() {
  setup
  unset CRAWLER_ALERT_EMAIL
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >/dev/null 2>&1
  local rc=$?
  [[ $rc == 2 ]] && ! mutations | grep -q . && ok requires_alert_email || fail requires_alert_email "rc $rc"
}

test_no_hardcoded_email() {
  if grep -rEn '[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[a-z]{2,}' "$ROOT/infrastructure" --include='*.sh' --include='*.json' |
    grep -vE 'iam\.gserviceaccount\.com|example\.invalid|example@example\.com|gcp-sa-aiplatform|noreply' >/dev/null; then
    fail no_hardcoded_email "an email address is hard-coded"
  else
    ok no_hardcoded_email
  fi
}

for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
  "$t"
done
echo "$PASSES passed, $FAILS failed"
((FAILS == 0))
