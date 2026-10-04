#!/usr/bin/env bash
# Run a real iroh-relay (HTTPS on 443 + QUIC address discovery on udp/7842) in jaunt-relay,
# with a throwaway test CA generated now into .state/relay (down.sh deletes it; nothing is
# committed). Copies the CA to every machine as /opt/jaunt/relay-ca.crt. Idempotent.
#   ./relay.sh [path/to/iroh-relay]     (default: spike/.tools/bin/iroh-relay)
# Relay URL for jaunt: https://relay.jaunt.test   (relay_ca = /opt/jaunt/relay-ca.crt)
set -euo pipefail
. "$(dirname "$0")/lib.sh"

BIN=${1:-$HARNESS_DIR/../../spike/.tools/bin/iroh-relay}
[ -x "$BIN" ] || die "no iroh-relay binary at $BIN (cargo install iroh-relay@1.3.0 --features server)"
docker container inspect "$C_RELAY" >/dev/null 2>&1 || die "harness is not up (./up.sh)"
S=$HARNESS_DIR/.state/relay
mkdir -p "$S"

if [ ! -f "$S/relay.crt" ]; then
  log "generating a test-only CA and relay certificate in $S"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 7 \
    -subj "/CN=jaunt harness test CA (throwaway)" -keyout "$S/ca.key" -out "$S/ca.crt" 2>/dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes \
    -subj "/CN=relay.jaunt.test" -keyout "$S/relay.key" -out "$S/relay.csr" 2>/dev/null
  printf 'subjectAltName=DNS:relay.jaunt.test\nbasicConstraints=CA:FALSE\nextendedKeyUsage=serverAuth\nkeyUsage=digitalSignature\n' > "$S/ext.cnf"
  openssl x509 -req -in "$S/relay.csr" -CA "$S/ca.crt" -CAkey "$S/ca.key" -CAcreateserial \
    -days 7 -extfile "$S/ext.cnf" -out "$S/relay.crt" 2>/dev/null
  chmod 600 "$S/ca.key" "$S/relay.key"
fi

cat > "$S/relay.toml" <<'EOF'
http_bind_addr = "0.0.0.0:80"
enable_metrics = false
enable_quic_addr_discovery = true
[tls]
https_bind_addr = "0.0.0.0:443"
quic_bind_addr = "0.0.0.0:7842"
hostname = "relay.jaunt.test"
cert_mode = "Manual"
manual_cert_path = "/etc/jaunt-relay/relay.crt"
manual_key_path = "/etc/jaunt-relay/relay.key"
EOF

docker exec "$C_RELAY" mkdir -p /etc/jaunt-relay /opt/jaunt/bin
docker cp -q "$BIN" "$C_RELAY:/opt/jaunt/bin/iroh-relay"
for f in relay.toml relay.crt relay.key; do docker cp -q "$S/$f" "$C_RELAY:/etc/jaunt-relay/$f"; done
docker exec "$C_RELAY" chmod 600 /etc/jaunt-relay/relay.key
if ! docker exec "$C_RELAY" pgrep -x iroh-relay >/dev/null; then
  docker exec -d "$C_RELAY" sh -c 'exec /opt/jaunt/bin/iroh-relay -c /etc/jaunt-relay/relay.toml >/tmp/iroh-relay.log 2>&1'
fi
for m in $MACHINES; do
  docker exec "$m" mkdir -p /opt/jaunt
  docker cp -q "$S/ca.crt" "$m:/opt/jaunt/relay-ca.crt"
  docker exec "$m" chmod 644 /opt/jaunt/relay-ca.crt
done
for _ in $(seq 1 40); do
  if docker exec "$C_A" curl -sf --cacert /opt/jaunt/relay-ca.crt https://relay.jaunt.test/ping >/dev/null 2>&1; then
    log "relay up: https://relay.jaunt.test (QAD udp/7842), CA at /opt/jaunt/relay-ca.crt on a b c"
    exit 0
  fi
  sleep 0.25
done
docker exec "$C_RELAY" tail -20 /tmp/iroh-relay.log >&2 || true
die "relay did not answer on https://relay.jaunt.test/ping"
