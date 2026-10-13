#!/usr/bin/env bash
# Laysi/trustiot-v2#372: rebuilds release v0.0.0-rc.5 (commit 716740a) on the build host in a
# linux/arm64 ubuntu:24.04 container under the host's existing qemu-user binfmt handler, and
# compares the result with what release.yml published from the ubuntu-24.04-arm runner.
# Started through rcheck from the worktree root, so $PWD is rcheck's snapshot of the branch and
# this directory's scripts are read from it.
#   bash docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild/arm-eval.sh rebuild
#       two fresh clones of the tag from GitHub (anonymous; the repository is public), built one
#       after the other in one container: the first at the runner's paths, the second at other
#       paths; then the image and binary comparisons (inner.sh, images.sh)
#   bash docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild/arm-eval.sh layers
#       after `rebuild` (whose identity binary matched the published one, the image not): the
#       tar headers of the COPY layer of the rebuilt and the published identity image
#       (tarhdr.ts), then the published binary packed again in an x64 container with podman as
#       root and rootless (layer-inner.sh), and the headers of those layers
#   bash docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild/arm-eval.sh variants
#       after `layers`: the published binary packed repeatedly, each time in fresh storage, as
#       root and rootless, with --layers=false and the vfs driver (variants.sh)
#   bash docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild/arm-eval.sh configs
#       the COPY step and history comments of every image config the modes above saved
#   bash docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild/arm-eval.sh rootless
#       the rebuilt binaries packed with podman rootless as on the runner, in an x64 and an
#       arm64 container (pack-rootless.sh)
#   bash docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild/arm-eval.sh cleanup
#       removes this evaluation's directory on the build host
set -euo pipefail
mode=${1:-rebuild}
E=$HOME/tv2/agents/372-eval
W=$E/run
DATA=docs/test-reports/data/2026-10-13-372-rc5-arm-rebuild
# The same ubuntu:24.04 manifests as Laysi/trustiot-v2#310 (index sha256:534baea6...), pinned by the
# digest of each platform's manifest: Docker 29 with the containerd store refuses to pull a second
# platform of an index it already holds.
UBUNTU_X64=docker.io/library/ubuntu@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b
UBUNTU_ARM64=docker.io/library/ubuntu@sha256:08571ca13e00ca07a2a84eab83a959b4242e22cceb16486a11bef1428c9e93a7
REPO_URL=https://github.com/Laysi/trustiot-v2.git
TAG=v0.0.0-rc.5
COMMIT=716740a9fa638a39e4655c54173653a36f2b121d

# Root-owned files (the container runs as root) are removed from inside a container.
remove() {
  docker run --rm -v "$(dirname "$1"):/p" "$UBUNTU_X64" rm -rf "/p/$(basename "$1")"
}

cleanup() {
  docker rm -f tv2-372-run > /dev/null 2>&1 || true
}
trap cleanup EXIT

case $mode in
  rebuild) ;;
  layers)
    cp "$DATA/layer-inner.sh" "$E/bundle/"
    echo "== published v0.0.0-rc.5 (built on the runner)"
    bun "$DATA/tarhdr.ts" "$W/out/layers/ghcr-identity"
    echo "== rebuild (podman as root, arm64 under emulation)"
    bun "$DATA/tarhdr.ts" "$W/out/layers/runner-identity"
    docker run --rm --name tv2-372-run --platform linux/amd64 --privileged --network host \
      --memory 11g --memory-swap 11g \
      -v "$W/runner:/work/repo:ro" -v "$W/out:/work/out" -v "$E/bundle:/work/bundle:ro" \
      -v /var/lib/containers -v /home \
      "$UBUNTU_X64" bash /work/bundle/layer-inner.sh
    for label in root rootless; do
      echo "== published binary packed again in the x64 container, podman $label"
      bun "$DATA/tarhdr.ts" "$W/out/layers-x64/$label/oci"
    done
    exit 0
    ;;
  variants)
    cp "$DATA/variants.sh" "$E/bundle/"
    docker run --rm --name tv2-372-run --platform linux/amd64 --privileged --network host \
      --memory 11g --memory-swap 11g \
      -v "$W/runner:/work/repo:ro" -v "$W/out:/work/out" -v "$E/bundle:/work/bundle:ro" \
      -v /var/lib/containers -v /home \
      "$UBUNTU_X64" bash /work/bundle/variants.sh
    exit 0
    ;;
  rootless)
    # The rebuilt binaries packed with podman rootless as the runner does (pack-rootless.sh), in
    # an x64 container and then in the arm64 container under emulation.
    cp "$DATA/pack-rootless.sh" "$E/bundle/"
    for platform in amd64 arm64; do
      image=$UBUNTU_X64
      if [[ $platform == arm64 ]]; then image=$UBUNTU_ARM64; fi
      docker run --rm --name tv2-372-run --platform "linux/$platform" --privileged --network host \
        --memory 11g --memory-swap 11g \
        -v "$W/runner:/work/repo:ro" -v "$W/out:/work/out" -v "$E/bundle:/work/bundle:ro" \
        -v /var/lib/containers -v /home \
        "$image" bash /work/bundle/pack-rootless.sh "$platform" || echo "linux/$platform container: exit $?"
    done
    exit 0
    ;;
  configs)
    # The COPY step and the history comments of every image config the other modes saved.
    cd "$W/out"
    for f in layers/ghcr-identity/* layers/runner-identity/blobs/sha256/* layers-x64/*/oci/blobs/sha256/* variants/*/config-*.json; do
      if grep -q '"history"' "$f" 2> /dev/null; then
        echo "$f: $(grep -o 'COPY file:[0-9a-f]*' "$f" | tr '\n' ' ')$(grep -o -E '"comment": *"FROM [0-9a-f]+"' "$f" | tr '\n' ' ')"
      fi
    done
    exit 0
    ;;
  cleanup)
    if [[ -d $E ]]; then remove "$E"; fi
    echo "removed the evaluation directory"
    exit 0
    ;;
  *)
    echo "usage: arm-eval.sh rebuild|layers|variants|configs|rootless|cleanup" >&2
    exit 2
    ;;
esac

echo "binfmt aarch64 handler (registered before this run, not changed by it):"
cat /proc/sys/fs/binfmt_misc/aarch64
echo "host docker: $(docker version --format '{{.Server.Version}}')"

if [[ -d $W ]]; then remove "$W"; fi
mkdir -p "$W/out" "$E/bundle"
cp "$DATA/inner.sh" "$DATA/images.sh" docs/test-reports/data/2026-10-12-310-release-arm-runner/elfdiff.ts "$E/bundle/"

# clone <dir>: a fresh clone of the repository with every tag, the tag checked out as
# actions/checkout does (detached HEAD at the tagged commit).
clone() {
  # No credential helper and no prompt: the clone is anonymous.
  GIT_TERMINAL_PROMPT=0 git -c credential.helper= clone -q "$REPO_URL" "$1"
  git -C "$1" -c advice.detachedHead=false checkout -q "refs/tags/$TAG"
  local head
  head=$(git -C "$1" rev-parse HEAD)
  if [[ $head != "$COMMIT" ]]; then
    echo "$1: $TAG is $head, not $COMMIT" >&2
    exit 1
  fi
  echo "$1: $TAG (tag object $(git -C "$1" rev-parse "refs/tags/$TAG")) → $head, committed $(git -C "$1" log -1 --format='%ct %cI')"
}
clone "$W/runner"
clone "$W/alt"

start=$(date +%s)
# One container: the first build mounts its clone at the runner's checkout path and uses the
# runner's CARGO_HOME; the second build its clone at /work/repo with another CARGO_HOME.
docker run --rm --name tv2-372-run --platform linux/arm64 --privileged --network host \
  --memory 11g --memory-swap 11g \
  -e CARGO_BUILD_JOBS=10 -e CARGO_TERM_COLOR=never \
  -v "$W/runner:/home/runner/work/trustiot-v2/trustiot-v2" \
  -v "$W/alt:/work/repo" \
  -v "$W/out:/work/out" -v "$E/bundle:/work/bundle:ro" \
  -v /var/lib/containers \
  "$UBUNTU_ARM64" bash /work/bundle/inner.sh
echo "container: $(($(date +%s) - start)) s wall clock"

echo "######## results"
for f in versions.txt plan-runner.json build-runner.json build-alt.json binaries.sha256 rustc-vV.txt compare.txt; do
  echo "==== $f"
  cat "$W/out/$f" 2> /dev/null || echo "(missing)"
done
