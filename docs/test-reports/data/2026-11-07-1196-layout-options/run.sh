#!/usr/bin/env bash
# Measures today's conversation-page layout and each prototype of Laysi/trustiot-v2#1196 in the UI
# preview. For every name in OPTIONS (default: baseline option-1 option-2 option-1-2 option-3) it
# applies `<name>.patch` from this directory to the work tree (nothing for `baseline`), starts the
# preview, runs browser-check.mjs in the pinned Playwright image, stops the preview and reverts the
# patch (also on failure or interruption). The JSON lines go to `<name>.jsonl` here; with SHOTS_DIR
# set, the screenshots are written there as JPEG files (keep it outside the repository).
#
# Run from the repository root with a clean `frontend/` (the script refuses otherwise, so it never
# reverts someone's work); needs proto-installed Bun, podman, and
# `bun install --cwd frontend --frozen-lockfile` done. The preview takes its own port (PREVIEW_PORT,
# default 5196) and the script refuses a port that already answers. VIEWPORTS and SHOTS pass
# through to the browser script (see its header).
set -euo pipefail

DIR=docs/test-reports/data/2026-11-07-1196-layout-options
PORT=${PREVIEW_PORT:-5196}
URL=http://127.0.0.1:$PORT
IMAGE=$(grep -o 'mcr.microsoft.com/playwright:[^"]*' frontend/e2e/image.ts)
OPTIONS=${OPTIONS:-baseline option-1 option-2 option-1-2 option-3}

# Staged or unstaged changes against HEAD, or new files: the measured code must be the commit's.
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
for option in $OPTIONS; do
  if [ "$option" != baseline ]; then
    git apply "$DIR/$option.patch"
    APPLIED=$DIR/$option.patch
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
    -e PREVIEW_URL="$URL" -e OPTION="$option" -e VIEWPORTS="${VIEWPORTS:-}" -e SHOTS="${SHOTS:-1}" \
    -v "$PWD:/w:Z" -w /w "$IMAGE" node "$DIR/browser-check.mjs" > "$out" || status=1
  grep '^{' "$out" > "$DIR/$option.jsonl" || true
  if [ -n "${SHOTS_DIR:-}" ]; then
    grep '^SHOT ' "$out" | while read -r _ name data; do
      printf '%s' "$data" | base64 -d > "$SHOTS_DIR/$name.jpg"
    done
  fi
  rm -f "$out"
  echo "$option: $(grep -c '' "$DIR/$option.jsonl") lines, $(grep -c 'FAILED' "$DIR/$option.jsonl" || true) failed steps"

  stop_preview
  revert
done
exit "$status"
