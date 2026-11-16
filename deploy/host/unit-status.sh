#!/usr/bin/env bash
# Records a crawler unit's outcome for the alert metrics (go-live review 4):
#
#   unit-status.sh ok UNIT       ExecStartPost of every crawler unit
#   unit-status.sh failed UNIT   voyager-crawler-failed@UNIT.service (OnFailure=)
#
# heartbeats row ('host', 'unit:UNIT'): {"failed": false, "last_ok_at": …} or
# {"failed": true, "failed_at": …} (last_ok_at is kept). The maintenance role
# exports crawler_host_units_failed (rows with failed = true), and the ages of
# the base backup and the light restore drill from their last_ok_at. A failure
# is also logged at priority err. Always exits 0: it must never fail the unit
# it reports on.
set -uo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=./lib.sh
. "$(dirname "$(readlink -f "$0")")/lib.sh"
LOG_TAG=voyager-crawler-unit-status

state="${1:-}" unit="${2:-}"
[[ "$unit" =~ ^voyager-crawler[a-z0-9@.-]*$ ]] || { log "usage: $0 ok|failed voyager-crawler-…"; exit 0; }
at="$(iso_utc "$(now_epoch)")"
case "$state" in
  ok) host_beat "unit:$unit" "{\"failed\": false, \"last_ok_at\": \"$at\"}" || true ;;
  failed)
    logger -p daemon.err -t voyager-crawler-failed -- "$unit failed" 2>/dev/null || true
    log "$unit FAILED"
    host_beat "unit:$unit" "{\"failed\": true, \"failed_at\": \"$at\"}" || true
    ;;
  *) log "usage: $0 ok|failed UNIT" ;;
esac
exit 0
