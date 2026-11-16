#!/usr/bin/env bash
# Runs the E2E suite (plan Task 16): builds vc-e2e:local, starts e2e/compose.yaml, finds out
# which vcrawl roles are wired, drives the crawl cycles and runs the oracles.
#
#   bash e2e/run.sh                     # compile inside Docker (slow first time)
#   E2E_BINS=prebuilt bash e2e/run.sh   # use Linux binaries already in e2e/.bin (CI)
#   E2E_KEEP=1 bash e2e/run.sh          # leave the stack up for debugging
#
# Exit status: 0 when every assertion passed or was skipped for a role that is not wired
# yet (each skip names the role); 1 otherwise. Logs and the summary go to e2e/out/.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/.." && pwd)"
out="${E2E_OUT:-$here/out}"
export E2E_OUT="$out"
export COMPOSE_PROJECT_NAME="${COMPOSE_PROJECT_NAME:-vc-e2e}"
dc() { docker compose -f "$here/compose.yaml" "$@"; }

# Role services (compose name -> vcrawl role).
role_of() { case "$1" in fetcher-*) echo fetcher ;; *) echo "$1" ;; esac; }
ROLE_SERVICES=(scheduler fetcher-a fetcher-b processor embedder maintenance api)
NOT_WIRED='not wired yet'

log() { printf '[e2e] %s\n' "$*" >&2; }

# shellcheck disable=SC2329 # invoked by the EXIT trap
cleanup() {
  local rc=$?
  dc logs --no-color --timestamps >"$out/compose.log" 2>&1 || true
  if [[ "${E2E_KEEP:-0}" != 1 ]]; then
    dc --profile tools down -v --remove-orphans >/dev/null 2>&1 || true
  fi
  exit "$rc"
}

rm -rf "$out" && mkdir -p "$out" && chmod 0777 "$out"

# --- throwaway TLS for the API: a CA (the runner trusts only it) and a cert for "api" ------
tls="$(mktemp -d)"
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 2 \
  -subj /CN=vc-e2e-ca -keyout "$tls/ca.key" -out "$out/ca.pem" \
  -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign 2>/dev/null
openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj /CN=api \
  -keyout "$tls/api.key" -out "$tls/api.csr" 2>/dev/null
printf 'subjectAltName=DNS:api\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\n' >"$tls/ext"
openssl x509 -req -in "$tls/api.csr" -CA "$out/ca.pem" -CAkey "$tls/ca.key" -CAcreateserial -CAserial "$tls/ca.srl" \
  -days 2 -extfile "$tls/ext" -out "$tls/api.pem" 2>/dev/null
chmod 0644 "$out/ca.pem"
E2E_TLS_CERT="$(cat "$tls/api.pem")"
E2E_TLS_KEY="$(cat "$tls/api.key")"
E2E_API_TOKEN="e2e.$(openssl rand -hex 24)"
export E2E_TLS_CERT E2E_TLS_KEY E2E_API_TOKEN
rm -rf "$tls"

# --- image ---------------------------------------------------------------------------------
log "building vc-e2e:local (BINS=${E2E_BINS:-build})"
docker build -q -f "$here/Dockerfile" --build-arg "BINS=${E2E_BINS:-build}" ${CARGO_BUILD_JOBS:+--build-arg "CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS"} -t vc-e2e:local "$root" >/dev/null

trap cleanup EXIT

# --- infrastructure ------------------------------------------------------------------------
log "starting postgres, fakes and chromium"
dc up -d --wait --wait-timeout 120 postgres fake-web trap trap-metadata
dc up -d fake-gcs fake-gemini chromium

missing=()
crashed=()

# --- migrate -------------------------------------------------------------------------------
prepare_args=()
if mig="$(dc --profile tools run --rm migrate 2>&1)"; then
  log "vcrawl migrate: ok"
elif grep -qF "$NOT_WIRED" <<<"$mig"; then
  log "vcrawl migrate: not wired yet; the runner applies the migrations"
  missing+=(migrate)
  prepare_args+=(--migrate)
else
  printf '%s\n' "$mig" >"$out/migrate.log"
  log "vcrawl migrate FAILED (e2e/out/migrate.log)"
  crashed+=(migrate)
  prepare_args+=(--migrate)
fi
dc --profile tools run --rm runner prepare "${prepare_args[@]}"

# --- roles ---------------------------------------------------------------------------------
log "starting roles: ${ROLE_SERVICES[*]}"
dc up -d --no-deps "${ROLE_SERVICES[@]}"

# classify <service>: prints running | not_wired | crashed
classify() {
  local id state
  id="$(dc ps -a -q "$1")"
  state="$(docker inspect -f '{{.State.Status}}' "$id")"
  if [[ "$state" == running ]]; then echo running; return; fi
  # Capture first: `docker logs | grep -q` loses to SIGPIPE under pipefail.
  local logs
  logs="$(docker logs "$id" 2>&1 || true)"
  if grep -qF "$NOT_WIRED" <<<"$logs"; then echo not_wired; else echo crashed; fi
}

sleep "${E2E_ROLE_SETTLE_SECS:-20}"
for s in "${ROLE_SERVICES[@]}"; do
  r="$(role_of "$s")"
  case "$(classify "$s")" in
    running) log "role $s: running" ;;
    not_wired) log "role $s: not wired yet (Task 15)"; missing+=("$r") ;;
    crashed) log "role $s: CRASHED"; crashed+=("$r") ;;
  esac
done

uniq_csv() { [[ $# -eq 0 ]] && return 0; printf '%s\n' "$@" | sort -u | paste -sd, -; }
E2E_MISSING_ROLES="$(uniq_csv "${missing[@]+"${missing[@]}"}")"
E2E_CRASHED_ROLES="$(uniq_csv "${crashed[@]+"${crashed[@]}"}")"
export E2E_MISSING_ROLES E2E_CRASHED_ROLES
log "missing roles: ${E2E_MISSING_ROLES:-none}; crashed: ${E2E_CRASHED_ROLES:-none}"

# --- cycles + oracles ----------------------------------------------------------------------
rc=0
dc --profile tools run --rm runner run || rc=$?

# A role that died during the run is a failure even if every assertion held.
for s in "${ROLE_SERVICES[@]}"; do
  if [[ "$(classify "$s")" == crashed ]] && ! grep -qw "$(role_of "$s")" <<<"$E2E_CRASHED_ROLES"; then
    log "role $s exited during the run"
    rc=1
  fi
done

# Database state for the logs (counts and errors only: never page content).
dc exec -T postgres psql -U vcrawl -d vcrawl -X -P pager=off >"$out/db.txt" 2>&1 <<'SQL' || true
SELECT kind, state, count(*), max(attempts) AS max_attempts, left(max(last_error), 300) AS last_error
  FROM work_items GROUP BY 1, 2 ORDER BY 1, 2;
SELECT role, fetch_mode, key, status, count(*) FROM captures GROUP BY 1, 2, 3, 4 ORDER BY 3, 1, 2, 4;
SELECT kind, source_id, state, count(*), max(current_version) FROM records GROUP BY 1, 2, 3 ORDER BY 2, 1;
SELECT count(*) AS record_versions FROM record_versions;
SELECT key, state, count(*) FROM url_state GROUP BY 1, 2 ORDER BY 1, 2;
SELECT key, delay_ms, crawl_delay_ms, parked_until, park_reason FROM politeness_keys ORDER BY 1;
SELECT role, instance, last_beat, progress FROM heartbeats ORDER BY 1, 2;
SQL

curl_admin() { dc --profile tools run --rm --entrypoint curl runner -fsS "$1"; }
curl_admin http://fake-web/__admin/log >"$out/fake-web.jsonl" 2>/dev/null || true
curl_admin http://trap/__admin/log >"$out/trap.jsonl" 2>/dev/null || true
curl_admin http://trap-metadata/__admin/log >"$out/trap-metadata.jsonl" 2>/dev/null || true

if [[ -f "$out/summary.md" ]]; then
  cat "$out/summary.md"
  if [[ -n "${GITHUB_STEP_SUMMARY:-}" ]]; then
    { echo "### E2E"; cat "$out/summary.md"; } >>"$GITHUB_STEP_SUMMARY"
  fi
fi
log "done (exit $rc); logs in $out"
exit "$rc"
