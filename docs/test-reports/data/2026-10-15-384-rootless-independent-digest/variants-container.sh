#!/usr/bin/env bash
# Laysi/trustiot-v2#384: runs pack-variants.sh with the runner's podman, inside a throwaway
# linux/amd64 ubuntu:24.04 container (podman, buildah and skopeo from Ubuntu's archive, as on the
# ubuntu-24.04-arm runner), as root and as an unprivileged user `runner` (uid 1001, its own
# group, subordinate IDs) whose rootless podman stands in for the runner's.
# Mounts: /work/bundle (this directory, read-only), /work/bin (the binary, read-only),
# /work/rust-binary.Containerfile (read-only), /work/out (results), /var/lib/containers and /home
# (directories of the host, so that overlay does not sit on the container's own overlay).
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
apt-get update -qq
apt-get install -y -qq podman crun skopeo uidmap ca-certificates > /dev/null
echo "Ubuntu packages in the container:"
dpkg-query -W -f '  ${Package} ${Version}\n' podman buildah skopeo crun golang-github-containers-image golang-github-containers-common 2> /dev/null || true
mkdir -p /etc/containers
cat > /etc/containers/containers.conf << 'EOF'
[containers]
netns = "host"
[engine]
cgroup_manager = "cgroupfs"
events_logger = "file"
runtime = "crun"
EOF
useradd --uid 1001 --user-group --create-home runner
echo "runner:100000:65536" > /etc/subuid
echo "runner:100000:65536" > /etc/subgid
install -d -o runner -g runner /home/runner/stores
mkdir -p /var/lib/containers/stores
chmod 1777 /work/out
AS_ROOT="" AS_USER="runuser -u runner --" OTHER_OWNER=1001:1001 \
  bash /work/bundle/pack-variants.sh "$1" /work/out "/work/bin/$2" /work/rust-binary.Containerfile /var/lib/containers/stores /home/runner/stores
