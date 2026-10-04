#!/usr/bin/env python3
"""Tiny UDP introducer (runs on the relay slot). Tool-agnostic stand-in for a STUN/QAD + signalling server.
Protocol (one datagram each):
  client -> "REG <session> <name>"        server records the OBSERVED (post-NAT) source addr
  server -> "WAIT"                         until a second name registers in the same session
  server -> "PEER <ip> <port> <yourip> <yourport>"   sent to BOTH sides at the same instant
  client -> "WHOAMI"  server -> "YOU <ip> <port>"     (plain STUN-like reflexive address probe)
"""
import socket, sys, time
port = int(sys.argv[1]) if len(sys.argv) > 1 else 9999
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("0.0.0.0", port))
sessions = {}
print(f"rendezvous listening on udp/{port}", flush=True)
while True:
    data, addr = s.recvfrom(2048)
    parts = data.decode(errors="replace").split()
    if not parts:
        continue
    if parts[0] == "WHOAMI":
        s.sendto(f"YOU {addr[0]} {addr[1]}".encode(), addr)
        print(f"{time.strftime('%H:%M:%S')} WHOAMI from {addr[0]}:{addr[1]}", flush=True)
        continue
    if parts[0] != "REG" or len(parts) < 3:
        continue
    sess, name = parts[1], parts[2]
    peers = sessions.setdefault(sess, {})
    peers[name] = addr
    print(f"{time.strftime('%H:%M:%S')} REG {sess} {name} observed {addr[0]}:{addr[1]}", flush=True)
    if len(peers) >= 2:
        (n1, a1), (n2, a2) = list(peers.items())[:2]
        s.sendto(f"PEER {a2[0]} {a2[1]} {a1[0]} {a1[1]}".encode(), a1)
        s.sendto(f"PEER {a1[0]} {a1[1]} {a2[0]} {a2[1]}".encode(), a2)
        del sessions[sess]
    else:
        s.sendto(b"WAIT", addr)
