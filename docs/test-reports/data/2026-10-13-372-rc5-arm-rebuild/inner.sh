#!/usr/bin/env bash
# Laysi/trustiot-v2#372. Runs inside a throwaway linux/arm64 ubuntu:24.04 container under
# user-mode emulation that stands in for the ubuntu-24.04-arm runner image (20261004.142.1):
# podman and skopeo from Ubuntu's archive, rustup 1.29.1, proto 0.60.2 and the tools that
# release.yml's plan and build jobs install from .prototools. It runs plan.ts and build.ts of the
# tagged commit twice, each in its own fresh clone and target directory, then images.sh compares
# the results with each other and with what release.yml published for v0.0.0-rc.5.
#
# Mounts:
#   /home/runner/work/trustiot-v2/trustiot-v2   first clone, at the runner's checkout path
#   /work/repo                                  second clone, at another path
#   /work/out (results), /work/bundle (these scripts, read-only)
#
# The postgres image is left out of the plan: its Containerfile has RUN steps, which podman
# cannot execute under user-mode emulation (see the Laysi/trustiot-v2#310 report), and it is not
# part of this comparison. Nothing is pushed or signed; the only registry access is pulls (the
# distroless base image and, in images.sh, the public identity image), without credentials.
set -euo pipefail
out=/work/out
mkdir -p "$out"
exec > >(tee "$out/inner.log") 2>&1
t0=$(date +%s)
stamp() { echo "== $(date -u +%FT%TZ) (+$(($(date +%s) - t0))s) $*"; }
TAG=v0.0.0-rc.5

# The runner's user directories: on ubuntu-24.04-arm, HOME is /home/runner and rustup lives in
# /home/runner/.rustup and /home/runner/.cargo.
export HOME=/home/runner RUSTUP_HOME=/home/runner/.rustup CARGO_HOME=/home/runner/.cargo
mkdir -p "$HOME"

stamp "host: $(uname -m), page size $(getconf PAGESIZE), $(nproc) CPUs"
export DEBIAN_FRONTEND=noninteractive
stamp "apt: podman, skopeo and the build basics the runner image has"
apt-get update -qq
apt-get install -y -qq podman crun skopeo git curl xz-utils ca-certificates build-essential jq openssl file > /dev/null
# Nested in a container: cgroupfs, file events and crun, as for Laysi/trustiot-v2#310. The rust
# images' Containerfile has no RUN, so the isolation never runs anything.
mkdir -p /etc/containers
cat > /etc/containers/containers.conf << 'EOF'
[containers]
netns = "host"
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
EOF
export BUILDAH_ISOLATION=chroot

stamp "rustup 1.29.1 (aarch64-unknown-linux-gnu), checked against the pinned SHA-256"
curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/1.29.1/aarch64-unknown-linux-gnu/rustup-init"
echo "15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433  /tmp/rustup-init" | sha256sum --check --strict
chmod +x /tmp/rustup-init
/tmp/rustup-init -y --no-modify-path --default-toolchain none --profile minimal > /dev/null

stamp "proto 0.60.2 (aarch64-unknown-linux-gnu), the archive and SHA-256 of .github/actions/setup-proto"
curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/proto.tar.xz "https://github.com/moonrepo/proto/releases/download/v0.60.2/proto_cli-aarch64-unknown-linux-gnu.tar.xz"
echo "bf04fec624b76dcf935ac4753e1ced8fc6a105ec1eb5abd6690f5ece308748cc  /tmp/proto.tar.xz" | sha256sum --check --strict
mkdir -p "$HOME/.proto/bin"
tar --extract --xz --file /tmp/proto.tar.xz --directory "$HOME/.proto/bin" --strip-components=1 \
  proto_cli-aarch64-unknown-linux-gnu/proto proto_cli-aarch64-unknown-linux-gnu/proto-shim
export PATH="$HOME/.proto/bin:$HOME/.proto/shims:$CARGO_HOME/bin:$PATH"

RUNNER_DIR=/home/runner/work/trustiot-v2/trustiot-v2
ALT_DIR=/work/repo
# The clones belong to the host user; this throwaway container runs as root.
git config --global --add safe.directory "$RUNNER_DIR"
git config --global --add safe.directory "$ALT_DIR"

cd "$RUNNER_DIR"
stamp "proto install: the tools of release.yml's plan job (bun helm) and build job (bun zig cargo-zigbuild cargo-auditable syft)"
for tool in bun helm zig cargo-zigbuild cargo-auditable syft; do
  proto install "$tool" --yes
done
{
  echo "uname -m: $(uname -m)"
  echo "page size: $(getconf PAGESIZE)"
  grep PRETTY_NAME /etc/os-release
  podman --version
  skopeo --version
  echo "podman graph driver: $(podman info --format '{{.Store.GraphDriverName}}')"
  echo "Ubuntu packages:"
  dpkg-query -W -f '  ${Package} ${Version}\n' podman skopeo crun buildah golang-github-containers-common golang-github-containers-image conmon 2> /dev/null || true
  rustup --version 2> /dev/null
  proto --version
  for tool in bun helm zig cargo-zigbuild cargo-auditable syft; do
    exe="$(proto bin "$tool" --reporter text)"
    echo "$tool: $exe: $(file -b "$exe" | cut -d, -f1-2)"
  done
  echo "bun $("$(proto bin bun --reporter text)" --version)"
  "$(proto bin helm --reporter text)" version --short
  echo "zig $("$(proto bin zig --reporter text)" version)"
  "$(proto bin cargo-zigbuild --reporter text)" --version
  "$(proto bin syft --reporter text)" version 2> /dev/null | grep -E '^Version'
} > "$out/versions.txt" 2>&1
cat "$out/versions.txt"
BUN="$(proto bin bun --reporter text)"

# build <label> <clone> <cargo home>: release.yml's plan job (plan.ts --tag, as the workflow runs
# it: no --dry-run, so the tag must point at the checked-out commit) and build job (build.ts)
# for the identity and release-selftest images.
build() {
  local label=$1 dir=$2 cargo_home=$3
  local o=$out/$label
  mkdir -p "$o"
  cd "$dir"
  stamp "[$label] plan.ts --tag $TAG in $dir"
  CARGO_HOME=$cargo_home "$BUN" scripts/release/plan.ts --tag "$TAG" --output "$o/plan.json"
  jq '.images |= map(select(.name != "postgres"))' "$o/plan.json" > "$o/plan-build.json"
  stamp "[$label] build.ts with CARGO_HOME=$cargo_home"
  CARGO_HOME=$cargo_home "$BUN" scripts/release/build.ts --plan "$o/plan-build.json" --out "$o/release"
  stamp "[$label] build.ts done"
  mkdir -p "$o/bin"
  for bin in identity release-selftest; do
    cp "target/aarch64-unknown-linux-musl/release/$bin" "$o/bin/$bin"
  done
  cp "$o/release/build.json" "$out/build-$label.json"
}

build runner "$RUNNER_DIR" /home/runner/.cargo
cp "$out/runner/plan.json" "$out/plan-runner.json"
mkdir -p /work/cargo-alt
build alt "$ALT_DIR" /work/cargo-alt
rustc -vV > "$out/rustc-vV.txt"
(cd "$out" && sha256sum runner/bin/* alt/bin/* > binaries.sha256)

stamp "compare with each other and with the published v0.0.0-rc.5"
bash /work/bundle/images.sh "$out" "$BUN"
stamp "done"
