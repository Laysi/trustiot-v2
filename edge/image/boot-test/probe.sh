#!/bin/bash
# The boot test's probe (edge/image/README.md, section 「開機測試」). Only the test's copy of the
# image has it, as /usr/local/sbin/trustiot-boot-test-probe, started by trustiot-boot-test.service.
# It waits until the boot has finished, whatever became of the data partition and the agent,
# writes what it finds to the virtual disk with the serial number trustiot-results, and powers the
# virtual machine off. src/inside/boot-test.ts reads the sections back (src/boot-results.ts).
set -u
export LC_ALL=C SYSTEMD_COLORS=0 SYSTEMD_PAGER=
out=/run/trustiot-boot-test.txt
data=/dev/disk/by-id/virtio-trustiot-gateway-part3
state=/var/lib/trustiot-agent
mount_unit=var-lib-trustiot\\x2dagent.mount

# One section: its name, the command's output and its exit code, each marker on a line of its own
# (some commands, such as cryptsetup luksDump --dump-json-metadata, end without a newline).
section() {
	local name=$1 output code
	shift
	output=$("$@" 2>&1)
	code=$?
	{
		printf '=== %s\n' "$name"
		[[ -z $output ]] || printf '%s\n' "$output"
		printf '=== exit %s\n' "$code"
	} >>"$out"
}

param() { sed -n "s/.*\btrustiot_test\.$1=\([^ ]*\).*/\1/p" /proc/cmdline; }

: >"$out"
# The boot has finished once no start job is left: the unlock, the mount and the agent have each
# succeeded, failed or given up by then (each has its own time limit).
section boot-finished timeout 1200 systemctl is-system-running --wait
section uname uname -r
section units systemctl show --property=Id,ActiveState,SubState,Result,ExecMainStatus,ExecMainStartTimestampMonotonic,InactiveExitTimestampMonotonic,StateChangeTimestampMonotonic,NRestarts,AssertResult,ConditionResult,ConditionTimestampMonotonic \
	trustiot-data-setup.service systemd-cryptsetup@trustiot_data.service "$mount_unit" trustiot-agent.service ssh.service
section failed-units systemctl list-units --failed --plain --no-legend
section findmnt findmnt --noheadings --output SOURCE,FSTYPE,OPTIONS --mountpoint "$state"
section state-dir stat --format='%U %G %a %m' "$state"
section state-dir-entries ls -A "$state"
section blkid-data blkid --probe --output export "$data"
section luks-json cryptsetup luksDump --dump-json-metadata "$data"
section enroll-list systemd-cryptenroll "$data"
# The name of the storage key systemd-cryptenroll persists at 0x81000001: the setup's owner
# fingerprint (its token in luks-json) must be the same name (Laysi/trustiot-v2#867).
section srk-name sh -c 'TPM2TOOLS_TCTI=device:/dev/tpmrm0 tpm2_readpublic --object-context=0x81000001 --name=/run/trustiot-boot-test-srk.name >/dev/null && od -An -tx1 -v /run/trustiot-boot-test-srk.name | tr -d " \n"'
# What the setup leaves while the volume carries the mark of a wipe; it keeps the agent off.
section factory-flag cat /run/trustiot-back-to-factory
# The time systemd gives the volume and the unlocked volume to appear (crypttab and fstab).
uuid=$(awk '$1 == "trustiot_data" { sub(/^UUID=/, "", $2); print $2 }' /etc/crypttab)
section device-timeouts systemctl show --property=Id,JobRunningTimeoutUSec \
	"$(systemd-escape --path --suffix=device "/dev/disk/by-uuid/$uuid")" dev-mapper-trustiot_data.device
section ask-password ls -A /run/systemd/ask-password
section swap swapon --show=NAME,TYPE --noheadings
section journal-files ls -A /var/log/journal
core_dirs=(/ /root)
[[ -d /var/lib/systemd/coredump ]] && core_dirs+=(/var/lib/systemd/coredump)
section core-files find "${core_dirs[@]}" -maxdepth 1 -xdev -name 'core*' -type f
section journal-setup journalctl --boot --no-pager --lines=200 --output=short-monotonic --unit=trustiot-data-setup.service
section journal-cryptsetup journalctl --boot --no-pager --lines=200 --output=short-monotonic --unit=systemd-cryptsetup@trustiot_data.service
section journal-mount journalctl --boot --no-pager --lines=50 --output=short-monotonic --unit="$mount_unit"
section journal-agent journalctl --boot --no-pager --lines=50 --output=short-monotonic --unit=trustiot-agent.service
section journal-prompts journalctl --boot --no-pager --lines=50 --grep='Please enter|ask-password|password prompt'

# The agent's firewall as the image installs it (Laysi/trustiot-v2#866): both tables, which the
# egress unit and the agent's ExecStartPre= load from one file; the units the agent's unit pulls
# in; the allowlist component's socket; and what the check of IPAddressDeny= logged at each start
# of the agent (src/boot-results.ts judges them only in boots where the agent started).
section nft-tables nft list tables
section default-deny-units systemctl show --property=Id,ActiveState,SubState,UnitFileState \
	trustiot-agent-allowlist.socket trustiot-agent-nameservers.path trustiot-agent-egress-check.timer
section allowlist-socket stat --format='%U %G %a' /run/trustiot-agent-allowlist/socket
section ip-deny-journal journalctl --boot --no-pager --lines=50 --output=cat --unit=trustiot-agent.service --grep='IPAddressDeny='
section ip-deny-errors journalctl --boot --no-pager --lines=50 --output=cat --priority=err --unit=trustiot-agent.service --grep='IPAddressDeny='

# The marker: written on the volume in one boot, read back in a later one (the volume keeps its
# data), and searched for in the disk's raw bytes by the test (nothing reaches the disk in clear).
marker=$(param write-marker)
if [[ $marker =~ ^[0-9a-f]{32}$ ]] && mountpoint --quiet "$state"; then
	section write-marker sh -c "printf '%s\n' '$marker' >$state/boot-test-marker && sync"
fi
section read-marker cat "$state/boot-test-marker"

# Someone starts the agent by hand while the data partition is locked (trustiot_test.start-agent=1;
# after the sections above, which show the boot as it was). First as anyone would, then skipping
# the agent's dependencies, so that only its AssertPathIsMountPoint= stands in the way. Neither may
# run the agent or write to the empty directory on the root partition.
if [[ $(param start-agent) == 1 ]]; then
	section start-agent timeout 600 systemctl start trustiot-agent.service
	section start-agent-ignoring-dependencies timeout 600 systemctl start --job-mode=ignore-dependencies trustiot-agent.service
	section start-agent-units systemctl show --property=Id,ActiveState,SubState,Result,AssertResult,AssertTimestampMonotonic,ExecMainPID,ExecMainStartTimestampMonotonic,InactiveExitTimestampMonotonic \
		trustiot-agent.service "$mount_unit" systemd-cryptsetup@trustiot_data.service
	# The plain start pulls in the unlock again: it must not leave a password prompt either.
	section start-agent-ask-password ls -A /run/systemd/ask-password
	section start-agent-findmnt findmnt --noheadings --output SOURCE,FSTYPE,OPTIONS --mountpoint "$state"
	section start-agent-state-dir-entries ls -A "$state"
	# Every process whose executable is the agent's, whoever started it.
	section start-agent-processes sh -c 'for p in /proc/[0-9]*; do [ "$(readlink "$p/exe" 2>/dev/null)" = /usr/bin/trustiot-agent ] && echo "${p#/proc/}"; done; true'
	section start-agent-journal journalctl --boot --no-pager --lines=50 --output=short-monotonic --unit=trustiot-agent.service
fi
# The agent's IP firewall and default deny at work (trustiot_test.firewall=1; README.md, section
# 「開機測試」). The real agent ends at once without the factory's configuration, so a drop-in in
# /run replaces only what it runs with a process that stays up and ignores the reload's SIGHUP;
# the unit's ExecStartPre= lines, its IPAddressDeny= and everything else stay the image's.
if [[ $(param firewall) == 1 ]]; then
	agent=trustiot-agent.service
	cgroup=/sys/fs/cgroup/system.slice/$agent
	dropin=/run/systemd/system/$agent.d
	mkdir -p "$dropin"
	printf '%s\n' '[Service]' Type=exec WatchdogSec=0 ExecStart= \
		"ExecStart=/bin/sh -c 'trap \"\" HUP; exec /bin/sleep infinity'" >"$dropin/zz-boot-test-stand-in.conf"
	systemctl daemon-reload
	# What IPAddressDeny= refuses, sent by root: the egress rules match only the agent's account, so
	# in the agent's cgroup only IPAddressDeny= can refuse it, and outside it nothing does.
	udp='exec 3>/dev/udp/127.0.0.1/9 && echo x >&3 && echo sent'
	invocation() { systemctl show --property=InvocationID --value "$agent"; }
	section fw-start timeout 120 systemctl restart "$agent"
	section fw-units systemctl show --property=Id,ActiveState,SubState "$agent"
	section fw-ip-deny journalctl --boot --no-pager --output=cat "_SYSTEMD_INVOCATION_ID=$(invocation)" --grep='IPAddressDeny='
	section fw-in-cgroup bash -c "echo \$\$ >$cgroup/cgroup.procs && $udp"
	section fw-outside bash -c "$udp"
	# The control: IPAddressDeny= emptied by a drop-in leaves the cgroup without the programs, as
	# when systemd cannot attach them. The check must log an error and the agent must run.
	printf '%s\n' '[Service]' IPAddressDeny= >"$dropin/zz-boot-test-no-ip-deny.conf"
	systemctl daemon-reload
	section fw-control-start timeout 120 systemctl restart "$agent"
	section fw-control-units systemctl show --property=Id,ActiveState,SubState "$agent"
	section fw-control-ip-deny journalctl --boot --no-pager --output=cat "_SYSTEMD_INVOCATION_ID=$(invocation)" --grep='IPAddressDeny='
	section fw-control-in-cgroup bash -c "echo \$\$ >$cgroup/cgroup.procs && $udp"
	rm "$dropin/zz-boot-test-no-ip-deny.conf"
	systemctl daemon-reload
	section fw-restart timeout 120 systemctl restart "$agent"
	# The DNS allowance follows /etc/resolv.conf (renamed over it, as NetworkManager writes it).
	nameservers() { nft list set inet trustiot_agent_allow nameserver4 | grep -o 'elements = {[^}]*}'; }
	# The disk boots again after this boot: keep what /etc/resolv.conf was, to put it back below.
	cp --no-dereference --preserve=all /etc/resolv.conf /run/resolv.conf.before 2>/dev/null
	printf 'nameserver 192.0.2.53\n' >/etc/resolv.conf.boot-test && mv /etc/resolv.conf.boot-test /etc/resolv.conf
	section fw-dns-path bash -c "for i in \$(seq 600); do nft list set inet trustiot_agent_allow nameserver4 | grep -q 192.0.2.53 && exit 0; sleep 0.1; done; exit 1"
	section fw-dns-set nameservers
	# The reload of nftables.service loads /etc/nftables.conf (flush ruleset) and the rules file in
	# one transaction, then fills the DNS allowance again (the drop-in's second ExecReload=).
	section fw-nftables-reload timeout 120 systemctl reload nftables.service
	section fw-after-reload-tables nft list tables
	section fw-after-reload-dns nameservers
	# The agent's start loads the rules file, which includes the default deny, and fills the DNS
	# allowance: with the default deny's table deleted, a restart brings it back. The egress check
	# would too (within 11 s), so its timer is stopped until the restart starts it again (the agent
	# wants it; it is ordered after the agent), and its journal must not show that it loaded them.
	since=$(date +%s.%N)
	section fw-check-stop systemctl stop trustiot-agent-egress-check.timer trustiot-agent-egress-check.service
	section fw-delete nft delete table inet trustiot_agent_allow
	section fw-after-delete-tables nft list tables
	section fw-restart-after-delete timeout 120 systemctl restart "$agent"
	section fw-after-restart-tables nft list tables
	section fw-after-restart-dns nameservers
	section fw-check-acted journalctl --no-pager --output=cat --since="@$since" --unit=trustiot-agent-egress-check.service --grep='is missing'
	if [[ -e /run/resolv.conf.before || -L /run/resolv.conf.before ]]; then
		mv --force /run/resolv.conf.before /etc/resolv.conf
	else
		rm --force /etc/resolv.conf
	fi
fi
printf '=== end\n' >>"$out"

results=/dev/disk/by-id/virtio-trustiot-results
for _ in $(seq 60); do
	[[ -b $results ]] && break
	sleep 1
done
dd if="$out" of="$results" bs=1M conv=fsync status=none
systemctl poweroff --no-block
