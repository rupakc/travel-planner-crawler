#!/usr/bin/env bash
# Voyager Crawler host watchdog (spec §17.5), every 2 minutes from
# voyager-crawler-watchdog.timer. Docker's `restart: unless-stopped` covers
# crashed processes; this covers containers that are running but failing
# their progress-based health check, or stuck restarting:
#
#   - a service that is unhealthy, exited, dead or restarting → restarted
#     (one `docker compose restart`, logged to $VC_STATE/watchdog.log);
#   - when 3 restarts in the last 30 minutes have not fixed things and a
#     service is bad again → the VM reboots (at most once per 2 hours, so a
#     broken image can't boot-loop the VM; deploy.sh then rolls it back or
#     the operator does).
#
# It never acts while deploy.sh holds the lock (a roll in progress) or before
# the first deploy.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=./lib.sh
. "$(dirname "$(readlink -f "$0")")/lib.sh"
LOG_TAG=voyager-crawler-watchdog

WINDOW="${VC_WATCHDOG_WINDOW:-1800}"
MAX_RESTARTS="${VC_WATCHDOG_MAX_RESTARTS:-3}"
REBOOT_GAP="${VC_WATCHDOG_REBOOT_GAP:-7200}"
REBOOT_CMD="${VC_REBOOT_CMD:-systemctl reboot}"
NOW="${VC_NOW:-$(date +%s)}"
RLOG="$VC_STATE/watchdog.log"

# bad_services → "service:state/health" for every service that needs help
bad_services() {
  vc_compose ps -a --format '{{.Service}} {{.State}} {{.Health}}' 2>/dev/null |
    awk '$1 != "migrate" && ($2 != "running" || $3 == "unhealthy") { print $1 ":" $2 "/" $3 }'
}

recent_restarts() {
  awk -v since=$((NOW - WINDOW)) '$1 >= since { n++ } END { print n + 0 }' "$RLOG" 2>/dev/null || echo 0
}

main() {
  [[ -n "$(current_image)" ]] || exit 0
  take_lock || { log "deploy in progress: skipping"; exit 0; }
  local bad entry svc last
  bad="$(bad_services)"
  [[ -n "$bad" ]] || exit 0

  if (($(recent_restarts) >= MAX_RESTARTS)); then
    last="$(cat "$VC_STATE/watchdog-reboot" 2>/dev/null || echo 0)"
    if ((NOW - last >= REBOOT_GAP)); then
      log "still failing after $MAX_RESTARTS restarts in $((WINDOW / 60)) min (${bad//$'\n'/ }): rebooting"
      echo "$NOW" >"$VC_STATE/watchdog-reboot"
      $REBOOT_CMD
      exit 0
    fi
    log "still failing (${bad//$'\n'/ }), but the VM rebooted less than $((REBOOT_GAP / 60)) min ago: restarting only"
  fi

  for entry in $bad; do
    svc="${entry%%:*}"
    log "restarting $svc (${entry#*:})"
    echo "$NOW $svc" >>"$RLOG"
    vc_compose restart "$svc" || log "restart of $svc failed"
  done
  # keep the log short
  if [[ -f "$RLOG" ]] && (($(wc -l <"$RLOG") > 500)); then
    tail -n 200 "$RLOG" >"$RLOG.new" && mv -f "$RLOG.new" "$RLOG"
  fi
}

main "$@"
