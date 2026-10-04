# shellcheck shell=bash disable=SC2034
# Shared topology definition for the jaunt NAT harness. Sourced by up.sh, down.sh, jh.
# Every docker object this harness creates is listed here, by name.

HARNESS_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
IMAGE=${JAUNT_HARNESS_IMAGE:-jaunt-harness:latest}

# ---- networks -------------------------------------------------------------
# "internet": TEST-NET-2 (RFC 5737) so mapped addresses look public, never routed for real.
NET_INET=jaunt-inet;   SUB_INET=198.51.100.0/24
NET_LANA=jaunt-lan-a;  SUB_LANA=10.231.1.0/24
NET_LANB=jaunt-lan-b;  SUB_LANB=10.231.2.0/24
NET_HARD=jaunt-lan-c;  SUB_HARD=10.231.3.0/24
NETWORKS="$NET_INET $NET_LANA $NET_LANB $NET_HARD"

# ---- containers -----------------------------------------------------------
C_RELAY=jaunt-relay;  IP_RELAY=198.51.100.10
C_RTRA=jaunt-rtr-a;   WAN_RTRA=198.51.100.11; LAN_RTRA=10.231.1.2
C_RTRB=jaunt-rtr-b;   WAN_RTRB=198.51.100.12; LAN_RTRB=10.231.2.2
C_RTRC=jaunt-rtr-c;   WAN_RTRC=198.51.100.13; LAN_RTRC=10.231.3.2
C_A=jaunt-a;          IP_A=10.231.1.100
C_B=jaunt-b;          IP_B=10.231.2.100
C_C=jaunt-c;          IP_C=10.231.3.100
ROUTERS="$C_RTRA $C_RTRB $C_RTRC"
MACHINES="$C_A $C_B $C_C"
CONTAINERS="$C_RELAY $ROUTERS $MACHINES"

# Hard-NAT mode for router c: symmetric | udpblock | both
HARD_MODE=${HARD_MODE:-symmetric}
# Conntrack UDP timeouts on all routers (Linux defaults 30 / 120).
NAT_UDP_TIMEOUT=${NAT_UDP_TIMEOUT:-30}
NAT_UDP_STREAM_TIMEOUT=${NAT_UDP_STREAM_TIMEOUT:-120}
RENDEZVOUS_PORT=9999

die() { echo "jh: $*" >&2; exit 1; }
log() { echo "[jaunt-harness] $*" >&2; }

# machine name (a|b|c|relay|jaunt-a|...) -> container name
cname() {
  case "$1" in
    a|jaunt-a) echo "$C_A" ;; b|jaunt-b) echo "$C_B" ;; c|jaunt-c) echo "$C_C" ;;
    relay|jaunt-relay) echo "$C_RELAY" ;;
    rtr-a|jaunt-rtr-a) echo "$C_RTRA" ;; rtr-b|jaunt-rtr-b) echo "$C_RTRB" ;; rtr-c|jaunt-rtr-c) echo "$C_RTRC" ;;
    *) die "unknown node '$1' (a b c relay rtr-a rtr-b rtr-c)" ;;
  esac
}
# lan name -> "network router_container router_lan_ip prefix"
laninfo() {
  case "$1" in
    a|lanA|lan-a|lana) echo "$NET_LANA $C_RTRA $LAN_RTRA 10.231.1" ;;
    b|lanB|lan-b|lanb) echo "$NET_LANB $C_RTRB $LAN_RTRB 10.231.2" ;;
    c|hard|lanC|lan-c|lanc) echo "$NET_HARD $C_RTRC $LAN_RTRC 10.231.3" ;;
    *) die "unknown lan '$1' (lanA lanB hard)" ;;
  esac
}
# interface name inside container $1 that carries an address in prefix $2 (e.g. 10.231.1)
ifname_for() {
  docker exec "$1" sh -c "ip -o -4 addr show | awk -v p='$2.' 'index(\$4,p)==1{print \$2; exit}'"
}
# the lan network a machine is currently attached to
current_lan_net() {
  docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$1" | tr ' ' '\n' | grep '^jaunt-lan-' | head -1
}
