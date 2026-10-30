#!/usr/bin/env bash
# Laysi/trustiot-v2#817: run the agent's lib tests $1 times under load (test evidence; see the report).
set -uo pipefail
runs=${1:?runs}
root=$PWD
mkdir -p "$root/.exp817"
# The load build's own target directory, created here and removed only by this script.
load_dir=$(mktemp -d "$root/.exp817/load.XXXXXX") || exit 1
cpus=$(nproc)
echo "stress: $(uname -m), $runs runs of cargo test --locked -p agent --lib"
cargo test --locked -p agent --lib --no-run > "$root/.exp817/build.log" 2>&1 || { tail -40 "$root/.exp817/build.log"; exit 1; }
pids=()
cleanup() {
  for pid in "${pids[@]}"; do kill "$pid" 2> /dev/null; done
  pkill -P $$ 2> /dev/null
  wait 2> /dev/null
  rm -rf "${load_dir:?}"
}
trap cleanup EXIT
# Load 1: a cold release build of another crate, again and again, in its own target directory.
( while :; do rm -rf "${load_dir:?}"/*; CARGO_TARGET_DIR=$load_dir nice -n 5 cargo build --locked --release -j "$cpus" -p identity > /dev/null 2>&1; done ) &
pids+=($!)
# Load 2: one busy loop per CPU.
for _ in $(seq "$cpus"); do ( while :; do :; done ) & pids+=($!); done
sleep 20
if [[ ${EXP:-0} == 1 ]]; then
  echo "experiments under load:"
  cargo test --locked -p agent --lib experiment_817 -- --nocapture --test-threads=2 2>&1 | grep -E "EXP817|test result|panicked"
fi
failed=0
for run in $(seq "$runs"); do
  out=$root/.exp817/run-$run.log
  start=$(date +%s)
  if cargo test --locked -p agent --lib -- ${TEST_ARGS:-} > "$out" 2>&1; then
    echo "run $run: ok ($(( $(date +%s) - start )) s, load $(cut -d' ' -f1 /proc/loadavg)) $(grep -o 'test result: .*' "$out" | head -1)"
  else
    failed=$((failed + 1))
    echo "run $run: FAILED ($(( $(date +%s) - start )) s, load $(cut -d' ' -f1 /proc/loadavg))"
    grep -E "^test .* FAILED$|panicked at|WouldBlock|in use by another|waited|test result:" "$out" | head -30
  fi
done
echo "stress: $failed of $runs runs failed"
exit "$failed"
