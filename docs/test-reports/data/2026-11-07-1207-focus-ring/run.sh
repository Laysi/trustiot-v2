#!/usr/bin/env bash
# Measures the keyboard focus outline of Laysi/trustiot-v2#1207 in the UI preview, before and after
# the fix, on the same tree. `before` applies `before.patch` (the fix reverted: source files only)
# to the work tree; `after` measures the tree as it is. For each mode it starts the preview, runs
# browser-check.mjs in the pinned Playwright image, stops the preview and reverts the patch (also
# on failure or interruption). The JSON lines go to `<mode>.jsonl` here; with SHOTS_DIR set, the
# screenshots are written there as JPEG files (keep it outside the repository).
#
# Run from the repository root with a clean `frontend/` (the script refuses otherwise, so it never
# reverts someone's work); needs proto-installed Bun, podman, and
# `bun install --cwd frontend --frozen-lockfile` done. The preview takes its own port (PREVIEW_PORT,
# default 5207) and the script refuses a port that already answers. With SHOTS_STDOUT=1 the `SHOT`
# lines (name and base64 JPEG) are also printed on stdout, for a run whose files stay on another host.
set -euo pipefail

DIR=docs/test-reports/data/2026-11-07-1207-focus-ring
PORT=${PREVIEW_PORT:-5207}
URL=http://127.0.0.1:$PORT
IMAGE=$(grep -o 'mcr.microsoft.com/playwright:[^"]*' frontend/e2e/image.ts)
MODES=${MODES:-before after}

if ! git diff --quiet HEAD -- frontend || [ -n "$(git ls-files --others --exclude-standard -- frontend)" ]; then
  echo "frontend/ has changes; commit or set them aside first" >&2
  exit 2
fi
if curl -fsS "$URL/" > /dev/null 2>&1; then
  echo "port $PORT already answers; set PREVIEW_PORT to a free one" >&2
  exit 2
fi
if [ -n "${SHOTS_DIR:-}" ]; then mkdir -p "$SHOTS_DIR"; fi

PREVIEW=
APPLIED=
LOG=$(mktemp)
stop_preview() {
  if [ -n "$PREVIEW" ]; then
    pkill -P "$PREVIEW" 2>/dev/null || true
    kill "$PREVIEW" 2>/dev/null || true
    wait "$PREVIEW" 2>/dev/null || true
    PREVIEW=
  fi
}
revert() {
  if [ -n "$APPLIED" ]; then
    git apply -R "$APPLIED"
    APPLIED=
  fi
}
cleanup() {
  stop_preview
  revert
  rm -f "$LOG"
}
trap cleanup EXIT

status=0
for mode in $MODES; do
  if [ "$mode" = before ]; then
    git apply "$DIR/before.patch"
    APPLIED=$DIR/before.patch
  fi
  (cd frontend && exec bun --bun vite --mode ui-preview --host 127.0.0.1 --port "$PORT" --strictPort) > "$LOG" 2>&1 &
  PREVIEW=$!
  for _ in $(seq 1 120); do
    if curl -fsS "$URL/" > /dev/null 2>&1; then break; fi
    sleep 1
  done
  curl -fsS "$URL/" > /dev/null || { cat "$LOG" >&2; exit 3; }

  out=$(mktemp)
  podman run --rm --network host --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
    -e PREVIEW_URL="$URL" -e MODE="$mode" \
    -v "$PWD:/w:Z" -w /w "$IMAGE" node "$DIR/browser-check.mjs" > "$out" || status=1
  grep '^{' "$out" > "$DIR/$mode.jsonl" || true
  if [ -n "${SHOTS_DIR:-}" ]; then
    grep '^SHOT ' "$out" | while read -r _ name data; do
      printf '%s' "$data" | base64 -d > "$SHOTS_DIR/$name.jpg"
    done
  fi
  if [ -n "${SHOTS_STDOUT:-}" ]; then grep '^SHOT ' "$out" || true; fi
  rm -f "$out"
  echo "$mode:$(grep -c '' "$DIR/$mode.jsonl") lines, $(grep -c 'FAILED' "$DIR/$mode.jsonl" || true) failed steps"

  stop_preview
  revert
done
exit "$status"
