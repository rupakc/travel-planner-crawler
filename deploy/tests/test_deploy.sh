#!/usr/bin/env bash
# shellcheck disable=SC2015 # `check && ok || fail` is the intended reporting idiom
# deploy.sh and watchdog.sh against fake docker/cosign/curl (deploy/tests/fakes).
# No root, no network, no Docker needed. Run: bash deploy/tests/test_deploy.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
DEPLOY="$HERE/../host/deploy.sh"
WATCHDOG="$HERE/../host/watchdog.sh"
OLD=sha256:$(printf 'a%.0s' {1..64})
NEW=sha256:$(printf 'b%.0s' {1..64})
IMG=ghcr.io/rupakc/voyager-crawler
FAILS=0
PASSES=0

setup() {
  T="$(mktemp -d)"
  export CALLS="$T/calls" FAKE_STATE="$T/fake" VC_STATE="$T/state" VC_SECRETS_DIR="$T/secrets"
  export VC_OPT="$T/opt" PATH="$HERE/fakes:$PATH" VC_HEALTH_POLL=0 VC_HEALTH_TIMEOUT=5
  unset FAKE_PROD_DIGEST FAKE_RUNNING FAKE_UNHEALTHY_DIGEST FAKE_MIGRATE_RC FAKE_SIGNED \
    FAKE_LABEL_SCHEMA FAKE_LABEL_MIN_SCHEMA FAKE_PS_A FAKE_LOCKED VC_NOW FAKE_DB_SCHEMA
  mkdir -p "$VC_STATE" "$VC_SECRETS_DIR" "$FAKE_STATE" "$VC_OPT"
  : >"$CALLS"
  echo "VC_API_BIND=10.142.0.9" >"$VC_STATE/host.env"
  # fresh secrets: deploy.sh must not try Secret Manager
  for f in pg_password_db pg_password api_token_voyager api_tls_key api_tls_cert; do
    echo x >"$VC_SECRETS_DIR/$f"
  done
  touch "$VC_STATE/secrets.stamp"
}

# running_on DIGEST — the stack is deployed and running on DIGEST
running_on() {
  echo "VC_IMAGE=$IMG@$1" >"$VC_STATE/image.env"
  local s
  for s in postgres chromium otelcol maintenance scheduler processor embedder fetcher api; do
    echo "$IMG@$1" >"$FAKE_STATE/$s"
  done
}

ok() { PASSES=$((PASSES + 1)); echo "ok   $1"; }
fail() {
  FAILS=$((FAILS + 1))
  echo "FAIL $1: $2"
  echo "---- calls"; cat "${CALLS:-/dev/null}"; echo "----"
}
has() { grep -qF -- "$1" "$CALLS"; }
line_of() { grep -nF -- "$1" "$CALLS" | head -n1 | cut -d: -f1; }

test_refuses_unsigned() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$OLD"
  bash "$DEPLOY" 2>"$T/err"
  local rc=$?
  [[ $rc == 3 ]] || { fail refuses_unsigned "exit $rc, want 3"; return; }
  has "cosign verify --certificate-identity https://github.com/rupakc/travel-planner-crawler/.github/workflows/deploy.yml@refs/heads/main --certificate-oidc-issuer https://token.actions.githubusercontent.com $IMG@$NEW" ||
    { fail refuses_unsigned "cosign not called with the pinned identity"; return; }
  has "pull" && { fail refuses_unsigned "pulled an unsigned image"; return; }
  has "run --rm migrate" && { fail refuses_unsigned "migrated"; return; }
  has " up " && { fail refuses_unsigned "rolled"; return; }
  grep -q "^$NEW signature check failed" "$VC_STATE/held" || { fail refuses_unsigned "digest not held"; return; }
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] || { fail refuses_unsigned "image.env changed"; return; }
  # next tick: held, so not even verified again
  : >"$CALLS"
  bash "$DEPLOY" 2>/dev/null
  rc=$?
  [[ $rc == 0 ]] || { fail refuses_unsigned "held run exit $rc"; return; }
  has cosign && { fail refuses_unsigned "re-verified a held digest"; return; }
  ok refuses_unsigned
}

test_refuses_wrong_identity() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW"
  VC_CERT_IDENTITY="https://github.com/someone/fork/.github/workflows/deploy.yml@refs/heads/main" \
    bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 3 ]] && ! has pull && ok refuses_wrong_identity || fail refuses_wrong_identity "exit $rc"
}

test_migrate_runs_before_roll() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_LABEL_SCHEMA=1207
  bash "$DEPLOY" 2>"$T/err"
  local rc=$?
  [[ $rc == 0 ]] || { fail migrate_runs_before_roll "exit $rc: $(cat "$T/err")"; return; }
  local v p m r
  v="$(line_of "cosign verify")"
  p="$(line_of "pull -q $IMG@$NEW")"
  m="$(grep -nF "run --rm migrate" "$CALLS" | head -n1 | cut -d: -f1)"
  r="$(grep -nF "up -d --no-deps maintenance" "$CALLS" | head -n1 | cut -d: -f1)"
  [[ -n "$v" && -n "$p" && -n "$m" && -n "$r" ]] || { fail migrate_runs_before_roll "missing step (v=$v p=$p m=$m r=$r)"; return; }
  ((v < p && p < m && m < r)) || { fail migrate_runs_before_roll "order verify=$v pull=$p migrate=$m roll=$r"; return; }
  grep -F "run --rm migrate" "$CALLS" | grep -qF "VC_IMAGE=$IMG@$NEW" ||
    { fail migrate_runs_before_roll "migrate did not use the new image"; return; }
  local order
  order="$(grep -oE 'up -d --no-deps [a-z]+' "$CALLS" | awk '{print $4}' | tr '\n' ' ')"
  [[ "$order" == "maintenance scheduler processor embedder fetcher api " ]] ||
    { fail migrate_runs_before_roll "roll order '$order'"; return; }
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$NEW" ]] || { fail migrate_runs_before_roll "image.env not updated"; return; }
  [[ "$(cat "$VC_STATE/previous")" == "$IMG@$OLD" ]] || { fail migrate_runs_before_roll "previous not recorded"; return; }
  [[ "$(cat "$VC_STATE/schema")" == 1207 ]] || { fail migrate_runs_before_roll "schema not recorded"; return; }
  ok migrate_runs_before_roll
}

test_migrate_sees_rollback_log() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW"
  # no log yet: deploy.sh creates it (a missing bind source would fail compose)
  bash "$DEPLOY" 2>/dev/null || { fail migrate_sees_rollback_log "first deploy failed"; return; }
  has "migrate bind rollbacks 0 lines" || { fail migrate_sees_rollback_log "log not created before migrate"; return; }
  [[ "$(stat -c %a "$VC_STATE/rollbacks" 2>/dev/null || stat -f %Lp "$VC_STATE/rollbacks")" == 644 ]] ||
    { fail migrate_sees_rollback_log "log not world-readable (uid 65532 reads it)"; return; }
  # an existing log is kept as it is
  setup
  running_on "$OLD"
  printf '2026-11-01T00:00:00Z\n2026-11-02T00:00:00Z\n' >"$VC_STATE/rollbacks"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW"
  bash "$DEPLOY" 2>/dev/null || { fail migrate_sees_rollback_log "deploy failed"; return; }
  has "migrate bind rollbacks 2 lines" && ok migrate_sees_rollback_log ||
    fail migrate_sees_rollback_log "existing log not seen"
}

test_migrate_failure_rolls_nothing() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_MIGRATE_RC=1
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] || { fail migrate_failure_rolls_nothing "exit $rc"; return; }
  has "up -d --no-deps" && { fail migrate_failure_rolls_nothing "rolled after a failed migrate"; return; }
  grep -q "^$NEW migrate failed" "$VC_STATE/held" && ok migrate_failure_rolls_nothing ||
    fail migrate_failure_rolls_nothing "not held"
}

test_rollback_on_unhealthy() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_UNHEALTHY_DIGEST="$NEW"
  bash "$DEPLOY" 2>"$T/err"
  local rc=$?
  [[ $rc == 1 ]] || { fail rollback_on_unhealthy "exit $rc"; return; }
  # the first role failed: it goes back to OLD and nothing else rolled
  has "compose VC_IMAGE=$IMG@$OLD -p voyager-crawler -f $VC_OPT/compose.yaml --env-file $VC_STATE/host.env --env-file $VC_STATE/image.env up -d --no-deps maintenance" ||
    { fail rollback_on_unhealthy "maintenance not rolled back to the old digest"; return; }
  has "up -d --no-deps scheduler" && { fail rollback_on_unhealthy "kept rolling after a failure"; return; }
  local s
  for s in maintenance scheduler processor embedder fetcher api; do
    [[ "$(cat "$FAKE_STATE/$s")" == "$IMG@$OLD" ]] || { fail rollback_on_unhealthy "$s runs $(cat "$FAKE_STATE/$s")"; return; }
  done
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] || { fail rollback_on_unhealthy "image.env moved"; return; }
  grep -q "^$NEW unhealthy after roll (maintenance)" "$VC_STATE/held" || { fail rollback_on_unhealthy "not held"; return; }
  # held: the next tick neither verifies nor rolls
  : >"$CALLS"
  bash "$DEPLOY" 2>/dev/null
  { has cosign || has "up -d --no-deps"; } && { fail rollback_on_unhealthy "retried a held digest"; return; }
  ok rollback_on_unhealthy
}

test_rollback_on_unhealthy_late_role() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW"
  # only the api turns unhealthy on NEW: wrap the fake through a marker
  export FAKE_UNHEALTHY_DIGEST="__none__"
  # emulate: api container image check — mark api unhealthy via a state trick
  mkdir -p "$T/bin"
  cat >"$T/bin/docker" <<EOF
#!/usr/bin/env bash
if [[ "\$1" == inspect && "\${*: -1}" == cid-api ]] && grep -q "$NEW" "$FAKE_STATE/api"; then
  echo "inspect \$*" >>"$CALLS"; echo unhealthy; exit 0
fi
exec "$HERE/fakes/docker" "\$@"
EOF
  chmod +x "$T/bin/docker"
  PATH="$T/bin:$PATH" bash "$DEPLOY" 2>/dev/null
  local rc=$? s
  [[ $rc == 1 ]] || { fail rollback_on_unhealthy_late_role "exit $rc"; return; }
  for s in maintenance scheduler processor embedder fetcher api; do
    [[ "$(cat "$FAKE_STATE/$s")" == "$IMG@$OLD" ]] || { fail rollback_on_unhealthy_late_role "$s runs $(cat "$FAKE_STATE/$s")"; return; }
  done
  grep -q "unhealthy after roll (maintenance scheduler processor embedder fetcher api)" "$VC_STATE/held" &&
    ok rollback_on_unhealthy_late_role || fail rollback_on_unhealthy_late_role "held reason"
}

test_idempotent_when_digest_unchanged() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$OLD" FAKE_SIGNED="$OLD"
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 0 ]] || { fail idempotent_when_digest_unchanged "exit $rc"; return; }
  { has cosign || has pull || has " up " || has " run "; } &&
    { fail idempotent_when_digest_unchanged "did work on an unchanged digest"; return; }
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] || { fail idempotent_when_digest_unchanged "image.env changed"; return; }
  # a stopped service is started again, still without pull or cosign
  : >"$CALLS"
  FAKE_RUNNING="postgres chromium otelcol maintenance scheduler processor embedder api" bash "$DEPLOY" 2>/dev/null
  { has cosign || has pull; } && { fail idempotent_when_digest_unchanged "pulled/verified to restart"; return; }
  has "up -d --no-recreate fetcher" || { fail idempotent_when_digest_unchanged "stopped fetcher not started"; return; }
  ok idempotent_when_digest_unchanged
}

test_first_deploy() {
  setup
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW"
  bash "$DEPLOY" 2>"$T/err"
  local rc=$?
  [[ $rc == 0 ]] || { fail first_deploy "exit $rc: $(cat "$T/err")"; return; }
  has "up -d postgres chromium" || { fail first_deploy "base services not started"; return; }
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$NEW" && ! -f "$VC_STATE/previous" ]] &&
    ok first_deploy || fail first_deploy "state"
}

test_manual_rollback_respects_min_schema() {
  setup
  running_on "$NEW"
  echo "$IMG@$OLD" >"$VC_STATE/previous"
  echo 1205 >"$VC_STATE/schema"
  export FAKE_SIGNED="$OLD" FAKE_LABEL_MIN_SCHEMA=1206
  bash "$DEPLOY" --rollback 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] && ! has "up -d --no-deps" || { fail manual_rollback_respects_min_schema "rolled to a too-new image (rc $rc)"; return; }
  : >"$CALLS"
  export FAKE_LABEL_MIN_SCHEMA=1201
  bash "$DEPLOY" --rollback 2>/dev/null
  rc=$?
  [[ $rc == 0 && "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] ||
    { fail manual_rollback_respects_min_schema "rollback failed (rc $rc)"; return; }
  grep -q "^$NEW manual rollback" "$VC_STATE/held" && ok manual_rollback_respects_min_schema ||
    fail manual_rollback_respects_min_schema "rolled-back digest not held"
}

test_deploy_refuses_min_schema_above_db() {
  setup
  running_on "$OLD"
  # migrate "succeeded" but the database is still below what the image needs
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_LABEL_SCHEMA=1999 \
    FAKE_LABEL_MIN_SCHEMA=1999 FAKE_DB_SCHEMA=1903
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] || { fail deploy_refuses_min_schema_above_db "exit $rc"; return; }
  has "up -d --no-deps" && { fail deploy_refuses_min_schema_above_db "rolled"; return; }
  grep -q "^$NEW needs schema 1999, the database is at 1903" "$VC_STATE/held" ||
    { fail deploy_refuses_min_schema_above_db "not held: $(cat "$VC_STATE/held" 2>/dev/null)"; return; }
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] ||
    { fail deploy_refuses_min_schema_above_db "image.env moved"; return; }
  # once the database is there, the same image deploys
  sed -i.bak "/^$NEW /d" "$VC_STATE/held"
  : >"$CALLS"
  export FAKE_DB_SCHEMA=1999
  bash "$DEPLOY" 2>/dev/null
  rc=$?
  [[ $rc == 0 && "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$NEW" ]] &&
    ok deploy_refuses_min_schema_above_db || fail deploy_refuses_min_schema_above_db "not deployed (rc $rc)"
}

test_rollback_checks_the_database_schema() {
  setup
  running_on "$NEW"
  echo "$IMG@$OLD" >"$VC_STATE/previous"
  # the recorded schema says yes, the database says no: the database wins
  echo 1300 >"$VC_STATE/schema"
  export FAKE_SIGNED="$OLD" FAKE_LABEL_MIN_SCHEMA=1206 FAKE_DB_SCHEMA=1205
  bash "$DEPLOY" --rollback 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] && ! has "up -d --no-deps" ||
    { fail rollback_checks_the_database_schema "rolled to a too-new image (rc $rc)"; return; }
  # an allowed rollback keeps the higher recorded schema (the database never goes back)
  : >"$CALLS"
  export FAKE_DB_SCHEMA=1300 FAKE_LABEL_SCHEMA=1206
  bash "$DEPLOY" --rollback 2>/dev/null
  rc=$?
  [[ $rc == 0 ]] || { fail rollback_checks_the_database_schema "rollback failed (rc $rc)"; return; }
  [[ "$(cat "$VC_STATE/schema")" == 1300 ]] && ok rollback_checks_the_database_schema ||
    fail rollback_checks_the_database_schema "schema went back to $(cat "$VC_STATE/schema")"
}

test_skips_when_locked() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_LOCKED=1
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 0 ]] && ! has cosign && ok skips_when_locked || fail skips_when_locked "rc $rc"
}

test_watchdog_restarts_then_reboots() {
  setup
  running_on "$OLD"
  export FAKE_PS_A=$'postgres running healthy\nfetcher running unhealthy\napi running healthy\n'
  export VC_REBOOT_CMD="$HERE/fakes/reboot" VC_NOW=100000
  local i
  for i in 1 2 3; do
    VC_NOW=$((100000 + i * 120)) bash "$WATCHDOG" 2>/dev/null
  done
  [[ "$(grep -c "restart fetcher" "$CALLS")" == 3 ]] || { fail watchdog_restarts_then_reboots "restarts: $(grep -c "restart fetcher" "$CALLS")"; return; }
  has REBOOT && { fail watchdog_restarts_then_reboots "rebooted too early"; return; }
  VC_NOW=100480 bash "$WATCHDOG" 2>/dev/null
  has REBOOT || { fail watchdog_restarts_then_reboots "no reboot after 3 failed restarts"; return; }
  # within the reboot gap: restart only, never a second reboot
  : >"$CALLS"
  VC_NOW=100600 bash "$WATCHDOG" 2>/dev/null
  has REBOOT && { fail watchdog_restarts_then_reboots "reboot loop"; return; }
  # healthy → nothing
  : >"$CALLS"
  FAKE_PS_A=$'postgres running healthy\nfetcher running healthy\n' VC_NOW=200000 bash "$WATCHDOG" 2>/dev/null
  has restart && { fail watchdog_restarts_then_reboots "restarted a healthy stack"; return; }
  ok watchdog_restarts_then_reboots
}

test_watchdog_idle_while_deploying() {
  setup
  running_on "$OLD"
  export FAKE_PS_A=$'fetcher running unhealthy\n' FAKE_LOCKED=1
  bash "$WATCHDOG" 2>/dev/null
  has restart && fail watchdog_idle_while_deploying "acted during a deploy" || ok watchdog_idle_while_deploying
}

test_compose_wiring() {
  local compose="$HERE/../compose.yaml" otel="$HERE/../otelcol/config.yaml" role roles base
  # Chromium starts through deploy/chromium/run.sh (the script vc-render's tests use)
  grep -qF 'entrypoint: ["/bin/sh", "/opt/voyager-crawler/chromium/run.sh"]' "$compose" ||
    { fail compose_wiring "chromium entrypoint is not run.sh"; return; }
  grep -qF '/opt/voyager-crawler/chromium/run.sh:/opt/voyager-crawler/chromium/run.sh:ro' "$compose" ||
    { fail compose_wiring "run.sh not mounted read-only"; return; }
  # otelcol scrapes every role's :9464 and writes custom.googleapis.com/crawler/*
  roles="$(env -i bash -c ". '$HERE/../host/lib.sh'; echo \$VC_ROLES")"
  base="$(env -i bash -c ". '$HERE/../host/lib.sh'; echo \$VC_BASE_SERVICES")"
  [[ -n "$roles" ]] || { fail compose_wiring "no roles from lib.sh"; return; }
  for role in $roles; do
    grep -qE "job_name: $role\$" "$otel" || { fail compose_wiring "otelcol does not scrape $role"; return; }
    grep -qE "\"($role|[0-9.]+):9464\"" "$otel" || { fail compose_wiring "no :9464 target for $role"; return; }
  done
  # shellcheck disable=SC2016 # the literal collector expression
  grep -qF 'new_name: custom.googleapis.com/crawler/$${1}' "$otel" ||
    { fail compose_wiring "otelcol does not rename to custom.googleapis.com/crawler/"; return; }
  grep -qF 'exporters: [googlecloud]' "$otel" || { fail compose_wiring "otelcol exporter"; return; }
  [[ " $base " == *" otelcol "* ]] || { fail compose_wiring "otelcol is not a base service"; return; }
  # migrate --contract reads <data_dir>/deploy/rollbacks: mounted read-only, never auto-created
  local mig
  mig="$(awk '/^  migrate:/{on=1; next} on && /^  [a-z]/{on=0} on' "$compose")"
  grep -qF 'source: /var/lib/voyager-crawler/deploy/rollbacks' <<<"$mig" &&
    grep -qF 'target: /var/lib/voyager-crawler/deploy/rollbacks' <<<"$mig" &&
    grep -qF 'read_only: true' <<<"$mig" &&
    grep -qF 'create_host_path: false' <<<"$mig" ||
    { fail compose_wiring "rollback log not mounted read-only into migrate"; return; }
  ok compose_wiring
}

# The body of one top-level service of compose.yaml.
compose_service() {
  awk -v s="  $2:" '$0 == s {on=1; next} on && /^  [a-z]/{on=0} on && /^[a-z]/{on=0} on' "$1"
}

test_spooling_roles_have_stable_instance() {
  local compose="$HERE/../compose.yaml" svc body n=0
  # Every service that spools must keep its spool dir (spool/<role>-<instance>/) across
  # container recreation: a stable VC_VCRAWL_INSTANCE (or hostname), never the container id.
  while read -r svc; do
    body="$(compose_service "$compose" "$svc")"
    grep -qE 'VC_STORE_SPOOL_DIR|:/spool' <<<"$body" || continue
    n=$((n + 1))
    grep -qE '^      VC_VCRAWL_INSTANCE: [a-z][a-z0-9-]+$|^    hostname: [a-z][a-z0-9-]+$' <<<"$body" ||
      { fail spooling_roles_have_stable_instance "$svc spools without a stable instance name"; return; }
  done < <(awk '/^services:/{on=1; next} on && /^[a-z]/{on=0} on && /^  [a-z][a-z0-9_-]*:$/{sub(":","",$1); print $1}' "$compose")
  ((n > 0)) || { fail spooling_roles_have_stable_instance "no spooling service found"; return; }
  ok spooling_roles_have_stable_instance
}

test_chromium_outside_interception_off() {
  local run="$HERE/../chromium/run.sh" f
  # Traffic Chromium could make outside Fetch interception: DNS prefetch, preconnect /
  # network prediction, pings, background services.
  for f in --dns-prefetch-disable '--disable-features=' NetworkPrediction --no-pings \
    --disable-background-networking --disable-domain-reliability --no-proxy-server; do
    grep -qF -- "$f" "$run" || { fail chromium_outside_interception_off "run.sh lacks $f"; return; }
  done
  ok chromium_outside_interception_off
}

test_fetcher_metrics_not_on_render_network() {
  local compose="$HERE/../compose.yaml" otel="$HERE/../otelcol/config.yaml" fetcher chromium addr ip
  fetcher="$(compose_service "$compose" fetcher)"
  chromium="$(compose_service "$compose" chromium)"
  # The fetcher shares the render network with Chromium: its /metrics must not listen there.
  addr="$(sed -n 's/^      VC_OBS_METRICS_ADDR: *"\{0,1\}\([0-9.]*:9464\)"\{0,1\}$/\1/p' <<<"$fetcher")"
  [[ -n "$addr" && "$addr" != 0.0.0.0:* ]] ||
    { fail fetcher_metrics_not_on_render_network "fetcher metrics bound to all interfaces"; return; }
  ip="${addr%:9464}"
  grep -qE "ipv4_address: $ip( |\$)" <<<"$fetcher" ||
    { fail fetcher_metrics_not_on_render_network "metrics address $ip is not the fetcher's pinned address"; return; }
  grep -qF "\"$addr\"" "$otel" ||
    { fail fetcher_metrics_not_on_render_network "otelcol does not scrape $addr"; return; }
  grep -qF 'metrics' <<<"$(grep -E '^    networks:' <<<"$chromium")" &&
    { fail fetcher_metrics_not_on_render_network "chromium joins the metrics network"; return; }
  ok fetcher_metrics_not_on_render_network
}

for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
  "$t"
done
echo "$PASSES passed, $FAILS failed"
((FAILS == 0))
