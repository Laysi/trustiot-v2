#!/bin/bash
# The stand-in gateway's helper for tests/egress.rs, installed in the test container as
# /usr/local/bin/lab. Every address is a test or documentation range inside the container.
#
#   lab network        build the gateway's network: a peer namespace behind a veth pair (the
#                      site's LAN, its DNS server and the cloud), local routes, a NAT rule back to
#                      the gateway, and the listeners
#   lab probe WHO PROTO ADDRESS PORT
#                      connect (tcp) or send one datagram (udp) as WHO (agent, other or root);
#                      prints ok, refused, not-permitted, timeout or the error
#   lab resolve WHO NAME
#                      resolve NAME through glibc as WHO; prints the addresses or "failed"
#   lab counters       the packet count of each refusal rule, as "<comment> <packets>"

set -euo pipefail

# The gateway's side of the LAN and the peer's (the site's router, DNS server and the cloud).
GATEWAY4=10.81.3.2
GATEWAY6=fd81:3::2
PEER4=10.81.3.1
PEER6=fd81:3::1

as() {
    case "$1" in
    agent) echo "setpriv --reuid=trustiot-agent --regid=trustiot-agent --clear-groups" ;;
    other) echo "setpriv --reuid=nobody --regid=nogroup --clear-groups" ;;
    root) echo "" ;;
    *)
        echo "unknown account $1" >&2
        exit 2
        ;;
    esac
}

# Starts a command in the background, detached from the caller's output.
background() {
    setsid "$@" < /dev/null > /dev/null 2>&1 &
}

# Waits up to 60 s until something listens on the TCP or UDP port in the namespace ("" for the
# gateway's own).
await_listener() {
    local namespace=$1 proto=$2 port=$3 tries=0
    local flag=-tln
    [ "$proto" = udp ] && flag=-uln
    local in=()
    [ -n "$namespace" ] && in=(ip netns exec "$namespace")
    until "${in[@]}" ss -H "$flag" "sport = :$port" | grep -q .; do
        tries=$((tries + 1))
        if [ "$tries" -ge 600 ]; then
            echo "nothing listens on $proto port $port in $namespace within 60 s" >&2
            exit 1
        fi
        sleep 0.1
    done
}

network() {
    # The container's own namespace is the gateway's.
    ip netns add peer
    ip link set lo up
    ip -n peer link set lo up
    ip link add veth0 type veth peer name veth1 netns peer
    ip addr add "$GATEWAY4/24" dev veth0
    ip -6 addr add "$GATEWAY6/64" dev veth0 nodad
    ip link set veth0 up
    ip -n peer addr add "$PEER4/24" dev veth1
    ip -n peer -6 addr add "$PEER6/64" dev veth1 nodad
    ip -n peer link set veth1 up
    ip route add default via "$PEER4"
    ip -6 route add default via "$PEER6"
    # The metadata addresses answer on the peer, so a refusal of them is the rule's, not a
    # missing route.
    ip -n peer addr add 169.254.169.254/32 dev lo
    ip -n peer -6 addr add fd00:ec2::254/128 dev lo nodad
    # An address range the gateway holds only through its routing table.
    ip route add local 198.51.100.0/24 dev lo
    # A NAT rule that sends a connection to an outside address back to the gateway itself.
    nft -f - << 'EOF'
table ip lab_nat {
    chain output {
        type nat hook output priority dstnat;
        ip daddr 203.0.113.80 tcp dport 80 dnat to 127.0.0.1:8080
    }
}
EOF
    # The gateway's local service, on every address.
    background socat TCP4-LISTEN:8080,fork,reuseaddr EXEC:/bin/true
    background socat TCP6-LISTEN:8080,ipv6only=1,fork,reuseaddr EXEC:/bin/true
    # The peer's services: a device on 80 (also reached as the metadata addresses) and the
    # cloud's 443.
    for port in 80 443; do
        background ip netns exec peer socat "TCP4-LISTEN:$port,fork,reuseaddr" EXEC:/bin/true
        background ip netns exec peer socat "TCP6-LISTEN:$port,ipv6only=1,fork,reuseaddr" EXEC:/bin/true
    done
    # The site's DNS server, which the gateway's /etc/resolv.conf names (NetworkManager writes
    # the network's servers there on Raspberry Pi OS), and a loopback stub on the gateway that
    # answers the same, for the case the rules refuse.
    background ip netns exec peer dnsmasq --keep-in-foreground --no-resolv --no-hosts \
        --bind-interfaces --listen-address="$PEER4" --host-record="cloud.test,$PEER4,$PEER6" \
        --pid-file=/run/dnsmasq-peer.pid
    background dnsmasq --keep-in-foreground --no-resolv --no-hosts \
        --bind-interfaces --listen-address=127.0.0.53 --host-record="cloud.test,$PEER4,$PEER6" \
        --pid-file=/run/dnsmasq-stub.pid
    printf 'nameserver %s\n' "$PEER4" > /etc/resolv.conf
    await_listener "" tcp 8080
    await_listener peer tcp 80
    await_listener peer tcp 443
    await_listener peer udp 53
    await_listener "" udp 53
}

probe() {
    local who=$1 proto=$2 address=$3 port=$4 target error status udp=UDP4-DATAGRAM
    case "$address" in
    *:*)
        target="[$address]:$port"
        udp=UDP6-DATAGRAM
        ;;
    *) target="$address:$port" ;;
    esac
    # shellcheck disable=SC2046 # the account prefix is several words
    if [ "$proto" = tcp ]; then
        error=$(timeout 60 $(as "$who") socat -u /dev/null "TCP:$target,connect-timeout=30" 2>&1) && status=0 || status=$?
    else
        error=$(printf x | timeout 60 $(as "$who") socat -u - "$udp:$target,broadcast" 2>&1) && status=0 || status=$?
    fi
    if [ "$status" -eq 0 ]; then
        echo ok
    elif [ "$status" -eq 124 ]; then
        echo timeout
    else
        case "$error" in
        *"Connection refused"*) echo refused ;;
        *"Operation not permitted"*) echo not-permitted ;;
        *"timed out"*) echo timeout ;;
        *) echo "error: $error" ;;
        esac
    fi
}

resolve() {
    local who=$1 name=$2 found
    # shellcheck disable=SC2046 # the account prefix is several words
    if found=$(timeout 60 $(as "$who") getent ahostsv4 "$name"); then
        echo "$found" | awk '{ print $1 }' | sort -u | tr '\n' ' '
        echo
    else
        echo failed
    fi
}

counters() {
    nft list chain inet trustiot_agent agent | awk '
        /counter packets/ {
            for (i = 1; i <= NF; i++) if ($i == "packets") packets = $(i + 1)
            match($0, /comment "[^"]*"/)
            name = substr($0, RSTART + 9, RLENGTH - 10)
            total[name] += packets
        }
        END { for (name in total) print name, total[name] }'
}

case "${1:-}" in
network) network ;;
probe) probe "$2" "$3" "$4" "$5" ;;
resolve) resolve "$2" "$3" ;;
counters) counters ;;
*)
    echo "usage: lab network | probe WHO PROTO ADDRESS PORT | resolve WHO NAME | counters" >&2
    exit 2
    ;;
esac
