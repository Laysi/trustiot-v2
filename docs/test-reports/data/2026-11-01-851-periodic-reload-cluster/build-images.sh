# Builds what the lab run of Laysi/trustiot-v2#851 needs, on an x86_64 Linux build host with
# rootless podman, from the repository root (docs/test-reports/2026-11-01-851-periodic-reload-cluster.md):
# - the arm64 images of the identity service, the migration Job, crates/service-kit's
#   example-service and the certs service, cross-compiled for aarch64-unknown-linux-musl with the
#   pinned zig and cargo-zigbuild as the release does (default features), and packed without
#   running anything in the arm64 image;
# - the certs test image: the same certs built once more with the test-only feature
#   `e2e-reload-panic` (services/certs/Cargo.toml), under another image name. Both certs binaries
#   are searched for the injected panic's message: only the test build may contain it;
# - three test roots, signed with certs-ca as at a root ceremony: A and B, and C whose common name
#   carries the feature's marker. Their keys are dropped at once: the run needs no signature.
# Output in ~/.cache/trustiot-v2-851: the image archives and root-a.pem, root-b.pem, root-c.pem.
set -euo pipefail
out=$HOME/.cache/trustiot-v2-851
# A previous run's store holds files of the user namespace's other UIDs.
if [[ -d $out ]]; then podman unshare rm -rf "$out"; fi
mkdir -p "$out"
pbin() { env -u AI_AGENT -u CLAUDECODE proto bin "$1" --reporter text | tail -n 1; }
proto install zig > /dev/null 2>&1 || true
proto install cargo-zigbuild > /dev/null 2>&1 || true
zig=$(pbin zig); zb=$(pbin cargo-zigbuild)
echo "zig: $("$zig" version)"
rustup target add aarch64-unknown-linux-musl > /dev/null 2>&1
target=${CARGO_TARGET_DIR:-target}
release=$target/aarch64-unknown-linux-musl/release
zigbuild() {
  CARGO_ZIGBUILD_ZIG_PATH="$zig" PATH="$(dirname "$zb"):$PATH" \
    cargo zigbuild --locked --release --target aarch64-unknown-linux-musl "$@" 2>&1 | tail -2
}
# Default features, as scripts/release/rust-build.ts builds the images.
zigbuild --package certs --bin certs --package identity --bin identity \
  --package migrate --bin migrate --package service-kit --bin example-service
mkdir -p "$out/bin"
for b in certs identity migrate example-service; do cp "$release/$b" "$out/bin/$b"; done
# The certs test build, with the injected panic.
zigbuild --package certs --bin certs --features e2e-reload-panic
cp "$release/certs" "$out/bin/certs-reload-panic"
cargo build --locked --release -p certs --bin certs-ca 2>&1 | tail -1
marker='injected reload panic'
for b in certs identity migrate example-service certs-reload-panic; do
  f="$out/bin/$b"
  file "$f" | sed "s#$out/bin/##"
  sha256sum "$f" | sed "s#$out/bin/##"
  echo "$b: '$marker' found $(grep -c -a -F "$marker" "$f" || true) times"
done
# Images in a podman store of this job's own.
cat > "$out/storage.conf" << CONF
[storage]
driver = "vfs"
graphroot = "$out/store"
runroot = "$out/run"
CONF
export CONTAINERS_STORAGE_CONF="$out/storage.conf"
for image in certs-reload-panic:certs-reload-panic identity:identity migrate:migrate example-service:example-service; do
  name=${image%%:*}; b=${image#*:}
  ctx="$out/ctx-$name"; mkdir -p "$ctx"
  if [[ $name == example-service ]]; then
    cp "$out/bin/$b" "$ctx/example-service"
    cp deploy/chart/tests/e2e/example-service/Containerfile "$ctx/Containerfile"
  elif [[ $name == certs-reload-panic ]]; then
    # The binary keeps the name certs: the chart runs the image's entrypoint.
    cp "$out/bin/$b" "$ctx/certs"
    sed "s/@BIN@/certs/g" deploy/images/rust-binary.Containerfile > "$ctx/Containerfile"
  else
    cp "$out/bin/$b" "$ctx/$b"
    sed "s/@BIN@/$b/g" deploy/images/rust-binary.Containerfile > "$ctx/Containerfile"
  fi
  podman build -q --platform linux/arm64 -t "localhost/trustiot-e2e/$name:851" "$ctx"
  podman save -q --format oci-archive -o "$out/$name.tar" "localhost/trustiot-e2e/$name:851"
  podman image inspect --format '{{.Id}} {{.Architecture}}' "localhost/trustiot-e2e/$name:851"
done
unset CONTAINERS_STORAGE_CONF
podman unshare rm -rf "$out/store" "$out/run" "$out/storage.conf"
rm -rf "$out"/ctx-*
# The test roots, as at a root ceremony; each key is dropped with its shell variable.
ca="$target/release/certs-ca"
for root in a:"TrustIoT e2e-851 test root A" b:"TrustIoT e2e-851 test root B" c:"TrustIoT e2e-851 test root C e2e-reload-panic"; do
  key=$("$ca" root create --common-name "${root#*:}" --days 30 --certificate "$out/root-${root%%:*}.pem")
  unset key
done
# Names, sizes and digests only: no file owner (an account name) in the output.
stat -c '%s %n' "$out"/*.pem "$out"/*.tar | sed "s#$HOME#~#"
sha256sum "$out"/*.pem "$out"/*.tar | sed "s#$HOME#~#"
