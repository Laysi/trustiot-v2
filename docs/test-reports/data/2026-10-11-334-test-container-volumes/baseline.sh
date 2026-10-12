#!/usr/bin/env bash
# Baseline for Laysi/trustiot-v2#334: how many volumes the old `rm --force` leaks.
#
# Run it from the worktree root, in a slot of the shared check queue, so that no other container
# test runs at the same time. It puts service-kit's test support back to the code of 1ad700f,
# runs two container-backed test files, and records the dangling volumes before and after.
#
#   OUT=<scratch directory> bash baseline.sh        # lists the leaked volumes, removes nothing
#   OUT=<scratch directory> CLEAN=1 bash baseline.sh # also removes them (see below)
#
# $OUT/support.patch is `git diff -- crates/service-kit/tests/support/mod.rs` of the fixed tree.
# CLEAN=1 removes every volume that became dangling during the run. That is only the run's own
# volumes when nothing else creates volumes meanwhile, which the queue slot gives you; check the
# list in $OUT/data/leaked-by-baseline.txt first. Never remove a volume this way on a machine
# where other work runs containers.
set -uo pipefail
d="${OUT:?set OUT to a scratch directory}"
out="$d/data"
mkdir -p "$out"
snap() {
  podman volume ls --filter dangling=true -q | sort > "$out/dangling-$1.txt"
  echo "snapshot $1: $(wc -l < "$out/dangling-$1.txt") dangling volumes, $(date +%T)"
}

git apply -R "$d/support.patch" || exit 1
trap 'git apply "$d/support.patch"' EXIT
snap baseline-before
cargo test --locked -p service-kit --test tenant_db --test job_lock > "$out/step-baseline-old-code.log" 2>&1
echo "baseline cargo test exit $?"
grep "test result" "$out/step-baseline-old-code.log"
snap baseline-after
comm -13 "$out/dangling-baseline-before.txt" "$out/dangling-baseline-after.txt" > "$out/leaked-by-baseline.txt"
echo "volumes leaked by the old code: $(wc -l < "$out/leaked-by-baseline.txt")"
if [ "${CLEAN:-}" = 1 ]; then
  while read -r v; do [ -n "$v" ] && podman volume rm "$v" > /dev/null; done < "$out/leaked-by-baseline.txt"
  snap baseline-cleaned
fi
git apply "$d/support.patch" && trap - EXIT
git diff --stat -- crates/service-kit/tests/support/mod.rs
