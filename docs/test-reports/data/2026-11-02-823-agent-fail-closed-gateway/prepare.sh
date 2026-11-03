#!/bin/bash
# Prepares the fail-closed test copy of the agent (Laysi/trustiot-v2#823) on the lab gateway,
# beside the running trustiot-agent.service, which it does not touch. Run as root:
# sudo bash prepare.sh
#
# Creates, and nothing else:
# - /run/trustiot-823/ (tmpfs, root 0700): the throwaway CA and its keys, TPM work files;
# - two TPM keys at persistent handles confirmed free: OTHER_KEY (noDA, empty password) and
#   PASSWORD_KEY (noDA, a random password read from /dev/urandom through a pipe, never stored
#   or put on a command line);
# - /etc/trustiot-823/ (the test configurations, a copy of broker-ca.pem);
# - /var/lib/trustiot-agent-823/ (the test state directory: a copy of desired-config.cbor and the
#   test copies of the certificate);
# - /run/systemd/system/trustiot-agent-823@.service (the test unit, made from the deployed one).
# It refuses to run when any of them already exists or a handle is taken.
set -euo pipefail
export TPM2TOOLS_TCTI=device:/dev/tpmrm0

WORK=/run/trustiot-823
CONF=/etc/trustiot-823
STATE=/var/lib/trustiot-agent-823
UNIT=/run/systemd/system/trustiot-agent-823@.service
REAL_KEY=0x81000100
OTHER_KEY=0x81000823
PASSWORD_KEY=0x81000824
EMPTY_HANDLE=0x81000825
AGENT=trustiot-agent

lockout() { tpm2_getcap properties-variable | awk '/TPM2_PT_LOCKOUT_COUNTER/ {print $2}'; }

for path in "$WORK" "$CONF" "$STATE" "$UNIT"; do
  if [ -e "$path" ]; then echo "refusing: $path exists" >&2; exit 1; fi
done
handles=$(tpm2_getcap handles-persistent)
echo "## persistent handles before"; echo "$handles"
for handle in "$OTHER_KEY" "$PASSWORD_KEY" "$EMPTY_HANDLE"; do
  if grep -qix -- "- $handle" <<<"$handles"; then echo "refusing: $handle is taken" >&2; exit 1; fi
done
grep -qix -- "- $REAL_KEY" <<<"$handles" || { echo "refusing: no key at $REAL_KEY" >&2; exit 1; }
lockout_before=$(lockout)
echo "lockout counter before: $lockout_before"

install -d -m 0700 "$WORK" "$WORK/ca" "$WORK/tpm"

echo "## throwaway CA (root and issuing CA) and a certificate for the real key's public key"
cd "$WORK/ca"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out root.key
openssl req -x509 -new -key root.key -subj "/CN=Throwaway root 823" -days 2 \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out root.pem
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out issuing.key
openssl req -new -key issuing.key -subj "/CN=Throwaway issuing CA 823" |
  openssl x509 -req -CA root.pem -CAkey root.key -days 2 -out issuing.pem -extfile <(printf '%s\n' \
    'basicConstraints=critical,CA:TRUE,pathlen:0' 'keyUsage=critical,keyCertSign,cRLSign' \
    'subjectKeyIdentifier=hash' 'authorityKeyIdentifier=keyid')
# The public key from the real certificate, which the agent checks against the TPM key.
openssl x509 -in /var/lib/trustiot-agent/gateway.pem -noout -pubkey -out gateway-pub.pem
subject=$(openssl x509 -in /var/lib/trustiot-agent/gateway.pem -noout -subject -nameopt RFC2253 | sed 's/^subject=//')
openssl x509 -new -subj "/$subject" -force_pubkey gateway-pub.pem -CA issuing.pem -CAkey issuing.key \
  -days 2 -out leaf.pem -extfile <(printf '%s\n' 'basicConstraints=critical,CA:FALSE' \
    'keyUsage=critical,digitalSignature' 'extendedKeyUsage=clientAuth' \
    'subjectKeyIdentifier=hash' 'authorityKeyIdentifier=keyid')
cat leaf.pem issuing.pem > chain.pem
openssl x509 -in leaf.pem -noout -pubkey | cmp - gateway-pub.pem && echo "leaf: same public key as the real certificate"
openssl verify -CAfile root.pem -untrusted issuing.pem leaf.pem
if openssl verify -CAfile /etc/trustiot/broker-ca.pem -untrusted issuing.pem leaf.pem >/dev/null 2>&1; then
  echo "refusing: broker.ca verifies the throwaway certificate" >&2; exit 1
fi
echo "leaf: broker.ca does not verify it (as intended)"
diff <(openssl x509 -in /var/lib/trustiot-agent/gateway.pem -noout -subject -ext keyUsage,extendedKeyUsage,basicConstraints) \
  <(openssl x509 -in leaf.pem -noout -subject -ext keyUsage,extendedKeyUsage,basicConstraints) &&
  echo "leaf: same subject, key usage, EKU and basic constraints as the real certificate"

echo "## TPM test keys under a transient owner primary (the owner hierarchy has no password)"
cd "$WORK/tpm"
signing="fixedtpm|fixedparent|sensitivedataorigin|userwithauth|sign"
tpm2_createprimary -Q -C o -G ecc256:null:aes128cfb -c primary.ctx
persist() {
  tpm2_load -Q -C primary.ctx -u key.pub -r key.priv -c key.ctx
  tpm2_evictcontrol -Q -C o -c key.ctx "$1"
  rm -f key.pub key.priv key.ctx
}
tpm2_create -Q -C primary.ctx -G ecc256:ecdsa-sha256:null -a "$signing|noda" -u key.pub -r key.priv
persist "$OTHER_KEY"
# The password goes from /dev/urandom through a pipe into tpm2_create's standard input: it is
# not in a variable, a file or an argument, and nobody knows it.
od -An -N16 -tx1 /dev/urandom | tr -d ' \n' |
  tpm2_create -Q -C primary.ctx -G ecc256:ecdsa-sha256:null -a "$signing|noda" -p file:- -u key.pub -r key.priv
persist "$PASSWORD_KEY"
rm -f primary.ctx
for handle in "$OTHER_KEY" "$PASSWORD_KEY"; do
  echo "$handle:"; tpm2_readpublic -c "$handle" | sed -n '/^attributes:/,/raw:/p;/^type:/,/raw:/p;/^curve-id:/,/raw:/p'
done
echo "## persistent handles after creating the test keys"; tpm2_getcap handles-persistent
lockout_after=$(lockout)
echo "lockout counter after: $lockout_after"
[ "$lockout_after" = "$lockout_before" ] || { echo "STOP: the lockout counter changed" >&2; exit 3; }

echo "## test configuration and state directories"
install -d -m 0755 "$CONF"
install -m 0644 /etc/trustiot/broker-ca.pem "$CONF/broker-ca.pem"
install -d -m 0700 -o "$AGENT" -g "$AGENT" "$STATE" "$STATE/real" "$STATE/no-certificate" \
  "$STATE/no-key-file" "$STATE/other-ca"
install -m 0600 -o "$AGENT" -g "$AGENT" /var/lib/trustiot-agent/desired-config.cbor "$STATE/desired-config.cbor"
for dir in real no-key-file; do
  install -m 0644 -o "$AGENT" -g "$AGENT" /var/lib/trustiot-agent/gateway.pem "$STATE/$dir/gateway.pem"
done
install -m 0644 -o "$AGENT" -g "$AGENT" "$WORK/ca/chain.pem" "$STATE/other-ca/gateway.pem"
cmp /var/lib/trustiot-agent/desired-config.cbor "$STATE/desired-config.cbor" && echo "desired-config.cbor: copy identical"

tpm_key() { echo "{ source = \"tpm\", device = \"/dev/tpmrm0\", handle = $1 }"; }
# A copy of /etc/trustiot/agent.toml with only the broker.ca and broker.crl paths, the
# certificate path and the key changed.
config() {
  local name="$1" certificate="$2" key="$3"
  sed -e "s|\"/etc/trustiot/broker-ca.pem\"|\"$CONF/broker-ca.pem\"|" \
    -e "s|\"/var/lib/trustiot-agent/root-crl.pem\"|\"$STATE/root-crl.pem\"|" \
    -e "s|^certificate = .*|certificate = \"$certificate\"|" \
    -e "s|^key = .*|key = $key|" /etc/trustiot/agent.toml >"$CONF/$name.toml"
  chmod 0644 "$CONF/$name.toml"
}
# no-config: there is no /etc/trustiot-823/no-config.toml.
config no-certificate "$STATE/no-certificate/gateway.pem" "$(tpm_key "$REAL_KEY")"
config no-key "$STATE/real/gateway.pem" "$(tpm_key "$EMPTY_HANDLE")"
config no-key-file "$STATE/no-key-file/gateway.pem" "{ source = \"file\", path = \"$STATE/no-key-file/gateway.key\" }"
config other-ca "$STATE/other-ca/gateway.pem" "$(tpm_key "$REAL_KEY")"
config other-key "$STATE/real/gateway.pem" "$(tpm_key "$OTHER_KEY")"
config password-key "$STATE/real/gateway.pem" "$(tpm_key "$PASSWORD_KEY")"

echo "## test unit"
systemctl cat trustiot-agent | sed \
  -e '1{/^# \//d}' \
  -e 's|^Description=.*|Description=TrustIoT gateway agent, fail-closed test copy %i (Laysi/trustiot-v2#823)|' \
  -e 's|^ExecStart=/usr/bin/trustiot-agent --config /etc/trustiot/agent.toml$|ExecStart=/usr/bin/trustiot-agent --config /etc/trustiot-823/%i.toml|' \
  -e 's|^Restart=always$|Restart=no|' \
  -e 's|^ConfigurationDirectory=trustiot$|ConfigurationDirectory=trustiot-823|' \
  -e 's|^StateDirectory=trustiot-agent$|StateDirectory=trustiot-agent-823|' \
  -e '/^\[Service\]$/a IPAccounting=yes' \
  -e '/^\[Install\]$/,$d' >"$WORK/unit"
for line in 'ExecStart=/usr/bin/trustiot-agent --config /etc/trustiot-823/%i.toml' 'Restart=no' \
  'ConfigurationDirectory=trustiot-823' 'StateDirectory=trustiot-agent-823' 'IPAccounting=yes'; do
  grep -qxF "$line" "$WORK/unit" || { echo "refusing: the test unit lacks $line" >&2; exit 1; }
done
if grep -qE '^(ExecStartPre|ExecStopPost|Requires|Wants|BindsTo|PartOf)=' "$WORK/unit"; then
  echo "refusing: the test unit has a line on other units or shared state" >&2; exit 1
fi
install -m 0644 "$WORK/unit" "$UNIT"
systemctl daemon-reload
echo "## diff deployed unit -> test unit"
diff <(systemctl cat trustiot-agent) <(systemctl cat trustiot-agent-823@no-config) || true
echo "## diff deployed configuration -> test configurations"
for name in no-certificate no-key no-key-file other-ca other-key password-key; do
  echo "### $name.toml"; diff /etc/trustiot/agent.toml "$CONF/$name.toml" || true
done
echo "## test state directory"; ls -laR "$STATE"
echo "prepared"
