#!/usr/bin/env bash
# Usage: node-probe.sh <probe .mjs in probe/> -- runs it with the Renovate image's node, with the
# before and after snapshots mounted read-only.
set -u
DIR=$(cd "$(dirname "$0")" && pwd)
IMAGE=ghcr.io/renovatebot/renovate:44.155.1@sha256:f5796f26088c0f5debb07c16fa9e801145e4d3ba9717d9cc22ca8cd265e2a0eb
podman run --rm --name renovate-69r-node -v "$DIR/probe:/probe:ro,Z" -v "$DIR/before:/repo-before:ro,Z" -v "$DIR/after:/repo-after:ro,Z" \
  -e LOG_LEVEL=warn --entrypoint node "$IMAGE" "/probe/$1"
status=$?
echo "exit=$status"
exit "$status"
