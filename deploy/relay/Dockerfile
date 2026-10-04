# iroh-relay 1.3.0, unmodified, for jaunt. Build: docker build -t jaunt-relay deploy/relay
# Builder: debian:12 + rustup (the same toolchain route as scripts/build-release.sh).
FROM debian:12 AS build
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates curl gcc libc6-dev \
 && rm -rf /var/lib/apt/lists/*
ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
RUN curl -sSf --proto =https --tlsv1.2 https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain 1.97.1
RUN cargo install iroh-relay@1.3.0 --features server --locked -j 4 --root /out

FROM debian:12-slim
RUN apt-get update \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home-dir /var/lib/iroh-relay --create-home relay
COPY --from=build /out/bin/iroh-relay /usr/local/bin/iroh-relay
COPY relay.toml /etc/iroh-relay/relay.toml
USER relay
# 80/tcp: ACME HTTP and /generate_204; 443/tcp: the relay (HTTPS + WebSocket); 7842/udp: QUIC address discovery.
EXPOSE 80/tcp 443/tcp 7842/udp
ENTRYPOINT ["/usr/local/bin/iroh-relay", "-c", "/etc/iroh-relay/relay.toml"]
