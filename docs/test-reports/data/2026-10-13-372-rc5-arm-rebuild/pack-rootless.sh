#!/usr/bin/env bash
# Laysi/trustiot-v2#372. Runs inside a throwaway ubuntu:24.04 container (linux/amd64 natively or
# linux/arm64 under emulation). Packs the binaries of the rebuild (/work/out/runner/bin) into
# their images the way build.ts runs on the GitHub runner: podman rootless, as an unprivileged
# user whose files (the build context) it owns with its own user and group, default storage.
# Each image is packed twice from a fresh copy of the context (another mtime) and fresh storage,
# and the digests are compared with what v0.0.0-rc.5 published.
#   pack-rootless.sh <label>
# Mounts: /work/repo (the clone of the tag, read-only), /work/out (results), /work/bundle (ro).
set -euo pipefail
label=$1
out=/work/out/pack-rootless-$label
rm -rf "$out"
mkdir -p "$out"
exec > >(tee "$out/pack.log") 2>&1
echo "== $(date -u +%FT%TZ) $(uname -m): apt: podman, skopeo, uidmap"
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq podman crun skopeo uidmap jq ca-certificates > /dev/null
dpkg-query -W -f '  ${Package} ${Version}\n' podman buildah uidmap
mkdir -p /etc/containers
cat > /etc/containers/containers.conf << 'EOF'
[containers]
netns = "host"
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
EOF
# The runner's account: uid 1001, its own group, subordinate ids for rootless podman.
useradd --uid 1001 --user-group --create-home runner
echo "runner:100000:65536" > /etc/subuid
echo "runner:100000:65536" > /etc/subgid
echo "user: $(id runner)"

EPOCH=1791832004
COMMIT=716740a9fa638a39e4655c54173653a36f2b121d
VERSION=0.0.0-rc.5
declare -A PUBLISHED=(
  [identity]=sha256:7a042972e7d25d3de66b7573982c04125bc04933b20ae3a923249ed848fcfa44
  [release-selftest]=sha256:bc2b33e06519a9d303195f4196ca7c73d9f14357e0106709d9f434e928421743
)

for attempt in 1 2; do
  for name in identity release-selftest; do
    d=$out/$attempt-$name
    mkdir -p "$d"
    # build.ts: a fresh temporary directory, the binary copied in with mode 0755, the rendered
    # Containerfile; here created by the user itself, as on the runner.
    script=$(cat << EOF
set -euo pipefail
cd /tmp
ctx=\$(mktemp -d /tmp/trustiot-release-XXXXXX)/$name
mkdir -p "\$ctx"
cp /work/out/runner/bin/$name "\$ctx/$name"
chmod 0755 "\$ctx/$name"
sed 's/@BIN@/$name/g' /work/repo/deploy/images/rust-binary.Containerfile > "\$ctx/Containerfile"
podman build --platform linux/arm64 --timestamp $EPOCH --pull=missing --format oci \
  --label org.opencontainers.image.source=https://github.com/Laysi/trustiot-v2 \
  --label org.opencontainers.image.revision=$COMMIT \
  --label org.opencontainers.image.version=$VERSION \
  --label org.opencontainers.image.title=$name \
  --file "\$ctx/Containerfile" --tag localhost/trustiot-release/$name:$VERSION "\$ctx" > $d/build.log 2>&1
podman push --format oci --compression-format gzip localhost/trustiot-release/$name:$VERSION oci-archive:$d/image.oci.tar 2> /dev/null
podman image rm localhost/trustiot-release/$name:$VERSION > /dev/null
rm -rf "\$(dirname "\$ctx")"
EOF
)
    chown runner:runner "$d"
    if su runner -s /bin/bash -c "$script"; then
      mkdir -p "$d/oci"
      tar -xf "$d/image.oci.tar" -C "$d/oci"
      digest=$(jq -r '.manifests[0].digest' "$d/oci/index.json")
      if [[ $digest == "${PUBLISHED[$name]}" ]]; then verdict=SAME; else verdict=DIFF; fi
      echo "$verdict  attempt $attempt, $name: $digest, published ${PUBLISHED[$name]}"
      rm -rf "$d/oci" "$d/image.oci.tar"
    else
      echo "FAILED  attempt $attempt, $name: rootless podman did not build; $d/build.log:"
      tail -5 "$d/build.log" || true
    fi
  done
  # Fresh storage for the second attempt.
  su runner -s /bin/bash -c 'podman system reset --force > /dev/null 2>&1 || true'
done
