#!/usr/bin/env bash
# docker compose for the crawler stack, with the host's env files.
# Used by the systemd units, and by hand: sudo /opt/voyager-crawler/host/vc-compose.sh ps
set -euo pipefail
# shellcheck source=deploy/host/lib.sh
. "$(dirname "$(readlink -f "$0")")/lib.sh"
vc_compose "$@"
