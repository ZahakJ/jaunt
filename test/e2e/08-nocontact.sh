#!/usr/bin/env bash
# "Contacts nothing but the relay and the peers" (design §2, §14), proved from outside the
# process: before any daemon starts, every machine and every router logs each new flow
# (conntrack events), and each machine counts its own outgoing packets by destination class
# with iptables (relay, peer router, peer LAN, loopback, other). Then the mesh is set up and
# used (sync, hole punching, run, cp, a hop) and left idle for a while (keep-alives, net
# reports). Every flow must go to the relay, a peer, or the machine itself; `other` must be 0.
#   JAUNT_BIN=target/release/jaunt test/e2e/08-nocontact.sh    (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=08-nocontact
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-*
IDLE=${IDLE:-60}
NODES="a b c rtr-a rtr-b rtr-c"

audit_off() {
  for n in $NODES; do
    docker exec "jaunt-$n" sh -c 'pkill -x conntrack; iptables -D OUTPUT -j JAUNT_AUDIT 2>/dev/null; iptables -F JAUNT_AUDIT 2>/dev/null; iptables -X JAUNT_AUDIT 2>/dev/null; true' >/dev/null 2>&1
  done
}
h_cleanup_extra() { audit_off; }

h_begin
for m in a b c; do h_root "$m" pkill -u jaunt -x jaunt 2>/dev/null; done
audit_off

note "audit on: conntrack event logs on machines and routers, packet counters on machines"
for n in $NODES; do
  # A conntrack match makes the kernel track this namespace's flows (routers already do: NAT).
  docker exec "jaunt-$n" sh -c 'iptables -N JAUNT_AUDIT 2>/dev/null; iptables -F JAUNT_AUDIT
    iptables -A JAUNT_AUDIT -m conntrack --ctstate NEW,ESTABLISHED,RELATED,INVALID,UNTRACKED
    iptables -I OUTPUT 1 -j JAUNT_AUDIT; rm -f /tmp/ct.log'
  docker exec -d "jaunt-$n" sh -c 'exec conntrack -E -e NEW -o timestamp > /tmp/ct.log 2>&1'
done
# Machines: count jaunt's own packets per destination class (first match wins).
for m in a b c; do
  docker exec "jaunt-$m" sh -c '
    for spec in "relay -d 198.51.100.10" "peer-router -d 198.51.100.11,198.51.100.12,198.51.100.13" \
                "peer-lan -d 10.231.0.0/16" "loopback -o lo"; do
      set -- $spec; name=$1; shift
      iptables -A JAUNT_AUDIT -m owner --uid-owner jaunt "$@" -m comment --comment "$name" -j RETURN
    done
    iptables -A JAUNT_AUDIT -m owner --uid-owner jaunt -m comment --comment other -j RETURN'
done
sleep 1
check "conntrack event logs are running on all six nodes" sh -c "for n in $NODES; do docker exec jaunt-\$n pgrep -x conntrack >/dev/null || exit 1; done"

note "use the mesh: start, trust, sync, punch, run, cp, hop; then $IDLE s idle"
check "mesh a b c linked and connected" h_mesh
check "a -> b DIRECT" wait_until 30 h_is_state a b direct
check "run a -> b" sh -c '[ "$(docker exec -u jaunt jaunt-a env JAUNT_HOME=/home/jaunt/jh PATH=/opt/jaunt/bin:/usr/bin jaunt run b -- cat /etc/hostname)" = b ]'
check "run a -> c" sh -c '[ "$(docker exec -u jaunt jaunt-a env JAUNT_HOME=/home/jaunt/jh PATH=/opt/jaunt/bin:/usr/bin jaunt run c -- cat /etc/hostname)" = c ]'
h_exec a sh -c 'head -c 2M /dev/urandom > /tmp/nc.bin'
check "cp a -> c" h_jaunt a cp /tmp/nc.bin c:/tmp/nc.bin
# shellcheck disable=SC2046
python3 "$E2E_DIR/hop_session.py" -- $(h_tty_cmd b c:nc) > "$OUT-hop.out" 2>&1
check "hop b -> c" [ $? = 0 ]
sleep "$IDLE"
for m in a b c; do h_jaunt "$m" check > "$OUT-check-$m.out" 2>&1; done

note "collect"
for n in $NODES; do docker exec "jaunt-$n" sh -c 'pkill -x conntrack; sleep 0.2; cat /tmp/ct.log' > "$OUT-ct-$n.log" 2>/dev/null; done
for m in a b c; do docker exec "jaunt-$m" iptables -L JAUNT_AUDIT -v -n -x > "$OUT-count-$m.txt"; done

# Classify every flow destination. Allowed: the relay, the routers (peers' public side), the
# lab LANs (peers' own addresses, and a machine's own gateway for nothing but forwarding),
# loopback (the daemon's local socket traffic, the IPC is a unix socket).
python3 - "$OUT" $NODES <<'EOF' > "$OUT-flows.txt"
import ipaddress, re, sys
out, nodes = sys.argv[1], sys.argv[2:]
allowed = {
    "relay": [ipaddress.ip_network("198.51.100.10/32")],
    "peer router": [ipaddress.ip_network(n) for n in ("198.51.100.11/32", "198.51.100.12/32", "198.51.100.13/32")],
    "lab LAN": [ipaddress.ip_network("10.231.0.0/16")],
    "loopback": [ipaddress.ip_network("127.0.0.0/8")],
}
bad = 0
for n in nodes:
    seen = {}
    for line in open(f"{out}-ct-{n}.log", errors="replace"):
        m = re.search(r"\[NEW\]\s+(\w+)\s+\d+\s+(?:\d+\s+)?(?:\w+\s+)?src=(\S+) dst=(\S+)(?: sport=\d+ dport=(\d+))?", line)
        if not m:
            continue
        proto, src, dst, dport = m.groups()
        key = (proto, dst, dport or "-")
        seen[key] = seen.get(key, 0) + 1
    for (proto, dst, dport), cnt in sorted(seen.items()):
        ip = ipaddress.ip_address(dst)
        cls = next((k for k, nets in allowed.items() if any(ip in net for net in nets)), None)
        if cls is None:
            bad += 1
        print(f"{n}\t{proto}\t{dst}\t{dport}\t{cnt}\t{cls or 'NOT ALLOWED'}")
print(f"not-allowed destinations: {bad}")
EOF
sed 's/^/    /' "$OUT-flows.txt"
check "every flow seen on the machines and routers goes to the relay, a peer or loopback" grep -q '^not-allowed destinations: 0$' "$OUT-flows.txt"
check "no DNS query left any machine (port 53)" sh -c "! awk -F'\t' '\$4==53' '$OUT-flows.txt' | grep -q ."
check "some flows were seen at all (the audit works)" [ "$(grep -c . "$OUT-flows.txt")" -gt 5 ]
for m in a b c; do
  note "packet counters on $m (jaunt user only)"
  # Sum per label (iptables expands an address list into one rule per address).
  sums=$(awk 'match($0, /\/\* [a-z-]+ \*\//) {k=substr($0, RSTART+3, RLENGTH-6); n[k]+=$1} END {for (k in n) print k, n[k]}' "$OUT-count-$m.txt" | sort)
  sed 's/^/    /' <<<"$sums"
  other=$(awk '$1=="other" {print $2}' <<<"$sums")
  relay=$(awk '$1=="relay" {print $2}' <<<"$sums")
  check "$m: 0 packets to anything but the relay, peers or loopback (other=$other)" [ "${other:-x}" = 0 ]
  check "$m: the counters saw the relay traffic (relay=$relay)" [ "${relay:-0}" -gt 0 ]
  check "$m: jaunt check's own contact audit flags nothing" sh -c "! grep -q 'not the relay and not a known machine' '$OUT-check-$m.out'"
done
note "jaunt check on a (contact audit)"; grep -E 'contact|remote addresses' "$OUT-check-a.out" | sed 's/^/    a| /'

finish
