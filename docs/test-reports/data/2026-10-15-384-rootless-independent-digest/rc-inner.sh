#!/usr/bin/env bash
# Laysi/trustiot-v2#384. Runs inside a throwaway linux/arm64 ubuntu:24.04 container under
# user-mode emulation that stands in for the ubuntu-24.04-arm runner (as in Laysi/trustiot-v2#372):
# podman, buildah and skopeo from Ubuntu's archive, rustup 1.29.1, proto 0.60.2 and the tools that
# release.yml's plan and build jobs install from .prototools. Runs release.yml's plan and build
# steps for the tag on a fresh clone, with podman as root (the runner's is rootless), for the
# Rust images. The postgres image is left out: its Containerfile has RUN steps, which podman cannot
# execute under user-mode emulation (Laysi/trustiot-v2#310), and it is not part of this issue.
#   rc-inner.sh <tag>
# Mounts: /home/runner/work/trustiot-v2/trustiot-v2 (the clone, at the runner's checkout path),
# /work/out (results), /var/lib/containers (a directory of the host).
set -euo pipefail
tag=$1
out=/work/out
mkdir -p "$out"
t0=$(date +%s)
stamp() { echo "== $(date -u +%FT%TZ) (+$(($(date +%s) - t0))s) $*"; }
export HOME=/home/runner RUSTUP_HOME=/home/runner/.rustup CARGO_HOME=/home/runner/.cargo
mkdir -p "$HOME"
export DEBIAN_FRONTEND=noninteractive
stamp "host: $(uname -m), $(nproc) CPUs; apt: podman, skopeo and the build basics the runner image has"
apt-get update -qq
apt-get install -y -qq podman crun skopeo git curl xz-utils ca-certificates build-essential jq openssl file > /dev/null
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

dir=/home/runner/work/trustiot-v2/trustiot-v2
# The clone belongs to the host user; this throwaway container runs as root.
git config --global --add safe.directory "$dir"
cd "$dir"
stamp "proto install: the tools of release.yml's plan job (bun helm) and build job (bun zig cargo-zigbuild cargo-auditable syft)"
for tool in bun helm zig cargo-zigbuild cargo-auditable syft; do
  proto install "$tool" --yes > /dev/null
done
{
  echo "uname -m: $(uname -m)"
  grep PRETTY_NAME /etc/os-release
  podman --version
  skopeo --version
  echo "Ubuntu packages:"
  dpkg-query -W -f '  ${Package} ${Version}\n' podman buildah skopeo crun golang-github-containers-image golang-github-containers-storage 2> /dev/null || true
  rustup --version 2> /dev/null
  proto --version
  for tool in bun helm zig cargo-zigbuild cargo-auditable syft; do
    echo "$tool: $(basename "$(dirname "$(proto bin "$tool" --reporter text)")")"
  done
} > "$out/versions.txt" 2>&1
cat "$out/versions.txt"
BUN="$(proto bin bun --reporter text)"

stamp "plan.ts --tag $tag (release.yml's plan step)"
"$BUN" scripts/release/plan.ts --tag "$tag" --output "$out/plan.json"
jq '.images |= map(select(.kind == "rust"))' "$out/plan.json" > "$out/plan-rust.json"
stamp "build.ts, podman as root (release.yml's build step)"
"$BUN" scripts/release/build.ts --plan "$out/plan-rust.json" --out "$out/release"
stamp "build.ts done"
mkdir -p "$out/bin"
for name in $(jq -r '.images[].package' "$out/plan-rust.json"); do
  cp "target/aarch64-unknown-linux-musl/release/$name" "$out/bin/$name"
done
cp "$out/release/build.json" "$out/build-root-arm64.json"
(cd "$out/bin" && sha256sum ./*)
chmod -R a+rX "$out"
stamp "done"
