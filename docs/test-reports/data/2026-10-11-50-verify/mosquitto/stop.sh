#!/bin/sh
# Run from anywhere: paths come from this script's location. Needs podman and an eclipse-mosquitto
# 2.1.2 image in MOSQUITTO_IMAGE (the run used a local image, id 8c4aa03990b1). Start the broker
# with start.sh first, and remove it with stop.sh.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
IMG=${MOSQUITTO_IMAGE:?set MOSQUITTO_IMAGE to an eclipse-mosquitto 2.1.2 image}
podman rm -f v50-mosq
rm -rf "$D/work"
