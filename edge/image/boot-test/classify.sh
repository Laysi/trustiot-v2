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
# repairs one copy of the header from the other.
#
# Then, unless main() decided to format, what udisks2 does later in the same boot: the source image
# runs it, and it reads every partition blkid finds LUKS on with libblockdev's bd_crypto_luks_info,
# that is with libcryptsetup's crypt_load, which repairs a metadata copy whose checksum fails from
# the other one, as `cryptsetup isLuks` does (Laysi/trustiot-v2#918).
#
# Prints key=value lines on stdout; the setup's journal lines go to stderr. changed_by= names what
# wrote the header area: setup, udisks2, both, or nothing.
set -u
setup=$1 part=$2 flag=$3
rm -f -- "$flag"
header_bytes=$(
	# shellcheck source=/dev/null
	source "$setup" >/dev/null 2>&1
	printf '%s\n' "${HEADER_BYTES:-}"
)
[[ $header_bytes =~ ^[0-9]+$ ]] || {
	printf 'classify.sh: %s does not set HEADER_BYTES\n' "$setup" >&2
	exit 1
}
header() { head -c "$header_bytes" -- "$part" | sha256sum; }
before=$(header)
out=$(
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
code=$?
printf '%s\n' "$out"
printf 'code=%s\n' "$code"
if [[ -f $flag ]]; then
	printf 'flag=%s\n' "$(tr '\n' ';' <"$flag")"
else
	printf 'flag=\n'
fi
after_setup=$(header)
by=()
[[ $after_setup == "$before" ]] || by+=(setup)
if [[ $'\n'$out$'\n' == *$'\n'decision=format$'\n'* ]]; then
	printf 'udisks2=skipped\n'
elif [[ $'\n'$(blkid --probe --output export -- "$part" 2>/dev/null)$'\n' == *$'\n'TYPE=crypto_LUKS$'\n'* ]]; then
	cryptsetup isLuks "$part" >/dev/null 2>&1
	if [[ $(header) == "$after_setup" ]]; then
		printf 'udisks2=same\n'
	else
		printf 'udisks2=changed\n'
		by+=(udisks2)
	fi
else
	printf 'udisks2=skipped\n'
fi
case ${#by[@]} in
0) printf 'changed_by=nothing\n' ;;
1) printf 'changed_by=%s\n' "${by[0]}" ;;
*) printf 'changed_by=both\n' ;;
esac
