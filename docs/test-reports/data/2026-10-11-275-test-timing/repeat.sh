#!/usr/bin/env bash
# repeat.sh <label> <runs> <command...>: run a command <runs> times from the current directory,
# keep each run's output in <label>-<n>.log next to this script and print one result line per run.
set -u
dir=$(dirname "$0")
label=$1
runs=$2
shift 2
fail=0
for n in $(seq "$runs"); do
  start=$(date +%s)
  "$@" > "$dir/$label-$n.log" 2>&1
  code=$?
  end=$(date +%s)
  [ "$code" -ne 0 ] && fail=$((fail + 1))
  echo "$label run $n: exit $code, $((end - start)) s, load $(cut -d' ' -f1 /proc/loadavg)"
done
echo "$label: $fail of $runs runs failed"
