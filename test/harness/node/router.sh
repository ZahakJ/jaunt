#!/bin/sh
# Runs INSIDE a router container. Configures a home-router-like NAT.
#   router.sh <wan_ip> <lan_prefix e.g. 10.231.1> <mode: cone|symmetric|udpblock|both> <relay_ip>
# cone      : Linux MASQUERADE (port-preserving, endpoint-independent mapping in practice,
#             address+port-dependent filtering) == typical home router ("port-restricted cone").
# symmetric : MASQUERADE --random-fully: a fresh random source port for every new 5-tuple
#             (endpoint-dependent mapping) == symmetric NAT; hole punching w/o port prediction fails.
# udpblock  : cone NAT, but all forwarded UDP is dropped except to the relay (UDP-hostile hotel/corp).
# both      : symmetric + udpblock.
set -eu
WAN_IP=$1 LAN_PREFIX=$2 MODE=$3 RELAY_IP=$4
WAN=$(ip -o -4 addr show | awk -v p="$WAN_IP/" 'index($4,p)==1{print $2; exit}')
LAN=$(ip -o -4 addr show | awk -v p="$LAN_PREFIX." 'index($4,p)==1{print $2; exit}')
[ -n "$WAN" ] && [ -n "$LAN" ] || { echo "router.sh: cannot find wan/lan ifaces" >&2; exit 1; }

iptables -t nat -F; iptables -F; iptables -X 2>/dev/null || true
iptables -P FORWARD DROP
iptables -P INPUT ACCEPT
# Forwarding: LAN -> WAN only; replies back in.
iptables -A FORWARD -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
case "$MODE" in udpblock|both)
  iptables -A FORWARD -i "$LAN" -o "$WAN" -p udp ! -d "$RELAY_IP" -j DROP ;;
esac
iptables -A FORWARD -i "$LAN" -o "$WAN" -j ACCEPT
# Unsolicited traffic hitting the router's own WAN address is silently dropped, like a real
# home router. (Without this, the kernel answers ICMP port-unreachable and the resulting
# conntrack entry makes the later outbound punch get a DIFFERENT source port -> punch fails.)
iptables -A INPUT -i "$WAN" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
iptables -A INPUT -i "$WAN" -p icmp --icmp-type echo-request -j ACCEPT
iptables -A INPUT -i "$WAN" -j DROP
case "$MODE" in
  symmetric|both) iptables -t nat -A POSTROUTING -o "$WAN" -j MASQUERADE --random-fully ;;
  *)              iptables -t nat -A POSTROUTING -o "$WAN" -j MASQUERADE ;;
esac
conntrack -F >/dev/null 2>&1 || true
echo "router $(cat /etc/hostname): wan=$WAN($WAN_IP) lan=$LAN mode=$MODE"
