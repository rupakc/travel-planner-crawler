#!/usr/bin/env bash
# Runs inside the throwaway postgres container of restore-drill.sh (uid 999,
# /drill = the scratch directory). Prints one line
#   DRILL_RESULT migrations=<n> segments=<n>
# on success; any failure exits non-zero.
set -euo pipefail
export PATH="/usr/lib/postgresql/17/bin:$PATH"
D=/drill/pgdata

wal-g backup-fetch "$D" LATEST
chmod 0700 "$D"
# A hot standby: read-only, replays the archived WAL up to the newest segment
touch "$D/standby.signal"
printf 'local all all trust\n' >/drill/hba.conf
pg_ctl -D "$D" -w -t 3600 -l /drill/postgres.log start -o "\
 -c hba_file=/drill/hba.conf -c listen_addresses='' -c unix_socket_directories=/drill \
 -c port=5433 -c archive_mode=off -c hot_standby=on -c default_transaction_read_only=on \
 -c shared_buffers=64MB -c max_connections=10 -c max_wal_senders=0 \
 -c restore_command='/usr/local/bin/wal-g wal-fetch %f %p'" ||
  { tail -n 50 /drill/postgres.log >&2; exit 1; }
trap 'pg_ctl -D "$D" -m fast -w stop >/dev/null 2>&1 || true' EXIT

q() { psql -h /drill -p 5433 -U vcrawl -d vcrawl -XtAq -v ON_ERROR_STOP=1 -c "$1"; }
[[ "$(q 'SELECT pg_is_in_recovery()')" == t ]] || { echo "not in recovery" >&2; exit 1; }
migrations="$(q 'SELECT count(*) FROM _sqlx_migrations WHERE success')"
segments="$(q 'SELECT count(*) FROM segments')"
echo "DRILL_RESULT migrations=$migrations segments=$segments"
