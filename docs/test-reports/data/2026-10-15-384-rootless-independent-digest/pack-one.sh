#!/usr/bin/env bash
# Laysi/trustiot-v2#384: one pack of pack-variants.sh, run as root or as the unprivileged user;
# its settings come from the environment that pack-variants.sh sets.
set -euo pipefail
rm -rf "$WORK"
mkdir -p "$WORK"
# rootless_storage_path: podman 4.9.3 ignores graphroot for a rootless user, and its libpod
# database then stays in the default store, whose recorded driver overrides this one (seen in the
# first run of this script).
cat > "$WORK/storage.conf" << CONF
[storage]
driver = "$DRIVER"
graphroot = "$WORK/storage"
runroot = "$WORK/run"
rootless_storage_path = "$WORK/storage"
CONF
export CONTAINERS_STORAGE_CONF=$WORK/storage.conf
# The store podman actually uses must be this one, with this driver.
store=$(podman info --format "{{.Store.GraphRoot}} {{.Store.GraphDriverName}}" 2>&1)
if [[ $store != "$WORK/storage $DRIVER" ]]; then
  echo "podman uses the store $store, not $WORK/storage $DRIVER"
  exit 1
fi

# Remove the store: unmount what podman left mounted under it, from inside the user namespace
# when rootless (its files belong to subordinate IDs there).
remove_store() {
  # shellcheck disable=SC2016 # expanded by the inner shell
  local script='awk -v dir="$1" "\$5 == dir || index(\$5, dir \"/\") == 1 { print \$5 }" /proc/self/mountinfo | sort -r | xargs -r umount; rm -rf "$1"'
  if [[ $(id -u) -eq 0 ]]; then sh -c "$script" sh "$WORK"; else podman unshare sh -c "$script" sh "$WORK"; fi
}
trap remove_store EXIT

# The build context as build.ts makes it: a new directory, the binary with mode 0755 and the
# rendered Containerfile, created by the identity that runs podman.
ctx=$WORK/context/$NAME
mkdir -p "$ctx"
cp "$BINARY" "$ctx/$NAME"
chmod 0755 "$ctx/$NAME"
sed "s/@BIN@/$NAME/g" "$TEMPLATE" > "$ctx/Containerfile"
if [[ $CONTAINERFILE == chown ]]; then
  sed -i 's/^COPY --chmod=0555 /COPY --chown=0:0 --chmod=0555 /' "$ctx/Containerfile"
  grep -q '^COPY --chown=0:0 --chmod=0555 ' "$ctx/Containerfile"
fi
if [[ $OWNER == other ]]; then
  if [[ $WHO == root ]]; then
    chown "$OTHER_OWNER" "$ctx" "$ctx/$NAME" "$ctx/Containerfile"
  else
    podman unshare chown 1000:1000 "$ctx" "$ctx/$NAME" "$ctx/Containerfile"
  fi
fi
# shellcheck disable=SC2086 # OPTIONS holds separate options
if ! podman build --platform linux/arm64 --timestamp "$EPOCH" --pull=missing --format oci $OPTIONS \
  --label org.opencontainers.image.source=https://github.com/Laysi/trustiot-v2 \
  --label "org.opencontainers.image.revision=$COMMIT" \
  --label "org.opencontainers.image.version=$VERSION" \
  --label "org.opencontainers.image.title=$NAME" \
  --file "$ctx/Containerfile" --tag "localhost/trustiot-release/$NAME:$VERSION" "$ctx" > "$WORK/build.log" 2>&1; then
  tail -5 "$WORK/build.log"
  exit 1
fi
rm -f "$OUT"
podman push --format oci --compression-format gzip "localhost/trustiot-release/$NAME:$VERSION" "oci-archive:$OUT" > "$WORK/push.log" 2>&1 || {
  tail -5 "$WORK/push.log"
  exit 1
}
