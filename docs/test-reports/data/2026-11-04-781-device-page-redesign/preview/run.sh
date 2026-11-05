#!/usr/bin/env bash
# One-off: build the preview, serve it with the product's CSP headers, take the screenshots in the
# pinned Playwright image, and print the result files (tar, base64) between markers.
set -euo pipefail
cd frontend
bun install --frozen-lockfile > /dev/null
bun --bun vite build --config .preview-781/vite.config.ts 2>&1 | tail -3
PORT=28911 bun .preview-781/serve.ts &
SERVER=$!
trap 'kill $SERVER' EXIT
sleep 2
OUT="$PWD/../shots-out"
mkdir -p "$OUT"
IMAGE="mcr.microsoft.com/playwright:v1.64.1-noble@sha256:47b510dd9230bf0b9bfc9b20102241e577ea7a99334ab4c3ca19f19770e9b8ea"
podman run --rm --network host --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -v "$PWD/node_modules:/pw:ro,Z" -v "$PWD/.preview-781:/work:ro,Z" -v "$OUT:/out:Z" -w /work \
  -e OUT=/out -e ORIGIN=http://127.0.0.1:28911 ${ONLY:+-e ONLY=$ONLY} ${VARIANTS:+-e VARIANTS=$VARIANTS} \
  "$IMAGE" node shots.mjs
du -sh "$OUT"
echo "=====BEGIN-SHOTS====="
tar czf - -C "$OUT" . | base64
echo "=====END-SHOTS====="
