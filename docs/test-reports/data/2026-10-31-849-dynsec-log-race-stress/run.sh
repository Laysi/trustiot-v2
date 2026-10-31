#!/usr/bin/env bash
# Laysi/trustiot-v2#849: run one set of the dynsec load test through the #838 stress.sh.
# Usage (repository root, CI=1): run.sh <after|before|after-exclusive> <runs> <seconds>
#   after            the tree as it is
#   before           first replaces services/certs/tests/dynsec.rs with the file that sits in
#                    .exp849-before/dynsec.rs (main 1da444a0's version, before the #838 fix); the
#                    replacement lives only in the job's own copy and is never committed
#   after-exclusive  as "after", for a job that holds a whole build VM: it counts the processes of
#                    other check jobs before and after, and refuses to start the load when any exist
set -uo pipefail
mode=${1:?mode}
runs=${2:?runs}
seconds=${3:?seconds}
stress=docs/test-reports/data/2026-10-31-838-dynsec-log-race/stress.sh
test_file=services/certs/tests/dynsec.rs

# Number of processes that belong to other check jobs in this VM (a count only: the list holds paths).
others() { ps -eo args | grep -E '[t]v2/agents/' | grep -vcF "tv2/agents/$(basename "$(dirname "$PWD")")"; }

case $mode in
  before)
    cp .exp849-before/dynsec.rs "$test_file" || exit 1
    echo "before-fix test file: sha256 $(sha256sum "$test_file" | cut -d' ' -f1)"
    ;;
  after | after-exclusive) echo "tree test file: sha256 $(sha256sum "$test_file" | cut -d' ' -f1)" ;;
  *) echo "unknown mode $mode" >&2; exit 2 ;;
esac

if [[ $mode == after-exclusive ]]; then
  echo "processes of other check jobs in the VM: $(others)"
  echo "1-minute load average before the load: $(cut -d' ' -f1 /proc/loadavg)"
  [ "$(others)" -eq 0 ] || { echo "other check jobs still run in the VM; the load was not started"; exit 4; }
fi

bash "$stress" "$runs" "$seconds"
code=$?

if [[ $mode == after-exclusive ]]; then
  echo "processes of other check jobs in the VM: $(others)"
fi
exit "$code"
