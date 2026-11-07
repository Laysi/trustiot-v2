#!/usr/bin/env bash
# Starts the UI preview, waits for it, runs browser-check.mjs in the pinned Playwright image and
# stops the preview (adapted from the projector walkthrough's run.sh,
# docs/test-reports/data/2026-11-07-1179-projector-walkthrough/). Run from the repository root;
# needs proto-installed Bun, podman, and `bun install --cwd frontend --frozen-lockfile` done. The
# preview takes its own port and the script refuses a port that already answers, so the page the
# browser sees is this checkout's. VIEWPORTS and SHOTS pass through to the browser script.
set -euo pipefail

PORT=${PREVIEW_PORT:-5194}
URL=http://127.0.0.1:$PORT
DIR=docs/test-reports/data/2026-11-07-1182-follow-up-scroll
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
  -e PREVIEW_URL="$URL" -e VIEWPORTS="${VIEWPORTS:-}" -e SHOTS="${SHOTS:-0}" \
  -v "$PWD:/w:Z" -w /w "$IMAGE" node "$DIR/browser-check.mjs"
