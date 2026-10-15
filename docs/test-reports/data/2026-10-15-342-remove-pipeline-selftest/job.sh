#!/usr/bin/env bash
# The build-host job of Laysi/trustiot-v2#342: the repository checks, the local release flow
# (plan --dry-run and build, as scripts/README.md describes) and the C cross-compiler
# experiments on ring. Run from the repository root:
#   bash docs/test-reports/data/2026-10-15-342-remove-pipeline-selftest/job.sh
# Each step prints a "=== <name> ===" header and its exit status; the job exits non-zero when any
# step that must pass failed, or when an experiment that must fail passed.
set -uo pipefail

root=$(pwd)
# A new directory inside the copy, under the git-ignored /target/, so the checkout the plan names
# stays clean; the job removes only this directory.
mkdir -p "$root/target"
work=$(mktemp -d "$root/target/release-342.XXXXXX") || exit 1
failed=()

step() {
  local name=$1
  shift
  echo
  echo "=== $name ==="
  "$@"
  local rc=$?
  echo "--- $name: exit $rc"
  return "$rc"
}
must_pass() { step "$@" || failed+=("$1"); }
# An experiment that must fail: passing is the failure.
must_fail() {
  if step "$@"; then
    failed+=("$1 (expected a failure)")
  fi
}

# podman keeps this job's images in a store of its own inside the work directory, so the job
# never touches the host's shared store and removing the directory removes every image it made.
cat > "$work/storage.conf" << EOF
[storage]
driver = "overlay"
graphroot = "$work/podman/storage"
runroot = "$work/podman/run"
EOF
export CONTAINERS_STORAGE_CONF="$work/storage.conf"

cleanup() {
  echo
  echo "=== cleanup ==="
  echo "images in the job's own store: $(podman images --all --quiet | wc -l)"
  # Files in the store belong to the user namespace's IDs, and podman mounts the store's overlay
  # directory there, so unmount and remove it from inside that namespace.
  # shellcheck disable=SC2016 # expanded by the inner shell
  podman unshare sh -c '
    awk -v dir="$1" "index(\$5, dir) == 1 { print \$5 }" /proc/self/mountinfo | sort -r | xargs -r umount
    rm -rf "$1"
  ' sh "$work/podman" || echo "cleanup: removing the job's podman store failed"
  rm -rf "$work"
  if [[ -e $work ]]; then
    echo "cleanup: $work is still there"
  else
    echo "work directory and the job's podman store removed"
  fi
}
trap cleanup EXIT

echo "=== environment ==="
uname -sm
podman --version
skopeo --version
env | grep -E '^(CC|TARGET_CC|CC_[a-z0-9_-]+|CARGO_TARGET_DIR)=' | sed 's/=.*$/ is set/' || echo "no CC, TARGET_CC, CC_<target> or CARGO_TARGET_DIR in the environment"

must_pass "proto install" proto install
for tool in zig cargo-zigbuild cargo-auditable syft helm; do
  echo "$tool: $(proto bin "$tool" --reporter text | sed "s|^$HOME|~|")"
done

must_pass "cargo fmt" cargo fmt --all --check
must_pass "cargo clippy" cargo clippy --locked --workspace --all-targets -- -D warnings
must_pass "cargo test" bash -o pipefail -c 'cargo test --locked --workspace 2>&1 | grep -E "^(test result|error|failures:|---- )|FAILED|panicked" | sort | uniq -c'
must_pass "cargo deny" bash -o pipefail -c 'cargo deny --locked check 2>&1 | tail -5'
must_pass "bun install" bun install --cwd scripts --frozen-lockfile
must_pass "typecheck" bun run --cwd scripts typecheck
must_pass "bun test scripts" bash -o pipefail -c 'bun test scripts 2>&1 | tail -8'
must_pass "bun test scripts/release" bash -o pipefail -c 'bun test scripts/release 2>&1 | tail -6'
must_pass "check-workflows" bun scripts/ci/check-workflows.ts
must_pass "check-secret-handling" bun scripts/ci/check-secret-handling.ts

for package in identity assets migrate; do
  must_pass "cargo tree $package" cargo tree --locked --target aarch64-unknown-linux-musl -p "$package" -e normal,build -i cc --depth 2
done

# The local release flow, as scripts/README.md "本機跑一次完整流程" runs it, up to build.
must_pass "plan" bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run \
  --registry 127.0.0.1:5068/laysi/trustiot-v2 --output "$work/plan.json"
must_pass "plan.json" bun -e '
  const plan = await Bun.file(process.argv[1]).json();
  console.log(JSON.stringify({ tag: plan.tag, version: plan.version, images: plan.images.map((i) => ({ name: i.name, kind: i.kind, package: i.package })), chart: plan.chart?.ref ?? plan.chart }, null, 2));
' "$work/plan.json"
must_pass "build" bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/out"
must_pass "build.json" cat "$work/out/build.json"
must_pass "archives and SBOMs" bun -e '
  const { readdirSync, statSync } = await import("node:fs");
  const dir = process.argv[1];
  for (const file of readdirSync(dir).sort()) {
    const path = `${dir}/${file}`;
    const line = [file, `${statSync(path).size} bytes`];
    if (file.endsWith(".spdx.json")) {
      const sbom = await Bun.file(path).json();
      const names = new Set(sbom.packages.map((p) => p.name));
      line.push(`${sbom.packages.length} packages`, `ring: ${names.has("ring")}`, `cc: ${names.has("cc")}`);
    }
    console.log(line.join("  "));
  }
' "$work/out/images"

# The C cross compiler. ring@0.17.14 is the only crate of the three images that compiles C (and
# aarch64 assembly); each experiment builds it alone, in its own target directory.
zig=$(proto bin zig --reporter text)
zigbuild=$(proto bin cargo-zigbuild --reporter text)
auditable=$(proto bin cargo-auditable --reporter text)
release_path="$(dirname "$auditable"):$(dirname "$zigbuild"):$PATH"
# The lines of a Cargo run that say what was built and why it failed, with local paths masked.
mask() { sed -u "s|$work|<work>|g; s|$root|<host copy>|g; s|$HOME|~|g"; }
cargo_summary() {
  grep -E 'Compiling ring|Finished|^error|error occurred in cc-rs|refusing to compile|^Error|Caused by|Failed to run|failed to find tool' |
    mask | cut -c1-400
}
ring_build() {
  local dir=$1
  shift
  env PATH="$release_path" CARGO_TARGET_DIR="$work/$dir" "$@" \
    cargo auditable zigbuild --locked --release --target aarch64-unknown-linux-musl -p ring@0.17.14 2>&1 | cargo_summary
}

# 1. Control: the release toolchain compiles ring, and cc reports Zig's wrapper as the target's
#    C compiler (CC_ENABLE_DEBUG_OUTPUT makes cc print the variables it read and the commands).
must_pass "ring with the release toolchain" ring_build ring-ok CARGO_ZIGBUILD_ZIG_PATH="$zig" CC_ENABLE_DEBUG_OUTPUT=1
ring_output() { cat "$work"/ring-ok/aarch64-unknown-linux-musl/release/build/ring-*/output; }
compiler_report() {
  ring_output | grep -E '^(CC_aarch64[-_a-z0-9]*|TARGET_CC|CC) = ' | sort -u | mask
  local compiles
  compiles=$(ring_output | grep -E '^running: .*zigcc-aarch64-unknown-linux-musl.*"-c" ' || true)
  echo "compiler runs with -c: $(grep -c . <<< "$compiles"), of which C files: $(grep -c '\.c"' <<< "$compiles"), assembly files: $(grep -c '\.S"' <<< "$compiles")"
  echo "assembly files compiled:"
  grep -o '[^/"]*\.S"' <<< "$compiles" | tr -d '"' | sort | sed 's/^/  /'
}
must_pass "the C compiler cc used for ring" compiler_report

# 2. A broken C cross compiler: Zig as cargo-zigbuild runs it, except that `zig cc` fails on
#    the C or assembly source files that match a pattern.
#    a. Every source file: cargo-zigbuild's own probe of `zig cc` fails before any build script.
#    b. Only ring's sources: the probe passes, so the failure must come from ring's build script,
#       which is what a C cross compiler that cannot compile the target's code looks like.
broken_zig() {
  local file=$1 pattern=$2
  cat > "$file" << EOF
#!/bin/sh
if [ "\$1" = cc ]; then
  for arg in "\$@"; do
    case "\$arg" in
      $pattern) echo "zig-broken-cc: refusing to compile \$arg" >&2; exit 1 ;;
    esac
  done
fi
exec "$zig" "\$@"
EOF
  chmod +x "$file"
}
broken_zig "$work/zig-broken-all" '*.c|*.S'
must_fail "ring with a zig cc that compiles nothing" ring_build ring-broken-all CARGO_ZIGBUILD_ZIG_PATH="$work/zig-broken-all"
broken_zig "$work/zig-broken-ring" '*/ring-[0-9]*/*.c|*/ring-[0-9]*/*.S'
must_fail "ring with a zig cc that cannot compile ring" ring_build ring-broken-ring CARGO_ZIGBUILD_ZIG_PATH="$work/zig-broken-ring"

# 3. No C cross compiler: plain cargo, without cargo-zigbuild, so cc has no CC_<target>.
must_pass "no aarch64-linux-musl-gcc on PATH" bash -o pipefail -c '! command -v aarch64-linux-musl-gcc'
plain_ring_build() {
  CARGO_TARGET_DIR="$work/ring-plain" cargo build --locked --release --target aarch64-unknown-linux-musl -p ring@0.17.14 2>&1 | cargo_summary
}
must_fail "ring without cargo-zigbuild" plain_ring_build

echo
if ((${#failed[@]} > 0)); then
  printf 'FAILED: %s\n' "${failed[@]}"
  exit 1
fi
echo "ALL STEPS AS EXPECTED"
