# iroh feasibility spike (jaunt)

Date: 2026-10-04. Author: research agent (iroh spike). Tags: **[V]** = VERIFIED today (ran it, or read
primary source / crate source today, cited); **[U]** = UNVERIFIED (from memory / not tested).

## TL;DR

* **Recommend building on iroh 1.3.0.** The spike dials by ed25519 id over a **self-hosted** iroh-relay with
  zero n0 services, gets a relay path in ~5-30 ms and upgrades to a **direct path ~15-250 ms later**, including
  through two simulated home NATs. A live QUIC stream survived the dialer losing its network interface. **[V]**
* The hypothesis needs **four corrections**:
  1. **Your relay needs public UDP (QAD, port 7842) and its own TLS certificate.** Without it, two machines that
     are both behind NAT never go direct. Behind a Cloudflare tunnel the relay is HTTP/WebSocket only, so it
     **works as a fallback relay but gives no hole punching** (unless one side has global IPv6 or UPnP).
     Host the relay on a small VPS with UDP open (Fly.io may also work **[U]**). **[V]**
  2. **"No third-party contact" is not the default, even with `presets::Minimal`.** iroh's DNS resolver falls back
     to **1.1.1.1 / 8.8.8.8 / 9.9.9.9 (UDP and DoH)** whenever the system nameserver fails, and it ignores
     `/etc/hosts`. Install a custom `Resolver` (code below, 25 lines). It also probes a captive portal at
     `http://<relay-host>:80/generate_204`. **[V]**
  3. **A bare id is not enough to dial.** It fails with `No addressing information available / No address lookup
     configured`. The minimum is **id + relay URL**. Since all of jaunt's machines share one relay, store the
     relay URL once in config and the user-visible handle can still be just the id or alias. **[V]**
  4. iroh 1.x is built on **noq** (n0's own QUIC, a fork of quinn with multipath and QUIC NAT traversal), not
     quinn. Names changed: `NodeId`→`EndpointId`, `NodeAddr`→`EndpointAddr`, `discovery`→`address_lookup`,
     `Endpoint::builder(preset)`. **[V]**
* **No published third-party audit** of iroh, noq or iroh-relay could be found. Three security fixes shipped
  in iroh 1.1.0 (2026-09-01); the blog still shows "xxx link to report once published". **[V]**
  rustls had a Cure53 audit in 2020 **[V]**. RustCrypto `spake2` says "never received an independent third
  party audit" **[V]**.

## 1. Versions and API (all [V], from crates.io API and the crate sources in ~/.cargo/registry)

| crate | version | date |
|---|---|---|
| iroh / iroh-relay / iroh-base | 1.3.0 | 2026-09-28 (1.0 released 2026-06-15, "wire format and API are stable") |
| noq / noq-proto / noq-udp | 1.3.0 | QUIC stack (quinn fork) |
| iroh-tickets | 1.0.0 | 2026-06-15 |
| iroh-mdns-address-lookup | 0.6.0 | 2026-09-28 (deps: iroh ^1.0, swarm-discovery) |
| spake2 | 0.4.0 stable (2023-07-28), 0.5.0-pre.0 (2026-01-25) | RustCrypto/PAKEs |
| portable-pty 0.9.0 (2025-02, wezterm repo); pty-process 0.5.3 (2025-07, tokio support); nix 0.31.3 | | |
| dumbpipe 0.39.0, sendme 0.36.0 | 2026-06-15 | |

Key API facts (iroh 1.3.0 source):
* `Endpoint::builder(presets::Minimal)` sets only the crypto provider: no relays and **no address lookup**.
  `presets::N0` adds the n0 relays (`RelayMode::Default` = `{use1,usw1,euc1,aps1}-1.relay.n0.iroh.link`), plus
  `PkarrPublisher::n0_dns()`, `PkarrResolver::n0_dns()` and `DnsAddressLookup::n0_dns()` (`iroh.link`).
  **Never use `N0` or `RelayMode::Default`.** The n0 hostnames appear only in `defaults.rs` and `presets.rs`,
  and the env var `IROH_FORCE_STAGING_RELAYS` only matters with `Default`.
* `RelayMode::{Disabled, Default, Staging, Custom(RelayMap)}`, `RelayMode::custom([RelayUrl])`.
  `RelayConfig { url, quic: Option<RelayQuicConfig{port}>, auth_token }` with `.with_auth_token()`.
* Address lookup: `.address_lookup(impl AddressLookupBuilder)`, `.clear_address_lookup()`, and the module
  `iroh::address_lookup::{memory, dns, pkarr}`. mDNS is a separate crate: `iroh-mdns-address-lookup`
  (`MdnsAddressLookup`). Not tested.
* `EndpointAddr::new(id).with_relay_url(u).with_ip_addr(sa)`. `ep.addr()` and `ep.watch_addr()` show your
  own addresses. Note that `addr()` lists **every interface**, including docker bridges, LAN IPs and global
  IPv6. Use `.addr_filter(..)` if tickets should not leak them.
* Accept loop: `.alpns(vec![ALPN])`, then `while let Some(inc) = ep.accept().await { let conn = inc.await?; }`.
  `conn.remote_id()` is authenticated by TLS 1.3 using raw public keys.
* **Allowlist hook:** `EndpointHooks::after_handshake(&Connection) -> AfterHandshakeOutcome::{Accept, Reject{error_code, reason}}`
  and `before_connect(&EndpointAddr, alpn)`. Install with `.hooks(h)`. The spike uses it.
* **Direct vs relay and RTT:** `conn.paths()` (a snapshot) gives `Path { is_selected(), is_ip(), is_relay(),
  remote_addr(), rtt(), stats() }`. `conn.path_events()` is a `'static` stream of
  `PathEvent::{Opened, Closed{last_stats}, Selected, Lagged}`, and `conn.paths_stream()` streams snapshots.
  `conn.rtt(PathId)` and `conn.stats()` are also available.
* Timeouts (iroh defaults): keep-alive 5 s (`HEARTBEAT_INTERVAL`), per-path idle 15 s, connection
  `max_idle_timeout` 30 s (noq default), up to 8 multipath paths. You can set these with
  `QuicTransportConfig::builder().keep_alive_interval(..).max_idle_timeout(Some(d.try_into()?))
  .default_path_max_idle_timeout(..)`.
* Network change: detected automatically through netwatch, or call `ep.network_change().await`
  (needed on Android).
* `ep.online().await` waits until the home relay is connected. `ep.home_relay_status()` exposes
  `auth_denied_reason()`.
* Gotcha (dumbpipe source **[V]**): the peer does not see a QUIC stream until the opener writes to it.
  dumbpipe therefore writes a 5-byte `HANDSHAKE` first. jaunt should send a header/version frame right away.

## 2. iroh-relay self-hosting [V unless marked]

* Install with `cargo install iroh-relay --features server` (13 MB binary, glibc ≥2.34, about 1m20s with -j4).
  An official docker image `n0computer/iroh-relay` and GitHub release binaries also exist **[U: docs page says so]**.
* Run: `iroh-relay --dev [-c cfg.toml]` serves plain HTTP on :3340. Without `--dev`, the defaults are HTTP :80,
  HTTPS :443, QUIC/QAD UDP :7842 and metrics :9090 (`enable_metrics=false` turns metrics off). Endpoints are
  `GET /relay` (WebSocket upgrade, also `/derp`), `/ping`, `/generate_204` and `/healthz`.
* Config (TOML, from `main.rs`): `enable_relay`, `http_bind_addr`, `enable_quic_addr_discovery` (default
  **false**, and it **requires `[tls]`**), `limits`, `key_cache_capacity`, `access`, and
  `[tls] { https_bind_addr, quic_bind_addr, hostname, cert_mode = Manual|LetsEncrypt|Reloading, cert_dir,
  manual_cert_path, manual_key_path, prod_tls, contact, dangerous_http_only }`. In `--dev`,
  `dangerous_http_only` serves HTTP while still running QUIC with the TLS cert.
* **Access control** (built in): `access = "everyone"`, `access.allowlist = ["<endpoint-id>", ...]`,
  `access.denylist`, `access.shared_token = [..]` (client sends `Authorization: Bearer`, set through
  `RelayConfig::with_auth_token`), or `access.http.url` (POST with header `X-Iroh-NodeId`; a 200 response
  containing `true` allows the client). The relay authenticates the endpoint id by challenge signature, or by a
  TLS exporter (RFC 9729-style), which falls back to the challenge behind proxies. **→ jaunt should use
  `allowlist` with its 4 machine ids, or a shared token.**
* **What the relay sees:** client IPs, the endpoint ids of both sides (`Datagrams{dst_endpoint_id}` /
  `{remote_endpoint_id}`), timing and sizes. Payload is end-to-end QUIC/TLS encrypted. Hole-punch candidate
  addresses travel inside the encrypted connection (QNT frames).
* **Behind a reverse proxy or Cloudflare tunnel:** relaying is plain HTTP plus a WebSocket on `/relay`, so a
  WS-capable proxy works in principle **[U: CF tunnel not touched, by rule]**. However **QAD always dials
  `<relay-url-host>:7842/udp`** (`RelayQuicConfig` has only a port, and `reportgen.rs` uses `url.host_str()`),
  so a CF-proxied hostname can never provide QAD. A RelayMap can hold two entries: CF for fallback, and a
  direct UDP-reachable host for QAD.
* Fly.io: needs UDP and a TLS cert on the box. Fly supports UDP via the `fly-global-services` bind **[U]**.
  Any $4 VPS with 80/443/tcp + 7842/udp is simpler.

### Measured: is QAD needed? (mini NAT lab, 2 Linux MASQUERADE routers, `jaunt-sp-*` containers, now removed)

| relay mode | result between two NATed peers |
|---|---|
| `--dev` HTTP only (equivalent to behind CF tunnel) | relay only for 10 s, never direct, `remote_candidates` were only private LAN IPs **[V]** |
| TLS (own CA) + `enable_quic_addr_discovery = true` | QAD found `10.232.0.12:port` (post-NAT). **Direct path selected 247 ms after start**, rtt 0.1 ms vs relay 1.4 ms **[V]** |

Gotcha seen in the lab, and also in the colleague's harness `router.sh`: a NAT mapping "poisoned" by an earlier
failed punch (Linux conntrack clash, so the source port was re-mapped 32936→3517) kept failing until the
listener restarted. Routers must silently DROP unsolicited WAN input. Real-world impact is **[U]**.

## 3. Spike (`spike/` in this repository)

`src/main.rs` (≈240 lines): `listen [--key f] [--allow id..]` and `dial <id> [--addr ip:port] [--bare] [--secs N]`,
plus the global options `--relay URL | --no-relay` and `--relay-ca ca.pem`. It uses `presets::Minimal`,
`RelayMode::custom`, a custom OS DNS resolver, the allowlist hook and an echo over one bi-stream. Every second it
prints the selected path and per-path RTT, and it logs path events.
The relay is at `spike/.tools/bin/iroh-relay`, and test configs and certs are in `spike/run/`.

Results [V]:
* Localhost, dev relay: handshake 11-35 ms after bind (relay path). The **direct path was selected at +12 to
  +52 ms** (`PathEvent::Selected`). App RTT is about 0.6-1 ms direct vs about 5 ms relay.
* `--no-relay --addr 127.0.0.1:port`: direct only, handshake 30 ms.
* `--bare` (id only, no lookup) gives `Error: No addressing information available ... No address lookup configured`.
* Docker, 2 networks: the dialer was on both, then `docker network disconnect` removed one at runtime. The stream
  **never broke**: it fell back to relay at +7.35 s and a new direct path came at +8.00 s. Worst ping was 113 ms,
  then the same happened again on the next change.
* Release build, `iroh default-features=false, features=["tls-ring"]`, `opt-level="s"`, fat LTO,
  `panic=abort`, strip: **7.3 MB**. With default features and thin LTO it is 17 MB. Slim drops `portmapper`
  (UPnP/NAT-PMP/PCP) and `metrics`. **Keep `portmapper` for jaunt**, since it helps a home desktop. Its size
  cost was not measured **[U]**.
* `ldd`: only libc/libm/libgcc_s. **No OpenSSL**: there is no `openssl-sys` in the tree, and `cargo tree -i`
  shows only `openssl-probe` (pure Rust cert-path finder, via `rustls-platform-verifier` ← `reqwest`). Crypto is ring.
* Idle listener RSS **13-14 MB** (18 threads). Dialer RSS during a session is 12-14 MB. Dev relay RSS is 19 MB.
* **Static musl build works:** in `debian:12` I installed `gcc libc6-dev musl-tools` and rustup with
  `--profile minimal --target x86_64-unknown-linux-musl` (rustc 1.99.0). The build took 74 s and produced
  `static-pie`, 7.4 MB, and that binary dialled successfully on the host. Download was about 100 MB apt plus
  the rustup toolchain (818 MB unpacked inside the throwaway container). The host has no rustup and no
  musl target (Arch `rust` 1.97.1), and installing them needs root. `rust:alpine` is 352 MB compressed
  (≈1 GB unpacked), so I did not pull it.
* The default iroh DNS resolver in the lab: system DNS SERVFAIL, so it logged
  `escalating to fallback` and `connecting to 1.1.1.1:443`. **[V]** With `OsResolver` there was no external
  contact and `/etc/hosts` was honoured.

## 4. Security review status
* iroh / noq / iroh-relay: **no public third-party audit found** (GitHub security tab: no advisories and no
  SECURITY.md; searches on iroh.computer). The iroh 1.1.0 blog (2026-09-01) lists 3 fixes: a malicious relay
  could pin a CPU at 100% (datagram batch), a panic when deserialising `CustomAddr`, and NAT probes redirected
  to third parties. The report links are placeholders. → **Pin ≥1.3.0. Restrict the relay with the allowlist.
  Treat the relay as untrusted but low impact** (it cannot read traffic).
* rustls (with ring and webpki): Cure53 audit, May-June 2020, funded by CNCF, "exceptionally high standard"
  (jbp.io/2020/06/14/rustls-audit.html) **[V]**. quinn: no formal audit known **[U]**, and noq has diverged from it.
* spake2 (RustCrypto): README says "never received an independent third party audit". It has Ed25519 group
  33-byte messages, is compatible with python-spake2 (and magic-wormhole uses python-spake2 **[U]**), and the
  last stable release was 2023. **Acceptable** for a 4-machine pairing ritual if the code is low-entropy and
  single-use, the SPAKE2 key confirms the two ed25519 ids exchanged inside the iroh connection, and attempts are
  rate-limited. Alternative: CPace / `pake-cpace` **[U]**.

## 5. Ecosystem to reuse
* **iroh-tickets `EndpointTicket`**: `"endpoint" + base32(postcard(EndpointAddr))`, trait `Ticket`.
  It is long (~60-100 chars) and not a short code, but useful for an out-of-band "add machine" string.
* **dumbpipe** (`listen` / `connect <ticket>`, `listen-tcp`, `listen-unix`, `IROH_SECRET` env key, HANDSHAKE
  bytes, `create_short_ticket` = id + relay urls). It uses `presets::N0`, so do not copy that. Its pattern of
  key-from-env and "short ticket = id + relay" is worth copying.
* sendme: blobs, not relevant.
* No pairing or short-code crate exists in the iroh ecosystem as far as I found **[U]**, so jaunt writes its own
  SPAKE2-over-relay ritual. Do the SPAKE2 exchange *inside* an iroh connection to a ticket that carries a
  one-time endpoint id. Alternatively the host can accept any id on a special `jaunt/pair/1` ALPN for 2 minutes
  and run SPAKE2 there. Either way TLS already gives channel binding: hash both endpoint ids into the SPAKE2
  identity or confirmation.
* PTY: `portable-pty` (cross-platform including Windows ConPTY, sync API, wezterm) vs `pty-process`
  (Unix, tokio-async). Host side is tmux on Linux and macOS. If a Windows laptop must *host*, portable-pty is
  the only option. Clients do not need a PTY, only raw-mode stdin through `crossterm` **[U]**.

## 6. Cargo lines

```toml
iroh = { version = "1.3", default-features = false, features = ["tls-ring", "portmapper"] }
iroh-tickets = "1"                      # optional, for paste-able machine tickets
iroh-mdns-address-lookup = "0.6"        # optional, LAN discovery without relay (untested)
n0-future = "0.3"                       # StreamExt for path_events
n0-error = "1"                          # only for the custom Resolver impl
rustls-pki-types = "1"                  # only if trusting a private relay CA
spake2 = "0.4"
tokio = { version = "1", features = ["full"] }
# relay: cargo install iroh-relay@1.3.0 --features server --locked
[profile.release]
lto = "fat"; codegen-units = 1; opt-level = "s"; strip = true; panic = "abort"
```

## 7. API cheat sheet (compiles against iroh 1.3.0; condensed from spike/src/main.rs)

```rust
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey,
    endpoint::{AfterHandshakeOutcome, Connection, EndpointHooks, QuicTransportConfig, presets}};
const ALPN: &[u8] = b"jaunt/0";

// identity: persist 32 bytes; EndpointId == PublicKey (Display/FromStr = 64 hex chars)
let sk = SecretKey::generate();               // SecretKey::from_bytes(&[u8;32]), sk.to_bytes()
let id: EndpointId = sk.public();

// endpoint: NO n0 services
let relay: RelayUrl = "https://relay.example.org".parse()?;
let ep = Endpoint::builder(presets::Minimal)  // crypto provider only; no lookup, no relays
    .secret_key(sk)
    .relay_mode(RelayMode::custom([relay.clone()]))  // or RelayMode::Custom(RelayMap) w/ auth_token
    .dns_resolver(iroh::dns::DnsResolver::custom(OsResolver))  // avoid 1.1.1.1 fallback
    // .ca_tls_config(iroh::tls::CaTlsConfig::custom_roots(certs)) // private relay CA
    .alpns(vec![ALPN.to_vec()])
    .transport_config(QuicTransportConfig::builder()
        .keep_alive_interval(Duration::from_secs(5))
        .max_idle_timeout(Some(Duration::from_secs(60).try_into()?)).build())
    .hooks(Allowlist(set))                      // see below
    .bind().await?;
ep.online().await;                              // home relay connected (wrap in timeout)

// allowlist (runs after TLS; remote id authenticated)
#[derive(Debug)] struct Allowlist(Arc<HashSet<EndpointId>>);
impl EndpointHooks for Allowlist {
    async fn after_handshake<'a>(&'a self, c: &'a Connection) -> AfterHandshakeOutcome {
        if self.0.contains(&c.remote_id()) { AfterHandshakeOutcome::accept() }
        else { AfterHandshakeOutcome::Reject { error_code: 403u32.into(), reason: b"not paired".to_vec() } }
    }
}

// accept loop
while let Some(inc) = ep.accept().await {
    tokio::spawn(async move {
        let conn = inc.await?;                          // hooks already ran
        let (mut send, mut recv) = conn.accept_bi().await?;  // fires only after peer writes
        /* ... */ anyhow::Ok(())
    });
}

// dial: minimum = id + relay URL (no address lookup configured)
let conn = ep.connect(EndpointAddr::new(peer_id).with_relay_url(relay), ALPN).await?;
let (mut send, mut recv) = conn.open_bi().await?;
send.write_all(b"JAUNT1").await?;               // write first so peer's accept_bi returns

// observe path
for p in conn.paths().iter() {
    println!("{} {} {:?} {:?}", p.is_selected(), if p.is_relay() {"relay"} else {"direct"}, p.remote_addr(), p.rtt());
}
let mut ev = conn.path_events();                // 'static Stream<PathEvent>; n0_future::StreamExt
// PathEvent::{Opened{id,remote_addr,..}, Closed{last_stats,..}, Selected{id,remote_addr,..}, Lagged}

conn.close(0u32.into(), b"bye"); ep.close().await;  // always close (else "dropped without close" error log)
```

OsResolver (honours /etc/hosts and has no public fallback). It implements `iroh::dns::Resolver`:
`lookup_ipv4` and `lookup_ipv6` use `tokio::net::lookup_host((host,0))` and filter by family,
`lookup_txt` returns `Err(DnsError::from(n0_error::AnyError::from("unsupported")))`, `clear_cache` is a
no-op, and `reset` returns `Box::new(self.clone())`. See `spike/src/main.rs`. A simpler variant that also
works but does not read /etc/hosts is
`DnsResolver::builder().with_system_defaults().disable_fallback().build()` **[U: not run]**.

## 8. Reproduce
```
cd spike; export CARGO_TARGET_DIR=$PWD/../target
cargo build --release -j 4
.tools/bin/iroh-relay --dev -c run/relay-dev.toml &        # 127.0.0.1:3340, no metrics
$CARGO_TARGET_DIR/release/jaunt-spike --relay http://127.0.0.1:3340 listen --key run/listen.key
$CARGO_TARGET_DIR/release/jaunt-spike --relay http://127.0.0.1:3340 dial <id> --secs 10
# TLS+QAD relay: run/nat/relay-tls.toml (+ ca.crt / relay.crt / relay.key, test-only CA), client adds --relay-ca run/nat/ca.crt
```
Note: binaries bind-mounted into containers keep the **old inode** after cargo rebuilds, so `docker cp` the
binary in again or mount the directory.

## 9. Open items, not done
* Did not test the colleague's harness (`test/harness`). It was not up, and up.sh builds an Arch image with
  a `pacman -Syu` download. I built my own 5-container lab instead (`jaunt-sp-*`, removed). Still untested:
  symmetric NAT and UDP-blocked networks (the relay over WSS should carry them), the mDNS crate, laptop
  sleep/resume (`jh freeze`), and the cost of the `portmapper` feature.
* Disk used: `target/` is 3.9 GB (debug 2.1 GB, release 1.8 GB). Delete `target/debug` if space is needed.
