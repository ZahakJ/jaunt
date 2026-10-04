#!/usr/bin/env python3
"""UDP hole-punch client: punch.py <rendezvous_ip:port> <session> <name> <local_port> [seconds]
Binds a FIXED local port, registers with the introducer, learns the peer's mapped address,
then both sides fire probes at each other simultaneously. Prints a JSON-ish result line:
  RESULT name=<n> local=<port> mapped=<ip:port> peer=<ip:port> punched=<yes|no> rtt_ms=<median|->
Exit code 0 if punched, 1 otherwise."""
import socket, sys, time, statistics
rv, sess, name, lport = sys.argv[1], sys.argv[2], sys.argv[3], int(sys.argv[4])
dur = float(sys.argv[5]) if len(sys.argv) > 5 else 6.0
rip, rport = rv.split(":"); rport = int(rport)
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", lport))
s.settimeout(0.5)
peer = mapped = None
deadline = time.time() + 30
while peer is None and time.time() < deadline:
    s.sendto(f"REG {sess} {name}".encode(), (rip, rport))
    try:
        d, a = s.recvfrom(2048)
    except socket.timeout:
        continue
    p = d.decode().split()
    if p and p[0] == "PEER":
        peer = (p[1], int(p[2])); mapped = f"{p[3]}:{p[4]}"
    elif p and p[0] == "WAIT":
        time.sleep(0.2)
if peer is None:
    print(f"RESULT name={name} local={lport} mapped=? peer=? punched=no rtt_ms=- (no rendezvous)"); sys.exit(1)
got_from = set(); rtts = []; seq = 0
s.settimeout(0.05)
end = time.time() + dur
while time.time() < end:
    seq += 1
    s.sendto(f"PING {name} {seq} {time.time():.6f}".encode(), peer)
    t0 = time.time()
    while time.time() - t0 < 0.2:
        try:
            d, a = s.recvfrom(2048)
        except socket.timeout:
            continue
        p = d.decode(errors="replace").split()
        if not p: continue
        if p[0] == "PING":
            got_from.add(f"{a[0]}:{a[1]}")
            s.sendto(f"PONG {p[3]}".encode(), a)
        elif p[0] == "PONG":
            got_from.add(f"{a[0]}:{a[1]}")
            rtts.append((time.time() - float(p[1])) * 1000)
ok = len(rtts) > 0
rtt = f"{statistics.median(rtts):.1f}" if rtts else "-"
print(f"RESULT name={name} local={lport} mapped={mapped} peer={peer[0]}:{peer[1]} punched={'yes' if ok else 'no'} "
      f"rtt_ms={rtt} heard_from={','.join(sorted(got_from)) or '-'}")
sys.exit(0 if ok else 1)
