#!/bin/sh
# Runs e2e.mjs in the pinned Playwright container against a `services/web` that is already
# serving a copy of frontend/dist (see the report's 「指令」). Usage:
#   WORK=<dir with e2e.mjs, package.json, node_modules> OUT=<empty output dir> FIXTURES=<fixtures.json> \
#   PROXY=<dir with proxy.mjs> IMAGE=<Playwright image from frontend/e2e/image.ts> PORT=29197 sh run.sh
# proxy.mjs forwards localhost:$PORT in the container to host.docker.internal:$PORT, so the
# browser reaches the host's services/web under a `localhost` origin.
docker run --rm --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -v "$WORK":/work:ro -v "$OUT":/out -v "$PROXY":/proxy:ro -v "$FIXTURES":/fixtures.json:ro -w /work \
  -e E2E_BASE_URL="http://localhost:${PORT:-29197}" -e E2E_OUT=/out -e E2E_FIXTURES=/fixtures.json "$IMAGE" \
  sh -c 'node /proxy/proxy.mjs & node e2e.mjs'
