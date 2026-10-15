#!/usr/bin/env bash
# The desktop and network tools in the Docker NAT harness: ports, tabs, send-tab,
# notify, clipboard sharing, kept forwards (across a roam mid-transfer, a host daemon
# restart and a client daemon restart), open, socks and rfwd. The machines have no
# desktop, so xdg-open, notify-send and xclip are stand-ins that write to /tmp/desk.log.
#   JAUNT_BIN=target/release/jaunt test/e2e/10-tools.sh   (KEEP_HARNESS=1 leaves it up)
set -uo pipefail
E2E_NAME=10-tools
. "$(dirname "$0")/lib.sh"

BIN=${JAUNT_BIN:-$ROOT/target/release/jaunt}
[ -x "$BIN" ] || { echo "no jaunt binary at $BIN (cargo build --release)"; exit 2; }

h_cleanup_extra() {
  for m in a b c; do h_root "$m" pkill -u jaunt -f 'http.server' 2>/dev/null; done
}

h_begin
check "mesh a b c linked and connected" h_mesh
check "a -> b is direct" wait_until 30 h_is_state a b direct

note "stand-in desktop tools"
for m in a b c; do
  h_root "$m" sh -c 'cat > /opt/jaunt/bin/xdg-open <<"EOF"
#!/bin/sh
echo "open $*" >> /tmp/desk.log
EOF
cat > /opt/jaunt/bin/notify-send <<"EOF"
#!/bin/sh
shift 3; echo "notify $*" >> /tmp/desk.log
EOF
cat > /opt/jaunt/bin/xclip <<"EOF"
#!/bin/sh
case "$*" in
  *TARGETS*) echo UTF8_STRING ;;
  *-out*) cat /tmp/clip 2>/dev/null || exit 1 ;;
  *-in*) cat > /tmp/clip; echo "clip $(cat /tmp/clip)" >> /tmp/desk.log ;;
esac
EOF
chmod 755 /opt/jaunt/bin/xdg-open /opt/jaunt/bin/notify-send /opt/jaunt/bin/xclip; : > /tmp/desk.log; chmod 666 /tmp/desk.log; rm -f /tmp/clip'
done
desk() { h_root "$1" cat /tmp/desk.log; }

note "ports and tabs"
h_exec b sh -c "mkdir -p /home/jaunt/www/site && head -c 40000000 /dev/urandom > /home/jaunt/www/site/big.bin && echo served-by-b > /home/jaunt/www/site/index.txt"
docker exec -d -u jaunt -w /home/jaunt/www/site jaunt-b python3 -m http.server 8100 --bind 127.0.0.1
wait_until 10 docker exec jaunt-b sh -c 'ss -ltn | grep -q 127.0.0.1:8100'
out=$(h_jaunt a ports b)
check "ports lists b's web server with its process and directory ($(grep -m1 '^8100' <<<"$out"))" grep -qP '^8100\t127.0.0.1\tpython3\tsite$' <<<"$out"
h_exec_i b python3 - <<'EOF'
import os, struct
d = "/home/jaunt/.config/chromium/Default/Sessions"; os.makedirs(d, exist_ok=True)
def cmd(i, body): return struct.pack("<H", len(body) + 1) + bytes([i]) + body
def s(b): return struct.pack("<i", len(b)) + b + b"\0" * (-len(b) % 4)
def nav(tab, url, title):
    t = title.encode("utf-16-le")
    p = struct.pack("<ii", tab, 0) + s(url.encode()) + struct.pack("<i", len(title)) + t + b"\0" * (-len(t) % 4)
    return struct.pack("<I", len(p)) + p
f = b"SNSS" + struct.pack("<i", 3)
f += cmd(0, struct.pack("<ii", 1, 7)) + cmd(6, nav(7, "https://example.org/a", "Tab A"))
f += cmd(0, struct.pack("<ii", 1, 8)) + cmd(6, nav(8, "https://example.org/b", "Tab B"))
open(d + "/Session_1", "wb").write(f)
EOF
out=$(h_jaunt a tabs b --list)
check "tabs lists b's two Chromium tabs ($(tr '\t\n' ' ;' <<<"$out"))" [ "$(grep -c '^chromium	Tab [AB]	https://example.org/[ab]$' <<<"$out")" = 2 ]
h_jaunt a tabs b --all >/dev/null 2>&1
check "tabs --all opens both on a" [ "$(desk a | grep -c '^open https://example.org/[ab]$')" = 2 ]

note "send-tab and notify, by name and to the machine hopped in"
check "send-tab a -> b" h_jaunt a send-tab b https://example.org/sent
check "b's browser got it" grep -q '^open https://example.org/sent$' <<<"$(desk b)"
out=$(h_jaunt a send-tab b file:///etc/passwd 2>&1)
check "send-tab refuses a file URL ($out)" grep -q 'only http' <<<"$out"
check "notify --to b" h_jaunt a notify --to b build done
check "b shows it, named after a" grep -q '^notify a · build done' <<<"$(desk b)"
out=$(h_jaunt b notify nobody here 2>&1)
check "notify with nobody hopped in says so ($out)" grep -q 'no machine has hopped in' <<<"$out"

note "clipboard: refused until shared, then synced both ways"
out=$(h_jaunt a clip get b 2>&1)
check "clip get is refused while b does not share ($out)" grep -q 'does not share its clipboard' <<<"$out"
check "clip on at a" h_jaunt a clip on
check "clip on at b" h_jaunt b clip on
h_exec a sh -c 'printf "copied on a" > /tmp/clip'
check "a's copy arrives on b" wait_until 10 sh -c "docker exec jaunt-b cat /tmp/clip 2>/dev/null | grep -qx 'copied on a'"
h_exec b sh -c 'printf "copied on b" > /tmp/clip'
check "b's copy arrives on a" wait_until 10 sh -c "docker exec jaunt-a cat /tmp/clip 2>/dev/null | grep -qx 'copied on b'"
sleep 3
check "nothing echoes back and forth" [ "$(desk a | grep -c '^clip')" = 1 ]
check "c (not sharing) got nothing" [ -z "$(desk c | grep '^clip')" ]

note "kept forward a:18010 -> b:8100"
check "fwd --keep" h_jaunt a fwd b 8100:18010 --keep --name site
check "listening on loopback only" wait_until 10 sh -c "docker exec jaunt-a ss -ltn | grep -q '127.0.0.1:18010'"
check "jaunt ls --json lists it" grep -q '"name": "site"' <<<"$(h_jaunt a ls --json)"
out=$(h_exec a curl -s --max-time 10 http://127.0.0.1:18010/index.txt)
check "curl through it ($out)" [ "$out" = served-by-b ]

note "a 40 MB download at 1 MB/s through the forward while a roams to the hard NAT and back"
SHA=$(h_exec b sha256sum /home/jaunt/www/site/big.bin | cut -d' ' -f1)
docker exec -d -u jaunt jaunt-a sh -c 'curl -s --limit-rate 1M -o /tmp/big.bin http://127.0.0.1:18010/big.bin; echo $? > /tmp/curl.rc'
sleep 8; "$H_JH" roam a hard >/dev/null 2>&1
sleep 10; "$H_JH" roam a lanA >/dev/null 2>&1
check "the download finishes" wait_until 90 docker exec jaunt-a test -s /tmp/curl.rc
check "curl exit 0 ($(h_root a cat /tmp/curl.rc))" [ "$(h_root a cat /tmp/curl.rc)" = 0 ]
check "the file arrived whole across both roams" [ "$(h_exec a sha256sum /tmp/big.bin | cut -d' ' -f1)" = "$SHA" ]

note "the forward outlives daemon restarts"
h_daemon_restart b
out=$(h_exec a curl -s --max-time 20 --retry 5 --retry-connrefused http://127.0.0.1:18010/index.txt)
check "after b's daemon restarted ($out)" [ "$out" = served-by-b ]
h_daemon_restart a
check "a's restarted daemon listens again" wait_until 15 sh -c "docker exec jaunt-a ss -ltn | grep -q '127.0.0.1:18010'"
out=$(h_exec a curl -s --max-time 20 http://127.0.0.1:18010/index.txt)
check "after a's daemon restarted ($out)" [ "$out" = served-by-b ]
check "unfwd" h_jaunt a unfwd site
check "gone from the config" sh -c "! docker exec jaunt-a grep -q site /home/jaunt/jh/config/config.toml"

note "open, socks, rfwd"
out=$(h_jaunt a open b:8100/index.txt --local 18011)
check "open forwards and opens the browser ($out)" grep -q '^open http://127.0.0.1:18011/index.txt$' <<<"$(desk a)"
check "the page is there" [ "$(h_exec a curl -s --max-time 10 http://127.0.0.1:18011/index.txt)" = served-by-b ]
docker exec -d -u jaunt jaunt-a "${H_ENV[@]}" jaunt socks c 18012
wait_until 10 sh -c "docker exec jaunt-a ss -ltn | grep -q '127.0.0.1:18012'"
out=$(h_exec a curl -s --max-time 15 --proxy socks5h://127.0.0.1:18012 http://localhost:8100/index.txt 2>&1)
check "socks via c reaches c's own loopback, not b's (curl exit $?)" [ "$out" != served-by-b ]
h_exec c sh -c "mkdir -p /home/jaunt/cw && echo served-by-c > /home/jaunt/cw/index.txt"
docker exec -d -u jaunt -w /home/jaunt/cw jaunt-c python3 -m http.server 8100 --bind 127.0.0.1
wait_until 10 docker exec jaunt-c sh -c 'ss -ltn | grep -q 127.0.0.1:8100'
out=$(h_exec a curl -s --max-time 15 --proxy socks5h://127.0.0.1:18012 http://localhost:8100/index.txt)
check "socks5h through relayed c ($out)" [ "$out" = served-by-c ]
docker exec -d -u jaunt jaunt-b "${H_ENV[@]}" jaunt rfwd a 18013:8100
check "rfwd: a listens on 18013 for b" wait_until 10 sh -c "docker exec jaunt-a ss -ltn | grep -q '127.0.0.1:18013'"
out=$(h_exec a curl -s --max-time 10 http://127.0.0.1:18013/index.txt)
check "a:18013 reaches b's loopback 8100 ($out)" [ "$out" = served-by-b ]
h_root b pkill -u jaunt -f 'jaunt rfwd'
check "stopping rfwd closes a's listener" wait_until 10 sh -c "! docker exec jaunt-a ss -ltn | grep -q '127.0.0.1:18013'"

h_save_logs
finish
