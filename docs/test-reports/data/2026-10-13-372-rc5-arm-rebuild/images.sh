#!/usr/bin/env bash
# Laysi/trustiot-v2#372. Run by inner.sh inside the arm64 container after both builds:
#   images.sh <out> <bun>
# Compares the two rebuilds with each other and with the v0.0.0-rc.5 digests that release.yml
# run 38295508885 logged. The identity package on GHCR is public, so its published image is
# pulled without credentials (--src-no-creds) and compared layer by layer, binary included.
# release-selftest is private and is compared by digest only.
set -euo pipefail
out=$1
BUN=$2
exec > >(tee "$out/compare.txt") 2>&1
GHCR=ghcr.io/laysi/trustiot-v2
declare -A PUBLISHED=(
  [identity]=sha256:7a042972e7d25d3de66b7573982c04125bc04933b20ae3a923249ed848fcfa44
  [release-selftest]=sha256:bc2b33e06519a9d303195f4196ca7c73d9f14357e0106709d9f434e928421743
)
work=$out/layers
rm -rf "$work"
mkdir -p "$work"

verdict() { if [[ $2 == "$3" ]]; then echo "SAME  $1  $2"; else echo "DIFF  $1  $2 / $3"; fi; }
digest_of() { jq -r --arg n "$2" '.images[] | select(.name == $n) | .digest' "$out/build-$1.json"; }
sha() { echo "sha256:$(sha256sum "$1" | cut -d' ' -f1)"; }

# unpack <oci-archive> <dir>: unpacks a one-image OCI layout; prints the manifest's path.
unpack() {
  mkdir -p "$2"
  tar -xf "$1" -C "$2"
  local digest
  digest=$(jq -r '.manifests[0].digest' "$2/index.json")
  echo "$2/blobs/sha256/${digest#sha256:}"
}
# describe <manifest> <blob dir> <label>: the manifest's config and layers, and the image's
# binary (in the last layer, the COPY of rust-binary.Containerfile) extracted to <dir>/bin.
describe() {
  local manifest=$1 blobs=$2 dir=$3
  echo "  manifest $(sha "$manifest"), $(wc -c < "$manifest") bytes"
  echo "  config   $(jq -r '.config.digest' "$manifest")"
  jq -r '.layers[] | "  layer    \(.digest) \(.size) \(.mediaType)"' "$manifest"
  local last
  last=$(jq -r '.layers[-1].digest' "$manifest")
  mkdir -p "$dir/rootfs"
  tar -xzf "$blobs/${last#sha256:}" -C "$dir/rootfs"
  find "$dir/rootfs" -type f -printf '  file     %P %s bytes, mode %m\n'
}

for name in identity release-selftest; do
  echo "######## $name"
  runner=$(digest_of runner "$name")
  alt=$(digest_of alt "$name")
  verdict "$name image: runner-path rebuild / alt-path rebuild" "$runner" "$alt"
  verdict "$name image: runner-path rebuild / published v0.0.0-rc.5" "$runner" "${PUBLISHED[$name]}"
  verdict "$name binary: runner-path rebuild / alt-path rebuild" "$(sha "$out/runner/bin/$name")" "$(sha "$out/alt/bin/$name")"
  echo "runner-path rebuild:"
  m=$(unpack "$out/runner/release/images/$name.oci.tar" "$work/runner-$name")
  describe "$m" "$work/runner-$name/blobs/sha256" "$work/runner-$name"
  verdict "$name binary: Cargo output / in the rebuilt image" "$(sha "$out/runner/bin/$name")" "$(sha "$work/runner-$name/rootfs/usr/local/bin/$name")"
  echo "config of the rebuilt image:"
  jq -S . "$work/runner-$name/blobs/sha256/$(jq -r '.config.digest' "$m" | cut -d: -f2)" | sed 's/^/  /'

  if [[ $name == identity ]]; then
    echo "published $GHCR/$name@${PUBLISHED[$name]}, pulled without credentials:"
    if skopeo copy --quiet --src-no-creds "docker://$GHCR/$name@${PUBLISHED[$name]}" "dir:$work/ghcr-$name"; then
      pm=$work/ghcr-$name/manifest.json
      verdict "$name: published manifest bytes hash to the published digest" "$(sha "$pm")" "${PUBLISHED[$name]}"
      describe "$pm" "$work/ghcr-$name" "$work/ghcr-$name"
      echo "manifest, rebuilt against published (diff of jq -S):"
      diff <(jq -S . "$m") <(jq -S . "$pm") && echo "  no difference"
      echo "config, rebuilt against published (diff of jq -S):"
      diff <(jq -S . "$work/runner-$name/blobs/sha256/$(jq -r '.config.digest' "$m" | cut -d: -f2)") \
        <(jq -S . "$work/ghcr-$name/$(jq -r '.config.digest' "$pm" | cut -d: -f2)") && echo "  no difference"
      published_bin=$work/ghcr-$name/rootfs/usr/local/bin/$name
      verdict "$name binary: runner-path rebuild / published v0.0.0-rc.5" "$(sha "$out/runner/bin/$name")" "$(sha "$published_bin")"
      cp "$published_bin" "$out/published-$name"
      if [[ $(sha "$out/runner/bin/$name") != "$(sha "$published_bin")" ]]; then
        echo "== elfdiff: runner-path rebuild against published"
        "$BUN" /work/bundle/elfdiff.ts "$out/runner/bin/$name" "$published_bin" || true
      fi
    else
      echo "DIFF  could not pull $GHCR/$name@${PUBLISHED[$name]} without credentials"
    fi
  fi
  if [[ $(sha "$out/runner/bin/$name") != "$(sha "$out/alt/bin/$name")" ]]; then
    echo "== elfdiff: runner-path rebuild against alt-path rebuild"
    "$BUN" /work/bundle/elfdiff.ts "$out/runner/bin/$name" "$out/alt/bin/$name" || true
  fi
done
