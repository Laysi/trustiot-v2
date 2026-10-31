#!/usr/bin/env bash
# Laysi/trustiot-v2#834: run the agent's lock-wait injection test $1 times under load (test evidence; see the report).
set -uo pipefail
runs=${1:?runs}
root=$PWD
test_name=the_agent_starts_only_once_a_childs_copy_of_the_buffer_lock_is_gone
# A work directory that is new on every run, so the cleanup removes only what this script created.
work=$(mktemp -d "$root/.exp834.XXXXXX") || exit 1
# The load build's own target directory.
load_dir=$work/load
mkdir "$load_dir" || exit 1
cpus=$(nproc)
echo "injection stress: $(uname -m), $runs runs of cargo test --locked -p agent --test binary -- --exact $test_name"
cargo test --locked -p agent --test binary --no-run > "$work/build.log" 2>&1 || { tail -40 "$work/build.log"; rm -rf "${work:?}"; exit 1; }
matching=$(cargo test --locked -p agent --test binary -- --list --exact "$test_name" 2> /dev/null | grep -c ': test$')
echo "tests matching the name: $matching"
[[ $matching == 1 ]] || { echo "the name must match exactly one test"; rm -rf "${work:?}"; exit 1; }
pids=()
build_pid=
cleanup() {
  # The build loop runs in its own process group, so killing the group also stops its running cargo and rustc.
  [[ -z $build_pid ]] || kill -- "-$build_pid" 2> /dev/null
  for pid in "${pids[@]}"; do kill "$pid" 2> /dev/null; done
  pkill -P $$ 2> /dev/null
  wait 2> /dev/null
  rm -rf "${work:?}"
}
trap cleanup EXIT
# Load 1: a cold release build of another crate, again and again, in its own target directory.
LOAD_DIR=$load_dir CPUS=$cpus setsid bash -c 'while :; do rm -rf "${LOAD_DIR:?}"/*; CARGO_TARGET_DIR=$LOAD_DIR nice -n 5 cargo build --locked --release -j "$CPUS" -p identity > /dev/null 2>&1; done' &
build_pid=$!
# Load 2: one busy loop per CPU.
for _ in $(seq "$cpus"); do ( while :; do :; done ) & pids+=($!); done
sleep 20
failed=0
counts=()
for run in $(seq "$runs"); do
  out=$work/run-$run.log
  start=${EPOCHREALTIME/./}
  cargo test --locked -p agent --test binary -- --nocapture --exact "$test_name" > "$out" 2>&1
  rc=$?
  elapsed=$(( (${EPOCHREALTIME/./} - start) / 1000 ))
  secs=$(printf '%d.%03d' $((elapsed / 1000)) $((elapsed % 1000)))
  n=$(grep -o 'the wait found the lock held [0-9]* times' "$out" | head -1 | grep -o '[0-9]*')
  if [[ $rc == 0 && -n $n && $n -ge 2 ]] && grep -q 'test result: ok. 1 passed' "$out"; then
    counts+=("$n")
    echo "run $run: ok ($secs s, load $(cut -d' ' -f1 /proc/loadavg), N $n)"
  else
    failed=$((failed + 1))
    echo "run $run: FAILED ($secs s, load $(cut -d' ' -f1 /proc/loadavg), N ${n:-none}, exit $rc)"
    head -80 "$out"
  fi
done
echo "injection stress: $failed of $runs runs failed"
echo "distribution of N (times the wait found the lock held), passing runs:"
printf '%s\n' "${counts[@]}" | sort -n | uniq -c | awk '{print "  N=" $2 ": " $1 " runs"}'
exit "$failed"
