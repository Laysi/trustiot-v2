#!/usr/bin/env bash
# Laysi/trustiot-v2#384: packs one aarch64 binary into its image the way scripts/release/build.ts
# of commit 75c9949 does (rust-binary.Containerfile, --platform linux/arm64, --timestamp, the
# release labels, push to an OCI archive), with podman as root and rootless and under each
# candidate setting. Every pack uses a podman store of its own (CONTAINERS_STORAGE_CONF), created
# for it and removed after it; nothing touches another store.
#   pack-variants.sh <prefix> <out dir> <binary> <rust-binary.Containerfile> <work dir for root> <work dir for the user>
# Environment:
#   AS_ROOT      runs a command as root: "sudo" on the build host, empty in a root container
#   AS_USER      runs a command as the unprivileged user: empty on the build host,
#                "runuser -u runner --" in the container
#   OTHER_OWNER  uid:gid of a user that does not run podman, for the "context owned by someone
#                else" variants as root
#   EPOCH COMMIT VERSION   the plan's sourceDateEpoch, commit and version (labels, --timestamp)
# Writes <out dir>/<prefix>-<root|rootless>-<variant>.oci.tar, prints one line per pack and exits
# non-zero when any pack failed.
set -uo pipefail
prefix=$1 out=$2 binary=$3 template=$4 work_root=$5 work_user=$6
: "${EPOCH:?}" "${COMMIT:?}" "${VERSION:?}" "${OTHER_OWNER:?}"
name=$(basename "$binary")
here=$(cd "$(dirname "$0")" && pwd)
failed=0

# pack <who: root|rootless> <variant> <storage driver> <Containerfile: plain|chown> <context owner: self|other> [build options...]
pack() {
  local who=$1 variant=$2 driver=$3 containerfile=$4 owner=$5
  shift 5
  local label=$prefix-$who-$variant work runner
  if [[ $who == root ]]; then work=$work_root/$label runner=${AS_ROOT:-}; else work=$work_user/$label runner=${AS_USER:-}; fi
  # shellcheck disable=SC2086 # $runner is a command prefix, empty or several words
  if $runner env WORK="$work" DRIVER="$driver" CONTAINERFILE="$containerfile" OWNER="$owner" WHO="$who" \
    OTHER_OWNER="$OTHER_OWNER" NAME="$name" BINARY="$binary" TEMPLATE="$template" OUT="$out/$label.oci.tar" \
    EPOCH="$EPOCH" COMMIT="$COMMIT" VERSION="$VERSION" OPTIONS="$*" \
    bash "$here/pack-one.sh"; then
    echo "packed $label ($driver, Containerfile $containerfile, context owner $owner${*:+, $*})"
  else
    echo "FAILED $label ($driver, Containerfile $containerfile, context owner $owner${*:+, $*})"
    failed=1
  fi
}

for who in rootless root; do
  # What build.ts does at 75c9949: overlay (the default driver), intermediate images (layers).
  pack "$who" 1-overlay-layers overlay plain self
  pack "$who" 2-overlay-nolayers overlay plain self --layers=false
  pack "$who" 3-vfs-layers vfs plain self
  pack "$who" 4-vfs-nolayers vfs plain self --layers=false
  # The context owned by another user than the one that runs podman, without and with --chown.
  pack "$who" 5-vfs-nolayers-other vfs plain other --layers=false
  pack "$who" 6-vfs-nolayers-chown-other vfs chown other --layers=false
done
exit "$failed"
