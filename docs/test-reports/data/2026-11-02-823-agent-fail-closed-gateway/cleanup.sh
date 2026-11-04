#!/bin/bash
# Removes everything prepare.sh created (Laysi/trustiot-v2#823), and only that. Run as root:
# sudo bash cleanup.sh
# Safe to run after a partial prepare.sh: each step skips what is not there. The only TPM
# handles it evicts are the two test keys, and only when they still have the test keys' noDA
# attribute.
set -u
export TPM2TOOLS_TCTI=device:/dev/tpmrm0
OTHER_KEY=0x81000823
PASSWORD_KEY=0x81000824

echo "## stop and remove the test unit"
for unit in $(systemctl list-units 'trustiot-agent-823@*' --all --no-legend --plain | awk '{print $1}'); do
  systemctl stop "$unit"
done
rm -f /run/systemd/system/trustiot-agent-823@.service
systemctl daemon-reload
systemctl reset-failed 'trustiot-agent-823@*' 2>/dev/null || true
systemctl list-units 'trustiot-agent-823@*' --all --no-legend --plain
echo "(end of test units)"

echo "## remove the test files"
rm -rf /etc/trustiot-823 /var/lib/trustiot-agent-823 /run/trustiot-823

echo "## evict the test TPM keys"
handles=$(tpm2_getcap handles-persistent)
for handle in "$OTHER_KEY" "$PASSWORD_KEY"; do
  if ! grep -qix -- "- $handle" <<<"$handles"; then echo "$handle: not present"; continue; fi
  if tpm2_readpublic -c "$handle" | grep -A1 '^attributes:' | grep -q 'noda'; then
    tpm2_evictcontrol -Q -C o -c "$handle" && echo "$handle: evicted"
  else
    echo "$handle: not a test key (no noda), left alone" >&2
  fi
done
echo "## persistent handles"; tpm2_getcap handles-persistent
ls -d /run/systemd/system/trustiot-agent-823@.service /etc/trustiot-823 /var/lib/trustiot-agent-823 \
  /run/trustiot-823 2>&1 || true
