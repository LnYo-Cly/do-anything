#!/usr/bin/env bash
# Guards .github/workflows/browser-repeat.yml against the two regressions that
# made the repeat workflow fail under runner contention:
#
#   1. an unbounded libtest thread count. libtest uses one thread per core, so
#      several test threads start Chrome at the same time and Page.navigate
#      misses its 30 s budget on a contended runner;
#   2. a run step that stops recording, after every iteration, the elapsed
#      seconds, the surviving Chrome processes and the leftover temporary
#      browser profile directories.
#
# Usage: scripts/check-browser-repeat.sh [workflow-file]
#
# The optional argument checks another copy of the workflow, for example the
# revision before the thread bound was added:
#
#   git show HEAD:.github/workflows/browser-repeat.yml > /tmp/browser-repeat.yml
#   bash scripts/check-browser-repeat.sh /tmp/browser-repeat.yml   # exits 1
#
# The script needs bash and the usual text tools only; it deliberately avoids a
# YAML parser because the runners have no guaranteed one.
set -euo pipefail

readonly want_threads=2

script_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo_root=$(CDPATH='' cd -- "$script_dir/.." && pwd)
workflow=${1:-"$repo_root/.github/workflows/browser-repeat.yml"}

failures=0

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  failures=$((failures + 1))
}

pass() {
  printf 'ok: %s\n' "$*"
}

if [ ! -f "$workflow" ]; then
  printf 'FAIL: no such workflow file: %s\n' "$workflow" >&2
  exit 1
fi

printf 'check-browser-repeat: %s\n' "$workflow"

# Prints the value of KEY in a flow mapping normalised to "k=v k=v ...".
field() {
  local pair
  for pair in $2; do
    case $pair in
      "$1"=*) printf '%s' "${pair#*=}"; return 0 ;;
    esac
  done
  return 1
}

is_uint() {
  case $1 in
    '' | *[!0-9]*) return 1 ;;
    *) return 0 ;;
  esac
}

# ---------------------------------------------------------------------------
# Matrix entries: one line per entry, "os=... shard=... total_shards=... ...".
# ---------------------------------------------------------------------------
matrix_entries=$(awk '
  /^[[:space:]]*-[[:space:]]*\{.*os:[[:space:]]*/ {
    line = $0
    sub(/^[[:space:]]*-[[:space:]]*\{/, "", line)
    sub(/\}[[:space:]]*$/, "", line)
    count = split(line, fields, ",")
    entry = ""
    for (i = 1; i <= count; i++) {
      item = fields[i]
      sub(/^[[:space:]]+/, "", item)
      sub(/[[:space:]]+$/, "", item)
      key = item
      sub(/:.*/, "", key)
      value = item
      sub(/^[^:]*:[[:space:]]*/, "", value)
      entry = entry (entry == "" ? "" : " ") key "=" value
    }
    print entry
  }
' "$workflow")

entry_total=$(printf '%s\n' "$matrix_entries" | grep -c . || true)

if [ "$entry_total" -eq 0 ]; then
  fail "no matrix entries found; the repeat job must shard the suite through matrix.include flow mappings (- { os: ..., shard: ... })"
fi

# ---------------------------------------------------------------------------
# Mechanism 1: RUST_TEST_THREADS in an env block that covers every step. The
# block is job or workflow scope when it is not nested inside the steps list,
# which is what lets the --no-run build step receive the same value.
# ---------------------------------------------------------------------------
env_scopes=$(awk '
  BEGIN { steps_indent = 999999; env_indent = 0 }
  /^[[:space:]]*steps:[[:space:]]*$/ { steps_indent = index($0, "steps:") - 1 }
  /^[[:space:]]*env:[[:space:]]*$/ { env_indent = index($0, "env:") - 1 }
  /^[[:space:]]*RUST_TEST_THREADS:/ {
    value = $0
    sub(/^[^:]*:[[:space:]]*/, "", value)
    sub(/[[:space:]]+#.*$/, "", value)
    gsub(/["\047[:space:]]/, "", value)
    scope = (env_indent <= steps_indent) ? "job" : "step"
    print scope " " value
  }
' "$workflow")

job_env_threads=""
step_env_threads=""
while read -r scope value; do
  case ${scope:-} in
    job) job_env_threads=$value ;;
    step) step_env_threads=$value ;;
  esac
done <<< "$env_scopes"

if [ -n "$job_env_threads" ] && [ "$job_env_threads" != "$want_threads" ]; then
  fail "RUST_TEST_THREADS is $job_env_threads at job level, want $want_threads"
fi
if [ -n "$step_env_threads" ] && [ "$step_env_threads" != "$want_threads" ]; then
  fail "RUST_TEST_THREADS is $step_env_threads on a step, want $want_threads"
fi

# ---------------------------------------------------------------------------
# Mechanism 2: an explicit --test-threads / RUST_TEST_THREADS= shell prefix on
# the cargo command itself. Every value must agree with the job-level bound.
# ---------------------------------------------------------------------------
cli_values=$(grep -o -E -- '--test-threads=[0-9]+' "$workflow" || true)
while read -r flag; do
  [ -n "$flag" ] || continue
  if [ "${flag#*=}" != "$want_threads" ]; then
    fail "the workflow passes $flag, want --test-threads=$want_threads"
  fi
done <<< "$cli_values"

prefix_values=$(grep -o -E -- 'RUST_TEST_THREADS=[0-9]+' "$workflow" || true)
while read -r assignment; do
  [ -n "$assignment" ] || continue
  if [ "${assignment#*=}" != "$want_threads" ]; then
    fail "the workflow sets $assignment, want RUST_TEST_THREADS=$want_threads"
  fi
done <<< "$prefix_values"

# ---------------------------------------------------------------------------
# Both cargo invocations must be covered: the run step because it executes the
# suite, the --no-run build step because the same bound has to apply there too.
# ---------------------------------------------------------------------------
cargo_lines=$(grep -n -E 'cargo[[:space:]]+test[[:space:]]+--test[[:space:]]+browser' "$workflow" || true)

build_lineno=""
build_text=""
run_lineno=""
run_text=""
while IFS= read -r raw; do
  [ -n "$raw" ] || continue
  lineno=${raw%%:*}
  text=${raw#*:}
  case $text in
    *--no-run*)
      build_lineno=$lineno
      build_text=$text
      ;;
    *)
      run_lineno=$lineno
      run_text=$text
      ;;
  esac
done <<< "$cargo_lines"

line_bound() {
  case $1 in
    *--test-threads="$want_threads"* | *RUST_TEST_THREADS="$want_threads"*) return 0 ;;
  esac
  return 1
}

job_env_ok=0
if [ "$job_env_threads" = "$want_threads" ]; then
  job_env_ok=1
fi

if [ -z "$run_lineno" ]; then
  fail "no 'cargo test --test browser' run step found"
elif [ "$job_env_ok" -eq 0 ] && ! line_bound "$run_text"; then
  fail "the run step is not bounded: set RUST_TEST_THREADS: \"$want_threads\" at job level or append -- --test-threads=$want_threads"
fi

if [ -z "$build_lineno" ]; then
  fail "no 'cargo test --test browser --no-run' build step found"
elif [ "$job_env_ok" -eq 0 ] && ! line_bound "$build_text"; then
  fail "the --no-run build step does not receive the thread bound: set RUST_TEST_THREADS at job level or append -- --test-threads=$want_threads (inert while nothing runs)"
fi

run_bound=0
if [ "$job_env_ok" -eq 1 ] || line_bound "$run_text"; then
  run_bound=1
fi

# ---------------------------------------------------------------------------
# Per matrix entry: an entry may carry its own threads: / RUST_TEST_THREADS:
# override, but every entry must end up bounded by the wanted thread count.
# ---------------------------------------------------------------------------
index=0
while IFS= read -r entry; do
  index=$((index + 1))
  [ -n "$entry" ] || continue

  os=$(field os "$entry" || true)
  shard=$(field shard "$entry" || true)
  total_shards=$(field total_shards "$entry" || true)
  start=$(field start "$entry" || true)
  end=$(field end "$entry" || true)

  if [ -z "$os" ] || [ -z "$shard" ] || [ -z "$total_shards" ] || [ -z "$start" ] || [ -z "$end" ]; then
    fail "matrix entry $index is incomplete (needs os, shard, total_shards, start, end): $entry"
    continue
  fi
  if ! is_uint "$shard" || ! is_uint "$total_shards" || ! is_uint "$start" || ! is_uint "$end"; then
    fail "matrix entry $index has a non-numeric bound: $entry"
    continue
  fi
  if [ "$start" -gt "$end" ] || [ "$shard" -gt "$total_shards" ]; then
    fail "matrix entry $index has an impossible range: $entry"
    continue
  fi

  entry_threads=$(field threads "$entry" || true)
  [ -n "$entry_threads" ] || entry_threads=$(field RUST_TEST_THREADS "$entry" || true)
  if [ -n "$entry_threads" ]; then
    if [ "$entry_threads" != "$want_threads" ]; then
      fail "matrix entry $index ($os shard $shard) sets threads=$entry_threads, want $want_threads"
    fi
  elif [ "$run_bound" -eq 0 ]; then
    fail "matrix entry $index ($os shard $shard) has no thread bound"
  fi
done <<< "$matrix_entries"

if [ "$entry_total" -gt 0 ] && [ "$run_bound" -eq 1 ] && [ "$failures" -eq 0 ]; then
  pass "$entry_total matrix entries, each bounded to $want_threads test threads"
fi

# ---------------------------------------------------------------------------
# Diagnostics: every iteration must log elapsed seconds, the surviving Chrome
# processes and the leftover temporary profile directories, on every runner.
# ---------------------------------------------------------------------------
if ! grep -q -E '^[[:space:]]*print_diag\(\)[[:space:]]*\{' "$workflow"; then
  fail "the run step has no print_diag helper"
fi

diag_line=$(grep -E 'DIAG:' "$workflow" || true)
if [ -z "$diag_line" ]; then
  fail "the run step prints no DIAG line"
else
  case $diag_line in
    *duration*) : ;;
    *) fail "the DIAG line does not report the elapsed time (duration)" ;;
  esac
  case $diag_line in
    *chrome*) : ;;
    *) fail "the DIAG line does not report the surviving Chrome processes" ;;
  esac
  case $diag_line in
    *profile*) : ;;
    *) fail "the DIAG line does not report the leftover profile directories" ;;
  esac
fi

if ! grep -q -E 'uname[[:space:]]+-s' "$workflow"; then
  fail "the diagnostics do not guard the platform-specific counting with uname"
fi
if ! grep -q -F 'tasklist' "$workflow"; then
  fail "the diagnostics cannot count Chrome processes on Windows (tasklist)"
fi
if ! grep -q -F 'ps -A -o comm=' "$workflow"; then
  fail "the diagnostics cannot count Chrome processes on Linux and macOS (ps -A -o comm=)"
fi
if ! grep -q -F 'da-chrome-profile' "$workflow"; then
  fail "the diagnostics do not count the temporary da-chrome-profile directories"
fi

elapsed_marks=$(grep -c -F 'date +%s' "$workflow" || true)
if [ "$elapsed_marks" -lt 2 ]; then
  fail "the run loop does not measure the elapsed seconds with date +%s before and after the suite"
fi

if ! grep -q -E 'for[[:space:]].*seq' "$workflow"; then
  fail "the run step has no per-iteration loop; diagnostics must follow every iteration"
fi

call_lineno=$(grep -n -E '^[[:space:]]*print_diag[[:space:]]+"?\$' "$workflow" | head -n 1 | cut -d: -f1 || true)
if [ -z "$call_lineno" ]; then
  fail "print_diag is never called with the iteration number"
elif [ -n "$run_lineno" ] && [ "$call_lineno" -le "$run_lineno" ]; then
  fail "print_diag is called before the suite runs; it must report after every iteration"
fi

if [ "$failures" -ne 0 ]; then
  printf 'check-browser-repeat: FAIL (%s problem(s))\n' "$failures" >&2
  exit 1
fi

pass "per-iteration diagnostics: elapsed seconds, Chrome processes, leftover profiles"
printf 'check-browser-repeat: PASS\n'
