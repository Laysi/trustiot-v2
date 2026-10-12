#!/usr/bin/env bash
# What podman does with a named volume mounted over one VOLUME path and an anonymous volume on
# the other (Mosquitto declares /mosquitto/data and /mosquitto/log), under rm --force --volumes
# and under run --rm. Creates and removes only trustiot-334-* objects.
set -uo pipefail
img=docker.io/library/eclipse-mosquitto@sha256:38c0da4f2ef84284d47b3b3eeea1cb3bdeabe81ee10caf0cd5c5ff61ee3ea408
named=trustiot-334-experiment-named
cleanup() {
  podman rm --force --volumes trustiot-334-exp-a trustiot-334-exp-b > /dev/null 2>&1
  podman volume rm --force "$named" > /dev/null 2>&1
}
trap cleanup EXIT
mounts() { podman inspect "$1" --format '{{range .Mounts}}{{.Type}}:{{.Name}}->{{.Destination}} {{end}}'; }

podman volume create "$named" > /dev/null
podman run --detach --name trustiot-334-exp-a --volume "$named:/mosquitto/data" --entrypoint sleep "$img" 600 > /dev/null
echo "A. run with a named volume on /mosquitto/data; mounts:"
mounts trustiot-334-exp-a | tr ' ' '\n' | sed 's/^/   /'
anon=$(podman inspect trustiot-334-exp-a --format '{{range .Mounts}}{{if eq .Destination "/mosquitto/log"}}{{.Name}}{{end}}{{end}}')
podman rm --force --time 1 trustiot-334-exp-a > /dev/null 2>&1
echo "   after plain 'rm --force': anonymous volume exists: $(podman volume exists "$anon" && echo yes || echo no)"
podman volume rm "$anon" > /dev/null

podman run --detach --name trustiot-334-exp-b --volume "$named:/mosquitto/data" --entrypoint sleep "$img" 600 > /dev/null
anon=$(podman inspect trustiot-334-exp-b --format '{{range .Mounts}}{{if eq .Destination "/mosquitto/log"}}{{.Name}}{{end}}{{end}}')
podman rm --force --volumes --time 1 trustiot-334-exp-b > /dev/null 2>&1
echo "B. after 'rm --force --volumes': anonymous volume exists: $(podman volume exists "$anon" && echo yes || echo no); named volume exists: $(podman volume exists "$named" && echo yes || echo no)"

podman run --rm --name trustiot-334-exp-a --entrypoint true "$img" > /dev/null 2>&1
echo "C. 'run --rm' of a container that exits: container exists: $(podman container exists trustiot-334-exp-a && echo yes || echo no)"
before=$(podman volume ls --filter dangling=true -q | wc -l)
podman run --rm --entrypoint true "$img" > /dev/null 2>&1
after=$(podman volume ls --filter dangling=true -q | wc -l)
echo "   dangling volumes before/after a 'run --rm' of the image: $before/$after (other agents may add some in between)"
