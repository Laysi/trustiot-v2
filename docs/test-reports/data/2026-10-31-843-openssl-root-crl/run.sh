#!/usr/bin/env bash
# Independent OpenSSL check of the root CRLs that certs-ca signs (Laysi/trustiot-v2#843).
# Run from the repository root on an x86_64 Linux host with podman and the pinned Rust toolchain:
#   bash docs/test-reports/data/2026-10-31-843-openssl-root-crl/run.sh <new or empty output directory>
# It signs a throwaway ceremony in a temporary directory (the private keys are deleted at exit)
# and writes the public files to <output>/ceremony and the OpenSSL outputs to <output>/openssl.
# A rerun makes new certificates, so its serials and fingerprints differ from the saved report:
# the output goes to a directory of its own and never replaces the saved evidence.
set -euo pipefail

[ "$#" -eq 1 ] || { echo "usage: run.sh <new or empty output directory>" >&2; exit 2; }
mkdir -p "$1"
[ -z "$(ls -A "$1")" ] || { echo "run.sh: $1 is not empty" >&2; exit 2; }
out="$(cd "$1" && pwd)"

# OpenSSL comes from a Debian image pinned by digest plus one exactly pinned package version.
IMAGE=docker.io/library/debian:trixie-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
OPENSSL_DEB_VERSION="3.5.7-1~deb13u3"

repo="$(pwd)"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT
mkdir -p "$out/ceremony"

# in_container <shell command run after the pinned openssl package is installed>
in_container() {
  podman run --rm -v "$repo:/repo:ro" -v "$out:/out" -v "$work:/work" "$IMAGE" sh -c "
    set -eu
    apt-get update -qq >/dev/null
    apt-get install -y -qq --no-install-recommends openssl=$OPENSSL_DEB_VERSION >/dev/null
    $1"
}

cargo build --locked -p certs --bin certs-ca
CA="${CARGO_TARGET_DIR:-$repo/target}/debug/certs-ca"

in_container 'openssl version -a; dpkg-query -W openssl libssl3t64; sha256sum /usr/bin/openssl' \
  >"$out/openssl-version.txt"
in_container 'sh /repo/docs/test-reports/data/2026-10-31-843-openssl-root-crl/check-inside.sh csr'

c="$out/ceremony"
cd "$work"
"$CA" root create --common-name "TrustIoT CRL check test root" --certificate "$c/root.pem" \
  >root-key.pem 2>"$c/stderr-root-create.txt"
for g in 1 2 3; do
  "$CA" root sign --root-certificate "$c/root.pem" --request "g$g.csr.pem" \
    --common-name "TrustIoT CRL check test issuing CA $g" \
    --certificate "$c/issuing-ca-generation-$g.pem" <root-key.pem 2>"$c/stderr-root-sign-$g.txt"
done
# CRL 1 is empty, CRL 2 revokes generation 1, CRL 3 revokes generation 2 and carries generation 1.
"$CA" root crl --root-certificate "$c/root.pem" --previous-crl none --previous-certificates none \
  --crl "$c/root-crl-1.pem" <root-key.pem 2>"$c/stderr-root-crl-1.txt"
"$CA" root crl --root-certificate "$c/root.pem" --previous-crl "$c/root-crl-1.pem" \
  --previous-certificates none --revoke "$c/issuing-ca-generation-1.pem" --reason ca-compromise \
  --crl "$c/root-crl-2.pem" <root-key.pem 2>"$c/stderr-root-crl-2.txt"
cp "$c/issuing-ca-generation-1.pem" revoked-issuing-cas.pem
"$CA" root crl --root-certificate "$c/root.pem" --previous-crl "$c/root-crl-2.pem" \
  --previous-certificates revoked-issuing-cas.pem --revoke "$c/issuing-ca-generation-2.pem" \
  --reason superseded --crl "$c/root-crl-3.pem" <root-key.pem 2>"$c/stderr-root-crl-3.txt"
cd "$repo"
# The tool prints the output paths; keep the host's directory layout out of the saved listings.
sed -i "s|$out/|<output>/|g" "$c"/stderr-*.txt

in_container 'sh /repo/docs/test-reports/data/2026-10-31-843-openssl-root-crl/check-inside.sh check'
