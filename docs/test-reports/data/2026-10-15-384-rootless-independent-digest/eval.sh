#!/usr/bin/env bash
# Laysi/trustiot-v2#384: whether a service image's digest depends on podman running as root or
# rootless. Runs on the build host through the check queue, from the root of the branch's
# snapshot; every podman store it uses is its own (CONTAINERS_STORAGE_CONF or --root/--runroot
# under the evaluation directory), and `cleanup` removes them all.
#   bash docs/test-reports/data/2026-10-15-384-rootless-independent-digest/eval.sh buildts <label>
#       plan.ts --dry-run and build.ts of the snapshot for identity, assets and migrate, once with
#       rootless podman (the user) and once as root (sudo), then compares the two sets of images;
#       the binaries are kept for `variants`
#   bash docs/test-reports/data/2026-10-15-384-rootless-independent-digest/eval.sh variants
#       the kept identity binary packed under each candidate setting (pack-variants.sh) with the
#       build host's podman and, in an ubuntu:24.04 container, with the runner's podman
#   bash docs/test-reports/data/2026-10-15-384-rootless-independent-digest/eval.sh rc <tag> <image>=<digest>...
#       compares a release candidate with this host's packaging of the same commit: a fresh
#       anonymous clone of the tag; release.yml's plan and build steps in an emulated arm64
#       ubuntu:24.04 container with the runner's podman as root (rc-inner.sh); then the binaries
#       built there packed again on the build host by pack.ts (pack-binaries.ts of the tag),
#       rootless and as root; every digest against the published ones given as arguments
#       (from the GitHub Release: gh release view <tag> --repo Laysi/trustiot-v2)
#   bash docs/test-reports/data/2026-10-15-384-rootless-independent-digest/eval.sh cleanup
#       removes the evaluation directory and every store in it
# Each step prints a "=== <name> ===" header and its exit status.
set -uo pipefail
mode=${1:?usage: eval.sh buildts <label> | variants | rc <tag> <image>=<digest>... | cleanup}
root=$(pwd)
DATA=$root/docs/test-reports/data/2026-10-15-384-rootless-independent-digest
E=$HOME/tv2/agents/384-eval
# ubuntu:24.04, the linux/amd64 manifest that Laysi/trustiot-v2#310 and #372 used.
UBUNTU_X64=docker.io/library/ubuntu@sha256:f610ab94648195aa356059f5b41d6085c9d4d903c072430cdd1af7bdb646106b
# ubuntu:24.04, the linux/arm64 manifest of the same index (Laysi/trustiot-v2#372).
UBUNTU_ARM64=docker.io/library/ubuntu@sha256:08571ca13e00ca07a2a84eab83a959b4242e22cceb16486a11bef1428c9e93a7
REPO_URL=https://github.com/Laysi/trustiot-v2.git
IMAGES=(identity assets migrate)
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
finish() {
  echo
  if ((${#failed[@]} > 0)); then
    printf 'FAILED: %s\n' "${failed[@]}"
    exit 1
  fi
  echo "ALL STEPS PASSED"
}

# remove_tree <dir>: unmounts whatever is mounted under <dir> and removes it, as root (stores of
# root podman, files of root containers, rootless stores with subordinate IDs). Only the
# evaluation directory or something beneath it, after resolving links.
remove_tree() {
  local target base
  if [[ -L $E ]]; then
    echo "remove_tree: $E is a symbolic link; not removing anything" >&2
    return 1
  fi
  base="$(realpath -m -- "$(dirname "$E")")/$(basename "$E")"
  target=$(realpath -m -- "$1")
  if [[ $target != "$base" && $target != "$base"/* ]]; then
    echo "remove_tree: $1 is not under $E; not removing it" >&2
    return 1
  fi
  [[ -e $target ]] || return 0
  # shellcheck disable=SC2016 # expanded by the inner shell
  local script='awk -v dir="$1" "\$5 == dir || index(\$5, dir \"/\") == 1 { print \$5 }" /proc/self/mountinfo | sort -r | xargs -r umount; rm -rf "$1"'
  sudo sh -c "$script" sh "$target"
  if [[ -e $target ]]; then
    echo "remove_tree: $1 is still there"
    return 1
  fi
}

# storage_conf <file> <dir>: a podman store of this job's own, with podman's default driver.
storage_conf() {
  mkdir -p "$2"
  printf '[storage]\ndriver = "overlay"\ngraphroot = "%s/storage"\nrunroot = "%s/run"\n' "$2" "$2" > "$1"
}

# bun_as_root <dir> <log> <bun arguments...>: runs bun as root (sudo) in <dir>, with the
# user's checkout, toolchains and Cargo target directory (built before as the user, so Cargo
# compiles nothing as root). SUDO_UID lets git read the user's checkout as root.
# CONTAINERS_STORAGE_CONF, when set, gives root podman a store of this job's own. Files that
# root created in the user's directories go back to the user afterwards.
bun_as_root() {
  local dir=$1 log=$2
  shift 2
  local env=(PATH="$PATH" HOME="$HOME" USER=root LOGNAME=root SUDO_UID="$(id -u)" SUDO_GID="$(id -g)" CI=1)
  local name
  for name in CARGO_TARGET_DIR CARGO_BUILD_JOBS CONTAINERS_STORAGE_CONF; do
    if [[ -n ${!name:-} ]]; then env+=("$name=${!name}"); fi
  done
  touch "$E/.before-root"
  (cd "$dir" && sudo env -i "${env[@]}" "$(proto bin bun --reporter text)" "$@") 2>&1 | tee "$log"
  local status=${PIPESTATUS[0]}
  echo "crates Cargo compiled as root: $(grep -cE '^ *Compiling ' "$log")"
  local dirs=() d
  for d in "$HOME/.proto" "$HOME/.rustup" "$HOME/.cargo" "$HOME/.bun" "${CARGO_TARGET_DIR:-}" "$root" "$dir"; do
    if [[ -n $d && -e $d ]]; then dirs+=("$d"); fi
  done
  echo "root-owned files created in the user's directories during the root run: $(sudo find "${dirs[@]}" -xdev -newer "$E/.before-root" -user 0 -print | wc -l)"
  sudo find "${dirs[@]}" -xdev -newer "$E/.before-root" -user 0 -exec chown -h "$(id -u):$(id -g)" {} +
  echo "left after chown: $(sudo find "${dirs[@]}" -xdev -newer "$E/.before-root" -user 0 -print | wc -l)"
  return "$status"
}

versions() {
  uname -srm
  grep PRETTY_NAME /etc/os-release
  podman --version
  sudo podman --version
  skopeo --version
  dpkg-query -W -f '  ${Package} ${Version}\n' podman buildah skopeo crun containers-storage golang-github-containers-image 2> /dev/null || true
  echo "bun $(bun --version)"
}

case $mode in
  buildts)
    label=${2:?usage: eval.sh buildts <label>}
    [[ $label =~ ^[A-Za-z0-9_-]{1,40}$ ]] || { echo "the label may only hold letters, digits, - and _" >&2; exit 2; }
    work=$E/buildts-$label
    remove_tree "$work"
    mkdir -p "$work" "$E/bin"
    must_pass "versions" versions
    must_pass "plan" bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run \
      --registry 127.0.0.1:5068/laysi/trustiot-v2 --output "$work/plan-all.json"
    # identity, assets and migrate; postgres (a Containerfile image with RUN) is not in scope.
    must_pass "plan of the Rust images" bun -e '
      const [from, to, ...names] = process.argv.slice(1);
      const plan = await Bun.file(from).json();
      plan.images = plan.images.filter((image) => names.includes(image.name));
      await Bun.write(to, JSON.stringify(plan));
      console.log(JSON.stringify({ commit: plan.commit, sourceDateEpoch: plan.sourceDateEpoch, version: plan.version, images: plan.images.map((i) => i.name) }));
    ' "$work/plan-all.json" "$work/plan.json" "${IMAGES[@]}"

    storage_conf "$work/rootless.conf" "$work/rootless-store"
    must_pass "build.ts, rootless podman" env CONTAINERS_STORAGE_CONF="$work/rootless.conf" \
      bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/rootless"
    for name in "${IMAGES[@]}"; do
      cp "$CARGO_TARGET_DIR/aarch64-unknown-linux-musl/release/$name" "$E/bin/$name"
    done

    # As root: the same checkout, toolchains and Cargo target directory (all built above, so
    # Cargo compiles nothing as root), a store of root's own. SUDO_UID lets git read the
    # user's checkout as root.
    storage_conf "$work/root.conf" "$work/root-store"
    CONTAINERS_STORAGE_CONF="$work/root.conf" must_pass "build.ts, podman as root" \
      bun_as_root "$root" "$work/root-build.log" scripts/release/build.ts --plan "$work/plan.json" --out "$work/root"

    mkdir -p "$work/compare"
    for name in "${IMAGES[@]}"; do
      cp "$work/rootless/images/$name.oci.tar" "$work/compare/rootless-$name.oci.tar"
      cp "$work/root/images/$name.oci.tar" "$work/compare/root-$name.oci.tar"
    done
    must_pass "build.json, rootless" cat "$work/rootless/build.json"
    must_pass "build.json, root" cat "$work/root/build.json"
    must_pass "compare" bun "$DATA/analyze.ts" "$work/compare"
    must_pass "binaries" bash -c 'cd "$1" && sha256sum *' sh "$E/bin"
    remove_tree "$work/root-store"
    # The rootless store: unmounted and removed inside the user's podman namespace, as root if anything is left.
    # shellcheck disable=SC2016 # expanded by the inner shell
    CONTAINERS_STORAGE_CONF="$work/rootless.conf" podman unshare sh -c 'awk -v dir="$1" "\$5 == dir || index(\$5, dir \"/\") == 1 { print \$5 }" /proc/self/mountinfo | sort -r | xargs -r umount; rm -rf "$1"' sh "$work/rootless-store"
    must_pass "remove the stores" remove_tree "$work/rootless-store"
    finish
    ;;
  variants)
    work=$E/variants
    remove_tree "$work"
    mkdir -p "$work/out" "$work/root" "$work/user" "$work/outer"
    must_pass "versions" versions
    plan=$(ls -t "$E"/buildts-*/plan.json | head -1)
    eval "$(bun -e 'const p = await Bun.file(process.argv[1]).json(); console.log(`EPOCH=${p.sourceDateEpoch} COMMIT=${p.commit} VERSION=${p.version}`)' "$plan")"
    export EPOCH COMMIT VERSION
    echo "plan: commit $COMMIT, epoch $EPOCH, version $VERSION; identity $(sha256sum "$E/bin/identity" | cut -c1-16)"
    must_pass "build host podman" env AS_ROOT=sudo AS_USER="" OTHER_OWNER="$(id -u):$(id -g)" \
      bash "$DATA/pack-variants.sh" host "$work/out" "$E/bin/identity" "$root/deploy/images/rust-binary.Containerfile" "$work/root" "$work/user"
    mkdir -p "$work/c-var" "$work/c-home"
    must_pass "runner's podman in ubuntu:24.04" sudo podman --root "$work/outer/storage" --runroot "$work/outer/run" \
      --tmpdir "$work/outer/tmp" --cgroup-manager cgroupfs --events-backend file \
      run --rm --privileged --network host --pull=missing \
      -v "$DATA:/work/bundle:ro" -v "$E/bin:/work/bin:ro" \
      -v "$root/deploy/images/rust-binary.Containerfile:/work/rust-binary.Containerfile:ro" \
      -v "$work/out:/work/out" -v "$work/c-var:/var/lib/containers" -v "$work/c-home:/home" \
      -e EPOCH="$EPOCH" -e COMMIT="$COMMIT" -e VERSION="$VERSION" \
      "$UBUNTU_X64" bash /work/bundle/variants-container.sh c493 identity
    # Also the build host's podman against the runner's, for the same settings.
    diffs=(--diff host-root-1-overlay-layers host-root-2-overlay-nolayers)
    for variant in 1-overlay-layers 3-vfs-layers 4-vfs-nolayers; do
      diffs+=(--diff "host-rootless-$variant" "c493-rootless-$variant")
    done
    must_pass "compare" bun "$DATA/analyze.ts" "$work/out" "${diffs[@]}"
    finish
    ;;
  rc)
    tag=${2:?usage: eval.sh rc <tag> <image>=<digest>...}
    shift 2
    [[ $tag =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-rc\.[0-9]+)?$ ]] || { echo "$tag is not a release tag (vX.Y.Z or vX.Y.Z-rc.N)" >&2; exit 2; }
    for pair in "$@"; do
      [[ $pair =~ ^[a-z][a-z0-9-]{0,63}=sha256:[0-9a-f]{64}$ ]] || { echo "$pair is not <image>=sha256:<64 hex digits>" >&2; exit 2; }
    done
    published=("$@")
    work=$E/rc-$tag
    remove_tree "$work"
    mkdir -p "$work/out" "$work/outer" "$work/c-var"
    must_pass "versions" versions
    echo "binfmt aarch64 handler (registered before this job, not changed by it): $(head -1 /proc/sys/fs/binfmt_misc/aarch64)"
    # An anonymous clone (no credential helper, no prompt) with the tag checked out as
    # actions/checkout does.
    must_pass "clone $tag" bash -c '
      GIT_TERMINAL_PROMPT=0 git -c credential.helper= clone -q "$1" "$2" &&
        git -C "$2" -c advice.detachedHead=false checkout -q "refs/tags/$3" &&
        echo "$3 → $(git -C "$2" rev-parse HEAD), committed $(git -C "$2" log -1 --format="%ct %cI")"
    ' sh "$REPO_URL" "$work/repo" "$tag"
    start=$(date +%s)
    must_pass "plan and build.ts in the emulated arm64 container, podman as root" sudo podman --root "$work/outer/storage" \
      --runroot "$work/outer/run" --tmpdir "$work/outer/tmp" --cgroup-manager cgroupfs --events-backend file \
      run --rm --platform linux/arm64 --privileged --network host --pull=missing --memory 11g --memory-swap 11g \
      -e CARGO_BUILD_JOBS=10 -e CARGO_TERM_COLOR=never \
      -v "$work/repo:/home/runner/work/trustiot-v2/trustiot-v2" -v "$work/out:/work/out" \
      -v "$work/c-var:/var/lib/containers" -v "$DATA:/work/bundle:ro" \
      "$UBUNTU_ARM64" bash /work/bundle/rc-inner.sh "$tag"
    echo "container: $(($(date +%s) - start)) s wall clock"
    # The binaries built there, packed by pack.ts of the tag on this host.
    pack=docs/test-reports/data/2026-10-15-384-rootless-independent-digest/pack-binaries.ts
    must_pass "pack.ts on the build host, rootless podman" bash -c 'cd "$1" && bun "$2" --plan "$3/plan-rust.json" --binaries "$3/bin" --out "$4"' \
      sh "$work/repo" "$pack" "$work/out" "$work/host-rootless"
    must_pass "pack.ts on the build host, podman as root" bun_as_root "$work/repo" "$work/host-root.log" \
      "$pack" --plan "$work/out/plan-rust.json" --binaries "$work/out/bin" --out "$work/host-root"
    must_pass "digests against $tag" bun -e '
      const args = process.argv.slice(1);
      const files = args.filter((arg) => !arg.includes("="));
      const want = Object.fromEntries(args.filter((arg) => arg.includes("=")).map((arg) => [arg.slice(0, arg.indexOf("=")), arg.slice(arg.indexOf("=") + 1)]));
      let differ = 0;
      for (const file of files) {
        const build = await Bun.file(file).json();
        console.log(`${file.split("/").slice(-3).join("/")}: podman ${build.builder?.podman ?? "?"}, buildah ${build.builder?.buildah ?? "?"}`);
        for (const image of build.images) {
          const same = image.digest === want[image.name];
          if (!same) differ += 1;
          console.log(`  ${same ? "SAME" : "DIFF"}  ${image.name}  ${image.digest}${same ? "" : ` / published ${want[image.name] ?? "(not given)"}`}`);
        }
      }
      process.exit(differ === 0 ? 0 : 1);
    ' "${published[@]}" "$work/out/build-root-arm64.json" "$work/host-rootless/build.json" "$work/host-root/build.json"
    finish
    ;;
  cleanup)
    remove_tree "$E" && echo "removed the evaluation directory"
    ;;
  *)
    echo "usage: eval.sh buildts <label> | variants | cleanup" >&2
    exit 2
    ;;
esac
