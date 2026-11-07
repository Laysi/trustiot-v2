#!/usr/bin/env bash
# Runs the frontend test shards 2 and 4 of 8 (split as `bun run --cwd frontend test` and CI split
# them) RUNS times each, default 6, while the test process and two busy loops share two cores, so
# its timers and React's scheduler interleave the way they do on a loaded runner. Prints each
# failed test and each shard's time (Laysi/trustiot-v2#1185: the CI failures of its pull request).
# GITHUB_ACTIONS is set as on CI, which only adds annotations to bun's output.
# Exits 1 when a test failed; exits 2, printing the shard's output, when a shard did not run, and
# when a busy loop stopped, so that not every run was contended.
# Run from the repository root on Linux (it uses taskset), after
# `bun install --cwd frontend --frozen-lockfile`. The shards hold this tree's files; a tree with
# other test files groups them differently.
set -euo pipefail

runs=${RUNS:-6}
taskset -c 0,1 true || { echo "contention.sh needs cores 0 and 1" >&2; exit 2; }
loops=()
for core in 0 1; do
  taskset -c "$core" sh -c 'while :; do :; done' &
  loops+=("$!")
done
trap 'kill "${loops[@]}" 2> /dev/null || true' EXIT

failed=0
for run in $(seq 1 "$runs"); do
  for shard in 2 4; do
    status=0
    out=$(cd frontend && GITHUB_ACTIONS=true taskset -c 0,1 bun test --shard="$shard/8" 2>&1) || status=$?
    # A shard that ran ends with bun's summary, failed tests or not.
    if ! grep -qE '^Ran [0-9]+ tests' <<< "$out"; then
      printf '%s\n' "$out" >&2
      echo "run $run shard $shard did not run (exit $status)" >&2
      exit 2
    fi
    if [ "$status" -ne 0 ]; then failed=1; fi
    grep -E '^\(fail\)|^ *[0-9]+ fail$|^Ran [0-9]+ tests' <<< "$out" | sort -u | sed "s/^/run $run shard $shard: /"
  done
done

for loop in "${loops[@]}"; do
  if ! kill -0 "$loop" 2> /dev/null; then
    echo "a busy loop stopped, so not every run was contended" >&2
    exit 2
  fi
done
exit "$failed"
