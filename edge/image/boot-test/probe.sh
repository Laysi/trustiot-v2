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
# (some commands, such as cryptsetup luksDump --dump-json-metadata, end without a newline). The
# section's name goes to the console first, so that the serial log names a section that hangs. A
# command that does not bring its own time limit (timeout ...) gets section_limit_s seconds: one
# that runs out of it fails its section, which says so, and the probe goes on with the next one, so
# that the virtual machine still powers off.
section_limit_s=300
section() {
	local name=$1 output code limited=0
	shift
	printf 'trustiot-boot-test: probe section %s\n' "$name"
	if [[ $1 != timeout ]]; then
		set -- timeout --kill-after=10 "$section_limit_s" "$@"
		limited=1
	fi
	output=$("$@" 2>&1)
	code=$?
	if ((limited && (code == 124 || code == 137))); then
		[[ -z $output ]] || output+=$'\n'
		output+="probe: the section ran out of its time limit of $section_limit_s s"
	fi
	{
		printf '=== %s\n' "$name"
		[[ -z $output ]] || printf '%s\n' "$output"
		printf '=== exit %s\n' "$code"
	} >>"$out"
}

param() { sed -n "s/.*\btrustiot_test\.$1=\([^ ]*\).*/\1/p" /proc/cmdline; }

# Every write queued to the data partition's header area (its first 16 MiB, LUKS.headerBytes in
# src/pins.ts: 32768 sectors of 512 bytes) since the kernel started, from the block_bio_queue trace
# event the test turns on with the kernel command line (trace_event=block:block_bio_queue): one
# "<program> <sector of the partition> <sectors>" line each, then "events <events>", the events the
# trace buffer holds, and "lost <events>", the ones it dropped. The event names the whole disk and
# counts sectors from its start. awk reads the trace in one pass: the trace is a seq_file, and a
# shell loop that reads it line by line seeks back after each line, which makes the kernel walk the
# trace from its start again each time (a boot queues some 25,000 block requests).
header_writes() {
	local tracing=/sys/kernel/tracing part name start devno
	mountpoint --quiet "$tracing" || mount -t tracefs tracefs "$tracing" || return 1
	if [[ $(cat "$tracing/events/block/block_bio_queue/enable") != 1 ]]; then
		echo "the block_bio_queue trace event is off"
		return 1
	fi
	part=$(readlink -f "$data") || return 1
	name=${part##*/}
	start=$(cat "/sys/class/block/$name/start") || return 1
	devno=$(cat "/sys/class/block/$name/../dev") || return 1
	# A line ends in "block_bio_queue: <major>,<minor> <rwbs> <sector> + <sectors> [<program>]".
	awk -v devno="${devno/:/,}" -v start="$start" '
		{
			i = index($0, " block_bio_queue: ")
			if (!i) next
			rest = substr($0, i + 18)
			if (split(rest, f, " ") < 6 || f[1] != devno || f[2] !~ /[WD]/ || f[4] != "+") next
			s = f[3] + 0
			n = f[5] + 0
			program = substr(rest, index(rest, " [") + 2)
			sub(/\]$/, "", program)
			if (n > 0 && s >= start && s < start + 32768) printf "%s %d %d\n", program, s - start, n
		}' "$tracing/trace" || return 1
	awk '$1 == "entries:" { e += $2 } $1 == "overrun:" { o += $2 } END { printf "events %d\nlost %d\n", e, o }' "$tracing"/per_cpu/cpu*/stats
}

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
# The LUKS2 header is read from a copy of the header area (the first 16 MiB, LUKS.headerBytes in
# src/pins.ts): whenever cryptsetup (also inside systemd-cryptenroll) reads a header whose two
# metadata copies differ, it writes the newer one over the other, and the probe must leave the
# partition as the setup left it (Laysi/trustiot-v2#918).
header_copy=/run/trustiot-boot-test-luks-header
section luks-header-copy dd if="$data" of="$header_copy" bs=1M count=16 status=none
section luks-json cryptsetup luksDump --dump-json-metadata "$header_copy"
section enroll-list systemd-cryptenroll "$header_copy"
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
# Last, so that it covers everything up to here: the writes to the header area in this boot.
export -f header_writes
export data
section header-writes bash -c header_writes
printf '=== end\n' >>"$out"

results=/dev/disk/by-id/virtio-trustiot-results
for _ in $(seq 60); do
	[[ -b $results ]] && break
	sleep 1
done
dd if="$out" of="$results" bs=1M conv=fsync status=none
systemctl poweroff --no-block
