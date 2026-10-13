#!/usr/bin/env bash
# Usage: node-probe-repo.sh <probe .mjs in probe/> <repository snapshot dir> [deps json in probe/]
# Runs the probe with the Renovate image's node, the snapshot mounted read-only at /repo.
set -u
DIR=$(cd "$(dirname "$0")" && pwd)
IMAGE=ghcr.io/renovatebot/renovate:44.155.1@sha256:f5796f26088c0f5debb07c16fa9e801145e4d3ba9717d9cc22ca8cd265e2a0eb
podman run --rm --name renovate-69r-node -v "$DIR/probe:/probe:ro,Z" -v "$2:/repo:ro,Z" \
  -e LOG_LEVEL=warn -e "PROBE_DEPS=/probe/${3:-branch-deps.json}" --entrypoint node "$IMAGE" "/probe/$1"
status=$?
echo "exit=$status"
exit "$status"
