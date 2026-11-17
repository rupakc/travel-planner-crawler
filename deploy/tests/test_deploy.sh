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
  export VC_RETRY_SLEEP=0 VC_DATA="$T/data"
  unset VC_ROLES FAKE_PROD_DIGEST FAKE_RUNNING FAKE_UNHEALTHY_DIGEST FAKE_MIGRATE_RC FAKE_SIGNED \
    FAKE_LABEL_SCHEMA FAKE_LABEL_MIN_SCHEMA FAKE_PS_A FAKE_LOCKED VC_NOW FAKE_DB_SCHEMA \
    FAKE_COSIGN_TRANSIENT FAKE_LABEL_MIGRATIONS FAKE_DB_MIGRATIONS FAKE_MIGRATE_FAILS FAKE_BEAT_RC FAKE_DRILL_OUT FAKE_DRILL_RC VC_DRILL_NO_CHOWN
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
  for s in postgres chromium otelcol ${VC_ROLES:-vcrawl}; do
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
  r="$(grep -nF "up -d --no-deps vcrawl" "$CALLS" | head -n1 | cut -d: -f1)"
  [[ -n "$v" && -n "$p" && -n "$m" && -n "$r" ]] || { fail migrate_runs_before_roll "missing step (v=$v p=$p m=$m r=$r)"; return; }
  ((v < p && p < m && m < r)) || { fail migrate_runs_before_roll "order verify=$v pull=$p migrate=$m roll=$r"; return; }
  grep -F "run --rm migrate" "$CALLS" | grep -qF "VC_IMAGE=$IMG@$NEW" ||
    { fail migrate_runs_before_roll "migrate did not use the new image"; return; }
  local order
  order="$(grep -oE 'up -d --no-deps [a-z]+' "$CALLS" | awk '{print $4}' | tr '\n' ' ')"
  [[ "$order" == "vcrawl " ]] ||
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
  [[ "$(grep -c 'run --rm migrate' "$CALLS")" == 3 ]] || { fail migrate_failure_rolls_nothing "migrate tries: $(grep -c 'run --rm migrate' "$CALLS")"; return; }
  # a failed migrate is retried next tick, never held
  [[ ! -s "$VC_STATE/held" ]] && ok migrate_failure_rolls_nothing ||
    fail migrate_failure_rolls_nothing "held: $(cat "$VC_STATE/held")"
}

# Go-live review 8: a transient migrate failure is retried (in the tick, then
# next tick) and the digest deploys once it passes; it is never held
test_migrate_transient_failure_is_retried() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_MIGRATE_FAILS=4
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 1 && ! -s "$VC_STATE/held" ]] || { fail migrate_transient_failure_is_retried "tick 1: rc $rc, held '$(cat "$VC_STATE/held" 2>/dev/null)'"; return; }
  bash "$DEPLOY" 2>/dev/null
  rc=$?
  [[ $rc == 0 && "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$NEW" ]] &&
    ok migrate_transient_failure_is_retried || fail migrate_transient_failure_is_retried "tick 2 rc $rc"
}

# Go-live review 8: only cosign's "no matching signatures" holds a digest; a
# registry/network failure is retried and never held
test_cosign_transient_failure_is_retried() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_COSIGN_TRANSIENT=5
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] || { fail cosign_transient_failure_is_retried "tick 1 exit $rc, want 1"; return; }
  [[ "$(grep -c 'cosign verify' "$CALLS")" == 3 ]] || { fail cosign_transient_failure_is_retried "tries: $(grep -c 'cosign verify' "$CALLS")"; return; }
  [[ ! -s "$VC_STATE/held" ]] || { fail cosign_transient_failure_is_retried "held: $(cat "$VC_STATE/held")"; return; }
  has pull && { fail cosign_transient_failure_is_retried "pulled an unverified image"; return; }
  # next tick: two more transient failures, then verified → deployed
  bash "$DEPLOY" 2>/dev/null
  rc=$?
  [[ $rc == 0 && "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$NEW" ]] &&
    ok cosign_transient_failure_is_retried || fail cosign_transient_failure_is_retried "tick 2 rc $rc"
}

# Go-live review 8: every tick reports running vs prod (+ since when they
# differ, kept across ticks) and the API certificate's expiry
test_deploy_reports_drift_and_tls() {
  setup
  running_on "$OLD"
  printf '[req]\ndistinguished_name = dn\nprompt = no\n[dn]\nCN = t\n' >"$T/ssl.cnf"
  openssl req -x509 -config "$T/ssl.cnf" -newkey rsa:2048 -nodes -days 400 \
    -keyout "$T/k.pem" -out "$VC_SECRETS_DIR/api_tls_cert" >/dev/null 2>&1 ||
    { fail deploy_reports_drift_and_tls "openssl"; return; }
  local end
  end="$(openssl x509 -enddate -noout -in "$VC_SECRETS_DIR/api_tls_cert" | sed 's/^notAfter=//')"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_COSIGN_TRANSIENT=99
  VC_NOW=1000 bash "$DEPLOY" 2>/dev/null
  VC_NOW=5000 bash "$DEPLOY" 2>/dev/null
  local want="beat deploy {\"running\": \"$OLD\", \"prod\": \"$NEW\", \"drift_since\": \"1970-01-01T00:16:40Z\", \"tls_not_after\": \"$end\"}"
  [[ "$(grep -cxF -- "$want" "$CALLS")" == 2 ]] ||
    { fail deploy_reports_drift_and_tls "drift beats: $(grep '^beat deploy' "$CALLS")"; return; }
  # deployed: no drift any more
  : >"$CALLS"
  export FAKE_COSIGN_TRANSIENT=0
  VC_NOW=6000 bash "$DEPLOY" 2>/dev/null || { fail deploy_reports_drift_and_tls "deploy failed"; return; }
  grep -qxF -- "beat deploy {\"running\": \"$NEW\", \"prod\": \"$NEW\", \"drift_since\": null, \"tls_not_after\": \"$end\"}" "$CALLS" ||
    { fail deploy_reports_drift_and_tls "no clean beat after deploy: $(grep '^beat' "$CALLS")"; return; }
  [[ ! -e "$VC_STATE/drift-since" ]] || { fail deploy_reports_drift_and_tls "drift marker kept"; return; }
  # Postgres down: the deploy itself is unaffected
  : >"$CALLS"
  FAKE_BEAT_RC=1 bash "$DEPLOY" 2>/dev/null && ok deploy_reports_drift_and_tls ||
    fail deploy_reports_drift_and_tls "a failed heartbeat failed the tick"
}

test_rollback_on_unhealthy() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_UNHEALTHY_DIGEST="$NEW"
  bash "$DEPLOY" 2>"$T/err"
  local rc=$?
  [[ $rc == 1 ]] || { fail rollback_on_unhealthy "exit $rc"; return; }
  # vcrawl (every role) failed: it goes back to OLD
  has "compose VC_IMAGE=$IMG@$OLD -p voyager-crawler -f $VC_OPT/compose.yaml --env-file $VC_STATE/host.env --env-file $VC_STATE/image.env up -d --no-deps vcrawl" ||
    { fail rollback_on_unhealthy "vcrawl not rolled back to the old digest"; return; }
  [[ "$(cat "$FAKE_STATE/vcrawl")" == "$IMG@$OLD" ]] || { fail rollback_on_unhealthy "vcrawl runs $(cat "$FAKE_STATE/vcrawl")"; return; }
  [[ "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] || { fail rollback_on_unhealthy "image.env moved"; return; }
  grep -q "^$NEW unhealthy after roll (vcrawl)" "$VC_STATE/held" || { fail rollback_on_unhealthy "not held"; return; }
  # held: the next tick neither verifies nor rolls
  : >"$CALLS"
  bash "$DEPLOY" 2>/dev/null
  { has cosign || has "up -d --no-deps"; } && { fail rollback_on_unhealthy "retried a held digest"; return; }
  ok rollback_on_unhealthy
}

# deploy.sh still rolls a list of services in order (VC_ROLES; one on the small VM): a late
# one turning unhealthy rolls every service back
test_rollback_on_unhealthy_late_role() {
  setup
  export VC_ROLES="maintenance scheduler processor embedder fetcher api"
  export FAKE_RUNNING="postgres chromium otelcol $VC_ROLES"
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
  unset VC_ROLES FAKE_RUNNING
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
  FAKE_RUNNING="postgres chromium otelcol" bash "$DEPLOY" 2>/dev/null
  { has cosign || has pull; } && { fail idempotent_when_digest_unchanged "pulled/verified to restart"; return; }
  has "up -d --no-recreate vcrawl" || { fail idempotent_when_digest_unchanged "stopped vcrawl not started"; return; }
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

# Owner blocks merge out of order: 1102 landed after 1906. The highest applied
# migration equals the image's min-schema, yet the database lacks 1102: the
# image's full migration list decides (fails on the highest-number gate).
test_deploy_refuses_out_of_order_gap() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_LABEL_SCHEMA=1906 \
    FAKE_LABEL_MIN_SCHEMA=1906 FAKE_LABEL_MIGRATIONS=1,2,1102,1903,1906 \
    FAKE_DB_SCHEMA=1906 FAKE_DB_MIGRATIONS=1,2,1903,1906
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] || { fail deploy_refuses_out_of_order_gap "exit $rc"; return; }
  has "up -d --no-deps" && { fail deploy_refuses_out_of_order_gap "rolled"; return; }
  grep -q "^$NEW needs migrations 1102, missing in the database" "$VC_STATE/held" ||
    { fail deploy_refuses_out_of_order_gap "not held: $(cat "$VC_STATE/held" 2>/dev/null)"; return; }
  # once the database has it, the same image deploys
  sed -i.bak "/^$NEW /d" "$VC_STATE/held"
  : >"$CALLS"
  export FAKE_DB_MIGRATIONS=1,2,1102,1903,1906,1907
  bash "$DEPLOY" 2>/dev/null
  rc=$?
  [[ $rc == 0 && "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$NEW" ]] &&
    ok deploy_refuses_out_of_order_gap || fail deploy_refuses_out_of_order_gap "not deployed (rc $rc)"
}

test_rollback_refuses_out_of_order_gap() {
  setup
  running_on "$NEW"
  echo "$IMG@$OLD" >"$VC_STATE/previous"
  export FAKE_SIGNED="$OLD" FAKE_LABEL_MIN_SCHEMA=1903 FAKE_LABEL_MIGRATIONS=1,1401,1903 \
    FAKE_DB_SCHEMA=1906 FAKE_DB_MIGRATIONS=1,1903,1906
  bash "$DEPLOY" --rollback 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] && ! has "up -d --no-deps" ||
    { fail rollback_refuses_out_of_order_gap "rolled to an image whose migration is missing (rc $rc)"; return; }
  : >"$CALLS"
  export FAKE_DB_MIGRATIONS=1,1401,1903,1906
  bash "$DEPLOY" --rollback 2>/dev/null
  rc=$?
  [[ $rc == 0 && "$(cat "$VC_STATE/image.env")" == "VC_IMAGE=$IMG@$OLD" ]] &&
    ok rollback_refuses_out_of_order_gap || fail rollback_refuses_out_of_order_gap "rollback failed (rc $rc)"
}

# The migration list cannot be read: the highest-number gate still applies.
test_unreadable_migration_list_falls_back_to_min_schema() {
  setup
  running_on "$OLD"
  export FAKE_PROD_DIGEST="$NEW" FAKE_SIGNED="$NEW" FAKE_LABEL_SCHEMA=1999 \
    FAKE_LABEL_MIN_SCHEMA=1999 FAKE_LABEL_MIGRATIONS=1,1999 FAKE_DB_SCHEMA=1903
  bash "$DEPLOY" 2>/dev/null
  local rc=$?
  [[ $rc == 1 ]] && grep -q "^$NEW needs schema 1999, the database is at 1903" "$VC_STATE/held" &&
    ok unreadable_migration_list_falls_back_to_min_schema ||
    fail unreadable_migration_list_falls_back_to_min_schema "rc $rc, held: $(cat "$VC_STATE/held" 2>/dev/null)"
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
  export FAKE_PS_A=$'postgres running healthy\nvcrawl running unhealthy\n'
  export VC_REBOOT_CMD="$HERE/fakes/reboot" VC_NOW=100000
  local i
  for i in 1 2 3; do
    VC_NOW=$((100000 + i * 120)) bash "$WATCHDOG" 2>/dev/null
  done
  [[ "$(grep -c "restart vcrawl" "$CALLS")" == 3 ]] || { fail watchdog_restarts_then_reboots "restarts: $(grep -c "restart vcrawl" "$CALLS")"; return; }
  has REBOOT && { fail watchdog_restarts_then_reboots "rebooted too early"; return; }
  VC_NOW=100480 bash "$WATCHDOG" 2>/dev/null
  has REBOOT || { fail watchdog_restarts_then_reboots "no reboot after 3 failed restarts"; return; }
  # within the reboot gap: restart only, never a second reboot
  : >"$CALLS"
  VC_NOW=100600 bash "$WATCHDOG" 2>/dev/null
  has REBOOT && { fail watchdog_restarts_then_reboots "reboot loop"; return; }
  # healthy → nothing
  : >"$CALLS"
  FAKE_PS_A=$'postgres running healthy\nvcrawl running healthy\n' VC_NOW=200000 bash "$WATCHDOG" 2>/dev/null
  has restart && { fail watchdog_restarts_then_reboots "restarted a healthy stack"; return; }
  ok watchdog_restarts_then_reboots
}

test_watchdog_idle_while_deploying() {
  setup
  running_on "$OLD"
  export FAKE_PS_A=$'vcrawl running unhealthy\n' FAKE_LOCKED=1
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
  # The fetcher runs inside vcrawl (`vcrawl run`)
  fetcher="$(compose_service "$compose" vcrawl)"
  chromium="$(compose_service "$compose" chromium)"
  # vcrawl shares the render network with Chromium: its /metrics must not listen there.
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

# The networks a compose service joins: its own `networks:` (flow or block form), else the
# role anchor's for services merging `<<: *role`.
compose_networks() {
  local compose="$1" body="$2" flow
  flow="$(sed -n 's/^    networks: \[\(.*\)\]$/\1/p' <<<"$body")"
  if [[ -n "$flow" ]]; then
    tr -d ' ' <<<"$flow" | tr ',' '\n'
  elif grep -qE '^    networks:$' <<<"$body"; then
    awk '/^    networks:$/{on=1; next} on && /^    [a-z]/{on=0} on && /^      [a-z][a-z0-9_-]*:/{sub(":","",$1); print $1}' <<<"$body"
  elif grep -qxF '    <<: *role' <<<"$body"; then
    awk '/^x-role:/{on=1; next} on && /^[a-z]/{on=0} on' "$compose" |
      sed -n 's/^  networks: \[\(.*\)\]$/\1/p' | tr -d ' ' | tr ',' '\n'
  fi
}

# Every job otelcol scrapes has a listener: the scraped service (job = role = service) binds
# its /metrics (`VC_OBS_METRICS_ADDR`, else vc-config's default obs.metrics_addr) on the
# scraped port, on all interfaces for a service-name target or on its pinned address for an
# IP target, and shares a network with otelcol. Every role is scraped.
test_otelcol_targets_have_listeners() {
  local compose="$HERE/../compose.yaml" otel="$HERE/../otelcol/config.yaml"
  local obs="$HERE/../../crates/vc-config/src/sections/obs.rs"
  local default job target host port body addr otel_nets nets ip n=0 roles role
  default="$(sed -n 's/^ *metrics_addr: "\([^"]*\)"\.into(),$/\1/p' "$obs")"
  [[ -n "$default" ]] || { fail otelcol_targets_have_listeners "no default obs.metrics_addr in $obs"; return; }
  otel_nets="$(compose_networks "$compose" "$(compose_service "$compose" otelcol)")"
  while read -r job target; do
    n=$((n + 1))
    host="${target%:*}" port="${target##*:}"
    body="$(compose_service "$compose" "$job")"
    [[ -n "$body" ]] || { fail otelcol_targets_have_listeners "job $job: no such compose service"; return; }
    grep -qE "command: \[\"($job|run)\"" <<<"$body" ||
      { fail otelcol_targets_have_listeners "job $job: service runs neither role $job nor vcrawl run"; return; }
    addr="$(sed -n 's/^      VC_OBS_METRICS_ADDR: *"\{0,1\}\([^"]*\)"\{0,1\}$/\1/p' <<<"$body")"
    addr="${addr:-$default}"
    [[ "${addr##*:}" == "$port" ]] ||
      { fail otelcol_targets_have_listeners "job $job scrapes :$port, $job listens on $addr"; return; }
    nets="$(compose_networks "$compose" "$body")"
    if [[ "$host" =~ ^[0-9.]+$ ]]; then
      [[ "${addr%:*}" == "$host" ]] ||
        { fail otelcol_targets_have_listeners "job $job scrapes $target, $job listens on $addr"; return; }
      ip="$(grep -E "ipv4_address: $host( |\$|\})" <<<"$body")"
      [[ -n "$ip" ]] || { fail otelcol_targets_have_listeners "$host is not $job's pinned address"; return; }
    else
      [[ "$host" == "$job" ]] ||
        { fail otelcol_targets_have_listeners "job $job scrapes host $host"; return; }
      [[ "${addr%:*}" == 0.0.0.0 ]] ||
        { fail otelcol_targets_have_listeners "job $job scrapes $host by name, $job listens on $addr only"; return; }
    fi
    comm -12 <(sort -u <<<"$nets") <(sort -u <<<"$otel_nets") | grep -q . ||
      { fail otelcol_targets_have_listeners "$job shares no network with otelcol"; return; }
  done < <(awk '/job_name:/{job=$3} /targets:/{match($0, /"[^"]+"/); print job, substr($0, RSTART+1, RLENGTH-2)}' "$otel")
  ((n > 0)) || { fail otelcol_targets_have_listeners "no scrape jobs"; return; }
  roles="$(env -i bash -c ". '$HERE/../host/lib.sh'; echo \$VC_ROLES")"
  for role in $roles; do
    grep -qE "job_name: $role\$" "$otel" || { fail otelcol_targets_have_listeners "role $role not scraped"; return; }
  done
  ok otelcol_targets_have_listeners
}

# wal-g must not overwrite archived WAL: a retried archive_command of an
# already uploaded segment exits 0 instead of wedging archiving
test_walg_prevents_wal_overwrite() {
  local env="$HERE/../postgres/walg.env"
  grep -qx 'WALG_PREVENT_WAL_OVERWRITE=true' "$env" && ok walg_prevents_wal_overwrite ||
    fail walg_prevents_wal_overwrite "WALG_PREVENT_WAL_OVERWRITE=true missing in walg.env"
}

# docker.service waits for the data disk (first-boot.sh's drop-in), once
test_docker_waits_for_data_disk() {
  setup
  mkdir -p "$T/bin" "$T/systemd"
  printf '#!/usr/bin/env bash\necho "systemctl $*" >>"%s"\n' "$CALLS" >"$T/bin/systemctl"
  chmod +x "$T/bin/systemctl"
  local conf="$T/systemd/docker.service.d/10-voyager-crawler-data.conf"
  PATH="$T/bin:$PATH" VC_SYSTEMD_DIR="$T/systemd" VC_FIRST_BOOT_FUNCTIONS_ONLY=1 bash -c \
    ". '$HERE/../host/first-boot.sh'; docker_waits_for_data_disk; docker_waits_for_data_disk" ||
    { fail docker_waits_for_data_disk "function failed"; return; }
  grep -qx 'RequiresMountsFor=/var/lib/voyager-crawler' "$conf" 2>/dev/null && grep -qx '\[Unit\]' "$conf" ||
    { fail docker_waits_for_data_disk "drop-in missing or wrong: $(cat "$conf" 2>/dev/null)"; return; }
  [[ "$(grep -c 'systemctl daemon-reload' "$CALLS")" == 1 ]] ||
    { fail docker_waits_for_data_disk "daemon-reload count $(grep -c 'systemctl daemon-reload' "$CALLS")"; return; }
  # main installs it right after mounting the disk
  awk '/^main\(\)/{on=1} on' "$HERE/../host/first-boot.sh" | grep -A1 '^  mount_data_disk$' |
    grep -q '^  docker_waits_for_data_disk$' && ok docker_waits_for_data_disk ||
    fail docker_waits_for_data_disk "not called after mount_data_disk"
}

# Go-live ramp: a fresh install writes the ramp-1 allowlist into
# host.local.env once (host.env picks it up, compose hands it to every role);
# a deleted line stays deleted, and a disk with a database is never ramped
test_ramp_default_on_fresh_install() {
  setup
  local fb="$HERE/../host/first-boot.sh" compose="$HERE/../compose.yaml" d="$T/data/deploy" path
  path="$(sed -n 's/^RAMP_ALLOWLIST=//p' "$fb")"
  [[ "$path" == /app/sources/* && -f "$HERE/../../sources/${path#/app/sources/}" ]] ||
    { fail ramp_default_on_fresh_install "RAMP_ALLOWLIST '$path' is not a file in sources/"; return; }
  grep -qx 'COPY sources/ /app/sources/' "$HERE/../../Dockerfile" ||
    { fail ramp_default_on_fresh_install "the image does not ship sources/ at /app/sources"; return; }
  grep -qE '^    VC_VCRAWL_SOURCE_ALLOWLIST: \$\{VC_VCRAWL_SOURCE_ALLOWLIST:-\}$' "$compose" ||
    { fail ramp_default_on_fresh_install "compose does not pass VC_VCRAWL_SOURCE_ALLOWLIST to the roles"; return; }
  run_ramp() { VC_FIRST_BOOT_FUNCTIONS_ONLY=1 bash -c ". '$fb'; logger() { :; }; ramp_default '$T/data'" 2>/dev/null; }
  run_ramp && run_ramp || { fail ramp_default_on_fresh_install "ramp_default failed"; return; }
  [[ "$(grep -c "^VC_VCRAWL_SOURCE_ALLOWLIST=$path\$" "$d/host.local.env")" == 1 ]] ||
    { fail ramp_default_on_fresh_install "fresh install: $(cat "$d/host.local.env" 2>&1)"; return; }
  # the operator lifts the ramp: the next boot does not put it back
  sed -i.bak '/^VC_VCRAWL_SOURCE_ALLOWLIST=/d' "$d/host.local.env"
  run_ramp
  grep -q '^VC_VCRAWL_SOURCE_ALLOWLIST=' "$d/host.local.env" &&
    { fail ramp_default_on_fresh_install "ramp re-added after the operator removed it"; return; }
  # an existing database (no marker yet): left alone
  rm -rf "$T/data" && mkdir -p "$T/data/pgdata" && echo 17 >"$T/data/pgdata/PG_VERSION"
  run_ramp
  grep -qs '^VC_VCRAWL_SOURCE_ALLOWLIST=' "$d/host.local.env" &&
    { fail ramp_default_on_fresh_install "an existing install was put on the ramp"; return; }
  [[ -e "$d/.ramp-default" ]] && ok ramp_default_on_fresh_install ||
    fail ramp_default_on_fresh_install "no marker on an existing install"
}

# Go-live review 4: the drill unit runs restore-drill.sh (the image's
# maintenance subcommand takes no arguments); every crawler unit reports
# success and failure for the alert metrics
test_units_report_outcomes() {
  local d="$HERE/../host/systemd" u
  grep -qx 'ExecStart=/opt/voyager-crawler/host/restore-drill.sh' "$d/voyager-crawler-restore-drill.service" ||
    { fail units_report_outcomes "restore-drill unit does not run restore-drill.sh"; return; }
  grep -q 'restore-drill --light' "$d"/*.service && { fail units_report_outcomes "unit still calls maintenance restore-drill --light"; return; }
  for u in "$d"/voyager-crawler*.service; do
    [[ "$u" == *failed@* ]] && continue
    grep -qx 'OnFailure=voyager-crawler-failed@%n.service' "$u" ||
      { fail units_report_outcomes "$(basename "$u") has no OnFailure="; return; }
    grep -qx 'ExecStartPost=-/opt/voyager-crawler/host/unit-status.sh ok %n' "$u" ||
      { fail units_report_outcomes "$(basename "$u") does not record success"; return; }
  done
  grep -qx 'ExecStart=/opt/voyager-crawler/host/unit-status.sh failed %i' "$d/voyager-crawler-failed@.service" &&
    ok units_report_outcomes || fail units_report_outcomes "failure unit"
}

test_unit_status_records_outcomes() {
  setup
  running_on "$OLD"
  local us="$HERE/../host/unit-status.sh"
  VC_NOW=86400 bash "$us" ok voyager-crawler-basebackup.service 2>/dev/null
  VC_NOW=90000 bash "$us" failed voyager-crawler-restore-drill.service 2>/dev/null
  grep -qxF 'beat unit:voyager-crawler-basebackup.service {"failed": false, "last_ok_at": "1970-01-02T00:00:00Z"}' "$CALLS" &&
    grep -qxF 'beat unit:voyager-crawler-restore-drill.service {"failed": true, "failed_at": "1970-01-02T01:00:00Z"}' "$CALLS" ||
    { fail unit_status_records_outcomes "$(grep '^beat' "$CALLS")"; return; }
  # never fails the unit, never records a foreign name
  : >"$CALLS"
  FAKE_BEAT_RC=1 bash "$us" ok voyager-crawler-deploy.service 2>/dev/null || { fail unit_status_records_outcomes "exit on a DB error"; return; }
  bash "$us" ok "x'; DROP TABLE heartbeats; --" 2>/dev/null || { fail unit_status_records_outcomes "exit on a bad name"; return; }
  grep -q "DROP" "$CALLS" && { fail unit_status_records_outcomes "recorded a foreign name"; return; }
  ok unit_status_records_outcomes
}

test_restore_drill() {
  local drill="$HERE/../host/restore-drill.sh"
  setup
  running_on "$OLD"
  mkdir -p "$VC_DATA/pgdata"
  echo x >"$VC_DATA/pgdata/PG_VERSION"
  export VC_DRILL_NO_CHOWN=1 FAKE_DRILL_OUT=$'restored\nDRILL_RESULT migrations=19 segments=4'
  bash "$drill" 2>"$T/err" || { fail restore_drill "ok drill exit $?: $(cat "$T/err")"; return; }
  local run
  run="$(grep '^run ' "$CALLS")"
  for want in "--network voyager-crawler_pgbackup" "--user 999:999" "$VC_DATA/restore-drill:/drill" \
    "$VC_OPT/postgres/walg.env" "$VC_OPT/host/restore-drill-inner.sh:/drill-inner.sh:ro" \
    "pgvector/pgvector:pg17@sha256:feed /drill-inner.sh"; do
    [[ "$run" == *"$want"* ]] || { fail restore_drill "container lacks '$want': $run"; return; }
  done
  [[ "$run" == *"$VC_DATA/pgdata"* || "$run" == *"--network voyager-crawler_db"* ]] &&
    { fail restore_drill "the drill container can reach the live database"; return; }
  [[ ! -e "$VC_DATA/restore-drill" ]] || { fail restore_drill "scratch left behind"; return; }
  # a failed restore, or a restore without migrations, fails the unit and cleans up
  FAKE_DRILL_RC=1 bash "$drill" 2>/dev/null && { fail restore_drill "failed restore exit 0"; return; }
  [[ ! -e "$VC_DATA/restore-drill" ]] || { fail restore_drill "scratch left after a failure"; return; }
  FAKE_DRILL_OUT="DRILL_RESULT migrations=0 segments=0" bash "$drill" 2>/dev/null &&
    { fail restore_drill "empty restore exit 0"; return; }
  # the inner script never archives and starts read-only
  local inner="$HERE/../host/restore-drill-inner.sh"
  grep -qF 'archive_mode=off' "$inner" && grep -qF 'standby.signal' "$inner" &&
    grep -qF 'default_transaction_read_only=on' "$inner" && grep -qF "wal-g backup-fetch \"\$D\" LATEST" "$inner" &&
    ok restore_drill || fail restore_drill "inner script settings"
}

# Go-live review 7: the Ops Agent drops per-process metrics, ships only the
# containers' logs (no syslog), DEBUG dropped; restarted only on a change
test_ops_agent_config() {
  setup
  mkdir -p "$T/bin" "$T/ops"
  printf '#!/usr/bin/env bash\necho "systemctl $*" >>"%s"\n' "$CALLS" >"$T/bin/systemctl"
  chmod +x "$T/bin/systemctl"
  PATH="$T/bin:$PATH" VC_OPS_AGENT_DIR="$T/ops" VC_FIRST_BOOT_FUNCTIONS_ONLY=1 bash -c \
    ". '$HERE/../host/first-boot.sh'; configure_ops_agent; configure_ops_agent" ||
    { fail ops_agent_config "function failed"; return; }
  local c="$T/ops/config.yaml" want
  for want in '        - agent.googleapis.com/processes/*' '        processors: [drop_process_metrics]' \
    '        - /var/lib/docker/containers/*/*-json.log' '        receivers: []' \
    '        - jsonPayload.level =~ "(?i)^(debug|trace)$"' \
    '        processors: [docker_envelope, role_json, drop_debug, severity]'; do
    grep -qxF -- "$want" "$c" || { fail ops_agent_config "config lacks: $want"; return; }
  done
  [[ "$(grep -c 'systemctl restart google-cloud-ops-agent' "$CALLS")" == 1 ]] ||
    { fail ops_agent_config "restarts: $(grep -c restart "$CALLS")"; return; }
  awk '/^main\(\)/{on=1} on' "$HERE/../host/first-boot.sh" | grep -q '^  configure_ops_agent$' &&
    ok ops_agent_config || fail ops_agent_config "main does not configure the Ops Agent"
}

for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
  "$t"
done
echo "$PASSES passed, $FAILS failed"
((FAILS == 0))
