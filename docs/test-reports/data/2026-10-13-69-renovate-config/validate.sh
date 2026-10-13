#!/usr/bin/env bash
# Usage: validate.sh <snapshot dir> -- runs renovate-config-validator --strict on the repo config.
set -u
IMAGE=ghcr.io/renovatebot/renovate:44.155.1@sha256:f5796f26088c0f5debb07c16fa9e801145e4d3ba9717d9cc22ca8cd265e2a0eb
podman run --rm --name renovate-69r-validate -v "$1:/repo:ro,Z" -w /repo --entrypoint renovate-config-validator "$IMAGE" --strict
status=$?
echo "exit=$status"
exit "$status"
