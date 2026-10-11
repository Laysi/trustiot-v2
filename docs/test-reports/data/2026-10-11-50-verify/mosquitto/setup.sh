#!/bin/sh
# Run from anywhere: paths come from this script's location. Needs podman and an eclipse-mosquitto
# 2.1.2 image in MOSQUITTO_IMAGE (the run used a local image, id 8c4aa03990b1). Start the broker
# with start.sh first, and remove it with stop.sh.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
IMG=${MOSQUITTO_IMAGE:?set MOSQUITTO_IMAGE to an eclipse-mosquitto 2.1.2 image}
# Registers the gateway role from the crate's JSON and a few clients, then reads the default ACL access.
ctrl() { podman run --rm --memory 64m --network host --entrypoint mosquitto_ctrl $IMG -h 127.0.0.1 -p 18831 -u admin -P adminpw "$@"; }
echo "--- createRole from the crate's JSON"
podman run --rm --memory 64m --network host -v "$D":/m:ro,Z --entrypoint mosquitto_pub $IMG -h 127.0.0.1 -p 18831 -u admin -P adminpw -V 5 -t '$CONTROL/dynamic-security/v1' -f /m/create-role.json
sleep 1
echo "--- default ACL access"
ctrl dynsec getDefaultACLAccess
echo "--- role"
ctrl dynsec getRole trustiot-gateway
for cn in gw-1 gw-10 'gw-1.x' 'a/b' '+' '#' 'gw-+'; do
  echo "--- createClient $cn"
  ctrl dynsec createClient "$cn" -p "pw-$cn" 2>&1 | head -2
  ctrl dynsec addClientRole "$cn" trustiot-gateway 2>&1 | head -2
done
echo "--- cloud-side subscriber role used by retained.sh"
ctrl dynsec createRole svc
ctrl dynsec addRoleACL svc subscribePattern 'trustiot/v1/gw/#' allow
ctrl dynsec addRoleACL svc publishClientReceive 'trustiot/v1/gw/#' allow
ctrl dynsec createClient svc -p pw-svc
ctrl dynsec addClientRole svc svc
ctrl dynsec listClients
