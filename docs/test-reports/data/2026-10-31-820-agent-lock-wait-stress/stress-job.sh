#!/usr/bin/env bash
# Laysi/trustiot-v2#820: one stress job: $1 = path of stress.sh, $2 = runs.
set -uo pipefail
echo "fix present: $(grep -c 'mod test_locks' edge/agent/src/buffer/disk.rs)"
if systemd-run --user --quiet --wait true > /dev/null 2>&1; then echo "systemd user manager: yes"; else echo "systemd user manager: no"; fi
bash "$1" "$2" 2>&1
rc=$?
echo "failure details:"
for f in $(ls .exp817/run-*.log | sort -t- -k2 -n); do
  if grep -q "test result: FAILED" "$f"; then
    echo "== $f"
    grep -A3 "panicked at" "$f"
  fi
done
exit "$rc"
