#!/usr/bin/env bash
# Laysi/trustiot-v2#834: the arm64 sets in one job that holds the whole VM (the job runs in exclusive mode).
set -uo pipefail
stress=docs/test-reports/data/2026-10-31-817-agent-buffer-lock-race/stress.sh
job=docs/test-reports/data/2026-10-31-820-agent-lock-wait-stress/stress-job.sh
injection=docs/test-reports/data/2026-10-31-834-agent-lock-wait-arm64-stress/injection-stress.sh
# Count of processes that belong to other check jobs (a count only: the process list holds host paths).
others() { ps -eo args | grep -E '[t]v2/agents/' | grep -vcF "tv2/agents/$(basename "$(dirname "$PWD")")"; }
echo "processes of other check jobs in the VM: $(others)"
[ "$(others)" -eq 0 ] || { echo "other check jobs still run in the VM; the load was not started"; exit 4; }
echo "1-minute load average before the load starts: $(cut -d' ' -f1 /proc/loadavg)"
echo "## lib set"
bash "$job" "$stress" 200
lib_rc=$?
echo "lib set exit: $lib_rc"
echo "## injection set"
bash "$injection" 50 2>&1
injection_rc=$?
echo "injection set exit: $injection_rc"
echo "processes of other check jobs in the VM: $(others)"
exit $((lib_rc != 0 || injection_rc != 0))
