#!/bin/bash
# Starts the test unit once for one case (Laysi/trustiot-v2#823) and records what systemd, the
# journal, the sockets, the TPM and the state directory show. Run as root after prepare.sh:
# sudo bash case.sh <no-config|no-certificate|no-key|no-key-file|other-ca|other-key|password-key>
# Exit status 3: the TPM lockout counter or the running agent changed; stop the test.
set -u
export TPM2TOOLS_TCTI=device:/dev/tpmrm0
case "${1:-}" in
  no-config | no-certificate | no-key | no-key-file | other-ca | other-key | password-key) ;;
  *) echo "usage: case.sh <case>" >&2; exit 2 ;;
esac
name="$1"
unit="trustiot-agent-823@$name.service"
STATE=/var/lib/trustiot-agent-823

lockout() { tpm2_getcap properties-variable | awk '/TPM2_PT_LOCKOUT_COUNTER/ {print $2}'; }
running() { systemctl show trustiot-agent -p MainPID -p NRestarts -p ActiveEnterTimestamp --value | paste -sd' '; }

echo "## case $name ($unit)"
echo "start: $(date -Is)"
if [ -e "/etc/trustiot-823/$name.toml" ]; then
  echo "configuration: /etc/trustiot-823/$name.toml"
else
  echo "configuration: /etc/trustiot-823/$name.toml does not exist"
fi
config_before=$(sha256sum <"$STATE/desired-config.cbor" | cut -d' ' -f1)
lockout_before=$(lockout)
running_before=$(running)
echo "desired-config.cbor sha256 before: $config_before"
echo "lockout counter before: $lockout_before"
echo "running agent before (MainPID NRestarts ActiveEnterTimestamp): $running_before"

systemctl start "$unit"
echo "systemctl start exit status: $?"
echo "## systemctl show"
systemctl show "$unit" -p ActiveState -p SubState -p Result -p NRestarts -p ExecMainCode \
  -p ExecMainStatus -p ExecMainPID -p ActiveEnterTimestampMonotonic -p StatusText \
  -p IPIngressPackets -p IPEgressPackets -p IPIngressBytes -p IPEgressBytes
pid=$(systemctl show "$unit" -p ExecMainPID --value)
echo "## sockets of the test unit's process ($pid) and of its cgroup"
ss -tuanpe | grep -E "pid=$pid,|trustiot-agent-823" || echo "(none)"
echo "## journal of $unit"
journalctl -u "$unit" -o short-iso-precise --no-pager
echo "poll or apply lines in its journal: $(journalctl -u "$unit" -o cat --no-pager |
  grep -cE 'polling with the saved configuration|applied the configuration|polled')"
echo "## test state directory after"
ls -la "$STATE"
[ -e "$STATE/buffer" ] && echo "the offline buffer directory exists" || echo "no offline buffer directory"
config_after=$(sha256sum <"$STATE/desired-config.cbor" | cut -d' ' -f1)
echo "desired-config.cbor sha256 after: $config_after"
[ "$config_after" = "$config_before" ] && echo "desired-config.cbor unchanged" || echo "desired-config.cbor CHANGED"
lockout_after=$(lockout)
running_after=$(running)
echo "lockout counter after: $lockout_after"
echo "running agent after (MainPID NRestarts ActiveEnterTimestamp): $running_after"
echo "end: $(date -Is)"
if [ "$lockout_after" != "$lockout_before" ] || [ "$running_after" != "$running_before" ]; then
  echo "STOP: the lockout counter or the running agent changed" >&2
  exit 3
fi
