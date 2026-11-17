#!/usr/bin/env bash
# shellcheck disable=SC2015 # `check && ok || fail` is the intended reporting idiom
# The host side of the Spot VM in a stateful MIG: shutdown.sh (preemption
# notice), first-boot.sh's swap file, and first boot after a MIG recreate
# (the stateful data disk comes back: no format, no ramp, units re-installed).
# No root, no network, no Docker: system commands are shell functions or
# fakes in a temp bin/. Run: bash deploy/tests/test_host.sh
set -uo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
FB="$ROOT/deploy/host/first-boot.sh"
SHUTDOWN="$ROOT/deploy/host/shutdown.sh"
FAILS=0
PASSES=0
ok() { PASSES=$((PASSES + 1)); echo "ok   $1"; }
fail() {
  FAILS=$((FAILS + 1))
  echo "FAIL $1: $2"
  echo "---- calls"; cat "${CALLS:-/dev/null}" 2>/dev/null; echo "----"
}

setup() {
  T="$(mktemp -d)"
  export CALLS="$T/calls"
  : >"$CALLS"
  mkdir -p "$T/bin"
}

# fake_docker — records `docker …` in $CALLS; `compose config --services`
# prints $FAKE_SERVICES (exit $FAKE_CONFIG_RC), `compose stop` exits $FAKE_STOP_RC
fake_docker() {
  cat >"$T/bin/docker" <<'EOF'
#!/usr/bin/env bash
args=" $* "
echo "docker $*" >>"$CALLS"
case "$args" in
  *" config --services "*)
    [[ "${FAKE_CONFIG_RC:-0}" == 0 ]] || exit "$FAKE_CONFIG_RC"
    printf '%s\n' ${FAKE_SERVICES-postgres chromium otelcol vcrawl}
    ;;
  *" stop "*) exit "${FAKE_STOP_RC:-0}" ;;
esac
exit 0
EOF
  cat >"$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
echo "curl $*" >>"$CALLS"
[[ "$*" == *instance/preempted* ]] && printf '%s' "${FAKE_PREEMPTED-TRUE}"
exit 0
EOF
  printf '#!/usr/bin/env bash\nexit 0\n' >"$T/bin/logger"
  chmod +x "$T/bin/docker" "$T/bin/curl" "$T/bin/logger"
}

# run_shutdown — shutdown.sh against the fakes, with a deployed state dir
run_shutdown() {
  PATH="$T/bin:$PATH" VC_OPT="$ROOT/deploy" VC_STATE="$T/state" VC_DATA="$T/data" \
    bash "$SHUTDOWN" 2>"$T/err"
}

# Spot preemption: the crawler (every service but postgres) stops first with
# -t 25, then postgres with -t 25; the preempted flag is logged
test_shutdown_stops_crawler_then_postgres() {
  setup
  fake_docker
  mkdir -p "$T/state"
  echo "VC_API_BIND=10.142.0.9" >"$T/state/host.env"
  run_shutdown || { fail shutdown_stops_crawler_then_postgres "exit $?"; return; }
  local stops
  stops="$(grep -E '^docker compose .* stop ' "$CALLS" | sed 's/.* stop /stop /')"
  [[ "$stops" == "stop -t 25 chromium otelcol vcrawl"$'\n'"stop -t 25 postgres" ]] ||
    { fail shutdown_stops_crawler_then_postgres "stops: $stops"; return; }
  grep -q -- "-p voyager-crawler -f $ROOT/deploy/compose.yaml --env-file $T/state/host.env" "$CALLS" ||
    { fail shutdown_stops_crawler_then_postgres "not the crawler's compose project"; return; }
  grep -q 'preempted: TRUE' "$T/err" && ok shutdown_stops_crawler_then_postgres ||
    fail shutdown_stops_crawler_then_postgres "preemption not logged: $(cat "$T/err")"
}

# never fails a shutdown: errors from compose are logged, exit 0; a compose
# that cannot list its services gets one whole-stack stop
test_shutdown_never_fails() {
  setup
  fake_docker
  mkdir -p "$T/state"
  echo "VC_API_BIND=10.142.0.9" >"$T/state/host.env"
  FAKE_STOP_RC=1 run_shutdown || { fail shutdown_never_fails "exit $? on a failed stop"; return; }
  : >"$CALLS"
  FAKE_CONFIG_RC=1 FAKE_PREEMPTED="" run_shutdown || { fail shutdown_never_fails "exit $? without services"; return; }
  [[ "$(grep -c ' stop ' "$CALLS")" == 1 ]] && grep -qE ' stop -t 25$' "$CALLS" ||
    { fail shutdown_never_fails "fallback stop: $(grep ' stop ' "$CALLS")"; return; }
  grep -q 'preempted: unknown' "$T/err" && ok shutdown_never_fails || fail shutdown_never_fails "$(cat "$T/err")"
}

# before the first deploy there is no stack: nothing is called
test_shutdown_before_first_deploy() {
  setup
  fake_docker
  run_shutdown || { fail shutdown_before_first_deploy "exit $?"; return; }
  grep -q '^docker' "$CALLS" && fail shutdown_before_first_deploy "called docker" || ok shutdown_before_first_deploy
}

# first-boot.sh with system commands replaced by shell functions that record
# their calls. FAKE_BLKID_UUID: the data disk's filesystem UUID ("" = no
# filesystem until mkfs runs); FAKE_SWAPON_ACTIVE=1: the swap file is active.
# shellcheck disable=SC2016 # expanded by the inner bash
fb_fakes='
fallocate() { echo "fallocate $*" >>"$CALLS"; : >"${@: -1}"; }
mkswap() { echo "mkswap $*" >>"$CALLS"; [[ "${FAKE_MKSWAP_RC:-0}" == 0 ]]; }
swapon() {
  if [[ "${1:-}" == --show* ]]; then [[ -f "$T/swap-active" ]] && echo "$SWAPFILE"; return 0; fi
  echo "swapon $*" >>"$CALLS"; : >"$T/swap-active"
}
sysctl() { echo "sysctl $*" >>"$CALLS"; }
systemctl() { echo "systemctl $*" >>"$CALLS"; }
logger() { :; }
data_disk_attached() { true; }
blkid() {
  local uuid=""; [[ -f "$T/fs-uuid" ]] && uuid="$(cat "$T/fs-uuid")"
  [[ -n "$uuid" ]] || return 2
  [[ "$1" == -s ]] && echo "$uuid"; return 0
}
mkfs.ext4() { echo "mkfs.ext4 $*" >>"$CALLS"; echo 0f0e-new >"$T/fs-uuid"; }
mountpoint() { [[ -f "$T/mounted" ]]; }
mount() { echo "mount $*" >>"$CALLS"; : >"$T/mounted"; }
install() {
  local -a a=(); while (($#)); do case "$1" in -o|-g) shift 2 ;; *) a+=("$1"); shift ;; esac; done
  command install "${a[@]}"
}
curl() {
  local url="${*: -1}"
  case "$url" in
    */instance/attributes/crawler-bundle) printf "%s\n200" "$(cat "$T/bundle.b64")" ;;
    */instance/network-interfaces/0/ip) printf "10.142.0.9\n200" ;;
    *) printf "\n404" ;;
  esac
}
'

# run_fb FUNCTIONS… — source first-boot.sh (functions only) with the fakes
# and paths under $T, then run the named functions in order
run_fb() {
  export T
  VC_FIRST_BOOT_FUNCTIONS_ONLY=1 VC_FB_DATA="$T/data" VC_FB_OPT="$T/opt" \
    VC_DATA_DISK="$T/disk" VC_FSTAB="$T/fstab" VC_SWAPFILE="$T/swapfile" VC_SYSCTL_DIR="$T/sysctl.d" \
    VC_SYSTEMD_DIR="$T/systemd" bash -c ". '$FB'; $fb_fakes
set -e
for f in $*; do \"\$f\"; done" 2>"$T/err"
}

# the bundle bootstrap.sh puts in metadata: deploy/ without tests/ and ci/
make_bundle() {
  (cd "$ROOT/deploy" && find . -type f ! -path './tests/*' ! -path './ci/*' | LC_ALL=C sort) >"$T/list"
  COPYFILE_DISABLE=1 tar -C "$ROOT/deploy" -czf - -T "$T/list" | base64 | tr -d '\n' >"$T/bundle.b64"
}

# 2 GB swap file with vm.swappiness=10, created once; a second boot only
# re-asserts fstab/sysctl and does not re-create or re-enable it
test_swap_file() {
  setup
  : >"$T/fstab"
  run_fb configure_swap configure_swap || { fail swap_file "exit $?: $(cat "$T/err")"; return; }
  [[ "$(grep -c '^fallocate -l 2G ' "$CALLS")" == 1 && "$(grep -c '^mkswap ' "$CALLS")" == 1 &&
    "$(grep -c '^swapon ' "$CALLS")" == 1 ]] || { fail swap_file "calls: $(cat "$CALLS")"; return; }
  [[ -f "$T/swapfile" && ! -e "$T/swapfile.tmp" ]] || { fail swap_file "swap file not in place"; return; }
  [[ "$(grep -c "^$T/swapfile none swap sw 0 0\$" "$T/fstab")" == 1 ]] || { fail swap_file "fstab: $(cat "$T/fstab")"; return; }
  grep -qx 'vm.swappiness=10' "$T/sysctl.d/99-voyager-crawler-swap.conf" ||
    { fail swap_file "no swappiness"; return; }
  # main makes swap before anything else (apt and Docker run with it)
  awk '/^main\(\)/{on=1} on' "$FB" | sed -n 2p | grep -qx '  configure_swap' && ok swap_file ||
    fail swap_file "main does not start with configure_swap"
}

# an interrupted earlier boot left a partial file: rebuilt from scratch; a
# failing mkswap never leaves a file swapon would trust
test_swap_file_is_atomic() {
  setup
  : >"$T/fstab"
  echo junk >"$T/swapfile.tmp"
  FAKE_MKSWAP_RC=1 run_fb configure_swap && { fail swap_file_is_atomic "mkswap failure ignored"; return; }
  [[ ! -e "$T/swapfile" ]] || { fail swap_file_is_atomic "a half-made swap file is in place"; return; }
  run_fb configure_swap || { fail swap_file_is_atomic "retry failed: $(cat "$T/err")"; return; }
  [[ -f "$T/swapfile" && ! -e "$T/swapfile.tmp" ]] && ok swap_file_is_atomic ||
    fail swap_file_is_atomic "not rebuilt"
}

# A brand-new data disk (no filesystem): formatted once, mounted, and the
# fresh install goes on the ramp
test_fresh_data_disk() {
  setup
  : >"$T/fstab"
  make_bundle
  run_fb mount_data_disk install_bundle || { fail fresh_data_disk "exit $?: $(cat "$T/err")"; return; }
  [[ "$(grep -c '^mkfs.ext4 ' "$CALLS")" == 1 ]] || { fail fresh_data_disk "mkfs calls: $(grep -c mkfs "$CALLS")"; return; }
  grep -qx "UUID=0f0e-new $T/data ext4 discard,defaults,nofail 0 2" "$T/fstab" ||
    { fail fresh_data_disk "fstab: $(cat "$T/fstab")"; return; }
  grep -q '^VC_VCRAWL_SOURCE_ALLOWLIST=' "$T/data/deploy/host.env" && ok fresh_data_disk ||
    fail fresh_data_disk "a fresh install is not on the ramp"
}

# MIG recreate after a preemption or autoheal: the stateful data disk comes
# back with its filesystem and database on a VM whose boot disk may be new.
# No format, the same fstab line (once), the operator's lifted ramp stays
# lifted, host.env is rebuilt with the overrides, units and timers are back.
test_recreate_keeps_data_disk() {
  setup
  : >"$T/fstab"
  make_bundle
  echo 1a2b-old >"$T/fs-uuid"
  mkdir -p "$T/data/pgdata" "$T/data/deploy"
  echo 17 >"$T/data/pgdata/PG_VERSION"
  : >"$T/data/deploy/.ramp-default"
  printf 'VC_LLM_ENABLED=true\n' >"$T/data/deploy/host.local.env"
  local f
  for f in 1 2; do # a fresh boot disk, then a plain reboot of the same VM
    run_fb mount_data_disk docker_waits_for_data_disk install_bundle install_units ||
      { fail recreate_keeps_data_disk "boot $f exit $?: $(cat "$T/err")"; return; }
  done
  grep -q '^mkfs' "$CALLS" && { fail recreate_keeps_data_disk "re-formatted the data disk"; return; }
  [[ "$(grep -c "^UUID=1a2b-old $T/data ext4 " "$T/fstab")" == 1 && "$(wc -l <"$T/fstab" | tr -d ' ')" == 1 ]] ||
    { fail recreate_keeps_data_disk "fstab: $(cat "$T/fstab")"; return; }
  [[ "$(cat "$T/data/deploy/host.local.env")" == VC_LLM_ENABLED=true ]] ||
    { fail recreate_keeps_data_disk "host.local.env changed: $(cat "$T/data/deploy/host.local.env")"; return; }
  grep -qx 'VC_API_BIND=10.142.0.9' "$T/data/deploy/host.env" && grep -qx 'VC_LLM_ENABLED=true' "$T/data/deploy/host.env" &&
    ! grep -q ALLOWLIST "$T/data/deploy/host.env" ||
    { fail recreate_keeps_data_disk "host.env: $(cat "$T/data/deploy/host.env")"; return; }
  [[ -x "$T/opt/host/shutdown.sh" && -f "$T/systemd/voyager-crawler-basebackup.timer" &&
    -f "$T/systemd/docker.service.d/10-voyager-crawler-data.conf" ]] ||
    { fail recreate_keeps_data_disk "bundle or units not installed"; return; }
  grep -q 'systemctl enable --now voyager-crawler-deploy.timer voyager-crawler-watchdog.timer voyager-crawler-basebackup.timer voyager-crawler-restore-drill.timer' "$CALLS" &&
    ok recreate_keeps_data_disk || fail recreate_keeps_data_disk "timers not enabled"
}

# a database on the disk but no ramp marker yet (a disk from before the ramp
# existed, moved to the recreated VM): never put on the ramp
test_recreate_existing_database_not_ramped() {
  setup
  : >"$T/fstab"
  make_bundle
  echo 1a2b-old >"$T/fs-uuid"
  mkdir -p "$T/data/pgdata"
  echo 17 >"$T/data/pgdata/PG_VERSION"
  run_fb mount_data_disk install_bundle || { fail recreate_existing_database_not_ramped "exit $?: $(cat "$T/err")"; return; }
  ! grep -q ALLOWLIST "$T/data/deploy/host.env" && [[ -e "$T/data/deploy/.ramp-default" ]] &&
    ok recreate_existing_database_not_ramped || fail recreate_existing_database_not_ramped "$(cat "$T/data/deploy/host.env")"
}

# Weekly full base backups (Saturday, before Sunday's drill); the lifecycle's
# 14 days then keep two of them and the WAL since the older one
test_weekly_full_base_backup() {
  local d="$ROOT/deploy/host/systemd" age
  grep -qx 'OnCalendar=Sat \*-\*-\* 03:15:00 UTC' "$d/voyager-crawler-basebackup.timer" ||
    { fail weekly_full_base_backup "timer is not weekly"; return; }
  grep -q '^ExecStart=.* wal-g backup-push --full ' "$d/voyager-crawler-basebackup.service" ||
    { fail weekly_full_base_backup "not a full backup"; return; }
  grep -q '^OnCalendar=Sun ' "$d/voyager-crawler-restore-drill.timer" ||
    { fail weekly_full_base_backup "drill no longer on Sunday"; return; }
  age="$(jq -r '.rule[] | select(.condition.matchesPrefix == ["walg/"]) | .condition.age' "$ROOT/infrastructure/gcs/ops-lifecycle.json")"
  [[ "$age" == 14 ]] && ok weekly_full_base_backup || fail weekly_full_base_backup "walg/ lifecycle age $age, want 14 (2 weekly backups)"
}

for t in $(declare -F | awk '{print $3}' | grep '^test_'); do
  "$t"
done
echo "$PASSES passed, $FAILS failed"
((FAILS == 0))
