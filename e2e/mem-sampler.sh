#!/usr/bin/env bash
# Memory sampler for the E2E stack (the 4 GB Spot VM budget, deploy/compose.yaml).
#
#   bash e2e/mem-sampler.sh sample OUT.tsv PROJECT   # every 2 s until killed: one
#                                                   # `docker stats` snapshot of PROJECT's
#                                                   # containers → "<t>\t<service>\t<bytes>"
#   bash e2e/mem-sampler.sh report OUT.tsv LIMIT_BYTES  # Markdown table on stdout; exit 1
#                                                   # when the counted total peak > LIMIT
#   bash e2e/mem-sampler.sh self-test
#
# "Memory" is what `docker stats` shows (the container's cgroup usage without inactive page
# cache): the closest thing to the containers' RSS that the OOM killer and the VM see.
# Counted: the services production runs (postgres, chromium, vcrawl, and the fetchers, which
# production runs inside vcrawl), plus otelcol's limit since E2E has no collector. The fakes
# and the runner are listed but not counted. The total peak is the largest sum over one
# snapshot (all containers sampled together), never a sum of peaks taken at different times.
set -euo pipefail

COUNTED_RE='^(postgres|chromium|vcrawl|fetcher-[a-z])$'
# otelcol's memory limit in deploy/compose.yaml (not part of the E2E stack).
OTELCOL_ALLOWANCE="${OTELCOL_ALLOWANCE:-$((96 * 1024 * 1024))}"

# to_bytes "123.4MiB" → 129394278
to_bytes() {
  awk -v v="$1" 'BEGIN {
    n = v; sub(/[A-Za-z]+$/, "", n); u = substr(v, length(n) + 1)
    m = 1
    if (u == "KiB") m = 1024; else if (u == "MiB") m = 1048576; else if (u == "GiB") m = 1073741824
    else if (u == "kB" || u == "KB") m = 1000; else if (u == "MB") m = 1000000; else if (u == "GB") m = 1000000000
    printf "%d\n", n * m
  }'
}

# service_of "vc-e2e-fetcher-a-1" PROJECT → fetcher-a ("" for one-off `run` containers)
service_of() {
  local n="${1#"$2"-}"
  [[ "$n" == *-run-* ]] && { echo "${n%%-run-*}"; return; }
  echo "${n%-[0-9]*}"
}

sample() {
  local out="$1" project="$2" t ids name usage
  : >"$out"
  while :; do
    t="$(date +%s)"
    ids="$(docker ps -q --filter "label=com.docker.compose.project=$project" 2>/dev/null || true)"
    if [[ -n "$ids" ]]; then
      # shellcheck disable=SC2086 # a list of ids
      while IFS=$'\t' read -r name usage; do
        [[ -n "$name" ]] || continue
        printf '%s\t%s\t%s\n' "$t" "$(service_of "$name" "$project")" "$(to_bytes "${usage%% *}")" >>"$out"
      done < <(docker stats --no-stream --format '{{.Name}}\t{{.MemUsage}}' $ids 2>/dev/null || true)
    fi
    sleep 2
  done
}

report() {
  local in="$1" limit="$2" rows total
  [[ -s "$in" ]] || { echo "no memory samples in $in"; return 2; }
  # awk (mawk on CI: no asorti) emits R|name|peak|counted rows and one T|total line.
  rows="$(awk -F'\t' -v re="$COUNTED_RE" -v allow="$OTELCOL_ALLOWANCE" '
    {
      if ($3 > peak[$2]) peak[$2] = $3
      if ($2 ~ re) { tot[$1] += $3; counted[$2] = 1 }
      seen[$2] = 1
    }
    END {
      best = 0
      for (t in tot) if (tot[t] > best) best = tot[t]
      for (s in seen) printf "R|%s|%d|%s\n", s, peak[s], (s in counted) ? "yes" : "no (test-only)"
      printf "T|%d\n", best + allow
    }' "$in")"
  total="$(sed -n 's/^T|//p' <<<"$rows")"
  echo "| container | peak MiB | counted |"
  echo "|---|---:|---|"
  grep '^R|' <<<"$rows" | sort -t'|' -k2,2 | while IFS='|' read -r _ name peak counted; do
    printf '| %s | %d | %s |\n' "$name" $((peak / 1048576)) "$counted"
  done
  printf '| otelcol (not in E2E: its limit) | %d | yes |\n' $((OTELCOL_ALLOWANCE / 1048576))
  printf '| **total peak (one snapshot, counted)** | **%d** | limit %d MiB |\n' \
    $((total / 1048576)) $((limit / 1048576))
  ((total <= limit))
}

self_test() {
  local d rc=0
  d="$(mktemp -d)"
  [[ "$(to_bytes 1.5GiB)" == 1610612736 && "$(to_bytes 512KiB)" == 524288 && "$(to_bytes 10MB)" == 10000000 ]] ||
    { echo "to_bytes broken"; rc=1; }
  [[ "$(service_of vc-e2e-fetcher-a-1 vc-e2e)" == fetcher-a && "$(service_of vc-e2e-runner-run-3f2a vc-e2e)" == runner ]] ||
    { echo "service_of broken"; rc=1; }
  # Two snapshots: the total peak is the larger snapshot sum, not the sum of the peaks.
  printf '1\tpostgres\t1000\n1\tvcrawl\t3000\n1\tfake-web\t99999\n2\tpostgres\t2000\n2\tvcrawl\t1000\n' >"$d/s.tsv"
  local out
  out="$(OTELCOL_ALLOWANCE=0 report "$d/s.tsv" 5000)" || { echo "report failed under the limit"; rc=1; }
  grep -q 'fake-web | 0 | no' <<<"$out" || { echo "fake-web not listed as test-only: $out"; rc=1; }
  if report "$d/s.tsv" 3500 >/dev/null; then echo "report passed over the limit"; rc=1; fi
  rm -rf "$d"
  ((rc == 0)) && echo "mem-sampler self-test ok"
  return "$rc"
}

case "${1:-}" in
  sample) sample "$2" "$3" ;;
  report) report "$2" "$3" ;;
  self-test) self_test ;;
  *) echo "usage: $0 sample OUT PROJECT | report IN LIMIT_BYTES | self-test" >&2; exit 2 ;;
esac
