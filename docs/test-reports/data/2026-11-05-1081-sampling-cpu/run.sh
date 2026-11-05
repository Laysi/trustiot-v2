#!/bin/sh
# Runs probe.mjs in the pinned Playwright container (frontend/e2e/image.ts), from the repository
# root, after:
#   bun install --cwd frontend --frozen-lockfile
#   bun run --cwd frontend build
#   bun frontend/e2e/custom-page/build.ts
# Usage: sh docs/test-reports/data/2026-11-05-1081-sampling-cpu/run.sh '<cases JSON>'
# Mounts are read-only (relabelled for SELinux: run it on a copy of the repository, as the build
# host does); nothing is written in the container. Uses podman, or docker without it.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../../.." && pwd)
image=$(sed -n 's/^  "\(mcr\.microsoft\.com\/playwright:[^"]*\)";$/\1/p' "$root/frontend/e2e/image.ts")
[ -n "$image" ] || { echo "no Playwright image in frontend/e2e/image.ts" >&2; exit 1; }
engine=$(command -v podman || command -v docker)
"$engine" run --rm --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -v "$root/frontend/node_modules":/probe/node_modules:ro,z \
  -v "$root/frontend/dist":/probe/dist:ro,z \
  -v "$root/frontend/e2e/custom-page/dist":/probe/harness:ro,z \
  -v "$here":/probe/src:ro,z -w /probe "$image" \
  node src/probe.mjs "$1"
