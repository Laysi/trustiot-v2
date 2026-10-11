#!/bin/sh
# Test double for the regression run: delays `podman stop` before it signals the container, as a
# loaded machine does. STOP_DELAY is in seconds (default 0).
if [ "$1" = stop ]; then
  sleep "${STOP_DELAY:-0}"
fi
exec /usr/bin/podman "$@"
