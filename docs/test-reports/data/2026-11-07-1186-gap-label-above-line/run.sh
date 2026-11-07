#!/usr/bin/env bash
# Starts the UI preview, runs check.mjs in the pinned Playwright image and stops the preview. Run
# from the repository root; needs proto-installed Bun, podman, and `bun install --cwd frontend
# --frozen-lockfile` done. Same method as the 1179 walkthrough's run.sh.
#   CASE=repro  chiller-2 over 7 days and boiler-1 over 24 hours (default)
#   CASE=demo   cold-room-1 over 1 hour with a reading every minute and none for the last five:
#               the run changes frontend/src/test/fake-devices.ts for its duration and restores it
#   LABEL=<word> tags every output line (use `before` on a checkout without the fix)
set -euo pipefail

PORT=${PREVIEW_PORT:-5197}
URL=http://127.0.0.1:$PORT
DIR=docs/test-reports/data/2026-11-07-1186-gap-label-above-line
FIXTURE=frontend/src/test/fake-devices.ts
IMAGE=$(grep -o 'mcr.microsoft.com/playwright:[^"]*' frontend/e2e/image.ts)

if curl -fsS "$URL/" > /dev/null 2>&1; then
  echo "port $PORT already answers; set PREVIEW_PORT to a free one" >&2
  exit 2
fi

LOG=$(mktemp)
BACKUP=$(mktemp)
cp "$FIXTURE" "$BACKUP"
PREVIEW=
cleanup() {
  [ -z "$PREVIEW" ] || pkill -P "$PREVIEW" 2>/dev/null || true
  [ -z "$PREVIEW" ] || kill "$PREVIEW" 2>/dev/null || true
  cp "$BACKUP" "$FIXTURE"
  rm -f "$LOG" "$BACKUP"
}
trap cleanup EXIT

if [ "${CASE:-repro}" = demo ]; then
  sed -i 's/const step = 10 \* 60_000;/const step = 60_000;/' "$FIXTURE"
  sed -i 's|previewReadings(now, (h) => float(4 + 1.2 \* Math.sin(h / 3)))|previewReadings(now, (h) => float(4 + 1.2 * Math.sin(h / 3)), { newest: 5 / 60 })|' "$FIXTURE"
  grep -q 'newest: 5 / 60' "$FIXTURE" || { echo "the fixture did not change as expected" >&2; exit 4; }
fi

(cd frontend && exec bun --bun vite --mode ui-preview --host 127.0.0.1 --port "$PORT" --strictPort) > "$LOG" 2>&1 &
PREVIEW=$!

for _ in $(seq 1 120); do
  if curl -fsS "$URL/" > /dev/null 2>&1; then break; fi
  sleep 1
done
curl -fsS "$URL/" > /dev/null || { cat "$LOG" >&2; exit 3; }

podman run --rm --network host --shm-size 1g --cap-drop ALL --security-opt no-new-privileges \
  -e PREVIEW_URL="$URL" -e LABEL="${LABEL:-after}" -e CASE="${CASE:-repro}" \
  -v "$PWD:/w:Z" -w /w "$IMAGE" node "$DIR/check.mjs"
