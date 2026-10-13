#!/usr/bin/env bash
# Laysi/trustiot-v2#372. Runs inside a throwaway linux/amd64 ubuntu:24.04 container (native).
# layer-inner.sh showed that rootless podman reproduces the published COPY layer of identity
# v0.0.0-rc.5 but not its image digest: the config's history comments ("FROM <id>") name
# intermediate images whose IDs differ. This packs the published binary as build.ts does, each
# time in fresh storage, with podman as root and rootless, and with --layers=false and the vfs
# storage driver, to see which settings give the same digest every time and as root and
# rootless. For the default settings it also saves the configs of the intermediate images.
#
# Mounts: /work/repo (the clone of the tag, read-only), /work/out (results), /work/bundle (ro).
set -euo pipefail
out=/work/out/variants
rm -rf "$out"
mkdir -p "$out"
exec > >(tee "$out/variants.log") 2>&1
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq podman crun skopeo uidmap jq ca-certificates > /dev/null
dpkg-query -W -f '  ${Package} ${Version}\n' podman buildah golang-github-containers-storage golang-github-containers-image 2> /dev/null || true
mkdir -p /etc/containers
cat > /etc/containers/containers.conf << 'EOF'
[containers]
netns = "host"
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
EOF
useradd --uid 1001 --create-home runner
echo "runner:100000:65536" > /etc/subuid
echo "runner:100000:65536" > /etc/subgid

EPOCH=1791832004
COMMIT=716740a9fa638a39e4655c54173653a36f2b121d
VERSION=0.0.0-rc.5
TAG=localhost/trustiot-release/identity:$VERSION
ctx=/tmp/trustiot-release-variants/identity
mkdir -p "$ctx"
cp /work/out/published-identity "$ctx/identity"
chmod 0755 "$ctx/identity"
sed 's/@BIN@/identity/g' /work/repo/deploy/images/rust-binary.Containerfile > "$ctx/Containerfile"
chmod -R a+rX /tmp/trustiot-release-variants

# pack <label> <user> <storage driver> [build options...]: fresh storage under /home/runner
# (an ext4 volume) for both users, then build.ts's build and push.
pack() {
  local label=$1 user=$2 driver=$3
  shift 3
  local d=$out/$label
  mkdir -p "$d"
  local root=/home/runner/storage/$label run=/tmp/runroot-$label
  mkdir -p "$root" "$run"
  chown "$user" "$d" "$root" "$run"
  local g="--root $root --runroot $run --storage-driver $driver"
  # skopeo reads a rootless store only inside podman's user namespace.
  local in_ns=""
  if [[ $user != root ]]; then in_ns="podman $g unshare"; fi
  local script
  script=$(cat << EOF
set -euo pipefail
cd /tmp
podman $g build --platform linux/arm64 --timestamp $EPOCH --pull=missing --format oci $* \
  --label org.opencontainers.image.source=https://github.com/Laysi/trustiot-v2 \
  --label org.opencontainers.image.revision=$COMMIT \
  --label org.opencontainers.image.version=$VERSION \
  --label org.opencontainers.image.title=identity \
  --file $ctx/Containerfile --tag $TAG $ctx > $d/build.log 2>&1
podman $g push --format oci --compression-format gzip $TAG oci-archive:$d/identity.oci.tar 2> /dev/null
# Every image in the store with its config, intermediate ones included.
for id in \$(podman $g images --all --no-trunc --quiet | sed 's/^sha256://' | sort -u); do
  $in_ns skopeo inspect --raw --config "containers-storage:[$driver@$root+$run]\$id" > $d/config-\${id:0:12}.json 2> /dev/null || true
done
EOF
)
  if [[ $user == root ]]; then bash -c "$script"; else su "$user" -s /bin/bash -c "$script"; fi
  mkdir -p "$d/oci"
  tar -xf "$d/identity.oci.tar" -C "$d/oci"
  local m c
  m=$(jq -r '.manifests[0].digest' "$d/oci/index.json")
  c=$(jq -r '.config.digest' "$d/oci/blobs/sha256/${m#sha256:}")
  local cfg=$d/oci/blobs/sha256/${c#sha256:}
  echo "$label: image $m, COPY diff_id $(jq -r '.rootfs.diff_ids[-1]' "$cfg"), history comments: $(jq -r '[.history[] | .comment // empty | select(startswith("FROM ") and (contains("/") | not))] | join(", ")' "$cfg")"
  rm -rf "$d/oci" "$d/identity.oci.tar"
  rm -rf "$root" "$run"
}

pack rootless-1 runner overlay || echo "rootless-1: FAILED"
pack rootless-2 runner overlay || echo "rootless-2: FAILED"
pack root-1 root overlay || echo "root-1: FAILED"
pack root-2 root overlay || echo "root-2: FAILED"
pack rootless-nolayers runner overlay --layers=false || echo "rootless-nolayers: FAILED"
pack root-nolayers root overlay --layers=false || echo "root-nolayers: FAILED"
pack rootless-vfs runner vfs || echo "rootless-vfs: FAILED"
pack root-vfs root vfs || echo "root-vfs: FAILED"
pack rootless-vfs-nolayers runner vfs --layers=false || echo "rootless-vfs-nolayers: FAILED"
pack root-vfs-nolayers root vfs --layers=false || echo "root-vfs-nolayers: FAILED"
echo "published: image sha256:7a042972e7d25d3de66b7573982c04125bc04933b20ae3a923249ed848fcfa44, COPY diff_id sha256:8b3d9619d37cb357b044179f07d9d0db705a2ed63db609660791e0575bccc2fb, history comments: FROM 5d2100980097, FROM c3b284fea6d2, FROM 45938a1e651f"

echo "== the intermediate image after COPY: rootless-1 against rootless-2, then rootless-1 against root-1"
# The intermediate after COPY: its history has the COPY step but not yet the USER step.
after_copy() { grep -l 'COPY file:' "$out/$1"/config-*.json | xargs -r grep -L 'USER 65532' | head -1; }
for pair in "rootless-1 rootless-2" "rootless-1 root-1"; do
  read -r a b <<< "$pair"
  ca=$(after_copy "$a")
  cb=$(after_copy "$b")
  echo "-- $a ($ca) against $b ($cb)"
  if [[ -n $ca && -n $cb ]]; then
    diff <(jq -S . "$ca") <(jq -S . "$cb") && echo "   no difference"
  fi
done
