#!/bin/sh
# Run from anywhere: paths come from this script's location. Needs podman and an eclipse-mosquitto
# 2.1.2 image in MOSQUITTO_IMAGE (the run used a local image, id 8c4aa03990b1). Start the broker
# with start.sh first, and remove it with stop.sh.
set -eu
D=$(cd "$(dirname "$0")" && pwd)
IMG=${MOSQUITTO_IMAGE:?set MOSQUITTO_IMAGE to an eclipse-mosquitto 2.1.2 image}
# ACL behaviour against a real Mosquitto 2.1.2 with the crate's gateway role.
pub() { # user topic [extra args]
  u="$1"; t="$2"; shift 2
  printf '%s\n' "PUB as '$u' to '$t' $*"
  podman run --rm --memory 64m --network host --entrypoint mosquitto_pub $IMG -h 127.0.0.1 -p 18831 -V 5 -q 1 -u "$u" -P "pw-$u" -i "c-$u" -t "$t" -m x -d "$@" 2>&1 | grep -E "PUBACK|Error|refused|not authori" | head -3
}
sub() { # user filter
  u="$1"; f="$2"
  printf '%s\n' "SUB as '$u' to '$f'"
  podman run --rm --memory 64m --network host --entrypoint mosquitto_sub $IMG -h 127.0.0.1 -p 18831 -V 5 -u "$u" -P "pw-$u" -i "c-$u" -t "$f" -W 1 -d 2>&1 | grep -E "SUBACK|Subscribed|Error|not authori" | head -3
}
echo "=== publish"
pub gw-1 trustiot/v1/gw/gw-1/up/telemetry
pub gw-1 trustiot/v1/gw/gw-1/up/status -r
pub gw-1 trustiot/v1/gw/gw-1/up/anything/deep/er
pub gw-1 trustiot/v1/gw/gw-1/up
pub gw-1 trustiot/v1/gw/gw-10/up/telemetry
pub gw-10 trustiot/v1/gw/gw-1/up/telemetry
pub gw-1 trustiot/v1/gw/gw-1/down/config
pub gw-1 trustiot/v1/gw/gw-1/down/telemetry-ack
pub gw-1 trustiot/v1/gw/+/up/telemetry
pub gw-1 'trustiot/v1/gw/gw-1.x/up/telemetry'
pub gw-1.x trustiot/v1/gw/gw-1.x/up/telemetry
pub gw-1.x trustiot/v1/gw/gw-1/up/telemetry
pub gw-1 'trustiot/v1/gw/gw-1/../gw-10/up/telemetry'
pub gw-1 '$SYS/broker/uptime'
pub gw-1 'other/topic'
echo "=== subscribe"
sub gw-1 trustiot/v1/gw/gw-1/down/config
sub gw-1 trustiot/v1/gw/gw-1/down/#
sub gw-1 trustiot/v1/gw/gw-1/down/+
sub gw-1 'trustiot/v1/gw/+/down/config'
sub gw-1 'trustiot/v1/gw/#'
sub gw-1 '#'
sub gw-1 '$SYS/#'
sub gw-1 trustiot/v1/gw/gw-1/up/telemetry
sub gw-1 trustiot/v1/gw/gw-10/down/config
sub gw-1 trustiot/v1/gw/gw-1.x/down/config
sub gw-1.x trustiot/v1/gw/gw-1/down/config
sub gw-1.x trustiot/v1/gw/gw-1.x/down/config
echo "=== users whose names are wildcards or contain a level separator"
pub '+' 'trustiot/v1/gw/+/up/telemetry'
pub '+' 'trustiot/v1/gw/gw-1/up/telemetry'
pub '#' 'trustiot/v1/gw/gw-1/up/telemetry'
pub 'a/b' 'trustiot/v1/gw/a/b/up/telemetry'
pub 'a/b' 'trustiot/v1/gw/gw-1/up/telemetry'
sub '+' 'trustiot/v1/gw/gw-1/down/config'
sub '+' 'trustiot/v1/gw/+/down/config'
sub '#' 'trustiot/v1/gw/gw-1/down/config'
sub 'a/b' 'trustiot/v1/gw/gw-1/down/config'
sub 'gw-+' 'trustiot/v1/gw/gw-1/down/config'
sub 'gw-+' 'trustiot/v1/gw/gw-+/down/config'
