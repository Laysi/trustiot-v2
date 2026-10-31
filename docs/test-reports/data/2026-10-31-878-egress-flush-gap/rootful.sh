#!/bin/bash
# Runs tests/egress.rs of edge/agent once as root (Laysi/trustiot-v2#878), so that systemd in the
# test container applies IPAddressDeny=: rootful podman, with a private container store that is
# removed afterwards. Run from the repository root on a build host where the account may use
# `sudo -n`; the optional argument is a test name filter.
set -uo pipefail
filter=${1:-}
built=$(cargo test --locked -p agent --test egress --no-run 2>&1)
echo "$built" | tail -2
bin=$(echo "$built" | sed -n 's/.*Executable tests\/egress.rs (\(.*\))$/\1/p' | head -1)
[ -x "$bin" ] || { echo "no test executable"; exit 2; }
store=$(mktemp -d)
chmod 0755 "$store"
printf '[storage]\ndriver = "overlay"\nrunroot = "%s/run"\ngraphroot = "%s/graph"\n' "$store" "$store" > "$store/storage.conf"
# A fixed PATH for everything root runs: podman and the tools the test calls come from the system.
as_root=(sudo -n env CONTAINERS_STORAGE_CONF="$store/storage.conf" PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin)
"${as_root[@]}" CI=1 "$bin" $filter --nocapture --test-threads=4 2>&1 | grep -vE '^\s*$'
rc=${PIPESTATUS[0]}
"${as_root[@]}" podman rm --all --force --volumes > /dev/null 2>&1
"${as_root[@]}" podman rmi --all --force > /dev/null 2>&1
"${as_root[@]}" podman system reset --force > /dev/null 2>&1
sudo -n rm -rf "$store"
echo "rootful run exit $rc"
exit "$rc"
