#!/usr/bin/env bash
# Shared by deploy.sh, watchdog.sh and vc-compose.sh (sourced, never run).
# Every path is overridable so the tests in deploy/tests run without root.
# shellcheck shell=bash

VC_OPT="${VC_OPT:-/opt/voyager-crawler}"
VC_DATA="${VC_DATA:-/var/lib/voyager-crawler}"
VC_STATE="${VC_STATE:-$VC_DATA/deploy}"
VC_SECRETS_DIR="${VC_SECRETS_DIR:-/run/voyager-crawler/secrets}"
VC_PROJECT_NAME="${VC_PROJECT_NAME:-voyager-crawler}"
VC_REPO_IMAGE="${VC_REPO_IMAGE:-ghcr.io/rupakc/voyager-crawler}"
# The vcrawl services deploy.sh rolls, in this order. One on the Spot e2-medium:
# `vcrawl run` runs every role in one container (deploy/compose.yaml)
VC_ROLES="${VC_ROLES:-vcrawl}"
# The service that serves the read API (recreated when its TLS pair changes)
VC_API_SERVICE="${VC_API_SERVICE:-vcrawl}"
# Third-party services, pinned by digest in compose.yaml (never rolled)
VC_BASE_SERVICES="${VC_BASE_SERVICES:-postgres chromium otelcol}"

log() {
  echo "${LOG_TAG:-voyager-crawler}: $*" >&2
  logger -t "${LOG_TAG:-voyager-crawler}" -- "$*" 2>/dev/null || true
}
die() {
  log "ERROR: $*"
  exit 1
}

# current_image → the last deployed (verified) image reference, or ""
current_image() {
  [[ -f "$VC_STATE/image.env" ]] || return 0
  sed -n 's/^VC_IMAGE=//p' "$VC_STATE/image.env" | tail -n1
}

# vc_compose ARGS… — docker compose for this stack. VC_IMAGE comes from the
# environment when set (a roll in progress), else from image.env.
vc_compose() {
  local -a files=(--env-file "$VC_STATE/host.env")
  [[ -f "$VC_STATE/image.env" ]] && files+=(--env-file "$VC_STATE/image.env")
  # A fresh install has no image.env yet: compose still needs *a* value for
  # ${VC_IMAGE:?} to read state (ps, logs). Every `up`/`run` passes the real ref.
  VC_IMAGE="${VC_IMAGE:-$(current_image)}" VC_IMAGE="${VC_IMAGE:-unset.invalid/voyager-crawler:none}" \
    docker compose -p "$VC_PROJECT_NAME" -f "$VC_OPT/compose.yaml" "${files[@]}" "$@"
}

# take_lock — the deploy lock (shared by deploy.sh and watchdog.sh). Returns 1
# when another holder has it. Without flock (macOS test runs) it is a no-op.
take_lock() {
  command -v flock >/dev/null 2>&1 || return 0
  mkdir -p "$VC_STATE"
  exec 9>"$VC_STATE/deploy.lock"
  flock -n 9
}

# host_beat INSTANCE JSON — upserts heartbeats row ('host', INSTANCE) with
# last_beat = now() and progress merged with JSON (keys in JSON win, others are
# kept, e.g. last_ok_at). The maintenance role exports alert metrics from these
# rows (crates/vc-obs/src/ops.rs). Best effort: logs and returns 1 when Postgres
# is not reachable, never aborts the caller. Values go in as psql variables
# (quoted by psql), never spliced into the SQL.
host_beat() {
  local instance="$1" json="$2"
  printf '%s\n' "INSERT INTO heartbeats (role, instance, last_beat, progress)
VALUES ('host', :'i', now(), :'p'::jsonb)
ON CONFLICT (role, instance) DO UPDATE
SET last_beat = now(), progress = heartbeats.progress || EXCLUDED.progress;" |
    vc_compose exec -T postgres psql -U vcrawl -d vcrawl -q -v ON_ERROR_STOP=1 \
      -v "i=$instance" -v "p=$json" >/dev/null 2>&1 && return 0
  log "could not record host heartbeat $instance (Postgres down?)"
  return 1
}

# now_epoch — seconds since the epoch ($VC_NOW in tests)
now_epoch() { echo "${VC_NOW:-$(date +%s)}"; }

# iso_utc EPOCH — 2026-11-16T05:30:00Z (GNU date, else BSD date)
iso_utc() {
  date -u -d "@$1" +%FT%TZ 2>/dev/null || date -u -r "$1" +%FT%TZ
}
