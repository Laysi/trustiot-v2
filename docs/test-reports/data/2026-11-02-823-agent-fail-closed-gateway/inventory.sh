#!/bin/bash
# Read-only inventory of the lab gateway before and after the fail-closed test
# (Laysi/trustiot-v2#823). Run as root on the gateway: sudo bash inventory.sh
# It changes nothing: it lists, shows and hashes.
set -u
export TPM2TOOLS_TCTI=device:/dev/tpmrm0

echo "## time"; date -Is
echo "## who"; who
echo "## transient units (run-*)"; systemctl list-units 'run-*' --all --no-legend --no-pager --plain
echo "## tpm2 and agent processes"; ps -eo pid,user,etimes,args | grep -E '[t]pm2_|[t]rustiot-agent' || true
echo "## running agent unit"
systemctl show trustiot-agent -p MainPID -p ActiveState -p SubState -p ActiveEnterTimestamp \
  -p NRestarts -p InvocationID -p ExecMainStartTimestamp
pid=$(systemctl show -p MainPID --value trustiot-agent)
echo "## running agent TCP sockets (ss -tinpe, pid $pid)"
ss -tinpe | grep -A1 -E "pid=$pid," || echo "(none)"
echo "## TPM persistent handles"; tpm2_getcap handles-persistent
echo "## TPM variable properties"; tpm2_getcap properties-variable
echo "## nftables tables"; nft list tables; echo "(end of nft tables)"
echo "## test files and units present"
ls -d /run/systemd/system/trustiot-agent-823@.service /etc/trustiot-823 /var/lib/trustiot-agent-823 \
  /run/trustiot-823 2>&1 || true
systemctl list-units 'trustiot-agent-823@*' --all --no-legend --no-pager --plain || true
echo "## deployed files"
deployed=(/usr/bin/trustiot-agent /usr/lib/systemd/system/trustiot-agent.service /usr/lib/sysusers.d/trustiot-agent.conf
  /etc/trustiot/agent.toml /etc/trustiot/broker-ca.pem /var/lib/trustiot-agent/gateway.pem
  /var/lib/trustiot-agent/desired-config.cbor)
sha256sum "${deployed[@]}"
stat -c '%a %U:%G %s %y %n' "${deployed[@]}" /etc/trustiot /var/lib/trustiot-agent /var/lib/trustiot-agent/buffer
