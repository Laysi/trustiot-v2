#!/usr/bin/env bash
# Runs run.sh twice on one checkout: first with the composer's two column overrides taken out (the
# state before the fix), then as committed. Run from the repository root; the checkout is a
# throwaway copy (the check queue's), because this edits Composer.tsx and restores it.
set -euo pipefail

FILE=frontend/src/chat/Composer.tsx
DIR=docs/test-reports/data/2026-11-07-1183-composer-alert-width
ORIG=$(mktemp)
cp "$FILE" "$ORIG"
trap 'cp "$ORIG" "$FILE"; rm -f "$ORIG"' EXIT

sed -i 's| className="grid-cols-\[1fr\]">|>|' "$FILE"
echo "overrides left in the file: $(grep -c 'grid-cols-\[1fr\]' "$FILE" || true)"
LABEL=before bash "$DIR/run.sh"

cp "$ORIG" "$FILE"
echo "overrides in the file: $(grep -c 'grid-cols-\[1fr\]' "$FILE")"
LABEL=after bash "$DIR/run.sh"
