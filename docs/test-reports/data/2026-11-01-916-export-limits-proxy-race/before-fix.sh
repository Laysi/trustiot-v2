#!/usr/bin/env bash
# Evidence for Laysi/trustiot-v2#916 (docs/test-reports/2026-11-01-916-export-limits-proxy-race.md): run from the repository root of main at afef49fe with instrument-before-fix.patch applied, with CI=1.
# Exits nonzero when the injected delay does not reproduce the failure or a run without it fails.
set -u -o pipefail
failed=0
fail() { echo "FAILED: $1"; failed=1; }
echo "== build"
bin=$(cargo test --locked -p lab-dataset --test export_limits --no-run 2>&1 | sed -n 's/.*Executable .*(\(.*\))$/\1/p' | tail -n 1)
[ -x "$bin" ] || { echo "FAILED: no test binary"; exit 1; }
echo "binary: ${bin##*/}"
echo "== 1. one read's answer recorded late (read 2), the CI failure's test"
out=$(LAB_PROXY_DELAY_READ=2 "$bin" a_row_written_into_a_window_before_its_read_is_refused --nocapture 2>&1); code=$?
printf '%s\n' "$out" | grep -E "PROXY|panicked|Exported|test result|rows were"
# The reproduction is the CI failure: the export is not refused and exports all 240 rows.
if [ "$code" -eq 0 ] || ! printf '%s\n' "$out" | grep -q "rows: 240"; then
  fail "the injected delay did not reproduce the failure"
fi
echo "== 2. no injected delay, 10 rounds of 2 parallel runs of the whole binary"
mkdir -p evidence-logs
for round in $(seq 1 10); do
  "$bin" --nocapture > "evidence-logs/r$round-a.log" 2>&1 & a=$!
  "$bin" --nocapture > "evidence-logs/r$round-b.log" 2>&1 & b=$!
  wait "$a"; ea=$?; wait "$b"; eb=$?
  echo "round $round: exit $ea $eb, $(grep -h 'test result' evidence-logs/r$round-*.log | tr '\n' ' ')"
  [ "$ea$eb" = "00" ] || fail "round $round"
done
echo "rows requests seen: $(cat evidence-logs/*.log | grep -c 'PROXY rows request')"
echo "misnumbered: $(cat evidence-logs/*.log | grep -c MISNUMBER)"
grep -h MISNUMBER evidence-logs/*.log | sort | uniq -c
exit "$failed"
