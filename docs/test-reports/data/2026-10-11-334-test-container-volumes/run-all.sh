#!/usr/bin/env bash
# Runs every check for #334 in one queue slot and snapshots the dangling podman volumes around
# the two measured steps. Runs from the worktree root; rcheck.sh sets CI=1.
set -uo pipefail
out="${OUT:?set OUT to a scratch directory}/data"
mkdir -p "$out"
snap() {
  podman volume ls --filter dangling=true -q | sort > "$out/dangling-$1.txt"
  podman ps -a --format '{{.Names}}' | sort > "$out/containers-$1.txt"
  echo "snapshot $1: $(wc -l < "$out/dangling-$1.txt") dangling volumes, $(podman volume ls -q | wc -l) volumes in total, $(wc -l < "$out/containers-$1.txt") containers, $(date +%T)"
}
step() {
  local name=$1
  shift
  echo "=== $name: $*"
  "$@" > "$out/step-$name.log" 2>&1
  local rc=$?
  tail -n 15 "$out/step-$name.log"
  echo "=== $name exit $rc"
  echo "$name $rc" >> "$out/exit-codes.txt"
}
: > "$out/exit-codes.txt"
step fmt cargo fmt --all --check
step clippy cargo clippy --workspace --all-targets --locked -- -D warnings
step clippy-nodefault cargo clippy -p service-kit --no-default-features --all-targets --locked -- -D warnings
snap before
step cargo-test cargo test --workspace --locked
snap after-cargo-test
step service-kit-nodefault cargo test -p service-kit --no-default-features --locked
step openapi cargo run --locked -p openapi -- --check
step deny cargo deny --locked check
step scripts-install bun install --cwd scripts --frozen-lockfile
step scripts-typecheck bun run --cwd scripts typecheck
step scripts-test bun test scripts
snap after-bun-test
step chart bun test deploy/chart
step workflows bun scripts/ci/check-workflows.ts
step secrets bun scripts/ci/check-secret-handling.ts
snap before-e2e
step e2e env TRUSTIOT_DEV_E2E=1 bun test --timeout 1500000 scripts/tests/e2e.test.ts
snap after-e2e
cat "$out/exit-codes.txt"
# Fail when any step failed, so a caller never mistakes a red run for a green one.
if awk '$2 != 0 { bad = 1 } END { exit !bad }' "$out/exit-codes.txt"; then
  echo "run-all: at least one step failed" >&2
  exit 1
fi
