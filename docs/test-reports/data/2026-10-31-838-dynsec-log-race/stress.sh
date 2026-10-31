#!/usr/bin/env bash
# Laysi/trustiot-v2#838: run the dynsec test that failed under load $1 times under load, starting
# no new run after $2 seconds (test evidence; see the report). Run from the repository root.
# MUTATE=1 first applies delay-log-line.patch (an experiment: the service logs a change 2 s after
# the broker answered it) and runs every test of tests/dynsec.rs once, without the load.
set -uo pipefail
runs=${1:?runs}
budget=${2:?seconds}
test=${TEST:-the_service_keeps_the_brokers_gateway_clients_in_line_with_postgres}
here=$(dirname "${BASH_SOURCE[0]}")
root=$PWD
work=$root/.exp838
mkdir -p "$work"
began=$(date +%s)
echo "stress: $(uname -m), $runs runs at most of cargo test --locked -p certs --test dynsec $test (CI=${CI:-})"

if [[ ${MUTATE:-0} == 1 ]]; then
  git apply "$here/delay-log-line.patch" || exit 1
  echo "experiment: applied delay-log-line.patch"
  cargo test --locked -p certs --test dynsec > "$work/experiment.log" 2>&1
  echo "experiment: exit $?"
  grep -E -A4 "panicked at" "$work/experiment.log" | head -20
  grep -E "^test .*(ok|FAILED)$|test result:" "$work/experiment.log"
  git apply -R "$here/delay-log-line.patch" || exit 1
  echo "experiment: reverted delay-log-line.patch"
fi

cargo test --locked -p certs --test dynsec --no-run > "$work/build.log" 2>&1 || { tail -40 "$work/build.log"; exit 1; }
cpus=$(nproc)
# The load build's own target directory, created here and removed only by this script.
load_dir=$(mktemp -d "$work/load.XXXXXX") || exit 1
pids=()
build_group=
cleanup() {
  # The load build runs in a session of its own, so its cargo and rustc processes end with it.
  if [[ -n $build_group ]]; then
    kill -TERM -- "-$build_group" 2> /dev/null
    for _ in $(seq 300); do pgrep -g "$build_group" > /dev/null || break; sleep 0.2; done
    kill -KILL -- "-$build_group" 2> /dev/null
  fi
  for pid in "${pids[@]}"; do kill "$pid" 2> /dev/null; done
  wait 2> /dev/null
  pgrep -g "$build_group" > /dev/null 2>&1 && echo "stress: load build processes still running" >&2
  rm -rf "${load_dir:?}"
}
trap cleanup EXIT
# Load 1: a cold release build of another crate, again and again, in its own target directory.
# setsid makes the loop the leader of a new process group (a background child of a
# non-interactive shell is not a group leader, so setsid does not fork and $! is that leader).
setsid bash -c 'while :; do rm -rf "${1:?}"/*; CARGO_TARGET_DIR=$1 nice -n 5 cargo build --locked --release -j "$2" -p identity > /dev/null 2>&1; done' load "$load_dir" "$cpus" &
build_group=$!
# Load 2: one busy loop per CPU.
for _ in $(seq "$cpus"); do ( while :; do :; done ) & pids+=($!); done
sleep 20
echo "load before the first run: $(cut -d' ' -f1-3 /proc/loadavg)"

failed=0
done_runs=0
for run in $(seq "$runs"); do
  if (( $(date +%s) - began > budget )); then
    echo "budget of $budget s reached"
    break
  fi
  out=$work/run-$run.log
  start=$(date +%s)
  load_start=$(cut -d' ' -f1 /proc/loadavg)
  cargo test --locked -p certs --test dynsec "$test" -- --exact > "$out" 2>&1
  code=$?
  load_end=$(cut -d' ' -f1 /proc/loadavg)
  done_runs=$((done_runs + 1))
  summary=$(grep -o 'test result: .*' "$out" | tail -1)
  if [[ $code == 0 && $summary == *"1 passed"* ]]; then
    echo "run $run: ok ($(( $(date +%s) - start )) s, load $load_start -> $load_end) $summary"
  else
    failed=$((failed + 1))
    echo "run $run: FAILED, exit $code ($(( $(date +%s) - start )) s, load $load_start -> $load_end)"
    grep -E -A4 "panicked at" "$out" | head -20
    grep -E "^test .* FAILED$|test result:" "$out"
  fi
done
echo "stress: $failed of $done_runs runs failed"
exit "$failed"
