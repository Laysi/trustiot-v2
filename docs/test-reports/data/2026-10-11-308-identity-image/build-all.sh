#!/usr/bin/env bash
# Laysi/trustiot-v2#308: builds the identity image with the release flow and with the nightly
# helper (podman), then extracts both binaries and describes them. Run from the worktree root.
set -euo pipefail
bin_dir=$(dirname "$(readlink -f "$0")")
wt=$PWD
work=$HOME/.cache/trustiot-v2-308
rm -rf "$work"; mkdir -p "$work"
echo "host: $(uname -m) $(nproc) cores, load $(cut -d' ' -f1-3 /proc/loadavg), CARGO_BUILD_JOBS=$CARGO_BUILD_JOBS, HEAD $(git rev-parse HEAD), target dir present: $(test -d target && echo yes || echo no)"
t0=$(date +%s)
bun scripts/release/plan.ts --tag v0.0.0-rc.1 --dry-run --output "$work/plan.json" > "$work/plan.log" 2>&1 || { cat "$work/plan.log"; exit 1; }
tail -5 "$work/plan.log"
bun scripts/release/build.ts --plan "$work/plan.json" --out "$work/out" > "$work/release-build.log" 2>&1 || { tail -80 "$work/release-build.log"; exit 1; }
t1=$(date +%s); echo "release_build_seconds=$((t1-t0)) load $(cut -d' ' -f1-3 /proc/loadavg)"
grep -v "^ *Compiling \|^ *Downloaded \|^ *Downloading " "$work/release-build.log" | tail -40
echo "--- build.json"; cat "$work/out/build.json"
ls -l "$work/out/images"
echo "--- nightly helper (podman)"
bun "$bin_dir/nightly-build-podman.ts" "$wt" "$work/nightly-helper.log"
t2=$(date +%s); echo "nightly_helper_build_seconds=$((t2-t1))"
grep -v "^ *Compiling " "$work/nightly-helper.log" | cut -c1-300 | tail -25
echo "--- images"
skopeo copy --quiet "oci-archive:$work/out/images/identity.oci.tar" containers-storage:localhost/trustiot-308-check/identity-release:0.0.0-rc.1
for img in localhost/trustiot-308-check/identity-release:0.0.0-rc.1 localhost/trustiot-e2e/identity:nightly; do
  podman image inspect "$img" --format "$img os={{.Os}} arch={{.Architecture}} size={{.Size}} user={{.User}} entrypoint={{.Config.Entrypoint}} id={{.Id}}"
  cid=$(podman create --platform linux/arm64 "$img" 2>/dev/null)
  out="$work/$(echo "$img" | tr '/:' '__')-identity"
  podman cp "$cid:/usr/local/bin/identity" "$out"
  podman rm "$cid" > /dev/null
  file "$out"; sha256sum "$out"; ls -l "$out"
done
skopeo inspect --raw "oci-archive:$work/out/images/identity.oci.tar" | bun -e 'const m=JSON.parse(await Bun.stdin.text()); console.log("compressed layer bytes:", m.layers.reduce((a,l)=>a+l.size,0), "layers:", m.layers.length)'
podman image rm localhost/trustiot-308-check/identity-release:0.0.0-rc.1 localhost/trustiot-e2e/identity:nightly > /dev/null
echo done
