#!/usr/bin/env bash
# Voyager Crawler VM provisioning: the GCE `startup-script` (runs on every
# boot, idempotent). Debian 12, e2-standard-2, 20 GB boot disk + the 100 GB
# pd-balanced data disk `crawler-data` (device name crawler-data).
#
# Reads instance metadata (set by infrastructure/bootstrap.sh):
#   crawler-bundle   base64 tar.gz of deploy/ (compose, postgres, chromium,
#                    host scripts and units; no tests, no CI scripts)
# and the VM's own internal IP (the reserved static one), which becomes
# VC_API_BIND. No secret is ever written to a persistent disk: deploy.sh
# reads them from Secret Manager into /run (tmpfs).
#
# Installs Docker Engine + compose/buildx (Docker's apt repo), jq, the Ops
# Agent (host metrics: disk usage for the disk alert), and two pinned,
# checksum-verified binaries: cosign (deploy.sh's verifier) and wal-g
# (mounted into the postgres container). Mounts the data disk at
# /var/lib/voyager-crawler (formatted only when it has no filesystem), which
# holds pgdata, the spool and the deploy state (§17.5). Installs and enables
# the units, then queues the first deploy.
set -euo pipefail

MD="http://metadata.google.internal/computeMetadata/v1"
OPT=/opt/voyager-crawler
DATA=/var/lib/voyager-crawler
DISK=/dev/disk/by-id/google-crawler-data
SYSTEMD_DIR="${VC_SYSTEMD_DIR:-/etc/systemd/system}"
OPS_AGENT_DIR="${VC_OPS_AGENT_DIR:-/etc/google-cloud-ops-agent}"
COSIGN_VERSION=v2.4.1
COSIGN_SHA256=8b24b946dd5809c6bd93de08033bcf6bc0ed7d336b7785787c080f574b89249b
WALG_VERSION=v3.0.9
WALG_ASSET=wal-g-pg-22.04-amd64
WALG_SHA256=d6795c663894d836ba20840bffe8970aed81bbe25d3151bd4951f6f9c362def9
export DEBIAN_FRONTEND=noninteractive

log() { echo "voyager-crawler-first-boot: $*" >&2; logger -t voyager-crawler-first-boot -- "$*" || true; }
die() { log "ERROR: $*"; exit 1; }

# md PATH → metadata value; a 404 is "", any other failure aborts (a
# transient error must never write empty settings)
md() {
  local out code
  out="$(curl -sS -w '\n%{http_code}' -H 'Metadata-Flavor: Google' "$MD/$1")" ||
    die "metadata server unreachable reading $1"
  code="${out##*$'\n'}"
  case "$code" in
    200) printf '%s' "${out%$'\n'*}" ;;
    404) ;;
    *) die "metadata read of $1 failed (HTTP $code)" ;;
  esac
}

install_packages() {
  if ! command -v docker >/dev/null || ! docker compose version >/dev/null 2>&1; then
    log "installing Docker"
    apt-get update -q
    apt-get install -yq ca-certificates curl gnupg jq unattended-upgrades
    install -m 0755 -d /etc/apt/keyrings
    curl -fsSL https://download.docker.com/linux/debian/gpg -o /etc/apt/keyrings/docker.asc
    chmod a+r /etc/apt/keyrings/docker.asc
    local codename arch
    # shellcheck source=/dev/null
    codename="$(. /etc/os-release && echo "$VERSION_CODENAME")"
    arch="$(dpkg --print-architecture)"
    echo "deb [arch=$arch signed-by=/etc/apt/keyrings/docker.asc] https://download.docker.com/linux/debian $codename stable" \
      >/etc/apt/sources.list.d/docker.list
    apt-get update -q
    apt-get install -yq docker-ce docker-ce-cli containerd.io docker-buildx-plugin docker-compose-plugin
  fi
  if ! systemctl list-unit-files google-cloud-ops-agent.service >/dev/null 2>&1; then
    log "installing the Ops Agent"
    curl -fsSL -o /tmp/add-google-cloud-ops-agent-repo.sh \
      https://dl.google.com/cloudagents/add-google-cloud-ops-agent-repo.sh
    bash /tmp/add-google-cloud-ops-agent-repo.sh --also-install
    rm -f /tmp/add-google-cloud-ops-agent-repo.sh
  fi
}

# fetch_pinned URL SHA256 DEST — download, verify, install 0755 (skipped
# when DEST already has that checksum)
fetch_pinned() {
  local url="$1" sum="$2" dest="$3" tmp
  if [[ -x "$dest" ]] && echo "$sum  $dest" | sha256sum -c - >/dev/null 2>&1; then
    return 0
  fi
  tmp="$(mktemp)"
  curl -fsSL -o "$tmp" "$url"
  echo "$sum  $tmp" | sha256sum -c - >/dev/null || { rm -f "$tmp"; die "checksum mismatch for $url"; }
  install -D -m 0755 "$tmp" "$dest"
  rm -f "$tmp"
}

install_binaries() {
  fetch_pinned "https://github.com/sigstore/cosign/releases/download/$COSIGN_VERSION/cosign-linux-amd64" \
    "$COSIGN_SHA256" /usr/local/bin/cosign
  fetch_pinned "https://github.com/wal-g/wal-g/releases/download/$WALG_VERSION/$WALG_ASSET" \
    "$WALG_SHA256" "$OPT/bin/wal-g"
}

configure_docker() {
  install -d /etc/docker
  local want
  want='{
  "log-driver": "json-file",
  "log-opts": { "max-size": "20m", "max-file": "5" },
  "ipv6": false,
  "live-restore": false
}'
  if [[ "$(cat /etc/docker/daemon.json 2>/dev/null)" != "$want" ]]; then
    printf '%s\n' "$want" >/etc/docker/daemon.json
    systemctl restart docker
  fi
  systemctl enable --now docker
  # Containers need IPv4 forwarding; the GCE image's 60-gce-network-security
  # sets it to 0 on any full sysctl reload (lesson from the app VM,
  # incident 2026-11-06). 99- wins at boot and on reload.
  printf 'net.ipv4.ip_forward=1\n' >/etc/sysctl.d/99-voyager-crawler-docker.conf
  sysctl -q -p /etc/sysctl.d/99-voyager-crawler-docker.conf || log "could not set ip_forward"
}

mount_data_disk() {
  [[ -b "$DISK" ]] || die "data disk $DISK not attached"
  if ! blkid "$DISK" >/dev/null 2>&1; then
    log "formatting the empty data disk"
    mkfs.ext4 -q -m 0 -L crawler-data -E lazy_itable_init=0,lazy_journal_init=0,discard "$DISK"
  fi
  local uuid
  uuid="$(blkid -s UUID -o value "$DISK")"
  install -d "$DATA"
  if ! grep -q "UUID=$uuid" /etc/fstab; then
    echo "UUID=$uuid $DATA ext4 discard,defaults,nofail 0 2" >>/etc/fstab
  fi
  mountpoint -q "$DATA" || mount "$DATA"
  mountpoint -q "$DATA" || die "$DATA is not mounted"
  install -d -m 0700 -o 999 -g 999 "$DATA/pgdata"
  install -d -m 0700 -o 65532 -g 65532 "$DATA/spool"
  install -d -m 0700 "$DATA/deploy"
}

# configure_ops_agent — the Ops Agent's own config (cost, go-live review 7):
#   metrics: host metrics for the disk alert, without the per-process series
#     (agent.googleapis.com/processes/*, the bulk of its billable samples);
#   logs: only the containers' json-file logs (no syslog), parsed (Docker's
#     envelope, then the role's JSON line), DEBUG/TRACE dropped on the VM, as
#     log crawler_docker (bootstrap.sh routes it to the 30-day crawler bucket).
# Restarts the agent only when the file changed.
configure_ops_agent() {
  local want
  want='# Written by first-boot.sh (deploy/host): see configure_ops_agent
metrics:
  processors:
    drop_process_metrics:
      type: exclude_metrics
      metrics_pattern:
        - agent.googleapis.com/processes/*
  service:
    pipelines:
      default_pipeline:
        receivers: [hostmetrics]
        processors: [drop_process_metrics]
logging:
  receivers:
    crawler_docker:
      type: files
      include_paths:
        - /var/lib/docker/containers/*/*-json.log
      record_log_file_path: false
  processors:
    docker_envelope:
      type: parse_json
      time_key: time
      time_format: "%Y-%m-%dT%H:%M:%S.%LZ"
    role_json:
      type: parse_json
      field: jsonPayload.log
    drop_debug:
      type: exclude_logs
      match_any:
        - jsonPayload.level =~ "(?i)^(debug|trace)$"
    severity:
      type: modify_fields
      fields:
        severity:
          move_from: jsonPayload.level
  service:
    pipelines:
      default_pipeline:
        receivers: []
      crawler_docker:
        receivers: [crawler_docker]
        processors: [docker_envelope, role_json, drop_debug, severity]'
  install -d "$OPS_AGENT_DIR"
  if [[ "$(cat "$OPS_AGENT_DIR/config.yaml" 2>/dev/null)" != "$want" ]]; then
    printf '%s\n' "$want" >"$OPS_AGENT_DIR/config.yaml"
    systemctl restart google-cloud-ops-agent || log "Ops Agent restart failed"
  fi
}

# docker_waits_for_data_disk — docker.service needs the data disk mounted
# (fstab has `nofail`, so a missing disk would otherwise let Docker start
# the stack on the boot disk's empty /var/lib/voyager-crawler)
docker_waits_for_data_disk() {
  local dir="$SYSTEMD_DIR/docker.service.d" want
  want="# Written by first-boot.sh: Docker starts only once the data disk is mounted
[Unit]
RequiresMountsFor=$DATA"
  install -d "$dir"
  if [[ "$(cat "$dir/10-voyager-crawler-data.conf" 2>/dev/null)" != "$want" ]]; then
    printf '%s\n' "$want" >"$dir/10-voyager-crawler-data.conf"
    systemctl daemon-reload
  fi
}

# Go-live ramp (docs/runbooks/go-live.md §3): a fresh install crawls only the
# sources listed in sources/ramp-1.txt (baked into the image) until the
# operator deletes the VC_VCRAWL_SOURCE_ALLOWLIST line from host.local.env.
# Decided once per data disk (the marker): deleting the line sticks, and a
# disk that already holds a database is never put back on the ramp.
RAMP_ALLOWLIST=/app/sources/ramp-1.txt
ramp_default() {
  local data="${1:-$DATA}"
  local d="$data/deploy"
  [[ -e "$d/.ramp-default" ]] && return 0
  install -d "$d"
  if [[ ! -e "$data/pgdata/PG_VERSION" ]] &&
    ! grep -q '^VC_VCRAWL_SOURCE_ALLOWLIST=' "$d/host.local.env" 2>/dev/null; then
    {
      echo "# Go-live ramp: only the sources in this file crawl (docs/runbooks/go-live.md)."
      echo "# Delete the next line (then rerun the startup script and restart) to crawl every source."
      echo "VC_VCRAWL_SOURCE_ALLOWLIST=$RAMP_ALLOWLIST"
    } >>"$d/host.local.env"
    log "fresh install: crawling only the ramp sources ($RAMP_ALLOWLIST)"
  fi
  : >"$d/.ramp-default"
}

install_bundle() {
  local b64 tmp ip
  b64="$(md instance/attributes/crawler-bundle)"
  [[ -n "$b64" ]] || die "metadata crawler-bundle is missing"
  tmp="$(mktemp -d)"
  printf '%s' "$b64" | base64 -d | tar -xzf - -C "$tmp"
  [[ -f "$tmp/compose.yaml" && -f "$tmp/host/deploy.sh" ]] || die "bundle incomplete"
  install -d "$OPT"
  # Replace everything except bin/ (the pinned binaries)
  find "$OPT" -mindepth 1 -maxdepth 1 ! -name bin -exec rm -rf {} +
  cp -a "$tmp/." "$OPT/"
  rm -rf "$tmp"
  chmod 0755 "$OPT"/host/*.sh
  chmod 0644 "$OPT/chromium/seccomp.json" "$OPT/chromium/run.sh" "$OPT/otelcol/config.yaml" \
    "$OPT/postgres/"*

  ip="$(md instance/network-interfaces/0/ip)"
  [[ "$ip" =~ ^10\.[0-9.]+$ ]] || die "unexpected internal IP '$ip'"
  # host.env: per-VM settings for compose. Owner overrides (e.g.
  # VC_LLM_ENABLED=true) go in host.local.env, which this never touches
  # (except ramp_default's one line on a fresh install).
  ramp_default
  {
    echo "# Written by first-boot.sh on every boot; put overrides in host.local.env"
    echo "VC_API_BIND=$ip"
    cat "$DATA/deploy/host.local.env" 2>/dev/null || true
  } >"$DATA/deploy/host.env"
}

install_units() {
  install -m 0644 "$OPT"/host/systemd/*.service "$OPT"/host/systemd/*.timer /etc/systemd/system/
  systemctl daemon-reload
  systemctl enable voyager-crawler.service
  systemctl enable --now voyager-crawler-deploy.timer voyager-crawler-watchdog.timer \
    voyager-crawler-basebackup.timer voyager-crawler-restore-drill.timer
}

main() {
  install_packages
  configure_ops_agent
  configure_docker
  mount_data_disk
  docker_waits_for_data_disk
  install_bundle
  install_binaries
  install_units
  log "provisioned; starting the stack and the first deploy"
  systemctl start --no-block voyager-crawler.service voyager-crawler-deploy.service
}

# deploy/tests source this file for its functions only
[[ -n "${VC_FIRST_BOOT_FUNCTIONS_ONLY:-}" ]] || main "$@"
