#!/usr/bin/env bash
# Laysi/trustiot-v2#310: runs release.yml's jobs for one commit on the build host, in a
# linux/amd64 container (today's x64 runner, cross-compiling) and in a linux/arm64 container
# under the host's existing qemu-user binfmt handler (the ubuntu-24.04-arm runner), then
# compares binaries, SBOMs and image digests. Started through rcheck from the worktree root,
# so $PWD is rcheck's snapshot of the branch.
#   bash ~/tv2/agents/310-eval/arm-eval.sh both     first round: x64 and arm64 from scratch
#   bash ~/tv2/agents/310-eval/arm-eval.sh round2   x64 Cargo rebuild in a fresh clone; the arm64
#                                                   release flow again on round one's clone and
#                                                   target directory (crun for RUN steps); an
#                                                   arm64 Cargo rebuild in a fresh clone
set -euo pipefail
which=${1:-both}
E=$HOME/tv2/agents/310-eval
W=$E/run
# ubuntu:24.04 (index sha256:534baea6a22c03a63003dbc8dbe78fe34bc0d7e595d9a9dc9834884ff530eb55), by the
# digest of each platform's manifest: Docker 29 with the containerd store refuses to pull a
# second platform of an index it already holds ("cannot overwrite digest").
UBUNTU_X64=docker.io/library/ubuntu@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b
UBUNTU_ARM64=docker.io/library/ubuntu@sha256:08571ca13e00ca07a2a84eab83a959b4242e22cceb16486a11bef1428c9e93a7
REGISTRY=docker.io/library/registry:3@sha256:ddf754342cfc8acc51a56d5d0ab6af06826461864460636d8bd5c546dab2a7b8
PORT=5168

echo "binfmt aarch64 handler (registered before this run, not changed by it):"
cat /proc/sys/fs/binfmt_misc/aarch64

# Root-owned files are removed from inside a container.
remove() {
  docker run --rm -v "$(dirname "$1"):/p" "$UBUNTU_X64" rm -rf "/p/$(basename "$1")"
}

cleanup() {
  docker rm -f tv2-310-reg tv2-310-run > /dev/null 2>&1 || true
}
trap cleanup EXIT

# run <label> <platform> <image> [ENV=value ...]: one container run of inner.sh, with the
# repository clone at $W/<label>/repo and the results at $W/<label>/out.
run() {
  local label=$1 platform=$2 image=$3
  shift 3
  local envs=()
  for kv in "$@"; do envs+=(-e "$kv"); done
  [[ -d $W/$label/repo ]] || git clone -q "$W/base" "$W/$label/repo"
  if [[ -d $W/$label/out ]]; then remove "$W/$label/out"; fi
  mkdir -p "$W/$label/out" "$W/$label/cargo-registry"
  docker run -d --rm --name tv2-310-reg -p "127.0.0.1:$PORT:5000" "$REGISTRY" > /dev/null
  local start=$(date +%s)
  # Sequential runs, each capped below the queue's 12 GiB. All use one port (one registry at a
  # time), so the plans, and the SBOM source names taken from them, are the same.
  docker run --rm --name tv2-310-run --platform "$platform" --privileged --network host \
    --memory 11g --memory-swap 11g \
    -e CARGO_BUILD_JOBS=10 -e CARGO_TERM_COLOR=never "${envs[@]}" \
    -v "$W/$label/repo:/work/repo" -v "$W/$label/out:/work/out" -v "$E:/work/bundle:ro" \
    -v "$W/$label/cargo-registry:/root/.cargo/registry" \
    -v /var/lib/containers \
    "$image" bash /work/bundle/inner.sh "$label" "$PORT"
  echo "$label: $(($(date +%s) - start)) s wall clock" | tee "$W/$label/out/wall.txt"
  docker rm -f tv2-310-reg > /dev/null
}

bun_() { PROTO_BUN_VERSION=1.4.2 "$HOME/.proto/shims/bun" "$@"; }

case $which in
  both)
    # One commit for every build: the branch snapshot plus PR #307's postgres Containerfile
    # (head e89bcf9, blob ed65e88), with a fixed commit time so SOURCE_DATE_EPOCH is known.
    if [[ -d $W ]]; then remove "$W"; fi
    mkdir -p "$W"
    git clone -q "$PWD" "$W/base"
    mkdir -p "$W/base/deploy/images/postgres"
    cp "$E/postgres.Containerfile" "$W/base/deploy/images/postgres/Containerfile"
    (
      cd "$W/base"
      git add deploy/images/postgres/Containerfile
      GIT_AUTHOR_DATE=2026-10-12T00:00:00Z GIT_COMMITTER_DATE=2026-10-12T00:00:00Z \
        git -c user.name=eval -c user.email=eval@localhost -c commit.gpgsign=false commit -q -m "eval: add the postgres image of PR 307"
      git -c tag.gpgsign=false tag v0.0.0-rc.1
      git log --format='%H %ct %s' -2
    )
    run x64 linux/amd64 "$UBUNTU_X64"
    run arm64 linux/arm64 "$UBUNTU_ARM64"
    bun_ "$E/compare.ts" "$W/x64/out" "$W/arm64/out" | tee "$W/compare.txt"
    ;;
  round2)
    git -C "$W/base" log --format='%H %ct %s' -2
    run x64b linux/amd64 "$UBUNTU_X64" REBUILD_ONLY=1
    run arm64 linux/arm64 "$UBUNTU_ARM64" ISOLATION=oci POSTGRES_FALLBACK=1
    run arm64b linux/arm64 "$UBUNTU_ARM64" REBUILD_ONLY=1
    echo "== x64 (round 1) against arm64 (round 2, same clone and target directory as round 1)"
    bun_ "$E/compare.ts" "$W/x64/out" "$W/arm64/out" | tee "$W/compare.txt"
    echo "== Cargo rebuilds in fresh clones"
    (cd "$W" && sha256sum x64/out/bin/* x64b/out/bin/* arm64/out/bin/* arm64b/out/bin/*) | tee "$W/rebuilds.txt"
    for bin in identity release-selftest; do
      echo "== elfdiff $bin: x64 against x64b"
      bun_ "$E/elfdiff.ts" "$W/x64/out/bin/$bin" "$W/x64b/out/bin/$bin"
      echo "== elfdiff $bin: arm64 against arm64b"
      bun_ "$E/elfdiff.ts" "$W/arm64/out/bin/$bin" "$W/arm64b/out/bin/$bin"
    done 2>&1 | tee "$W/elfdiff.txt"
    ;;
  round3)
    # Round two stopped after the arm64 release flow, at `podman run` of the selftest image,
    # which user-mode emulation cannot do. Here: the arm64 Cargo rebuild, the comparisons, and
    # the selftest image with the host's Docker (its runc; the binary under the qemu handler).
    git -C "$W/base" log --format='%H %ct %s' -2
    run arm64b linux/arm64 "$UBUNTU_ARM64" REBUILD_ONLY=1
    echo "== x64 (round 1) against arm64 (round 2, same clone and target directory as round 1)"
    bun_ "$E/compare.ts" "$W/x64/out" "$W/arm64/out" | tee "$W/compare.txt"
    echo "== Cargo rebuilds in fresh clones"
    (cd "$W" && sha256sum x64/out/bin/* x64b/out/bin/* arm64/out/bin/* arm64b/out/bin/*) | tee "$W/rebuilds.txt"
    for bin in identity release-selftest; do
      echo "== elfdiff $bin: x64 against x64b"
      bun_ "$E/elfdiff.ts" "$W/x64/out/bin/$bin" "$W/x64b/out/bin/$bin"
      echo "== elfdiff $bin: arm64 against arm64b"
      bun_ "$E/elfdiff.ts" "$W/arm64/out/bin/$bin" "$W/arm64b/out/bin/$bin"
      echo "== elfdiff $bin: x64 against arm64"
      bun_ "$E/elfdiff.ts" "$W/x64/out/bin/$bin" "$W/arm64/out/bin/$bin"
    done 2>&1 | tee "$W/elfdiff.txt"
    echo "== release-selftest image of the arm64 run, with the host's Docker"
    docker load -i "$W/arm64/out/release/images/release-selftest.oci.tar" | tee "$W/selftest-load.txt" || true
    loaded=$(sed -n 's/^Loaded image[^:]*: //p' "$W/selftest-load.txt" | tail -1)
    echo "loaded: $loaded"
    if [[ -n $loaded ]]; then
      docker run --rm --platform linux/arm64 "$loaded" | tee "$W/selftest-run.txt" || echo "selftest run failed"
      docker rmi "$loaded" > /dev/null || true
    fi
    ;;
  *)
    echo "usage: arm-eval.sh both|round2|round3" >&2
    exit 2
    ;;
esac
