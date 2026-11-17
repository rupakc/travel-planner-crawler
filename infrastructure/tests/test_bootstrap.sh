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
  unset FAKE_EXIST PROJECT_ID BILLING_ACCOUNT CRAWLER_INTERNAL_IP FAKE_MIG_TEMPLATE FAKE_NO_MIG
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
  # the template name carries a hash of deploy/'s content: normalised, so a
  # change anywhere in deploy/ does not churn the golden file
  sed -E -e "s|$BOOTSTRAP_WORK|<work>|g" -e "s|$ROOT|<root>|g" \
    -e 's/voyager-crawler-(spot|std)-[0-9a-f]{12}/voyager-crawler-\1-<hash>/g' "$out" >"$T/norm"
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
  PATH="$p" bash "$SCRIPT" --dry-run --update-host --standard >/dev/null 2>&1
  PATH="$p" bash "$SCRIPT" --dry-run --standard >/dev/null 2>&1
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
    "compute disks create crawler-data --zone=us-east1-b --size=30GB --type=pd-balanced" \
    "firewall-rules create allow-crawler-api" "firewall-rules create allow-crawler-iap-ssh" \
    "firewall-rules create deny-crawler-ingress" "instance-templates create crawler-drill-template" \
    "firewall-rules create allow-crawler-health-check" "health-checks create https crawler-healthz" \
    "instance-templates create voyager-crawler-spot-" "instance-groups managed create voyager-crawler-mig" \
    "instance-groups managed create-instance voyager-crawler-mig --zone=us-east1-b --instance=voyager-crawler" \
    "monitoring channels create" "email_address=alerts@example.invalid" \
    "billing budgets create" "--budget-amount=3000INR" \
    "--notifications-rule-monitoring-notification-channels=projects/voyager-prod-ohzkl/notificationChannels/42"; do
    grep -qF -- "$want" "$CALLS" || { fail fresh_project_creates_everything "missing: $want"; return; }
  done
  local n
  n="$(find "$ROOT/infrastructure/monitoring" -name '*.json' | wc -l | tr -d ' ')"
  [[ "$n" == 14 && "$(grep -c 'monitoring policies create' "$CALLS")" == "$n" ]] || { fail fresh_project_creates_everything "policies: $(grep -c 'monitoring policies create' "$CALLS") of $n files"; return; }
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
    "billing budgets update billingAccounts/010A0B-F39593-14A899/budgets/b1 --billing-account=010A0B-F39593-14A899 --budget-amount=3000INR" \
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

# fresh_run — a full run against an empty project; the calls are in $CALLS
fresh_run() {
  setup
  export FAKE_EXIST=0
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" "$@" >"$T/out" 2>&1
}
# call PATTERN — the one recorded gcloud call containing PATTERN
call() { grep -F -- "$1" "$CALLS"; }
# has_all LINE WORD… — every WORD is in LINE (exact, as a separate argument)
has_all() {
  local line=" $1 " w
  shift
  for w in "$@"; do [[ "$line" == *" $w "* ]] || { echo "missing $w"; return 1; }; done
}
# template_name ARGS… — the template a dry run with ARGS would create
template_name() {
  CRAWLER_ALERT_EMAIL=x@example.invalid PATH="/usr/bin:/bin" bash "$SCRIPT" --dry-run "$@" 2>&1 |
    sed -nE 's/^==> Instance template (voyager-crawler-[a-z]+-[0-9a-f]{12})$/\1/p'
}

# The VM is a Spot e2-medium in an instance template: shielded, OS Login,
# crawler-runtime, 10 GB pd-balanced boot disk (device crawler-boot), STOP on
# preemption, startup + shutdown scripts and the host bundle as metadata
test_spot_instance_template() {
  fresh_run || { fail spot_instance_template "exit $?: $(tail -5 "$T/out")"; return; }
  local c why
  c="$(call 'instance-templates create voyager-crawler-spot-')"
  why="$(has_all "$c" --machine-type=e2-medium --provisioning-model=SPOT --instance-termination-action=STOP \
    --maintenance-policy=TERMINATE --no-restart-on-failure --boot-disk-size=10GB --boot-disk-type=pd-balanced \
    --boot-disk-device-name=crawler-boot --image-family=debian-12 --network-tier=STANDARD \
    --service-account=crawler-runtime@voyager-prod-ohzkl.iam.gserviceaccount.com --shielded-secure-boot \
    --shielded-vtpm --shielded-integrity-monitoring --labels=app=voyager-crawler \
    --metadata=enable-oslogin=TRUE,block-project-ssh-keys=TRUE)" || { fail spot_instance_template "$why: $c"; return; }
  [[ "$c" == *"--metadata-from-file=startup-script=$ROOT/deploy/host/first-boot.sh,shutdown-script=$ROOT/deploy/host/shutdown.sh,crawler-bundle=$BOOTSTRAP_WORK/bundle.b64"* ]] ||
    { fail spot_instance_template "metadata: $c"; return; }
  [[ "$c" == *--deletion-protection* || "$c" == *--address=* ]] && { fail spot_instance_template "per-VM settings in the template: $c"; return; }
  ! grep -q 'compute instances create' "$CALLS" && ok spot_instance_template || fail spot_instance_template "a plain VM is still created"
}

# One stateful group, size 1 through create-instance: the boot disk and both
# IPs stateful by policy, the data disk and the two reserved addresses in the
# per-instance config, never auto-deleted; autohealing on crawler-healthz
test_stateful_mig() {
  fresh_run || { fail stateful_mig "exit $?: $(tail -5 "$T/out")"; return; }
  local c why
  c="$(call 'instance-groups managed create voyager-crawler-mig')"
  why="$(has_all "$c" --zone=us-east1-b --size=0 --base-instance-name=voyager-crawler \
    --stateful-disk=device-name=crawler-boot,auto-delete=never \
    --stateful-internal-ip=interface-name=nic0,auto-delete=never \
    --stateful-external-ip=interface-name=nic0,auto-delete=never \
    --health-check=projects/voyager-prod-ohzkl/global/healthChecks/crawler-healthz --initial-delay=1800 \
    --default-action-on-vm-failure=repair --force-update-on-repair --update-policy-type=opportunistic)" ||
    { fail stateful_mig "$why: $c"; return; }
  [[ "$c" =~ --template=voyager-crawler-spot-[0-9a-f]{12}\  ]] || { fail stateful_mig "template: $c"; return; }
  c="$(call 'instance-groups managed create-instance')"
  why="$(has_all "$c" --instance=voyager-crawler \
    --stateful-disk=device-name=crawler-data,source=projects/voyager-prod-ohzkl/zones/us-east1-b/disks/crawler-data,mode=rw,auto-delete=never \
    --stateful-internal-ip=address=projects/voyager-prod-ohzkl/regions/us-east1/addresses/crawler-internal-ip,interface-name=nic0,auto-delete=never \
    --stateful-external-ip=address=projects/voyager-prod-ohzkl/regions/us-east1/addresses/crawler-external-ip,interface-name=nic0,auto-delete=never)" ||
    { fail stateful_mig "$why: $c"; return; }
  # order: disk and addresses before the group, the template before the group
  local ln_disk ln_tpl ln_mig ln_inst
  ln_disk="$(grep -n 'disks create crawler-data' "$CALLS" | cut -d: -f1)"
  ln_tpl="$(grep -n 'instance-templates create voyager-crawler-spot-' "$CALLS" | cut -d: -f1)"
  ln_mig="$(grep -n 'managed create voyager-crawler-mig' "$CALLS" | cut -d: -f1)"
  ln_inst="$(grep -n 'managed create-instance' "$CALLS" | cut -d: -f1)"
  ((ln_disk < ln_tpl && ln_tpl < ln_mig && ln_mig < ln_inst)) && ok stateful_mig || fail stateful_mig "order $ln_disk $ln_tpl $ln_mig $ln_inst"
}

# HTTPS /healthz on 8443 for autohealing, and a firewall rule for Google's
# health-check ranges only, to that port only, on the crawler's SA only
test_health_check_and_firewall() {
  fresh_run || { fail health_check_and_firewall "exit $?"; return; }
  local c why
  c="$(call 'health-checks create https crawler-healthz')"
  why="$(has_all "$c" --global --port=8443 --request-path=/healthz --check-interval=90s --timeout=10s \
    --healthy-threshold=1 --unhealthy-threshold=10)" || { fail health_check_and_firewall "$why: $c"; return; }
  c="$(call 'firewall-rules create allow-crawler-health-check')"
  why="$(has_all "$c" --action=ALLOW --priority=900 --rules=tcp:8443 --source-ranges=130.211.0.0/22,35.191.0.0/16 \
    --target-service-accounts=crawler-runtime@voyager-prod-ohzkl.iam.gserviceaccount.com)" ||
    { fail health_check_and_firewall "$why: $c"; return; }
  # a second run re-asserts both
  : >"$CALLS"
  export FAKE_EXIST=1
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail health_check_and_firewall "second run exit $?"; return; }
  grep -qF 'health-checks update https crawler-healthz --global --port=8443' "$CALLS" &&
    grep -qF 'firewall-rules update allow-crawler-health-check --priority=900 --rules=tcp:8443 --source-ranges=130.211.0.0/22,35.191.0.0/16' "$CALLS" &&
    grep -qF 'instance-groups managed update voyager-crawler-mig --zone=us-east1-b --health-check=projects/voyager-prod-ohzkl/global/healthChecks/crawler-healthz --initial-delay=1800' "$CALLS" &&
    ok health_check_and_firewall || fail health_check_and_firewall "not re-asserted"
}

# No disk snapshot schedule any more (wal-g PITR + the restore drill)
test_no_snapshot_schedule() {
  fresh_run || { fail no_snapshot_schedule "exit $?"; return; }
  local bad
  bad="$(grep -E 'resource-policies|snapshot' "$CALLS" || true)"
  [[ -z "$bad" ]] || { fail no_snapshot_schedule "$bad"; return; }
  grep -qE 'snapshot-schedule|resource-policies' "$SCRIPT" && fail no_snapshot_schedule "bootstrap.sh still makes a snapshot schedule" ||
    ok no_snapshot_schedule
}

# A second run on an up-to-date group changes nothing on it; a changed
# deploy/ (another template name) is created and adopted with a refresh only
test_second_run_template_roll() {
  setup
  local tpl
  tpl="$(template_name)"
  [[ "$tpl" =~ ^voyager-crawler-spot-[0-9a-f]{12}$ ]] || { fail second_run_template_roll "template name '$tpl'"; return; }
  export FAKE_EXIST=1 FAKE_MIG_TEMPLATE="$tpl"
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail second_run_template_roll "exit $?"; return; }
  grep -qE 'set-instance-template|update-instances|create-instance|instance-templates create voyager' "$CALLS" &&
    { fail second_run_template_roll "touched an up-to-date group"; return; }
  : >"$CALLS"
  export FAKE_MIG_TEMPLATE=voyager-crawler-spot-000000000000
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" >"$T/out" 2>&1 || { fail second_run_template_roll "exit $?"; return; }
  grep -qF "instance-templates describe $tpl" "$CALLS" &&
    grep -qxF "gcloud --project=voyager-prod-ohzkl --quiet compute instance-groups managed set-instance-template voyager-crawler-mig --zone=us-east1-b --template=$tpl" "$CALLS" &&
    grep -qxF 'gcloud --project=voyager-prod-ohzkl --quiet compute instance-groups managed update-instances voyager-crawler-mig --zone=us-east1-b --instances=voyager-crawler --minimal-action=refresh --most-disruptive-allowed-action=replace' "$CALLS" &&
    ok second_run_template_roll || fail second_run_template_roll "roll calls missing"
}

# The template's name follows deploy/'s content, not file times: an identical
# copy gives the same name, one changed byte another
test_template_name_follows_content() {
  setup
  local a b c
  a="$(template_name)"
  mkdir -p "$T/copy"
  cp -R "$ROOT/infrastructure" "$ROOT/deploy" "$T/copy/"
  touch "$T/copy/deploy/host/first-boot.sh"
  b="$(CRAWLER_ALERT_EMAIL=x@example.invalid PATH="/usr/bin:/bin" bash "$T/copy/infrastructure/bootstrap.sh" --dry-run 2>&1 |
    sed -nE 's/^==> Instance template (voyager-crawler-[a-z]+-[0-9a-f]{12})$/\1/p')"
  echo "# changed" >>"$T/copy/deploy/host/shutdown.sh"
  c="$(CRAWLER_ALERT_EMAIL=x@example.invalid PATH="/usr/bin:/bin" bash "$T/copy/infrastructure/bootstrap.sh" --dry-run 2>&1 |
    sed -nE 's/^==> Instance template (voyager-crawler-[a-z]+-[0-9a-f]{12})$/\1/p')"
  [[ -n "$a" && "$a" == "$b" && -n "$c" && "$c" != "$a" ]] && ok template_name_follows_content ||
    fail template_name_follows_content "'$a' '$b' '$c'"
}

# --standard: one command (--update-host --standard) moves the group to a
# standard template (live migration, automatic restart, no termination
# action) and applies it; later runs without a flag keep standard
test_standard_flag() {
  setup
  FAKE_MIG_TEMPLATE="$(template_name)"
  export FAKE_EXIST=1 FAKE_MIG_TEMPLATE
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" --update-host --standard >"$T/out" 2>&1 || { fail standard_flag "exit $?: $(tail -3 "$T/out")"; return; }
  local c std
  std="$(template_name --standard)"
  [[ "$std" =~ ^voyager-crawler-std-[0-9a-f]{12}$ ]] || { fail standard_flag "dry-run name '$std'"; return; }
  # FAKE_EXIST=1: the template "exists"; the group is pointed at it and updated
  grep -qF "set-instance-template voyager-crawler-mig --zone=us-east1-b --template=$std" "$CALLS" &&
    grep -qF 'update-instances voyager-crawler-mig --zone=us-east1-b --instances=voyager-crawler' "$CALLS" ||
    { fail standard_flag "group not moved: $(cat "$CALLS")"; return; }
  # what the template looks like (fresh project, full run)
  fresh_run --standard || { fail standard_flag "fresh exit $?"; return; }
  c="$(call 'instance-templates create voyager-crawler-std-')"
  [[ "$c" == *" --provisioning-model=STANDARD --maintenance-policy=MIGRATE --restart-on-failure "* &&
    "$c" != *instance-termination-action* && "$c" != *SPOT* ]] || { fail standard_flag "template: $c"; return; }
  # no flag: the group's current model stays (here: standard)
  setup
  export FAKE_EXIST=1 FAKE_MIG_TEMPLATE=voyager-crawler-std-000000000000
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" --update-host >"$T/out" 2>&1 || { fail standard_flag "exit $?"; return; }
  grep -qF "set-instance-template voyager-crawler-mig --zone=us-east1-b --template=$std" "$CALLS" &&
    ! grep -q 'voyager-crawler-spot-' "$CALLS" && ok standard_flag || fail standard_flag "flipped back to Spot: $(grep template "$CALLS")"
}

# --update-host refreshes the group's template, never a VM's metadata; it
# refuses to run without the group
test_update_host_uses_template() {
  setup
  export FAKE_EXIST=1 FAKE_MIG_TEMPLATE=voyager-crawler-spot-000000000000
  PATH="$HERE/fakes:$PATH" bash "$SCRIPT" --update-host >"$T/out" 2>&1 || { fail update_host_uses_template "exit $?"; return; }
  grep -q 'instances add-metadata' "$CALLS" && { fail update_host_uses_template "set VM metadata"; return; }
  grep -q 'set-instance-template' "$CALLS" || { fail update_host_uses_template "no template roll"; return; }
  grep -qE ' (iam|secrets|storage|firewall-rules|monitoring|billing) ' "$CALLS" && { fail update_host_uses_template "touched more than the group"; return; }
  : >"$CALLS"
  FAKE_NO_MIG=1 PATH="$HERE/fakes:$PATH" bash "$SCRIPT" --update-host >"$T/out" 2>&1 &&
    { fail update_host_uses_template "ran without a group"; return; }
  grep -qE 'instance-templates create|set-instance-template' "$CALLS" && fail update_host_uses_template "created without a group" ||
    ok update_host_uses_template
}

# The 6 h "crawler down" policy, and every absence condition aggregated
# across instances: a MIG recreate gives the VM a new instance id, and a
# per-instance absence would fire for the old one forever
test_absence_conditions_survive_recreate() {
  local f bad=""
  for f in "$ROOT"/infrastructure/monitoring/*.json; do
    bad+="$(jq -r --arg f "$(basename "$f")" '.conditions[] | select(.conditionAbsent)
      | select((.conditionAbsent.aggregations[0].crossSeriesReducer // "") == ""
        or ((.conditionAbsent.aggregations[0].groupByFields // []) | map(startswith("resource.")) | any))
      | "\($f): \(.displayName)\n"' "$f")"
  done
  [[ -z "$bad" ]] || { fail absence_conditions_survive_recreate "$bad"; return; }
  f="$ROOT/infrastructure/monitoring/crawler-down-6h.json"
  jq -e '.conditions[0].conditionAbsent | .duration == "21600s"
    and (.filter | contains("custom.googleapis.com/crawler/heartbeat"))
    and (.aggregations[0].groupByFields == null)' "$f" >/dev/null &&
    jq -e '.notificationChannels == ["__CHANNEL__"] and (.documentation.content | contains("--update-host --standard"))' "$f" >/dev/null &&
    ok absence_conditions_survive_recreate || fail absence_conditions_survive_recreate "crawler-down-6h.json"
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
