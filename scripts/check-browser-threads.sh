#!/usr/bin/env bash
# Guards every workflow that runs the browser suite against an unbounded libtest
# thread count.
#
# libtest uses one thread per core, and several browser cases launch their own
# Chrome, so a four-core runner can have four browsers competing at once. Each
# case keeps its own 30 s navigation budget, which then misses on code that
# passes locally: the failure was seen on CI as `Page.navigate` not answering
# within 30000 ms in a job whose every sibling was green.
#
# Usage: scripts/check-browser-threads.sh [workflow-file ...]
#
# Without arguments it checks .github/workflows/ci.yml and
# .github/workflows/browser-repeat.yml. The script needs bash and awk only.
set -euo pipefail

readonly want_threads=2

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)

if [ "$#" -eq 0 ]; then
  set -- "$repo_root/.github/workflows/ci.yml" "$repo_root/.github/workflows/browser-repeat.yml"
fi

failures=0

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

for workflow in "$@"; do
  if [ ! -f "$workflow" ]; then
    fail "no such workflow file: $workflow"
    continue
  fi
  printf 'check-browser-threads: %s\n' "$workflow"

  # For every job that runs the browser suite, print "job<TAB>bound|unbound".
  # A job starts at a two-space key under `jobs:`; its env block is either at
  # job level (before `steps:`) or on the step that runs cargo.
  report=$(awk '
    function flush(   ) {
      if (job != "" && runs_browser) print job "\t" (bound ? "bound" : "unbound")
    }
    /^[[:space:]]*jobs:[[:space:]]*$/ { in_jobs = 1; next }
    in_jobs && /^  [A-Za-z0-9_-]+:[[:space:]]*$/ {
      flush()
      job = $0
      sub(/^[[:space:]]*/, "", job)
      sub(/:.*/, "", job)
      runs_browser = 0; bound = 0; steps_indent = 999999; next
    }
    /^[[:space:]]*steps:[[:space:]]*$/ { steps_indent = index($0, "steps:") - 1 }
    /^[[:space:]]*RUST_TEST_THREADS:/ {
      value = $0
      sub(/^[^:]*:[[:space:]]*/, "", value)
      sub(/[[:space:]]+#.*$/, "", value)
      gsub(/["\047[:space:]]/, "", value)
      indent = index($0, "RUST_TEST_THREADS:") - 1
      if (value == want && indent <= steps_indent) bound = 1
    }
    /cargo[[:space:]]+test[^\n]*--test[[:space:]]+browser/ {
      runs_browser = 1
      if ($0 ~ /--test-threads=2/ || $0 ~ /RUST_TEST_THREADS=2/) bound = 1
    }
    END { flush() }
  ' want="$want_threads" "$workflow")

  if [ -z "$report" ]; then
    fail "$workflow runs no browser suite; if that changed, update this check"
    continue
  fi

  while IFS="$(printf '\t')" read -r job state; do
    [ -n "$job" ] || continue
    case $state in
      bound) printf 'ok: job %s runs the browser suite with %s threads\n' "$job" "$want_threads" ;;
      *) fail "job $job runs the browser suite without RUST_TEST_THREADS=$want_threads or --test-threads=$want_threads" ;;
    esac
  done <<< "$report"
done

if [ "$failures" -ne 0 ]; then
  printf 'check-browser-threads: FAIL (%s problem(s))\n' "$failures" >&2
  exit 1
fi
printf 'check-browser-threads: PASS\n'
