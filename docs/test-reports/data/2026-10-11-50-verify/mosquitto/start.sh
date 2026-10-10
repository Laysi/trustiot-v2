#!/bin/sh
# Run from anywhere: paths come from this script's location. Needs podman and an eclipse-mosquitto
# 2.1.2 image in MOSQUITTO_IMAGE (the run used a local image, id 8c4aa03990b1). Start the broker
# with start.sh first, and remove it with stop.sh.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
IMG=${MOSQUITTO_IMAGE:?set MOSQUITTO_IMAGE to an eclipse-mosquitto 2.1.2 image}
# Initialises dynamic security (admin / adminpw, a throwaway local credential) and starts the broker
# on 127.0.0.1:18831 with a 128 MB memory cap.
mkdir -p "$D/work/data"
chmod -R a+rwX "$D/work"
podman run --rm --memory 128m --userns=keep-id -v "$D/work/data":/mosquitto/data:Z --entrypoint mosquitto_ctrl "$IMG" dynsec init /mosquitto/data/dynamic-security.json admin adminpw
podman run -d --name v50-mosq --memory 128m --userns=keep-id -p 127.0.0.1:18831:1883 -v "$D/work/data":/mosquitto/data:Z -v "$D/mosquitto.conf":/mosquitto/config/mosquitto.conf:ro,Z --entrypoint mosquitto "$IMG" -c /mosquitto/config/mosquitto.conf
