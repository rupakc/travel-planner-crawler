#!/usr/bin/env bash
# Voyager Crawler signed-digest deployer (spec §8, §17.5, §17.6). Runs every
# 2 minutes from voyager-crawler-deploy.timer:
#
#   1. Resolve ghcr.io/rupakc/voyager-crawler:prod to a digest. Same as the
#      running one → make sure every service is up, and stop (no pull, no
#      cosign, no migrate). A held digest (failed before) is skipped too.
#   2. cosign verify the digest, pinned to the deploy workflow on main and
#      GitHub's OIDC issuer. Unsigned or wrongly signed ("no matching
#      signatures") → held, never pulled. Any other cosign failure (registry,
#      Rekor or network trouble) is retried, then the next tick tries again.
#   3. Pull by digest; make sure postgres + chromium run (pinned in compose).
#   4. `vcrawl migrate` with the NEW image (expand-only: the running image
#      keeps working on the new schema). Retried; still failing → nothing
#      rolled, not held: the next tick tries again (a migration is one
#      transaction, so a retry is safe), and the drift alert fires after 30 min.
#      Then the image's io.voyager-crawler.min-schema label must not be above
#      the database's schema (highest applied migration); else → held.
#   5. Roll the roles one at a time (maintenance … api); after each, wait
#      for its progress-based health check. Unhealthy → every rolled role
#      goes back to the previous digest, and the new one is held.
#   6. Success: image.env names the new digest; the old one becomes the
#      rollback target.
#
# Every tick also records a deploy heartbeat in Postgres (lib.sh host_beat):
# the running and the prod digest, since when they differ, and the installed
# API certificate's expiry. The maintenance role exports them as
# crawler_deploy_drift_seconds (alert: > 30 min) and crawler_tls_cert_days_left.
#
# Held digests are listed in $VC_STATE/held ("<digest> <reason>"); deleting
# a line releases it, and publishing a newer digest ends the hold.
#
#   deploy.sh             one pass (the timer)
#   deploy.sh --rollback  back to the previous digest (held: the current one);
#                         refused if its min-schema label is above the schema
#                         the database is at
#   deploy.sh --secrets   only refresh the secret files (boot, before compose)
#
# Exit: 0 done or nothing to do, 1 failure (retried next tick), 3 refused.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=./lib.sh
. "$(dirname "$(readlink -f "$0")")/lib.sh"
LOG_TAG=voyager-crawler-deploy

TAG="${VC_TAG:-prod}"
CERT_IDENTITY="${VC_CERT_IDENTITY:-https://github.com/rupakc/travel-planner-crawler/.github/workflows/deploy.yml@refs/heads/main}"
CERT_ISSUER="${VC_CERT_ISSUER:-https://token.actions.githubusercontent.com}"
HEALTH_TIMEOUT="${VC_HEALTH_TIMEOUT:-420}"
HEALTH_POLL="${VC_HEALTH_POLL:-5}"
SECRETS_MAX_AGE_MIN="${VC_SECRETS_MAX_AGE_MIN:-60}"
HELD="$VC_STATE/held"
DRIFT="$VC_STATE/drift-since"
RETRIES="${VC_RETRIES:-3}"
RETRY_SLEEP="${VC_RETRY_SLEEP:-15}"
MD="http://metadata.google.internal/computeMetadata/v1"

# ── Secrets: Secret Manager → tmpfs, owned by the uid that reads them ─────
# name in Secret Manager : file : owner uid
SECRET_MAP="crawler-pg-password:pg_password_db:999
crawler-pg-password:pg_password:65532
crawler-api-token-voyager:api_token_voyager:65532
crawler-api-tls-key:api_tls_key:65532
crawler-api-tls-cert:api_tls_cert:65532"

secrets_fresh() {
  local file
  [[ -n "$(find "$VC_STATE/secrets.stamp" -mmin "-$SECRETS_MAX_AGE_MIN" 2>/dev/null)" ]] || return 1
  while IFS=: read -r _ file _; do
    [[ -s "$VC_SECRETS_DIR/$file" ]] || return 1
  done <<<"$SECRET_MAP"
  return 0
}

# refresh_secrets — hourly (Secret Manager free tier: 10k accesses/month).
# Everything is fetched first; a TLS key that doesn't match the certificate
# (a rotation caught half-way) keeps the old pair. A changed pair recreates
# the api container (rotation, §17.5).
refresh_secrets() {
  secrets_fresh && return 0
  local project token name file uid changed=0 ok=1
  project="$(curl -fsS -H 'Metadata-Flavor: Google' "$MD/project/project-id")" ||
    { log "metadata server unreachable: secrets not refreshed"; return 1; }
  token="$(curl -fsS -H 'Metadata-Flavor: Google' "$MD/instance/service-accounts/default/token" | jq -r .access_token)" ||
    { log "no access token: secrets not refreshed"; return 1; }
  install -d -m 0711 "$VC_SECRETS_DIR"
  while IFS=: read -r name file uid; do
    if ! (umask 077 && curl -fsS -H "Authorization: Bearer $token" \
      "https://secretmanager.googleapis.com/v1/projects/$project/secrets/$name/versions/latest:access" |
      jq -er .payload.data | base64 -d >"$VC_SECRETS_DIR/.$file.new") || [[ ! -s "$VC_SECRETS_DIR/.$file.new" ]]; then
      rm -f "$VC_SECRETS_DIR/.$file.new"
      log "secret $name could not be read (no version yet?): keeping the old file"
      ok=0
      continue
    fi
    chown "$uid:$uid" "$VC_SECRETS_DIR/.$file.new"
    chmod 0400 "$VC_SECRETS_DIR/.$file.new"
  done <<<"$SECRET_MAP"

  local k="$VC_SECRETS_DIR/.api_tls_key.new" c="$VC_SECRETS_DIR/.api_tls_cert.new"
  if [[ -s "$k" && -s "$c" ]] &&
    [[ "$(openssl pkey -in "$k" -pubout 2>/dev/null)" != "$(openssl x509 -in "$c" -noout -pubkey 2>/dev/null)" ]]; then
    log "TLS key and certificate don't match (rotation in progress?): keeping the old pair"
    rm -f "$k" "$c"
    ok=0
  fi

  while IFS=: read -r _ file _; do
    [[ -f "$VC_SECRETS_DIR/.$file.new" ]] || continue
    if cmp -s "$VC_SECRETS_DIR/.$file.new" "$VC_SECRETS_DIR/$file"; then
      rm -f "$VC_SECRETS_DIR/.$file.new"
    else
      [[ "$file" == api_tls_* ]] && changed=1
      mv -f "$VC_SECRETS_DIR/.$file.new" "$VC_SECRETS_DIR/$file"
    fi
  done <<<"$SECRET_MAP"
  ((ok)) && touch "$VC_STATE/secrets.stamp"
  if ((changed)) && [[ -n "$(current_image)" ]] && [[ -n "$(vc_compose ps -q api 2>/dev/null)" ]]; then
    log "TLS certificate changed: recreating api"
    vc_compose up -d --no-deps --force-recreate api || log "api recreate failed"
  fi
  return 0
}

# ── Images ────────────────────────────────────────────────────────────────
resolve_digest() {
  local d
  d="$(docker buildx imagetools inspect "$VC_REPO_IMAGE:$TAG" --format '{{json .Manifest}}' 2>/dev/null |
    grep -oE 'sha256:[0-9a-f]{64}' | head -n1)" || true
  [[ "$d" =~ ^sha256:[0-9a-f]{64}$ ]] || return 1
  echo "$d"
}

held_reason() {
  [[ -f "$HELD" ]] || return 0
  awk -v d="$1" '$1 == d { $1 = ""; sub(/^ /, ""); print; exit }' "$HELD"
}
hold() {
  [[ -n "$(held_reason "$1")" ]] && return 0
  echo "$1 $2" >>"$HELD"
  log "HELD $1: $2 (delete its line in $HELD to release it)"
}

# verify REF → 0 signed by the pinned identity; 2 refused (cosign found no
# matching signature: hold it); 1 cosign failed for another reason RETRIES
# times (registry/Rekor/network: try again next tick, never hold)
verify() {
  local ref="$1" i out
  for ((i = 1; i <= RETRIES; i++)); do
    log "verifying $ref (attempt $i/$RETRIES)"
    if out="$(cosign verify --certificate-identity "$CERT_IDENTITY" --certificate-oidc-issuer "$CERT_ISSUER" \
      "$ref" 2>&1 >/dev/null)"; then
      return 0
    fi
    if grep -qiE 'no matching signatures|no signatures found' <<<"$out"; then
      log "signature refused: $(grep -iE 'no matching signatures|no signatures found' <<<"$out" | head -n1)"
      return 2
    fi
    log "cosign verify failed (not a signature verdict): $(tail -n1 <<<"$out")"
    ((i < RETRIES)) && sleep "$RETRY_SLEEP"
  done
  return 1
}

# migrate_with REF → 0 once `vcrawl migrate` of REF succeeded (RETRIES tries)
migrate_with() {
  local ref="$1" i
  for ((i = 1; i <= RETRIES; i++)); do
    VC_IMAGE="$ref" vc_compose run --rm migrate && return 0
    log "migrate with $ref failed (attempt $i/$RETRIES)"
    ((i < RETRIES)) && sleep "$RETRY_SLEEP"
  done
  return 1
}

# report PROD — the deploy heartbeat: running vs PROD digest, since when they
# differ ($DRIFT keeps the first moment across ticks), the API certificate's
# notAfter. Best effort.
report() {
  local prod="$1" running since="" tls="" drift=null tlsv=null
  running="$(current_image)"
  running="${running##*@}"
  if [[ -n "$prod" && "$prod" != "$running" ]]; then
    [[ -s "$DRIFT" ]] || now_epoch >"$DRIFT"
    since="$(head -n1 "$DRIFT")"
    [[ "$since" =~ ^[0-9]+$ ]] && drift="\"$(iso_utc "$since")\""
  else
    rm -f "$DRIFT"
  fi
  tls="$(openssl x509 -enddate -noout -in "$VC_SECRETS_DIR/api_tls_cert" 2>/dev/null | sed -n 's/^notAfter=//p')" || tls=""
  [[ "$tls" =~ ^[A-Za-z0-9:\ ]+$ ]] && tlsv="\"$tls\""
  host_beat deploy "{\"running\": \"$running\", \"prod\": \"$prod\", \"drift_since\": $drift, \"tls_not_after\": $tlsv}" || true
}

label() { docker image inspect -f "{{ index .Config.Labels \"$2\" }}" "$1" 2>/dev/null || true; }

# db_schema → the database's schema: the highest successfully applied expand
# migration (sqlx's _sqlx_migrations), else the last recorded one
db_schema() {
  local v
  v="$(vc_compose exec -T postgres psql -U vcrawl -d vcrawl -tAc \
    "SELECT coalesce(max(version), 0) FROM _sqlx_migrations WHERE success" 2>/dev/null |
    tr -d '[:space:]')" || v=""
  [[ "$v" =~ ^[0-9]+$ ]] || v="$(cat "$VC_STATE/schema" 2>/dev/null || true)"
  echo "$v"
}

# schema_too_new IMAGE → 0 (and a reason on stdout) when IMAGE's min-schema
# label is above the database's schema; images without the label pass
schema_too_new() {
  local min db
  min="$(label "$1" io.voyager-crawler.min-schema)"
  db="$(db_schema)"
  if [[ "$min" =~ ^[0-9]+$ && "$db" =~ ^[0-9]+$ ]] && ((10#$min > 10#$db)); then
    echo "needs schema $min, the database is at $db"
    return 0
  fi
  return 1
}

# ── Services ──────────────────────────────────────────────────────────────
all_services() { echo "$VC_BASE_SERVICES $VC_ROLES"; }

missing_services() {
  local running s out=""
  running=" $(vc_compose ps --services --status running 2>/dev/null | tr '\n' ' ') "
  for s in $(all_services); do
    [[ "$running" == *" $s "* ]] || out+="$s "
  done
  echo "${out% }"
}

# wait_healthy SERVICE → 0 once healthy, 1 when unhealthy/exited or on timeout
wait_healthy() {
  local svc="$1" cid st deadline=$((SECONDS + HEALTH_TIMEOUT))
  while :; do
    cid="$(vc_compose ps -q "$svc" 2>/dev/null | head -n1)"
    st=""
    [[ -n "$cid" ]] && st="$(docker inspect -f '{{if .State.Health}}{{.State.Health.Status}}{{else}}{{.State.Status}}{{end}}' "$cid" 2>/dev/null || true)"
    case "$st" in
      healthy) return 0 ;;
      unhealthy | exited | dead) log "$svc is $st"; return 1 ;;
    esac
    ((SECONDS < deadline)) || { log "$svc not healthy after ${HEALTH_TIMEOUT}s (last: ${st:-no container})"; return 1; }
    sleep "$HEALTH_POLL"
  done
}

# roll_to REF ROLE… → 0 when every role is up and healthy on REF; on the
# first failure prints the roles already touched (incl. the failed one)
roll_to() {
  local ref="$1" r done=""
  shift
  for r in "$@"; do
    done+="$r "
    log "rolling $r → $ref"
    if ! VC_IMAGE="$ref" vc_compose up -d --no-deps "$r" || ! wait_healthy "$r"; then
      echo "${done% }"
      return 1
    fi
  done
}

# The rollback log is bind-mounted read-only into the migrate container
# (compose.yaml), so it must exist (a missing source is an error, never an
# empty directory Docker would create in its place).
ensure_rollback_log() {
  [[ -f "$VC_STATE/rollbacks" ]] || : >>"$VC_STATE/rollbacks"
  chmod 0644 "$VC_STATE/rollbacks"
}

record() {
  local new="$1" prev="$2" tmp="$VC_STATE/image.env.new"
  printf 'VC_IMAGE=%s\n' "$new" >"$tmp"
  mv -f "$tmp" "$VC_STATE/image.env"
  [[ -n "$prev" ]] && printf '%s\n' "$prev" >"$VC_STATE/previous"
  # the database never goes back: keep the highest schema seen (a rollback
  # target's label is lower than what its successor migrated to)
  local schema old
  schema="$(label "$new" io.voyager-crawler.schema)"
  old="$(cat "$VC_STATE/schema" 2>/dev/null || true)"
  if [[ "$schema" =~ ^[0-9]+$ ]]; then
    [[ "$old" =~ ^[0-9]+$ ]] && ((10#$old > 10#$schema)) && schema="$old"
    printf '%s\n' "$schema" >"$VC_STATE/schema"
  fi
  return 0
}

base_up() {
  local ref="$1"
  # shellcheck disable=SC2086 # a list of service names
  VC_IMAGE="$ref" vc_compose up -d $VC_BASE_SERVICES || return 1
  wait_healthy postgres
}

deploy_once() {
  local new cur ref why touched
  new="$(resolve_digest)" || die "could not resolve $VC_REPO_IMAGE:$TAG (registry unreachable?)"
  ref="$VC_REPO_IMAGE@$new"
  cur="$(current_image)"
  report "$new"

  if [[ "$ref" == "$cur" ]]; then
    local missing
    missing="$(missing_services)"
    if [[ -n "$missing" ]]; then
      log "up to date ($new); starting stopped services: $missing"
      # shellcheck disable=SC2086 # a list of service names
      vc_compose up -d --no-recreate $missing
    fi
    return 0
  fi

  why="$(held_reason "$new")"
  if [[ -n "$why" ]]; then
    log "prod is $new, held ($why): staying on ${cur:-nothing}"
    return 0
  fi

  local v=0
  verify "$ref" || v=$?
  case "$v" in
    0) ;;
    2)
      hold "$new" "signature check failed (identity $CERT_IDENTITY)"
      exit 3
      ;;
    *) die "could not verify $ref (cosign/registry trouble): retrying next tick" ;;
  esac
  log "pulling $ref"
  docker pull -q "$ref" >/dev/null || die "pull of $ref failed (retrying next tick)"

  base_up "$ref" || die "postgres is not healthy: not migrating"
  ensure_rollback_log
  log "migrating with $ref"
  migrate_with "$ref" || die "migrate with $ref failed $RETRIES times: nothing rolled; retrying next tick"
  if why="$(schema_too_new "$ref")"; then
    hold "$new" "$why"
    exit 1
  fi

  # shellcheck disable=SC2086 # VC_ROLES is a list
  if ! touched="$(roll_to "$ref" $VC_ROLES)"; then
    hold "$new" "unhealthy after roll ($touched)"
    if [[ -n "$cur" ]]; then
      log "rolling back $touched → $cur"
      # shellcheck disable=SC2086 # a list of roles
      roll_to "$cur" $touched >/dev/null || log "ROLLBACK INCOMPLETE: check with vc-compose.sh ps"
    fi
    exit 1
  fi
  record "$ref" "$cur"
  report "$new"
  log "deployed $ref (previous: ${cur:-none})"
}

rollback() {
  local cur target why touched
  cur="$(current_image)"
  target="$(cat "$VC_STATE/previous" 2>/dev/null || true)"
  [[ -n "$target" && "$target" != "$cur" ]] || die "no previous image to roll back to"
  docker pull -q "$target" >/dev/null || die "pull of $target failed"
  verify "$target" || die "$target does not verify (now): refusing"
  if why="$(schema_too_new "$target")"; then
    die "$target $why: refusing (§17.6)"
  fi
  # shellcheck disable=SC2086 # VC_ROLES is a list
  touched="$(roll_to "$target" $VC_ROLES)" || die "rollback to $target unhealthy at: $touched"
  [[ -n "$cur" ]] && hold "${cur##*@}" "manual rollback"
  record "$target" "$cur"
  ensure_rollback_log
  date -u +%FT%TZ >>"$VC_STATE/rollbacks"
  log "rolled back to $target"
}

main() {
  mkdir -p "$VC_STATE"
  [[ -f "$VC_STATE/host.env" ]] || die "$VC_STATE/host.env missing (written by first-boot.sh)"
  case "${1:-}" in
    --secrets) refresh_secrets; return ;;
    --rollback | "") ;;
    *) echo "usage: $0 [--rollback|--secrets]" >&2; exit 2 ;;
  esac
  take_lock || { log "another deploy or watchdog run holds the lock"; return 0; }
  refresh_secrets || log "secret refresh failed; using the files present"
  if [[ "${1:-}" == --rollback ]]; then rollback; else deploy_once; fi
}

main "$@"
