#!/usr/bin/env bash
# Evidence for Laysi/trustiot-v2#916 (docs/test-reports/2026-11-01-916-export-limits-proxy-race.md): run from the repository root of the fixed branch, with CI=1 and this directory's inject-after-fix.patch copied to the root as inject-916.patch.
# Exits nonzero when any check fails.
set -u -o pipefail
failed=0
fail() { echo "FAILED: $1"; failed=1; }
echo "== fmt"
if cargo fmt --all --check; then echo "fmt ok"; else fail "cargo fmt --check"; fi
echo "== clippy"
cargo clippy --locked -p lab-dataset --all-targets -- -D warnings 2>&1 | tail -n 2 || fail "clippy"
echo "== 20 sequential runs: cargo test --locked -p lab-dataset --test export_limits"
pass=0
for i in $(seq 1 20); do
  out=$(cargo test --locked -p lab-dataset --test export_limits 2>&1); code=$?
  line=$(printf '%s\n' "$out" | grep 'test result' | tail -n 1)
  echo "run $i: exit $code, $line"
  if [ "$code" -eq 0 ]; then
    pass=$((pass + 1))
  else
    printf '%s\n' "$out" | grep -E "panicked|FAILED|failures:" | head -n 20
  fi
done
echo "sequential passed: $pass/20"
[ "$pass" -eq 20 ] || fail "sequential runs"
echo "== 10 rounds of 3 parallel runs (load)"
bin=$(cargo test --locked -p lab-dataset --test export_limits --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\))$/\1/p' | tail -n 1)
[ -x "$bin" ] || { fail "no test binary"; exit 1; }
ppass=0
for round in $(seq 1 10); do
  "$bin" > p-a.log 2>&1 & a=$!
  "$bin" > p-b.log 2>&1 & b=$!
  "$bin" > p-c.log 2>&1 & c=$!
  wait "$a"; ea=$?; wait "$b"; eb=$?; wait "$c"; ec=$?
  echo "round $round: exit $ea $eb $ec"
  for e in $ea $eb $ec; do [ "$e" -eq 0 ] && ppass=$((ppass + 1)); done
  [ "$ea$eb$ec" = "000" ] || grep -hE "panicked|FAILED" p-*.log | head -n 20
done
echo "parallel passed: $ppass/30"
[ "$ppass" -eq 30 ] || fail "parallel runs"
echo "== injected: read 2's task held 500 ms after the client has its answer"
if { git apply inject-916.patch || patch -p1 < inject-916.patch; } > /dev/null; then
  echo "injection applied"
  LAB_PROXY_DELAY_READ=2 cargo test --locked -p lab-dataset --test export_limits a_row_written_into_a_window_before_its_read_is_refused -- --nocapture 2>&1 \
    | grep -E "PROXY|panicked|Exported|test result|refused:" || fail "injected test"
  LAB_PROXY_DELAY_READ=2 cargo test --locked -p lab-dataset --test export_limits 2>&1 \
    | grep -E "test result|panicked" || fail "injected test file"
else
  fail "injection patch"
fi
exit "$failed"
