#!/bin/sh
# Runs inside the pinned container. /repo is the repository (read-only), /out the data directory
# (read-write), /work the temporary directory. Usage: check-inside.sh csr | check
set -eu
OUT=/out
case "$1" in
csr)
  # CSRs for the ceremony; the private keys stay in /work and are deleted by run.sh.
  for g in 1 2 3; do
    openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
      -keyout "/work/issuing-key-$g.pem" -subj "/CN=request" -out "/work/g$g.csr.pem"
  done
  ;;
check)
  mkdir -p "$OUT/openssl"
  fx=/repo/edge/agent/tests/fixtures/root-crl
  cc="$OUT/ceremony"
  fail() {
    echo "check-inside: $*" >&2
    exit 1
  }
  # run <name> <root certificate> <crl> <revoked generation names, space separated>
  #     <generation certificate>...
  # Every result is asserted: a CRL that does not verify, a control that fails, a revoked
  # generation that passes or a listed-as-good generation that fails stops the run.
  run() {
    name=$1 root=$2 crl=$3 revoked=$4
    shift 4
    o="$OUT/openssl/$name"
    mkdir -p "$o"
    openssl crl -in "$crl" -CAfile "$root" -verify -noout >"$o/crl-verify.txt" 2>&1 ||
      fail "$name: the CRL does not verify against the root"
    [ "$(cat "$o/crl-verify.txt")" = "verify OK" ] || fail "$name: unexpected verify output"
    openssl crl -in "$crl" -text -noout >"$o/crl-text.txt" 2>&1
    openssl crl -in "$crl" -noout -crlnumber -lastupdate -nextupdate -issuer -fingerprint -sha256 >"$o/crl-summary.txt" 2>&1
    openssl x509 -in "$root" -noout -text >"$o/root-text.txt" 2>&1
    for cert in "$@"; do
      b=$(basename "$cert" .pem)
      {
        echo "serial: $(openssl x509 -in "$cert" -noout -serial)"
        echo "-- verify without CRL (control)"
        openssl verify -CAfile "$root" "$cert" 2>&1 || fail "$name $b: the control verification failed"
        echo "-- verify -crl_check"
        rc=0
        openssl verify -crl_check -CAfile "$root" -CRLfile "$crl" "$cert" 2>&1 || rc=$?
        if [ "$rc" -eq 0 ]; then
          got=ok
        else
          got=failed
          echo "exit $rc"
        fi
        case " $revoked " in
        *" $b "*) want=failed ;;
        *) want=ok ;;
        esac
        [ "$got" = "$want" ] || fail "$name $b: -crl_check result $got, expected $want"
      } >"$o/verify-$b.txt"
      if [ "$want" = failed ]; then
        grep -q "certificate revoked" "$o/verify-$b.txt" || fail "$name $b: failed for a reason other than revocation"
      fi
    done
  }
  run fixture "$fx/root.pem" "$fx/root-crl.pem" "issuing-ca-generation-1" \
    "$fx/issuing-ca-generation-1.pem"
  g1=issuing-ca-generation-1 g2=issuing-ca-generation-2
  gens="$cc/$g1.pem $cc/issuing-ca-generation-2.pem $cc/issuing-ca-generation-3.pem"
  # shellcheck disable=SC2086
  run ceremony-crl-1 "$cc/root.pem" "$cc/root-crl-1.pem" "" $gens
  # shellcheck disable=SC2086
  run ceremony-crl-2 "$cc/root.pem" "$cc/root-crl-2.pem" "$g1" $gens
  # shellcheck disable=SC2086
  run ceremony-crl-3 "$cc/root.pem" "$cc/root-crl-3.pem" "$g1 $g2" $gens
  ;;
*)
  echo "usage: check-inside.sh csr|check" >&2
  exit 2
  ;;
esac
