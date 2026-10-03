#!/usr/bin/env bash
# Run an E2E script repeatedly and stop at the first failing run.
#
# Usage: tests/e2e/repeat.sh <count> <script> [args...]
#   tests/e2e/repeat.sh 10 tests/e2e/lifecycle.sh
# The environment is passed through (see lib.sh). Each run's output goes to
# _output/e2e/<script>-<n>.log; one result line per run is printed.
set -euo pipefail

count="${1:?usage: $0 <count> <script> [args...]}"
script="${2:?usage: $0 <count> <script> [args...]}"
shift 2
logs="_output/e2e"
mkdir -p "$logs"
name="$(basename "$script" .sh)"

for i in $(seq 1 "$count"); do
  log="$logs/$name-$i.log"
  start=$SECONDS
  if "$script" "$@" >"$log" 2>&1; then
    printf '%s run %d of %d: %s (%ds)\n' "$(date -u +%T)" "$i" "$count" "$(tail -1 "$log")" "$(( SECONDS - start ))"
  else
    printf '%s run %d of %d FAILED (%ds), see %s:\n' "$(date -u +%T)" "$i" "$count" "$(( SECONDS - start ))" "$log"
    grep -E "FAIL|result" "$log" || tail -5 "$log"
    exit 1
  fi
done
echo "all $count runs passed"
