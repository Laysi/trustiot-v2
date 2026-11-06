#!/bin/bash
# The boot test's look at what a power cut can leave on the data partition (edge/image/README.md,
# section 「開機測試」; src/write-log.ts rebuilds the states). No copy of the image carries it: the
# test runs it in a chroot of the power-cut copy's root file system, so the setup runs with the
# image's own bash, blkid, cryptsetup and systemd-cryptenroll, on a file instead of the partition.
#
#   classify.sh <setup> <partition file> <flag file>
#
# <setup> is a copy of the image's trustiot-data-setup whose FACTORY_FLAG is <flag file>. This
# sources it and runs its main() once, with what needs a booted gateway replaced: data_partition
# (the file), unique_uuid (there is no other disk here), and format() and check_or_wipe(), which
# only say what main() decided. format() ends the run (it would write a new header); check_or_wipe()
# returns as when the TPM opens the volume (what the TPM does is the boot's part), so main() goes
# on to read the mark. Everything else is the setup's own, including what cryptsetup writes when it
# repairs one copy of the header from the other. Prints key=value lines on stdout; the setup's
# journal lines go to stderr.
set -u
setup=$1 part=$2 flag=$3
rm -f -- "$flag"
(
	# shellcheck source=/dev/null
	source "$setup"
	data_partition() { printf '%s\n' "$part"; }
	unique_uuid() { :; }
	format() {
		printf 'decision=format\nmark=%s\n' "$2"
		exit 0
	}
	check_or_wipe() {
		local owner
		owner=$(recorded_owner "$1") || owner=""
		printf 'decision=check\nowner=%s\n' "$owner"
	}
	main
)
printf 'code=%s\n' "$?"
if [[ -f $flag ]]; then
	printf 'flag=%s\n' "$(tr '\n' ';' <"$flag")"
else
	printf 'flag=\n'
fi
