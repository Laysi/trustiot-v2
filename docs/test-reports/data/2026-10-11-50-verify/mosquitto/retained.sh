#!/bin/sh
# Run from anywhere: paths come from this script's location. Needs podman and an eclipse-mosquitto
# 2.1.2 image in MOSQUITTO_IMAGE (the run used a local image, id 8c4aa03990b1). Start the broker
# with start.sh first, and remove it with stop.sh.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
IMG=${MOSQUITTO_IMAGE:?set MOSQUITTO_IMAGE to an eclipse-mosquitto 2.1.2 image}
# Can a gateway park retained messages on topics that are not one of the protocol's channels?
head -c 200000 /dev/zero | tr '\0' x > "$D/work/big.bin"
for n in 1 2 3; do
  podman run --rm --memory 64m --network host -v "$D/work":/m:ro,Z --entrypoint mosquitto_pub $IMG -h 127.0.0.1 -p 18831 -V 5 -q 1 -u gw-1 -P pw-gw-1 -i c-gw-1 -t trustiot/v1/gw/gw-1/up/spam/$n -r -f /m/big.bin -d 2>&1 | grep -E "PUBACK"
done
echo "--- retained messages a cloud subscriber sees under trustiot/v1/gw/gw-1/up/#"
podman run --rm --memory 64m --network host --entrypoint mosquitto_sub $IMG -h 127.0.0.1 -p 18831 -V 5 -u svc -P pw-svc -i c-svc -t 'trustiot/v1/gw/gw-1/up/#' -W 2 -v --retained-only 2>&1 | cut -c1-80
echo "--- stored retained bytes (mosquitto persistence is not enabled; count from \$SYS is not available to the gateway)"
