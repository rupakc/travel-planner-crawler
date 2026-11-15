#!/usr/bin/env bash
# Fail when a branch touches a path its task does not own (docs/ownership.md).
#
# Usage:
#   scripts/check-ownership.sh [--task <id>] [--branch <name>] [--base <ref>]
#                              [--files-from <file>] [--map <ownership.md>]
#   scripts/check-ownership.sh --self-test
#
# The task comes from --task, else from the branch (--branch, $GITHUB_HEAD_REF, or the
# current git branch) named agent/<task>[-slug]. Changed files come from --files-from
# (one path per line), else `git diff --name-only <base>...HEAD` (base default origin/main).
# Branches not named agent/* are controller branches and are not checked.
# Exit codes: 0 ok, 1 ownership violation, 2 usage error.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

die() { echo "check-ownership: $*" >&2; exit 2; }

# Normalise a task id: p0a, p0b, t1..t18, td. Accepts task-1, task1, T1, task-d, t1-slug.
normalise_task() {
  local raw
  raw="$(printf '%s' "$1" | tr '[:upper:]' '[:lower:]')"
  raw="${raw#agent/}"
  if [[ "$raw" =~ ^(p0[ab])([-_].*)?$ ]]; then
    printf '%s\n' "${BASH_REMATCH[1]}"
  elif [[ "$raw" =~ ^(task|t)[-_]?([0-9]+|d)([-_].*)?$ ]]; then
    local n="${BASH_REMATCH[2]}"
    if [[ "$n" == d ]]; then printf 'td\n'; else printf 't%d\n' "$((10#$n))"; fi
  else
    return 1
  fi
}

# Print "<task> <glob>" lines from the ```ownership block of the map.
read_map() {
  awk '/^```ownership[[:space:]]*$/{on=1; next} on && /^```/{on=0} on' "$1" \
    | sed -e 's/#.*$//' -e 's/[[:space:]]*$//' | awk 'NF == 2'
}

# check <task> <map> <files-file>: print violations, return 1 if any.
check() {
  local task="$1" map="$2" files="$3" rules path t glob ok bad=0
  rules="$(read_map "$map")"
  [[ -n "$rules" ]] || die "no ownership block in $map"
  if ! printf '%s\n' "$rules" | awk -v t="$task" '$1 == t {f=1} END {exit !f}'; then
    die "task '$task' has no entries in $map"
  fi
  while IFS= read -r path || [[ -n "$path" ]]; do
    [[ -z "$path" ]] && continue
    ok=0
    while read -r t glob; do
      [[ "$t" == "$task" ]] || continue
      if [[ "$glob" == !* ]]; then
        # shellcheck disable=SC2053  # glob match is intended
        if [[ "$path" == ${glob#!} ]]; then ok=0; break; fi
      else
        # shellcheck disable=SC2053
        if [[ "$path" == $glob ]]; then ok=1; fi
      fi
    done <<< "$rules"
    if [[ "$ok" -ne 1 ]]; then
      echo "not owned by $task: $path"
      bad=1
    fi
  done < "$files"
  return "$bad"
}

self_test() {
  local tmp rc fails=0
  tmp="$(mktemp -d)"
  cat > "$tmp/map.md" <<'EOF'
# test map
```ownership
t1 crates/vc-policy/*
t2 crates/vc-frontier/*
t2 migrations/11[0-9][0-9]_*
td deploy/*
td !deploy/chromium/*
p0b scripts/*
```
EOF
  expect() { # expect <want-rc> <desc> <task> <paths...>
    local want="$1" desc="$2" task="$3"; shift 3
    printf '%s\n' "$@" > "$tmp/files"
    set +e; check "$task" "$tmp/map.md" "$tmp/files" > "$tmp/out" 2>&1; rc=$?; set -e
    if [[ "$rc" -eq "$want" ]]; then echo "ok   - $desc"
    else echo "FAIL - $desc (want $want, got $rc)"; sed 's/^/       /' "$tmp/out"; fails=$((fails + 1)); fi
  }
  expect 0 "owned crate path passes"            t1 crates/vc-policy/src/lib.rs crates/vc-policy/Cargo.toml
  expect 1 "fake diff touching a non-owned path exits 1" t1 crates/vc-policy/src/lib.rs crates/vc-core/src/lib.rs
  expect 1 "another crate's file fails"         t2 crates/vc-policy/src/robots.rs
  expect 0 "own migration block passes"         t2 migrations/1101_claims.sql
  expect 1 "foreign migration block fails"      t2 migrations/1201_segments.sql
  expect 1 "P0 migration fails for P1 task"     t2 migrations/0003_frontier.sql
  expect 0 "deploy path passes for td"          td deploy/host/deploy.sh
  expect 1 "negated deploy/chromium fails for td" td deploy/chromium/compose.yaml
  expect 0 "empty diff passes"                  t1

  # Task-id normalisation.
  local pair got
  for pair in "agent/p0b:p0b" "agent/task-d:td" "agent/t2-frontier:t2" "agent/task-1:t1" \
              "agent/Task10_sources:t10" "agent/t07:t7" "p0a:p0a"; do
    got="$(normalise_task "${pair%%:*}" || echo '?')"
    if [[ "$got" == "${pair##*:}" ]]; then echo "ok   - normalise ${pair%%:*} -> $got"
    else echo "FAIL - normalise ${pair%%:*}: want ${pair##*:}, got $got"; fails=$((fails + 1)); fi
  done
  if normalise_task "agent/feature-x" >/dev/null; then
    echo "FAIL - unknown branch should not normalise"; fails=$((fails + 1))
  else echo "ok   - unknown branch is rejected"; fi

  # End to end through git: a temp repo with a branch that touches a non-owned path.
  local repo="$tmp/repo"
  mkdir -p "$repo/crates/vc-policy/src" "$repo/crates/vc-core/src" "$repo/docs"
  cp "$tmp/map.md" "$repo/docs/ownership.md"
  (
    cd "$repo"
    git init -q -b main
    git -c user.name=t -c user.email=t@example.invalid commit -q --allow-empty -m init
    git checkout -q -b agent/t1-policy
    echo x > crates/vc-policy/src/lib.rs
    echo y > crates/vc-core/src/lib.rs
    git add -A
    git -c user.name=t -c user.email=t@example.invalid commit -q -m change
  )
  set +e
  (cd "$repo" && GITHUB_HEAD_REF="" "$ROOT/scripts/check-ownership.sh" --base main --map docs/ownership.md) > "$tmp/out" 2>&1
  rc=$?
  set -e
  if [[ "$rc" -eq 1 ]] && grep -q "not owned by t1: crates/vc-core/src/lib.rs" "$tmp/out"; then
    echo "ok   - git diff on agent/t1-policy touching vc-core exits 1"
  else
    echo "FAIL - git end-to-end (rc=$rc)"; sed 's/^/       /' "$tmp/out"; fails=$((fails + 1))
  fi

  rm -rf "$tmp"
  if [[ "$fails" -ne 0 ]]; then echo "self-test: $fails failure(s)"; return 1; fi
  echo "self-test: all passed"
}

main() {
  local task="" branch="" base="origin/main" files_from="" map=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --self-test) self_test; return ;;
      --task) task="${2:?}"; shift 2 ;;
      --branch) branch="${2:?}"; shift 2 ;;
      --base) base="${2:?}"; shift 2 ;;
      --files-from) files_from="${2:?}"; shift 2 ;;
      --map) map="${2:?}"; shift 2 ;;
      -h|--help) sed -n '2,13p' "${BASH_SOURCE[0]}"; return 0 ;;
      *) die "unknown argument: $1" ;;
    esac
  done
  local top
  top="$(git rev-parse --show-toplevel 2>/dev/null || echo "$ROOT")"
  [[ -n "$map" ]] || map="$top/docs/ownership.md"
  [[ -f "$map" ]] || map="$top/$map"
  [[ -f "$map" ]] || die "ownership map not found"

  if [[ -z "$task" ]]; then
    [[ -n "$branch" ]] || branch="${GITHUB_HEAD_REF:-}"
    [[ -n "$branch" ]] || branch="$(git rev-parse --abbrev-ref HEAD)"
    if [[ "$branch" != agent/* ]]; then
      echo "check-ownership: '$branch' is not an agent/* branch; nothing to check"
      return 0
    fi
    task="$(normalise_task "$branch")" || die "cannot derive a task id from branch '$branch'"
  else
    task="$(normalise_task "$task")" || die "unknown task id '$task'"
  fi

  local list
  list="$(mktemp)"
  # shellcheck disable=SC2064
  trap "rm -f '$list'" EXIT
  if [[ -n "$files_from" ]]; then
    cp "$files_from" "$list"
  else
    git diff --name-only --no-renames "$base...HEAD" > "$list"
  fi
  if check "$task" "$map" "$list"; then
    echo "check-ownership: $(grep -c . "$list" || true) changed path(s), all owned by $task"
  else
    echo "check-ownership: FAILED. Paths and owners: docs/ownership.md" >&2
    return 1
  fi
}

main "$@"
