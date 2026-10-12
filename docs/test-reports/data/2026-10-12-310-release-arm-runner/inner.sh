#!/usr/bin/env bash
# Runs inside a throwaway ubuntu:24.04 container (linux/amd64 natively, or linux/arm64 under
# user-mode emulation) that stands in for the GitHub-hosted runner image: the same Ubuntu
# release, podman and skopeo from Ubuntu's archive (4.9.3 and 1.13.3, the versions the runner
# images list), rustup 1.29.1, proto 0.60.2 and the tools pinned in .prototools. Then it runs
# release.yml's jobs with the repository's scripts in local test mode (local registry, a
# throwaway cosign key; nothing leaves the host).
#
#   inner.sh <label> <registry-port>
# Mounts: /work/repo (a clone of the commit under test), /work/out (results), /work/bundle (ro).
set -euo pipefail
label=$1
port=$2
out=/work/out
mkdir -p "$out"
exec > >(tee "$out/inner.log") 2>&1
t0=$(date +%s)
stamp() { echo "== $(date -u +%FT%TZ) (+$(($(date +%s) - t0))s) [$label] $*"; }

stamp "host: $(uname -m), page size $(getconf PAGESIZE), $(nproc) CPUs"
export DEBIAN_FRONTEND=noninteractive
stamp "apt: podman skopeo and the build basics the runner image has"
apt-get update -qq
apt-get install -y -qq podman crun skopeo git curl xz-utils ca-certificates build-essential jq openssl file > /dev/null
# Nested in a container: cgroupfs, file events and crun. RUN steps use chroot isolation, except
# under user-mode emulation, where chroot isolation cannot re-execute podman (/proc/self/exe is
# the host's qemu binary) and ISOLATION=oci runs them with crun instead. Neither changes what
# goes into an image layer.
mkdir -p /etc/containers
cat > /etc/containers/containers.conf << 'EOF'
[containers]
netns = "host"
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
EOF
export BUILDAH_ISOLATION=${ISOLATION:-chroot}

case "$(uname -m)" in
  x86_64) target=x86_64-unknown-linux-gnu; proto_sha=fe96d99ede103639becd6ab624688efeacc91049574174b31e67d1affd1c295c; rustup_sha=dda7234360b7f578ca8b0ddcb80145646fa61a67c1720a5abc7051b35c9fcb71 ;;
  aarch64) target=aarch64-unknown-linux-gnu; proto_sha=bf04fec624b76dcf935ac4753e1ced8fc6a105ec1eb5abd6690f5ece308748cc; rustup_sha=15f6e4ce9f583b929c996c91562bad6d4454f3281de858b02cdfdef615fac433 ;;
  *) echo "unsupported $(uname -m)"; exit 1 ;;
esac

stamp "rustup 1.29.1 ($target), checked against the pinned SHA-256"
curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/rustup-init "https://static.rust-lang.org/rustup/archive/1.29.1/$target/rustup-init"
echo "$rustup_sha  /tmp/rustup-init" | sha256sum --check --strict
chmod +x /tmp/rustup-init
/tmp/rustup-init -y --no-modify-path --default-toolchain none --profile minimal > /dev/null

stamp "proto 0.60.2 ($target), the archive and SHA-256 of .github/actions/setup-proto"
curl --proto '=https' --tlsv1.2 -sSfL -o /tmp/proto.tar.xz "https://github.com/moonrepo/proto/releases/download/v0.60.2/proto_cli-$target.tar.xz"
echo "$proto_sha  /tmp/proto.tar.xz" | sha256sum --check --strict
mkdir -p ~/.proto/bin
tar --extract --xz --file /tmp/proto.tar.xz --directory ~/.proto/bin --strip-components=1 "proto_cli-$target/proto" "proto_cli-$target/proto-shim"
export PATH="$HOME/.proto/bin:$HOME/.proto/shims:$HOME/.cargo/bin:$PATH"

cd /work/repo
# The clone belongs to the host user; this throwaway container runs as root (on the runner the
# checkout and the job share one user).
git config --global --add safe.directory /work/repo
stamp "proto install: the tools release.yml's jobs list (plan: bun helm; build: bun zig cargo-zigbuild cargo-auditable syft; publish, verify: bun cosign helm)"
for tool in bun helm zig cargo-zigbuild cargo-auditable syft cosign; do
  proto install "$tool" --yes
done
{
  echo "label: $label"
  echo "uname -m: $(uname -m)"
  echo "page size: $(getconf PAGESIZE)"
  grep PRETTY_NAME /etc/os-release
  podman --version
  skopeo --version
  rustup --version 2> /dev/null
  proto --version
  for tool in bun helm zig cargo-zigbuild cargo-auditable syft cosign; do
    exe="$(proto bin "$tool" --reporter text)"
    echo "$tool: $exe: $(file -b "$exe" | cut -d, -f1-2)"
  done
  echo "bun $("$(proto bin bun --reporter text)" --version)"
  "$(proto bin helm --reporter text)" version --short
  echo "zig $("$(proto bin zig --reporter text)" version)"
  "$(proto bin cargo-zigbuild --reporter text)" --version
  "$(proto bin cargo-auditable --reporter text)" --version 2>&1 | head -1 || true
  "$(proto bin syft --reporter text)" version 2> /dev/null | grep -E '^Version'
  "$(proto bin cosign --reporter text)" version 2> /dev/null | grep -E '^GitVersion'
} > "$out/versions.txt" 2>&1
cat "$out/versions.txt"
BUN="$(proto bin bun --reporter text)"

if [[ ${REBUILD_ONLY:-0} == 1 ]]; then
  stamp "rebuild: plan.ts, then only the Cargo command of build.ts (rust-build.ts) in this fresh clone"
  "$BUN" scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --registry "127.0.0.1:$port/laysi/trustiot-v2" --output "$out/plan.json"
  "$BUN" /work/bundle/rebuild.ts "$out/plan.json"
  rustc -vV > "$out/rustc-vV.txt"
  mkdir -p "$out/bin"
  for bin in identity release-selftest; do
    cp "target/aarch64-unknown-linux-musl/release/$bin" "$out/bin/$bin"
  done
  (cd "$out" && sha256sum bin/* > binaries.sha256)
  cat "$out/binaries.sha256"
  stamp "done"
  exit 0
fi

stamp "plan job: plan.ts --dry-run (helm lint runs here)"
"$BUN" scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --registry "127.0.0.1:$port/laysi/trustiot-v2" --output "$out/plan.json"
stamp "plan job: release notes (Laysi/release-notes-action ad6ed96 notes.sh, no model configured)"
TAG=v0.0.0-rc.1 TAG_PATTERN='v[0-9]*' OUTPUT_FILE="$out/notes.md" GITHUB_OUTPUT="$out/notes.outputs" bash /work/bundle/notes.sh
delimiter="EOF_$(openssl rand -hex 16)"
{ echo "notes<<$delimiter"; cat "$out/notes.md"; echo "$delimiter"; } > "$out/notes-text.outputs"

stamp "build job: build.ts"
if ! "$BUN" scripts/release/build.ts --plan "$out/plan.json" --out "$out/release"; then
  if [[ ${POSTGRES_FALLBACK:-0} != 1 ]]; then
    exit 1
  fi
  stamp "build job FAILED with the postgres image; building the plan without it so the other jobs still run"
  jq -c '.images |= map(select(.name != "postgres"))' "$out/plan.json" > "$out/plan-no-postgres.json"
  mv "$out/plan-no-postgres.json" "$out/plan.json"
  rm -rf "$out/release"
  "$BUN" scripts/release/build.ts --plan "$out/plan.json" --out "$out/release"
fi
stamp "build job done"
rustc -vV > "$out/rustc-vV.txt"
mkdir -p "$out/bin" "$out/cobj"
for bin in identity release-selftest; do
  cp "target/aarch64-unknown-linux-musl/release/$bin" "$out/bin/$bin"
done
for f in target/aarch64-unknown-linux-musl/release/build/release-selftest-*/out/*; do
  cp "$f" "$out/cobj/"
done
(cd "$out" && sha256sum bin/* cobj/* > binaries.sha256 && file bin/* >> binaries.sha256)

stamp "publish job: publish.ts in local test mode"
export DOCKER_CONFIG="$out/docker" HELM_REGISTRY_CONFIG="$out/helm-registry.json" COSIGN_PASSWORD=
mkdir -p /tmp/key
"$(proto bin cosign --reporter text)" generate-key-pair --output-key-prefix /tmp/key/test > /dev/null
"$BUN" scripts/release/publish.ts --plan "$out/plan.json" --artifacts "$out/release" --notes "$out/notes.md" \
  --output "$out/published.json" --local-test-key /tmp/key/test
stamp "publish job: re-run (same digests, nothing pushed again)"
"$BUN" scripts/release/publish.ts --plan "$out/plan.json" --artifacts "$out/release" --notes "$out/notes.md" \
  --output "$out/published2.json" --local-test-key /tmp/key/test
stamp "verify job: verify.ts in local test mode"
"$BUN" scripts/release/verify.ts --plan "$out/plan.json" --published "$out/published.json" \
  --output "$out/report.md" --local-test-key /tmp/key/test
rm -rf /tmp/key

stamp "bun test scripts/release on this architecture"
"$BUN" install --cwd scripts --frozen-lockfile > /dev/null
"$BUN" test scripts/release 2>&1 | tail -5

# Running an image here is not possible under user-mode emulation (crun re-executes itself
# through a memory file descriptor, which qemu-user cannot run); arm-eval.sh runs the
# release-selftest image with the host's Docker instead.
stamp "done"
