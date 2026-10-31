#!/bin/sh
# Checks the gateway invariants that the agent's egress rules rely on (edge/agent/README.md,
# section "連線位址的限制", Laysi/trustiot-v2#813). It only reads: ss, nft list, id,
# /etc/resolv.conf, getent ahosts on its numeric nameserver addresses and ip route get. Run it as root on the gateway, or on the image booted, for example
#
#   sudo sh check-gateway.sh --allow tcp:22
#
# Checks:
#   listeners  Every listening TCP socket and every bound, unconnected UDP socket is on a loopback
#              address (127.0.0.0/8, ::1, ::ffff:127.0.0.0/104). A service on any other address,
#              the wildcard addresses included, can be reached from outside and through hairpin
#              NAT on a router, which the egress rules cannot see. Unix sockets are not listed.
#   egress     The table inet trustiot_agent is loaded and is exactly trustiot-agent-egress.nft:
#              the output hook jumps from the agent's account to the refusals of the five kinds of
#              destination, and nothing else is in the table.
#   resolver   /etc/resolv.conf names a nameserver, and none that the rules refuse to the agent:
#              a local address in any spelling glibc accepts (systemd-resolved's 127.0.0.53, a
#              dnsmasq on the gateway's own LAN address), broadcast, multicast, the unspecified
#              address or a metadata address. Such a nameserver, or none (glibc then asks
#              127.0.0.1), would leave the agent unable to resolve the cloud's host name.
#
# Options:
#   --allow PROTO:PORT[:PROCESS]  accept a listener on another address: PROTO is tcp or udp, PORT
#                                 a number or *, PROCESS (optional) the name ss shows for the
#                                 owning process. Each accepted listener is printed. Repeatable.
#
# Exit status: 0 when every check holds, 1 when one fails, 2 on a usage error or a missing tool.

set -u
# No pathname expansion: unquoted words split from /etc/resolv.conf (`nameserver *`) stay words.
set -f

allow=""
while [ "$#" -gt 0 ]; do
    case "$1" in
    --allow)
        if [ "$#" -lt 2 ]; then
            echo "usage: check-gateway.sh [--allow PROTO:PORT[:PROCESS]]..." >&2
            exit 2
        fi
        case "$2" in
        tcp:*:* | udp:*:* | tcp:* | udp:*) ;;
        *)
            echo "--allow wants PROTO:PORT[:PROCESS] with PROTO tcp or udp: $2" >&2
            exit 2
            ;;
        esac
        allow="$allow $2"
        shift 2
        ;;
    *)
        echo "usage: check-gateway.sh [--allow PROTO:PORT[:PROCESS]]..." >&2
        exit 2
        ;;
    esac
done

# nft is in /usr/sbin, which an account's PATH may lack.
PATH="$PATH:/usr/sbin:/sbin"
export PATH
for tool in ss nft awk grep sed id getent ip; do
    if ! command -v "$tool" > /dev/null 2>&1; then
        echo "check-gateway: $tool is missing" >&2
        exit 2
    fi
done

failed=0

# listeners: one line per socket, "<netid> <local address:port> <process column>".
if ! sockets=$(ss -H -l -n -t -u -p 2>&1); then
    echo "FAIL listeners: ss failed: $sockets"
    failed=1
else
    report=$(printf '%s\n' "$sockets" | awk -v allow="$allow" '
        function loopback(address) {
            return address ~ /^127\./ || address == "::1" || address ~ /^::ffff:127\./
        }
        BEGIN { count = split(allow, rules, " ") }
        NF >= 5 {
            proto = $1; local = $5; process = ""
            for (i = 7; i <= NF; i++) process = process $i
            # The port follows the last colon; the address may be [v6], may carry %interface.
            port = local; sub(/^.*:/, "", port)
            address = local; sub(/:[^:]*$/, "", address)
            gsub(/^\[|\]$/, "", address); sub(/%.*$/, "", address)
            if (loopback(address)) next
            names = ""
            rest = process
            while (match(rest, /"[^"]*"/)) {
                names = names " " substr(rest, RSTART + 1, RLENGTH - 2)
                rest = substr(rest, RSTART + RLENGTH)
            }
            accepted = 0
            for (r = 1; r <= count; r++) {
                n = split(rules[r], part, ":")
                if (part[1] != proto) continue
                if (part[2] != "*" && part[2] != port) continue
                if (n >= 3 && index(names " ", " " part[3] " ") == 0) continue
                accepted = 1
            }
            printf "%s %s %s %s%s\n", (accepted ? "ALLOWED" : "FAIL"), "listeners:", proto, local, (names == "" ? "" : " (" substr(names, 2) ")")
        }')
    if [ -n "$report" ]; then
        printf '%s\n' "$report"
    fi
    if printf '%s\n' "$report" | grep -q '^FAIL'; then
        failed=1
    else
        echo "ok listeners: every TCP and UDP listener is on a loopback address or allowed"
    fi
fi

# egress: the loaded table is exactly the agent's rules, as nft lists trustiot-agent-egress.nft
# (indentation, blank lines and counter values left out). Finding the right rules somewhere in
# the table is not enough: they only protect while the output hook jumps from the account's match
# to them, before any other verdict, and the refusals end in the reject chain. Comparing the whole
# listing checks that structure, and any rule added, removed, reordered or moved to another chain.
# nft lists the account as the uid it resolved when the rules were loaded, so the listing is
# compared with the account's uid now: rules loaded for an account that was since recreated with
# another uid fail too. `meta l4proto tcp` is listed as `6` (nftables 1.1.3, Debian 13); both
# spellings are accepted. When trustiot-agent-egress.nft changes, change this listing with it
# (tests/egress.rs loads the file and expects ok).
expected='table inet trustiot_agent {
chain output {
type filter hook output priority filter; policy accept;
meta skuid AGENT_UID jump agent
}
chain agent {
fib daddr type local counter goto refuse comment "local"
fib daddr type anycast counter goto refuse comment "anycast"
fib daddr type broadcast counter goto refuse comment "broadcast"
fib daddr type multicast counter goto refuse comment "multicast"
ip daddr 169.254.169.254 counter goto refuse comment "metadata"
ip6 daddr fd00:ec2::254 counter goto refuse comment "metadata"
}
chain refuse {
meta l4proto 6 reject with tcp reset
reject with icmpx admin-prohibited
}
}'
if ! table=$(nft list table inet trustiot_agent 2>&1); then
    echo "FAIL egress: table inet trustiot_agent is not loaded: $table"
    failed=1
elif ! uid=$(id -u trustiot-agent 2> /dev/null); then
    echo "FAIL egress: the account trustiot-agent does not exist"
    failed=1
else
    expected=$(printf '%s\n' "$expected" | sed "s/AGENT_UID/$uid/")
    loaded=$(printf '%s\n' "$table" | sed -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' \
        -e 's/counter packets [0-9]* bytes [0-9]*/counter/' \
        -e 's/^meta skuid "trustiot-agent" /meta skuid '"$uid"' /' \
        -e 's/^meta l4proto tcp /meta l4proto 6 /' -e '/^$/d')
    if [ "$loaded" = "$expected" ]; then
        echo "ok egress: table inet trustiot_agent is loaded and is the agent's rules"
    else
        # The first line that differs, to point at the change.
        difference=$(printf '%s\n' "$loaded" | EXPECTED="$expected" awk '
            BEGIN { count = split(ENVIRON["EXPECTED"], line, "\n") }
            !found && $0 != line[NR] {
                printf "line %d is \"%s\", the agent'"'"'s rules have \"%s\"", NR, $0, (NR <= count ? line[NR] : "(nothing)")
                found = 1
            }
            END { if (!found && NR < count) printf "it ends after line %d, the agent'"'"'s rules go on with \"%s\"", NR, line[NR + 1] }')
        echo "FAIL egress: table inet trustiot_agent is not the agent's rules: $difference"
        failed=1
    fi
fi

# resolver: what a nameserver entry is to the agent: `ignored` when glibc ignores it (not a
# numeric address), `refused <why>` when the agent's egress rules refuse it, `ok` otherwise. The
# address is read the way glibc reads it (getaddrinfo, through getent: `127.1`, `0:0:0:0:0:0:0:1`
# and `::ffff:7f00:1` are all loopback), and the kernel says what kind of destination it is (ip
# route get), as the rules' `fib daddr type` does, so the gateway's own interface addresses count
# as local too.
nameserver_kind() {
    address=${1%%\%*}
    # Only a numeric address is a nameserver to glibc; getent would look a name up.
    case "$address" in
    '' | *[!0-9A-Fa-fXx.:]*)
        echo ignored
        return
        ;;
    [0-9]* | *:*) ;;
    *)
        echo ignored
        return
        ;;
    esac
    canonical=$(getent ahosts "$address" | awk 'NR == 1 { print $1 }')
    case "$canonical" in
    '')
        echo ignored
        return
        ;;
    ::ffff:*.*) canonical=${canonical#::ffff:} ;; # IPv4-mapped: the kernel sends it as IPv4
    esac
    case "$canonical" in
    0.0.0.0 | ::)
        echo "refused the unspecified address $canonical, which the kernel sends to the gateway itself"
        return
        ;;
    169.254.169.254 | fd00:ec2::254)
        echo "refused the metadata address $canonical"
        return
        ;;
    esac
    kind=$(ip route get "$canonical" 2> /dev/null | awk 'NR == 1 { print $1 }')
    case "$kind" in
    local | broadcast | multicast) echo "refused a $kind address ($canonical)" ;;
    *) echo ok ;;
    esac
}

# resolver: every nameserver glibc uses is one the agent may reach, and there is one.
if [ ! -r /etc/resolv.conf ]; then
    echo "FAIL resolver: /etc/resolv.conf cannot be read"
    failed=1
else
    servers=$(awk '$1 == "nameserver" { print $2 }' /etc/resolv.conf)
    refused=0
    usable=0
    for server in $servers; do
        kind=$(nameserver_kind "$server")
        case "$kind" in
        ignored) ;;
        refused*)
            echo "FAIL resolver: /etc/resolv.conf names the nameserver $server, ${kind#refused }, which the agent's egress rules refuse"
            usable=$((usable + 1))
            refused=1
            ;;
        *) usable=$((usable + 1)) ;;
        esac
    done
    if [ "$usable" = 0 ]; then
        echo "FAIL resolver: /etc/resolv.conf names no nameserver glibc can use (only numeric addresses count), so glibc asks 127.0.0.1, which the agent's egress rules refuse"
        refused=1
    fi
    if [ "$refused" = 1 ]; then
        failed=1
    else
        echo "ok resolver: no nameserver in /etc/resolv.conf is refused by the agent's egress rules"
    fi
fi

exit "$failed"
