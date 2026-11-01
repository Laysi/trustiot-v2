#!/usr/bin/env bash
# Laysi/trustiot-v2#563, acceptance criterion 7: the method of
# docs/test-reports/data/2026-10-13-8-contract-type-changes/run-mutations.sh applied to the new
# heartbeat field. Each patch here renames a Rust field of the new type and keeps its wire name
# (serde rename), so the CBOR stays the same and only the Rust contract changes: the protocol
# library still builds, and every crate that uses the field (protocol's own tests, the agent,
# gateway-ops' tests) must fail to compile. Records exit codes and the compiler's error lines and
# reverts the tree. Run from the repository root, in a git work tree whose HEAD is the unmutated
# code.
set -uo pipefail

dir=docs/test-reports/data/2026-11-01-563-root-crl-update

# Keeps the error headline, its location line and cargo's per-crate summaries.
errors() {
  grep -E '^error(\[E[0-9]+\])?:|^ +--> |^error: could not compile|^warning: build failed' | head -n 120
}

run() {
  local label="$1"
  shift
  local log
  log="$(mktemp)"
  "$@" >"$log" 2>&1
  local code=$?
  echo "### $label"
  echo "\$ $*"
  echo "exit code: $code"
  if ((code != 0)); then
    errors <"$log"
  fi
  rm -f "$log"
  echo
}

# Each mutation is reverted with `git reset --hard`, which would also erase real changes, so
# refuse to start unless the index and the tracked files match HEAD.
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "refusing to run: tracked files or the index have uncommitted changes"
  exit 2
fi

echo "rustc: $(rustc -V)"
echo "cargo: $(cargo -V)"
echo

echo "## baseline (no mutation)"
run "baseline: acceptance criterion command" cargo check --workspace --all-targets --locked

for patch in "$dir"/*.patch; do
  name="$(basename "$patch" .patch)"
  echo "## mutation $name"
  git apply --index --whitespace=nowarn "$patch" || {
    echo "patch did not apply"
    exit 2
  }
  git diff --cached --stat | tail -n 8
  echo
  run "changed crate on its own: protocol (library)" cargo check --package protocol --lib --locked
  run "acceptance criterion command with --keep-going (lists every failing crate)" \
    cargo check --workspace --all-targets --locked --keep-going
  git reset --quiet --hard HEAD
  if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
    echo "tree not clean after revert"
    exit 2
  fi
done
echo "all mutations reverted; tree clean"
