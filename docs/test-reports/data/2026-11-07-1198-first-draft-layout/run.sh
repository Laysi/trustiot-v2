#!/usr/bin/env bash
# Starts the UI preview, waits for it, runs browser-check.mjs in the pinned Playwright image and
# stops the preview. Run from the repository root of the tree to measure; needs proto-installed Bun,
# podman, an arm64 host (the image is pinned to arm64) and `bun install --cwd frontend
# --frozen-lockfile` done. The preview takes its own port (the build host is shared, so the default
# 5174 may be someone else's) and the script refuses a port that already answers, so the page the
# browser sees is this checkout's. TREE, VIEWPORTS, REPEATS, SHOTS and FRAMES pass through to the
# browser script (see its header); TREE names the tree in every JSON line, LATENCY_MS (0, 2000 or
# 10000, the toolbar's slow settings) makes every API request wait that long.
set -euo pipefail

PORT=${PREVIEW_PORT:-5198}
URL=http://127.0.0.1:$PORT
DIR=docs/test-reports/data/2026-11-07-1198-first-draft-layout
IMAGE=$(grep -o 'mcr.microsoft.com/playwright:[^"]*' frontend/e2e/image.ts)

if curl -fsS "$URL/" > /dev/null 2>&1; then
  echo "port $PORT already answers; set PREVIEW_PORT to a free one" >&2
  exit 2
fi

LOG=$(mktemp)
(cd frontend && exec bun --bun vite --mode ui-preview --host 127.0.0.1 --port "$PORT" --strictPort) > "$LOG" 2>&1 &
PREVIEW=$!
cleanup() {
  pkill -P "$PREVIEW" 2>/dev/null || true
  kill "$PREVIEW" 2>/dev/null || true
  rm -f "$LOG"
}
trap cleanup EXIT

for _ in $(seq 1 120); do
  if curl -fsS "$URL/" > /dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$URL/" > /dev/null || { cat "$LOG" >&2; exit 3; }

podman run --rm --network host --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -e PREVIEW_URL="$URL" -e TREE="${TREE:-unlabelled}" -e VIEWPORTS="${VIEWPORTS:-}" \
  -e REPEATS="${REPEATS:-2}" -e SHOTS="${SHOTS:-0}" -e FRAMES="${FRAMES:-0}" -e LATENCY_MS="${LATENCY_MS:-0}" \
  -v "$PWD:/w:Z" -w /w "$IMAGE" node "$DIR/browser-check.mjs"
