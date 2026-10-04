#!/usr/bin/env bash
# Services (DESIGN §5, §8) in the Docker NAT harness, on the direct path (a <-> b) and the
# relayed path (a <-> c): run (stdio, exit status, a client that leaves ends the command),
# cp both ways with checksums (a tree with modes and a link, and a 200 MB file whose
# throughput is recorded), fwd to a web server on the far loopback, and pipe as an ssh
# ProxyCommand with real OpenSSH (ssh and scp) on top.
#   JAUNT_BIN=target/release/jaunt test/e2e/07-services.sh     (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=07-services
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }
OUT=$ROOT/target/tmp/$E2E_NAME
rm -f "$OUT"-* "$OUT.tsv"
BIG_MB=${BIG_MB:-200}

h_begin
check "mesh a b c linked and connected" h_mesh
check "a -> b DIRECT" wait_until 30 h_is_state a b direct
sleep 5
check "a -> c RELAYED" h_is_state a c relay

# time_cmd <node> cmd... : run as jaunt in node, print elapsed seconds (3 decimals) on stdout.
time_cmd() {
  local n=$1; shift
  h_exec "$n" python3 -c 'import subprocess,sys,time
t=time.time(); r=subprocess.run(sys.argv[1:]); d=time.time()-t
print(f"{d:.3f}"); sys.exit(r.returncode)' "$@"
}
sha() { h_exec "$1" sha256sum "$2" | cut -d' ' -f1; }
mbps() { python3 -c "import sys; print(f'{$1/$2:.1f}')"; }

note "test data on a"
h_exec a sh -c "rm -rf /home/jaunt/xfer && mkdir -p /home/jaunt/xfer/tree/sub && cd /home/jaunt/xfer &&
  head -c ${BIG_MB}M /dev/urandom > big.bin &&
  echo hello > tree/a.txt && chmod 640 tree/a.txt &&
  printf '#!/bin/sh\necho ran\n' > tree/sub/run.sh && chmod 750 tree/sub/run.sh && chmod 711 tree/sub &&
  ln -s a.txt tree/link && : > tree/empty"
BIG_SHA=$(sha a /home/jaunt/xfer/big.bin)
tree_sig() { h_exec "$1" sh -c "cd $2 && find . -printf '%p %y %m %l\n' | sort && cat a.txt sub/run.sh | sha256sum"; }
TREE_SIG=$(tree_sig a /home/jaunt/xfer/tree)

for peer in b c; do
  path=$([ "$peer" = b ] && echo direct || echo relayed)
  note "run a -> $peer ($path)"
  out=$(echo through | h_exec_i a jaunt run "$peer" -- cat); check "[$path] stdin and stdout pass through" [ "$out" = through ]
  h_jaunt a run "$peer" -- sh -c 'exit 9'; check "[$path] exit status 9 passes through" [ $? = 9 ]
  err=$(h_jaunt a run "$peer" -- sh -c 'echo to-stderr >&2' 2>&1 >/dev/null); check "[$path] stderr passes through" [ "$err" = to-stderr ]
  h_jaunt a run "$peer" -- sh -c 'kill -TERM $$'; check "[$path] death by SIGTERM is status 143" [ $? = 143 ]
  out=$(h_jaunt a run "$peer" -- nosuchcommand 2>&1); rc=$?
  check "[$path] a missing command is status 127 and says so ($out)" [ "$rc" = 127 ]

  note "cp a <-> $peer ($path)"
  h_exec "$peer" sh -c 'rm -rf /home/jaunt/in && mkdir -p /home/jaunt/in'
  h_exec a sh -c 'rm -rf /home/jaunt/back && mkdir -p /home/jaunt/back'
  check "[$path] cp tree a -> $peer" h_jaunt a cp /home/jaunt/xfer/tree "$peer:in/"
  check "[$path] tree arrives with modes, link and contents" [ "$(tree_sig "$peer" /home/jaunt/in/tree)" = "$TREE_SIG" ]
  check "[$path] cp tree $peer -> a" h_jaunt a cp "$peer:in/tree" /home/jaunt/back/
  check "[$path] tree comes back identical" [ "$(tree_sig a /home/jaunt/back/tree)" = "$TREE_SIG" ]
  s=$(time_cmd a jaunt cp /home/jaunt/xfer/big.bin "$peer:in/big.bin"); rc=$?
  check "[$path] cp ${BIG_MB} MB a -> $peer ($s s)" [ "$rc" = 0 ]
  check "[$path] ${BIG_MB} MB checksum matches on $peer" [ "$(sha "$peer" /home/jaunt/in/big.bin)" = "$BIG_SHA" ]
  h_row "$OUT.tsv" "cp_push" "$path" "$BIG_MB" "$s" "$(mbps "$BIG_MB" "$s")" "ls after: $(h_peer_state a "$peer")"
  s=$(time_cmd a jaunt cp "$peer:in/big.bin" /home/jaunt/back/big.bin); rc=$?
  check "[$path] cp ${BIG_MB} MB $peer -> a ($s s)" [ "$rc" = 0 ]
  check "[$path] ${BIG_MB} MB checksum matches back on a" [ "$(sha a /home/jaunt/back/big.bin)" = "$BIG_SHA" ]
  h_row "$OUT.tsv" "cp_pull" "$path" "$BIG_MB" "$s" "$(mbps "$BIG_MB" "$s")" "ls after: $(h_peer_state a "$peer")"
  h_exec "$peer" rm -f /home/jaunt/in/big.bin; h_exec a rm -f /home/jaunt/back/big.bin
  out=$(h_jaunt a cp "$peer:/nonexistent/file" /home/jaunt/back/ 2>&1)
  check "[$path] cp of a missing file fails in words ($out)" grep -q 'No such file' <<<"$out"
done

note "a client that goes away ends the remote command"
docker exec -d -u jaunt jaunt-a "${H_ENV[@]}" jaunt run b -- sleep 777
check "sleep 777 runs on b" wait_until 15 sh -c 'docker exec jaunt-b pgrep -u jaunt -x sleep >/dev/null'
cpid=$(h_root a pgrep -u jaunt -f 'jaunt run b -- sleep 777' | head -1)
h_root a kill -INT "$cpid"
check "killing the client on a ends sleep on b" wait_until 10 sh -c '! docker exec jaunt-b pgrep -u jaunt -x sleep >/dev/null'

note "fwd: a web server on b's and c's loopback"
for peer in b c; do
  port=$([ "$peer" = b ] && echo 18001 || echo 18002)
  h_exec "$peer" sh -c "mkdir -p /home/jaunt/www && echo served-by-$peer > /home/jaunt/www/index.txt"
  docker exec -d -u jaunt -w /home/jaunt/www "jaunt-$peer" python3 -m http.server 8000 --bind 127.0.0.1
  wait_until 10 docker exec "jaunt-$peer" sh -c 'ss -ltn | grep -q 127.0.0.1:8000'
  docker exec -d -u jaunt "jaunt-a" "${H_ENV[@]}" jaunt fwd "$peer" "8000:$port"
  check "fwd a:$port -> $peer:8000 listening on loopback only" wait_until 10 sh -c "docker exec jaunt-a ss -ltn | grep -q '127.0.0.1:$port'"
  out=$(h_exec a curl -s --max-time 10 "http://127.0.0.1:$port/index.txt")
  check "curl through fwd reaches $peer's loopback web server ($out)" [ "$out" = "served-by-$peer" ]
  out=$(h_exec a curl -s --max-time 10 "http://127.0.0.1:$port/index.txt")
  check "a second connection through the same fwd works" [ "$out" = "served-by-$peer" ]
done
out=$(h_jaunt a pipe b 8999 </dev/null 2>&1)
check "pipe to a closed port is refused in words ($out)" grep -q 'nothing accepts connections' <<<"$out"

note "pipe: OpenSSH over jaunt (ProxyCommand jaunt pipe %h) to the sshd in b and c"
SSHO=(-o "ProxyCommand=jaunt pipe %h" -o BatchMode=yes -o ConnectTimeout=20)
for peer in b c; do
  out=$(h_exec a ssh "${SSHO[@]}" "jaunt@$peer" cat /etc/hostname 2>&1)
  check "ssh a -> $peer through jaunt pipe ($out)" [ "$out" = "$peer" ]
  h_exec a scp -q "${SSHO[@]}" /home/jaunt/xfer/tree/sub/run.sh "jaunt@$peer:/tmp/scp-run.sh"
  out=$(h_exec "$peer" sh /tmp/scp-run.sh)
  check "scp a -> $peer through jaunt pipe" [ "$out" = ran ]
  s=$(time_cmd a sh -c "head -c 50M /dev/zero | ssh -o 'ProxyCommand=jaunt pipe %h' -o BatchMode=yes jaunt@$peer 'cat > /dev/null'")
  h_row "$OUT.tsv" "ssh_over_pipe" "$([ "$peer" = b ] && echo direct || echo relayed)" 50 "$s" "$(mbps 50 "$s")" "ls after: $(h_peer_state a "$peer")"
done

note "throughput (op, path, MB, seconds, MB/s)"; sed 's/^/    /' "$OUT.tsv"
finish
