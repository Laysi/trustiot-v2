#!/usr/bin/env bash
# Laysi/trustiot-v2#372. Runs inside a throwaway linux/amd64 ubuntu:24.04 container (native, no
# emulation). Packs the identity binary that v0.0.0-rc.5 published (extracted from its GHCR
# image by images.sh; byte-identical to the rebuild) into the identity image the way build.ts
# does: the same podman 4.9.3 from Ubuntu's archive, the same Containerfile, options and labels.
# It packs it twice, with podman as root (how inner.sh ran) and rootless as an unprivileged user
# (how build.ts runs on the GitHub runner, as the user `runner`), and prints each result's digest
# and the diff_id of its COPY layer, to compare with the published image.
#
# Mounts: /work/repo (the clone of the tag, read-only), /work/out (results), /work/bundle (ro).
set -euo pipefail
out=/work/out/layers-x64
rm -rf "$out"
mkdir -p "$out"
exec > >(tee "$out/layer-inner.log") 2>&1
export DEBIAN_FRONTEND=noninteractive
echo "== $(date -u +%FT%TZ) apt: podman, skopeo, uidmap (rootless podman needs newuidmap)"
apt-get update -qq
apt-get install -y -qq podman crun skopeo uidmap jq ca-certificates > /dev/null
dpkg-query -W -f '  ${Package} ${Version}\n' podman skopeo crun buildah golang-github-containers-common golang-github-containers-image uidmap
mkdir -p /etc/containers
cat > /etc/containers/containers.conf << 'EOF'
[containers]
netns = "host"
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
EOF

# The plan's values for v0.0.0-rc.5 (plan-runner.json).
EPOCH=1791832004
COMMIT=716740a9fa638a39e4655c54173653a36f2b121d
VERSION=0.0.0-rc.5
TEMPLATE=/work/repo/deploy/images/rust-binary.Containerfile

# pack <label> <user>: the build context as build.ts makes it (a temporary directory with the
# binary, mode 0755, and the rendered Containerfile), then build.ts's podman build and push.
pack() {
  local label=$1 user=$2
  local ctx
  ctx=$(mktemp -d /tmp/trustiot-release-XXXXXX)/identity
  mkdir -p "$ctx"
  cp /work/out/published-identity "$ctx/identity"
  chmod 0755 "$ctx/identity"
  sed 's/@BIN@/identity/g' "$TEMPLATE" > "$ctx/Containerfile"
  chown -R "$user" "$(dirname "$ctx")"
  mkdir -p "$out/$label"
  chown "$user" "$out/$label"
  local script
  script=$(cat << EOF
set -euo pipefail
cd /tmp
echo "[$label] podman as \$(id -u):\$(id -g), graph driver \$(podman info --format '{{.Store.GraphDriverName}}' 2> /dev/null), rootless \$(podman info --format '{{.Host.Security.Rootless}}' 2> /dev/null)"
podman build --platform linux/arm64 --timestamp $EPOCH --pull=missing --format oci \
  --label org.opencontainers.image.source=https://github.com/Laysi/trustiot-v2 \
  --label org.opencontainers.image.revision=$COMMIT \
  --label org.opencontainers.image.version=$VERSION \
  --label org.opencontainers.image.title=identity \
  --file $ctx/Containerfile --tag localhost/trustiot-release/identity:$VERSION $ctx
podman push --format oci --compression-format gzip localhost/trustiot-release/identity:$VERSION oci-archive:$out/$label/identity.oci.tar
podman image rm localhost/trustiot-release/identity:$VERSION > /dev/null
EOF
)
  if [[ $user == root ]]; then
    bash -c "$script"
  else
    su "$user" -s /bin/bash -c "$script"
  fi
  mkdir -p "$out/$label/oci"
  tar -xf "$out/$label/identity.oci.tar" -C "$out/$label/oci"
  local manifest config
  manifest=$(jq -r '.manifests[0].digest' "$out/$label/oci/index.json")
  config=$(jq -r '.config.digest' "$out/$label/oci/blobs/sha256/${manifest#sha256:}")
  echo "[$label] image $manifest"
  echo "[$label] COPY layer $(jq -r '.layers[-1].digest' "$out/$label/oci/blobs/sha256/${manifest#sha256:}"), diff_id $(jq -r '.rootfs.diff_ids[-1]' "$out/$label/oci/blobs/sha256/${config#sha256:}")"
}

pack root root
# The runner's user: uid 1001 `runner`, with subordinate ids for rootless podman.
useradd --uid 1001 --create-home runner
echo "runner:100000:65536" > /etc/subuid
echo "runner:100000:65536" > /etc/subgid
pack rootless runner
echo "published: image sha256:7a042972e7d25d3de66b7573982c04125bc04933b20ae3a923249ed848fcfa44, COPY layer diff_id sha256:8b3d9619d37cb357b044179f07d9d0db705a2ed63db609660791e0575bccc2fb"
