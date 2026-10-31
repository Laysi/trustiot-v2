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
section units systemctl show --property=Id,ActiveState,SubState,Result,ExecMainStatus,ExecMainStartTimestampMonotonic,InactiveExitTimestampMonotonic,StateChangeTimestampMonotonic,NRestarts,AssertResult,ConditionResult \
	trustiot-data-setup.service systemd-cryptsetup@trustiot_data.service "$mount_unit" trustiot-agent.service ssh.service
section failed-units systemctl list-units --failed --plain --no-legend
section findmnt findmnt --noheadings --output SOURCE,FSTYPE,OPTIONS --mountpoint "$state"
section state-dir stat --format='%U %G %a %m' "$state"
section state-dir-entries ls -A "$state"
section blkid-data blkid --probe --output export "$data"
section luks-json cryptsetup luksDump --dump-json-metadata "$data"
section enroll-list systemd-cryptenroll "$data"
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

# The marker: written on the volume in one boot, read back in a later one (the volume keeps its
# data), and searched for in the disk's raw bytes by the test (nothing reaches the disk in clear).
marker=$(param write-marker)
if [[ $marker =~ ^[0-9a-f]{32}$ ]] && mountpoint --quiet "$state"; then
	section write-marker sh -c "printf '%s\n' '$marker' >$state/boot-test-marker && sync"
fi
section read-marker cat "$state/boot-test-marker"
printf '=== end\n' >>"$out"

results=/dev/disk/by-id/virtio-trustiot-results
for _ in $(seq 60); do
	[[ -b $results ]] && break
	sleep 1
done
dd if="$out" of="$results" bs=1M conv=fsync status=none
systemctl poweroff --no-block
