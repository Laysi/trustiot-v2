#!/usr/bin/env bash
# Bounded CPU load for the under-load reproduction of Laysi/trustiot-v2#275.
#   load.sh start <workers> <max-seconds>   start busy loops that stop by themselves
#   load.sh stop                            kill them now
# Every worker is wrapped in timeout(1), so a forgotten load ends after <max-seconds>.
set -euo pipefail
dir=$(dirname "$0")
pidfile="$dir/load.pids"
case "${1:-}" in
  start)
    workers=${2:?workers}
    seconds=${3:?seconds}
    : > "$pidfile"
    for _ in $(seq "$workers"); do
      setsid timeout "$seconds" sh -c 'while :; do :; done' > /dev/null 2>&1 &
      echo $! >> "$pidfile"
    done
    echo "started $workers busy loops for at most ${seconds}s"
    ;;
  stop)
    if [ -f "$pidfile" ]; then
      while read -r pid; do kill -- "-$pid" 2> /dev/null || kill "$pid" 2> /dev/null || true; done < "$pidfile"
      rm -f "$pidfile"
    fi
    echo stopped
    ;;
  *) echo "usage: $0 start <workers> <seconds> | stop" >&2; exit 2 ;;
esac
