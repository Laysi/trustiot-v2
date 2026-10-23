#!/usr/bin/env bash
# Applies each mutation patch in this directory to a clean tree, runs the rust job's clippy step
# and the acceptance criterion's cargo check, records exit codes and the compiler's error lines,
# and reverts the tree. Run from the repository root, in a git work tree whose HEAD is the
# unmutated code (Laysi/trustiot-v2#8, acceptance criterion 2). p2 was added on 2026-10-21 for
# edge/agent, the first crate that depends on protocol (Laysi/trustiot-v2#86, acceptance
# criterion 5).
set -uo pipefail

dir=docs/test-reports/data/2026-10-13-8-contract-type-changes

# Keeps the error headline, its location line and cargo's per-crate summaries.
errors() {
  grep -E '^error(\[E[0-9]+\])?:|^ +--> |^warning: build failed' | head -n 120
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

check_all() {
  # $1: crate whose own build must stay green, $2: the cargo target selection for it
  local own="$1"
  shift
  run "changed crate on its own: $own" cargo check --package "$own" "$@" --locked
  run "ci.yml rust job, clippy step" cargo clippy --workspace --all-targets --locked -- -D warnings
  run "acceptance criterion command" cargo check --workspace --all-targets --locked
  run "acceptance criterion command with --keep-going (lists every failing crate)" \
    cargo check --workspace --all-targets --locked --keep-going
}

# Each mutation is reverted with `git reset --hard`, which would also erase real changes, so
# refuse to start unless the index and the tracked files match HEAD.
if [[ -n "$(git status --porcelain --untracked-files=no)" ]]; then
  echo "refusing to run: tracked files or the index have uncommitted changes"
  exit 2
fi

echo "rustc: $(rustc -V)"
echo "cargo: $(cargo -V)"
echo "base commit: $(git rev-parse --short HEAD)"
echo

echo "## baseline (no mutation)"
run "baseline: acceptance criterion command" cargo check --workspace --all-targets --locked
run "baseline: ci.yml rust job, clippy step" cargo clippy --workspace --all-targets --locked -- -D warnings

for patch in "$dir"/*.patch; do
  name="$(basename "$patch" .patch)"
  echo "## mutation $name"
  git apply --index --whitespace=nowarn "$patch" || {
    echo "patch did not apply"
    exit 2
  }
  git diff --cached --stat | tail -n 8
  echo
  case "$name" in
    p[0-9]*) check_all protocol --all-targets ;;
    *) check_all contract --all-targets ;;
  esac
  git reset --quiet --hard HEAD
  if [[ -n "$(git status --porcelain)" ]]; then
    echo "tree not clean after revert"
    exit 2
  fi
done
echo "all mutations reverted; tree clean"
