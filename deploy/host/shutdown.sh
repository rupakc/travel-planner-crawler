#!/usr/bin/env bash
# Voyager Crawler GCE `shutdown-script` (instance metadata, set from this file
# by infrastructure/bootstrap.sh's instance template). Runs on every shutdown,
# reboot and stop, and on a Spot preemption, where GCE gives the guest about
# 30 s (best effort) before it powers the VM off.
#
# Stops the crawler first (everything but Postgres, `docker compose stop -t 25`:
# SIGTERM, then SIGKILL after 25 s), so no role writes while Postgres goes
# down, then Postgres (`stop -t 25`; the image's stop signal is SIGINT, a fast
# shutdown that usually takes a second or two). If the power goes off before
# Postgres is done, it recovers from its WAL on the next start (crash safe);
# the spool is fsynced and replayed. Never fails: a shutdown must not wait on
# us. voyager-crawler.service's own ExecStop does the same on an ordinary
# shutdown; stopping twice is harmless.
set -uo pipefail

LOG_TAG=voyager-crawler-shutdown
# shellcheck source=deploy/host/lib.sh
. "${VC_OPT:-/opt/voyager-crawler}/host/lib.sh"
MD="${VC_METADATA_URL:-http://metadata.google.internal/computeMetadata/v1}"
STOP_TIMEOUT="${VC_SHUTDOWN_STOP_TIMEOUT:-25}"

main() {
  if [[ ! -f "$VC_STATE/host.env" ]]; then
    log "nothing deployed yet (no $VC_STATE/host.env): nothing to stop"
    return 0
  fi
  local preempted
  preempted="$(curl -s -m 2 -H 'Metadata-Flavor: Google' "$MD/instance/preempted" 2>/dev/null)" || preempted=unknown
  [[ -n "$preempted" ]] || preempted=unknown
  log "shutdown (preempted: $preempted): stopping the crawler, then postgres"

  local -a crawler=()
  local s
  while IFS= read -r s; do
    [[ -n "$s" && "$s" != postgres ]] && crawler+=("$s")
  done < <(vc_compose config --services 2>/dev/null)
  if ((${#crawler[@]} == 0)); then
    # compose could not list its services: stop everything at once
    log "could not list the compose services; stopping the whole stack"
    vc_compose stop -t "$STOP_TIMEOUT" || log "compose stop failed"
    return 0
  fi
  vc_compose stop -t "$STOP_TIMEOUT" "${crawler[@]}" || log "stopping the crawler failed"
  vc_compose stop -t "$STOP_TIMEOUT" postgres || log "stopping postgres failed"
  log "stopped"
}

main "$@"
exit 0
