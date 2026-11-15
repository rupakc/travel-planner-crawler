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
# The roles deploy.sh rolls, in this order (the API last: it serves Voyager)
VC_ROLES="${VC_ROLES:-maintenance scheduler processor embedder fetcher api}"
# Third-party services, pinned by digest in compose.yaml (never rolled)
VC_BASE_SERVICES="${VC_BASE_SERVICES:-postgres chromium}"

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
  VC_IMAGE="${VC_IMAGE:-$(current_image)}" \
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
