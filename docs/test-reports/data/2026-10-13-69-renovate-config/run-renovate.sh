#!/usr/bin/env bash
# Usage: run-renovate.sh <snapshot dir> <log file> <renovate args...>
set -u
SNAP="$1"; LOG="$2"; shift 2
IMAGE=ghcr.io/renovatebot/renovate:44.155.1@sha256:f5796f26088c0f5debb07c16fa9e801145e4d3ba9717d9cc22ca8cd265e2a0eb
timeout 1500 podman run --rm --name renovate-69r -v "$SNAP:/repo:ro,Z" -w /repo \
  -e LOG_LEVEL=debug -e LOG_FORMAT=json -e RENOVATE_BASE_DIR=/tmp/renovate \
  "$IMAGE" "$@" > "$LOG" 2>&1
status=$?
echo "exit=$status"
exit "$status"
