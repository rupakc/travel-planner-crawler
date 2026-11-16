#!/usr/bin/env bash
# Weekly light restore drill (spec §17.9, go-live review 4), from
# voyager-crawler-restore-drill.timer:
#
#   1. a scratch directory on the data disk (refused when the disk has less
#      free space than the live pgdata needs, plus 10 %);
#   2. a throwaway container of the running postgres image, on the pgbackup
#      network only (wal-g's route to GCS), with no access to the live
#      pgdata: `wal-g backup-fetch LATEST` into the scratch directory, then
#      Postgres on it as a read-only hot standby (archive_mode off, so it
#      never writes to the WAL archive; WAL replayed with wal-g wal-fetch);
#   3. sanity queries: in recovery, at least one applied migration, the
#      segments table readable;
#   4. the scratch directory and the container are removed on every exit.
#
# Success → exit 0 (the unit's ExecStartPost records last_ok_at, exported as
# crawler_backup_restore_age_seconds); failure → exit 1 (OnFailure= records
# it, crawler_host_units_failed). Runs inside restore-drill-inner.sh.
set -euo pipefail
# shellcheck source-path=SCRIPTDIR
# shellcheck source=./lib.sh
. "$(dirname "$(readlink -f "$0")")/lib.sh"
LOG_TAG=voyager-crawler-restore-drill

SCRATCH="${VC_DRILL_DIR:-$VC_DATA/restore-drill}"
NAME="${VC_PROJECT_NAME}-restore-drill"
NETWORK="${VC_PROJECT_NAME}_pgbackup"
PG_UID=999

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  rm -rf "$SCRATCH"
}

main() {
  [[ -n "$(current_image)" ]] || { log "nothing deployed yet: no drill"; exit 0; }
  local cid image need avail out migrations segments
  cid="$(vc_compose ps -q postgres 2>/dev/null | head -n1)"
  [[ -n "$cid" ]] || die "postgres is not running: cannot tell its image"
  image="$(docker inspect -f '{{.Config.Image}}' "$cid")"
  [[ -n "$image" ]] || die "could not read the postgres image"

  # Room for a full copy of the database next to the live one
  need="$(du -sk "$VC_DATA/pgdata" 2>/dev/null | awk '{print int($1 * 1.1)}')"
  avail="$(df -Pk "$VC_DATA" | awk 'NR == 2 {print $4}')"
  [[ "$need" =~ ^[0-9]+$ && "$avail" =~ ^[0-9]+$ ]] || die "could not size the drill (need '$need' KiB, free '$avail' KiB)"
  ((avail > need)) || die "not enough free space on $VC_DATA for the drill: need ${need} KiB, ${avail} KiB free"

  trap cleanup EXIT
  cleanup
  install -d -m 0700 "$SCRATCH"
  chown "$PG_UID:$PG_UID" "$SCRATCH" 2>/dev/null || [[ -n "${VC_DRILL_NO_CHOWN:-}" ]] ||
    die "could not hand $SCRATCH to uid $PG_UID"

  log "restoring the latest base backup into $SCRATCH with $image"
  out="$(docker run --rm --name "$NAME" --network "$NETWORK" --user "$PG_UID:$PG_UID" \
    --memory 768m --cap-drop ALL --security-opt no-new-privileges:true \
    --env-file "$VC_OPT/postgres/walg.env" \
    -v "$SCRATCH:/drill" \
    -v "$VC_OPT/bin/wal-g:/usr/local/bin/wal-g:ro" \
    -v "$VC_OPT/host/restore-drill-inner.sh:/drill-inner.sh:ro" \
    --entrypoint /bin/bash "$image" /drill-inner.sh)" || die "restore drill FAILED (see journalctl -u voyager-crawler-restore-drill)"

  migrations="$(sed -n 's/^DRILL_RESULT migrations=\([0-9]*\) segments=\([0-9]*\)$/\1/p' <<<"$out" | tail -n1)"
  segments="$(sed -n 's/^DRILL_RESULT migrations=\([0-9]*\) segments=\([0-9]*\)$/\2/p' <<<"$out" | tail -n1)"
  if [[ ! "$migrations" =~ ^[0-9]+$ ]] || ((migrations == 0)); then
    die "restore drill FAILED: the restored database has no applied migrations"
  fi
  log "restore drill ok: $migrations migrations, $segments segments in the restored copy"
}

main "$@"
