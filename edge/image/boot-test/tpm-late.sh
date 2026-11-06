#!/bin/bash
# The boot test's late TPM (edge/image/README.md, section 「開機測試」). Only the test's copy of the
# image has it, as /usr/local/sbin/trustiot-boot-test-tpm-late, started early in the boot by
# trustiot-boot-test-tpm-late.service.
#
# With trustiot_test.tpm-late=<seconds> on the kernel command line, together with
# modprobe.blacklist=tpm_tis (so that udev does not load the driver of QEMU's TPM device), it loads
# the driver by name that many seconds after the start of the boot: until then the gateway has no
# /dev/tpmrm0, as when its TPM does not answer for a while. Without the parameter it does nothing.
set -u
late=$(sed -n 's/.*\btrustiot_test\.tpm-late=\([0-9][0-9]*\).*/\1/p' /proc/cmdline)
[[ -n $late ]] || exit 0
while :; do
	read -r up _ </proc/uptime
	((${up%.*} >= late)) && break
	sleep 1
done
printf '<5>trustiot-boot-test: loading tpm_tis %s s after the start of the boot\n' "$late" >/dev/kmsg
# A blacklist entry only keeps udev from loading the module for the device (by its alias); a load
# by name is not affected (modprobe.d(5)).
exec modprobe tpm_tis
