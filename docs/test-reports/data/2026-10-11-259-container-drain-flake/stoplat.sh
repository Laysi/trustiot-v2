#!/bin/bash
# Measures how long `podman stop` takes to deliver SIGTERM: the container prints the epoch
# time at which its TERM trap fires; compare with the epoch time just before `podman stop`.
IMG=docker.io/library/alpine:3.24
N=${1:-5}
for i in $(seq 1 $N); do
  name=stoplat-259-$$-$i
  podman run -d --name $name --stop-timeout 10 $IMG sh -c 'trap "echo TERM \$(date +%s.%N); exit 0" TERM; echo ready; while :; do sleep 0.05; done' >/dev/null
  while ! podman logs $name 2>/dev/null | grep -q ready; do sleep 0.05; done
  t0=$(date +%s.%N)
  podman stop $name >/dev/null
  t1=$(date +%s.%N)
  t_term=$(podman logs $name | awk '/^TERM/{print $2}')
  echo "run $i: signal_after=$(echo "$t_term - $t0" | bc) stop_total=$(echo "$t1 - $t0" | bc) load=$(cut -d' ' -f1 /proc/loadavg)"
  podman rm -f $name >/dev/null
done
