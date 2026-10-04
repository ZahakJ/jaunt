#!/usr/bin/env bash
# Bring up the jaunt NAT simulation. Idempotent: safe to re-run; re-applies router/route config.
#   HARD_MODE=symmetric|udpblock|both  NAT_UDP_TIMEOUT=30  ./up.sh
set -euo pipefail
. "$(dirname "$0")/lib.sh"

if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
  log "building $IMAGE"
  docker build -t "$IMAGE" "$HARNESS_DIR"
fi

# All networks: --internal (no route to the real internet, no docker MASQUERADE) and
# inhibit_ipv4 (the host gets NO address on them, so containers cannot reach host services).
mknet() { # name subnet
  if docker network inspect "$1" >/dev/null 2>&1; then return; fi
  log "network $1 $2"
  docker network create --internal -o com.docker.network.bridge.inhibit_ipv4=true \
    --label jaunt-harness=1 --subnet "$2" "$1" >/dev/null
}
mknet "$NET_INET" "$SUB_INET"
mknet "$NET_LANA" "$SUB_LANA"
mknet "$NET_LANB" "$SUB_LANB"
mknet "$NET_HARD" "$SUB_HARD"

# run_node name network ip mem [extra docker args...]
run_node() {
  local name=$1 net=$2 ip=$3 mem=$4; shift 4
  local state
  if docker container inspect "$name" >/dev/null 2>&1; then
    state=$(docker container inspect -f '{{.State.Status}}' "$name")
  else state=absent; fi
  case "$state" in
    running) return ;;
    paused) docker unpause "$name" >/dev/null; return ;;
    absent) ;;
    *) docker start "$name" >/dev/null; return ;;
  esac
  log "container $name ($ip on $net)"
  docker run -d --name "$name" --hostname "${name#jaunt-}" --label jaunt-harness=1 \
    --network "$net" --ip "$ip" --cap-add NET_ADMIN --cap-add NET_RAW \
    --memory "$mem" --pids-limit 512 --init \
    --sysctl net.ipv6.conf.all.disable_ipv6=1 --add-host "relay.jaunt.test:$IP_RELAY" \
    -v "$HARNESS_DIR:/harness:ro" "$@" "$IMAGE" sleep infinity >/dev/null
}
# connect container to an extra network with fixed ip (idempotent)
attach() { # container net ip
  if ! docker inspect -f '{{range $k,$v := .NetworkSettings.Networks}}{{$k}} {{end}}' "$1" | grep -qw -- "$2"; then
    docker network connect --ip "$3" "$2" "$1"
  fi
}

RTR_SYSCTL=(--sysctl net.ipv4.ip_forward=1
            --sysctl net.netfilter.nf_conntrack_udp_timeout="$NAT_UDP_TIMEOUT"
            --sysctl net.netfilter.nf_conntrack_udp_timeout_stream="$NAT_UDP_STREAM_TIMEOUT")

run_node "$C_RELAY" "$NET_INET" "$IP_RELAY" 512m
run_node "$C_RTRA" "$NET_INET" "$WAN_RTRA" 128m "${RTR_SYSCTL[@]}"; attach "$C_RTRA" "$NET_LANA" "$LAN_RTRA"
run_node "$C_RTRB" "$NET_INET" "$WAN_RTRB" 128m "${RTR_SYSCTL[@]}"; attach "$C_RTRB" "$NET_LANB" "$LAN_RTRB"
run_node "$C_RTRC" "$NET_INET" "$WAN_RTRC" 128m "${RTR_SYSCTL[@]}"; attach "$C_RTRC" "$NET_HARD" "$LAN_RTRC"
run_node "$C_A" "$NET_LANA" "$IP_A" 768m
run_node "$C_B" "$NET_LANB" "$IP_B" 768m
run_node "$C_C" "$NET_HARD" "$IP_C" 768m

docker exec "$C_RTRA" /harness/node/router.sh "$WAN_RTRA" 10.231.1 cone      "$IP_RELAY"
docker exec "$C_RTRB" /harness/node/router.sh "$WAN_RTRB" 10.231.2 cone      "$IP_RELAY"
docker exec "$C_RTRC" /harness/node/router.sh "$WAN_RTRC" 10.231.3 "$HARD_MODE" "$IP_RELAY"

# Machines: default route via whichever LAN router they are currently attached to (survives roam).
for m in $MACHINES; do
  net=$(current_lan_net "$m")
  case "$net" in
    "$NET_LANA") gw=$LAN_RTRA ;; "$NET_LANB") gw=$LAN_RTRB ;; "$NET_HARD") gw=$LAN_RTRC ;;
    *) die "$m is attached to no lan network" ;;
  esac
  docker exec "$m" /harness/node/machine.sh "$gw"
done

# SSH: one throwaway harness keypair for user 'jaunt' everywhere (baseline "ssh via relay" tests).
STATE=$HARNESS_DIR/.state; mkdir -p "$STATE"
[ -f "$STATE/id_ed25519" ] || ssh-keygen -q -t ed25519 -N '' -C jaunt-harness -f "$STATE/id_ed25519"
for n in $C_RELAY $MACHINES; do
  docker exec "$n" sh -c 'install -d -m700 -o jaunt -g jaunt /home/jaunt/.ssh'
  docker cp -q "$STATE/id_ed25519" "$n:/home/jaunt/.ssh/id_ed25519"
  docker cp -q "$STATE/id_ed25519.pub" "$n:/home/jaunt/.ssh/authorized_keys"
  docker exec "$n" sh -c '
    cat > /home/jaunt/.ssh/config <<EOF
Host *
  StrictHostKeyChecking no
  UserKnownHostsFile /dev/null
  LogLevel ERROR
  ServerAliveInterval 10
Host relay
  HostName '"$IP_RELAY"'
EOF
    chown -R jaunt:jaunt /home/jaunt/.ssh; chmod 600 /home/jaunt/.ssh/*
    passwd -d jaunt >/dev/null 2>&1 || true
    grep -q "^GatewayPorts" /etc/ssh/sshd_config || printf "PasswordAuthentication no\nGatewayPorts no\nAllowTcpForwarding yes\n" >> /etc/ssh/sshd_config
    pgrep -x sshd >/dev/null || /usr/bin/sshd'
done
# Introducer on the relay slot (udp/9999). The real relay binary will be installed next to it later.
docker exec "$C_RELAY" pgrep -f rendezvous.py >/dev/null || \
  docker exec -d "$C_RELAY" sh -c "exec python3 -u /harness/node/rendezvous.py $RENDEZVOUS_PORT >/tmp/rendezvous.log 2>&1"

log "up: relay=$IP_RELAY rtr-a=$WAN_RTRA rtr-b=$WAN_RTRB rtr-c=$WAN_RTRC(hard:$HARD_MODE) a=$IP_A b=$IP_B c=$IP_C"
