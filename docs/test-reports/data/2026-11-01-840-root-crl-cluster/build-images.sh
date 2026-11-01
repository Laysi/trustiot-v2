# Builds what the lab run of Laysi/trustiot-v2#840 needs, on an x86_64 Linux build host with
# rootless podman, from the repository root (docs/test-reports/2026-11-01-840-root-crl-cluster.md):
# - the arm64 images of the certs service, the migration Job and crates/service-kit's
#   example-service (which plays the identity service: the chart publishes the internal identity
#   keys that certs requires only while identity is enabled), cross-compiled for
#   aarch64-unknown-linux-musl with the pinned zig and cargo-zigbuild and packed without running
#   anything in the arm64 image;
# - a test root and its root CRLs 1 and 2, signed with certs-ca as at a root ceremony.
# Output in ~/.cache/trustiot-v2-840: the image archives and root.pem, crl-1.pem, crl-2.pem.
set -euo pipefail
out=$HOME/.cache/trustiot-v2-840
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
CARGO_ZIGBUILD_ZIG_PATH="$zig" PATH="$(dirname "$zb"):$PATH" \
  cargo zigbuild --locked --release --target aarch64-unknown-linux-musl \
  --package certs --bin certs --package migrate --bin migrate \
  --package service-kit --bin example-service 2>&1 | tail -3
cargo build --locked --release -p certs --bin certs-ca 2>&1 | tail -2
for b in certs migrate example-service; do
  f="$target/aarch64-unknown-linux-musl/release/$b"
  file "$f" | sed "s#$target#<target>#"
  sha256sum "$f" | sed "s#$target#<target>#"
done
# Images in a podman store of this job's own.
cat > "$out/storage.conf" << CONF
[storage]
driver = "vfs"
graphroot = "$out/store"
runroot = "$out/run"
CONF
export CONTAINERS_STORAGE_CONF="$out/storage.conf"
for b in certs migrate example-service; do
  ctx="$out/ctx-$b"; mkdir -p "$ctx"
  cp "$target/aarch64-unknown-linux-musl/release/$b" "$ctx/$b"
  if [[ $b == example-service ]]; then
    cp deploy/chart/tests/e2e/example-service/Containerfile "$ctx/Containerfile"
  else
    sed "s/@BIN@/$b/g" deploy/images/rust-binary.Containerfile > "$ctx/Containerfile"
  fi
  podman build -q --platform linux/arm64 -t "localhost/trustiot-e2e/$b:840" "$ctx"
  podman save -q --format oci-archive -o "$out/$b.tar" "localhost/trustiot-e2e/$b:840"
  podman image inspect --format '{{.Id}} {{.Architecture}}' "localhost/trustiot-e2e/$b:840"
done
unset CONTAINERS_STORAGE_CONF
podman unshare rm -rf "$out/store" "$out/run" "$out/storage.conf"
# The test root and its root CRLs, as at a root ceremony: the key stays in this shell's memory and
# is dropped at the end of the job. The root is trusted only by the run's namespace.
ca="$target/release/certs-ca"
key=$("$ca" root create --common-name "TrustIoT e2e-840 test root" --days 30 --certificate "$out/root.pem")
printf '%s\n' "$key" | "$ca" root crl --root-certificate "$out/root.pem" --previous-crl none --previous-certificates none --crl "$out/crl-1.pem"
printf '%s\n' "$key" | "$ca" root crl --root-certificate "$out/root.pem" --previous-crl "$out/crl-1.pem" --previous-certificates none --crl "$out/crl-2.pem"
unset key
rm -rf "$out"/ctx-*
# Names, sizes and digests only: no file owner (an account name) in the output.
stat -c '%s %n' "$out"/* | sed "s#$HOME#~#"
sha256sum "$out"/*.pem "$out"/*.tar | sed "s#$HOME#~#"
