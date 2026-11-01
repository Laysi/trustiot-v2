#!/bin/bash
# The boot test's power cut (edge/image/README.md, section 「開機測試」). Only the power-cut copy of
# the image has it, as /usr/local/sbin/trustiot-boot-test-power-cut, linked in /usr/local/sbin as
# each program below; /usr/local/sbin comes first in the PATH systemd gives a service, so
# trustiot-data-setup runs this in their place.
#
# Without trustiot_test.power-cut=<step> on the kernel command line, and for every process outside
# trustiot-data-setup.service, it only runs the real program (exec). With it, in the setup, the
# virtual machine loses its power at that step: the record "power-cut <step>" goes to the disk with
# the serial number trustiot-results, then the kernel resets at once (sysrq b: no sync, no
# unmount; the test runs QEMU with -no-reboot, so it exits). What the virtual machine had not
# handed to its disk is lost. This models a power cut between two steps: writes the disk already
# had stay (the test's disk is cache=unsafe), and a cut in the middle of a step, a lost card cache
# or torn or reordered writes are not modelled.
#
# Steps (the order of trustiot-data-setup's format()):
#   zero-header     after dd has zeroed the first 16 MiB
#   luks-format     after cryptsetup luksFormat
#   tpm-enroll      after systemd-cryptenroll has enrolled the TPM
#   mkfs            after mke2fs
#   kill-slot       after cryptsetup luksKillSlot
#   before-relabel  before cryptsetup config --label
#   after-relabel   after cryptsetup config --label
set -u

case ${0##*/} in
cryptsetup) real=/usr/sbin/cryptsetup ;;
systemd-cryptenroll) real=/usr/bin/systemd-cryptenroll ;;
mke2fs) real=/usr/sbin/mke2fs ;;
dd) real=/usr/bin/dd ;;
*)
	printf 'trustiot-boot-test-power-cut: not a program it stands in for: %s\n' "${0##*/}" >&2
	exit 127
	;;
esac

cut=$(sed -n 's/.*\btrustiot_test\.power-cut=\([^ ]*\).*/\1/p' /proc/cmdline)
if [[ -z $cut ]] || ! grep -q '/trustiot-data-setup\.service$' /proc/self/cgroup; then
	exec "$real" "$@"
fi

# The step this call ends, by the program and its arguments.
step=""
case ${0##*/} in
cryptsetup)
	case ${1:-} in
	luksFormat) step=luks-format ;;
	luksKillSlot) step=kill-slot ;;
	config) step=relabel ;;
	esac
	;;
systemd-cryptenroll) [[ " $* " == *" --tpm2-device=auto "* ]] && step=tpm-enroll ;;
mke2fs) step=mkfs ;;
dd) [[ " $* " == *" if=/dev/zero "* ]] && step=zero-header ;;
esac

power_cut() {
	local results=/dev/disk/by-id/virtio-trustiot-results
	for _ in $(seq 60); do
		[[ -b $results ]] && break
		sleep 1
	done
	printf 'power-cut %s\n' "$1" | /usr/bin/dd of="$results" bs=4096 conv=sync,fsync status=none
	printf '<2>trustiot-boot-test: power cut at %s\n' "$1" >/dev/kmsg
	echo b >/proc/sysrq-trigger
	# The reset does not return; should it, the setup must not go on as if nothing happened.
	sleep 600
	exit 1
}

[[ $step == relabel && $cut == before-relabel ]] && power_cut before-relabel
"$real" "$@"
code=$?
if ((code == 0)); then
	[[ $step == relabel && $cut == after-relabel ]] && power_cut after-relabel
	[[ $step != relabel && $step == "$cut" ]] && power_cut "$cut"
fi
exit "$code"
