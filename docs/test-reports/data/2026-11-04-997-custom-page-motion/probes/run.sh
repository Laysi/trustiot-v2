#!/bin/sh
# Runs one probe in the pinned Playwright container (no server: each probe sets its own page).
# Usage:
#   MODULES=<node_modules with playwright-core> IMAGE=<Playwright image from frontend/e2e/image.ts> \
#   sh run.sh probe4.mjs > probe4.json
# The directory of this script is mounted read-only; nothing is written in the container.
here=$(cd "$(dirname "$0")" && pwd)
docker run --rm --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -v "$MODULES":/probe/node_modules:ro -v "$here":/probe/src:ro -w /probe "$IMAGE" \
  node "src/$1"
